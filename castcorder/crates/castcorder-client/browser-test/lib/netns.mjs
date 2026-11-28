// Two home networks behind their own routers, for tests of two people on
// two installs (journey.mjs). Linux network namespaces, so it needs root
// (CI runs it with sudo):
//
//   inet  203.0.113.1/24 on a bridge: the internet (the rendezvous and its
//         STUN responder listen here)
//   rd    router of home D: wan 203.0.113.11, lan 10.1.0.1, masquerading
//   d     D's computer: 10.1.0.2, default route via rd
//   re/e  the same for home E: wan 203.0.113.12, lan 10.2.0.1; e is 10.2.0.2
//
// The homes cannot reach each other's private addresses: only through the
// routers' public ones, as two real homes. Masquerading keeps a flow's
// port where it can, and lets in only replies to what went out (no inbound
// mapping), so a peer is reached through ICE checks from both sides.
// `browser(ns)` gives an executable path that runs Playwright's Chromium
// inside a namespace, so each person's browser sees its own computer's
// localhost. Everything printed is ASCII.

import { execFileSync } from 'node:child_process';
import { writeFileSync, chmodSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const PREFIX = 'cc' + (process.pid % 10000) + '-';
export const NS = { inet: PREFIX + 'inet', rd: PREFIX + 'rd', d: PREFIX + 'd', re: PREFIX + 're', e: PREFIX + 'e' };
export const INET_IP = '203.0.113.1';

function run(...args) {
  execFileSync(args[0], args.slice(1), { stdio: ['ignore', 'ignore', 'pipe'] });
}
const inNs = (ns, ...args) => run('ip', 'netns', 'exec', ns, ...args);

// Whether this process may make namespaces (root, and `ip` works).
export function available() {
  try {
    if (process.getuid && process.getuid() !== 0) return 'not root (run with sudo)';
    run('ip', 'netns', 'list');
    run('iptables', '--version');
    return null;
  } catch (e) {
    return String(e.message || e).split('\n')[0];
  }
}

export function setup() {
  teardown();
  for (const ns of Object.values(NS)) run('ip', 'netns', 'add', ns);
  for (const ns of Object.values(NS)) inNs(ns, 'ip', 'link', 'set', 'lo', 'up');
  inNs(NS.inet, 'ip', 'link', 'add', 'br0', 'type', 'bridge');
  inNs(NS.inet, 'ip', 'addr', 'add', INET_IP + '/24', 'dev', 'br0');
  inNs(NS.inet, 'ip', 'link', 'set', 'br0', 'up');
  const home = (router, pc, wan, lanNet) => {
    // router wan <-> inet bridge
    run('ip', 'link', 'add', 'w-' + router.slice(-2), 'netns', router, 'type', 'veth', 'peer', 'name', 'p-' + router.slice(-2), 'netns', NS.inet);
    inNs(router, 'ip', 'link', 'set', 'w-' + router.slice(-2), 'name', 'wan');
    inNs(NS.inet, 'ip', 'link', 'set', 'p-' + router.slice(-2), 'master', 'br0');
    inNs(NS.inet, 'ip', 'link', 'set', 'p-' + router.slice(-2), 'up');
    inNs(router, 'ip', 'addr', 'add', wan + '/24', 'dev', 'wan');
    inNs(router, 'ip', 'link', 'set', 'wan', 'up');
    // router lan <-> the computer
    run('ip', 'link', 'add', 'l-' + router.slice(-2), 'netns', router, 'type', 'veth', 'peer', 'name', 'eth-' + pc.slice(-1), 'netns', pc);
    inNs(router, 'ip', 'link', 'set', 'l-' + router.slice(-2), 'name', 'lan');
    inNs(pc, 'ip', 'link', 'set', 'eth-' + pc.slice(-1), 'name', 'eth0');
    inNs(router, 'ip', 'addr', 'add', lanNet + '.1/24', 'dev', 'lan');
    inNs(router, 'ip', 'link', 'set', 'lan', 'up');
    inNs(pc, 'ip', 'addr', 'add', lanNet + '.2/24', 'dev', 'eth0');
    inNs(pc, 'ip', 'link', 'set', 'eth0', 'up');
    inNs(pc, 'ip', 'route', 'add', 'default', 'via', lanNet + '.1');
    inNs(router, 'sysctl', '-qw', 'net.ipv4.ip_forward=1');
    inNs(router, 'iptables', '-t', 'nat', '-A', 'POSTROUTING', '-o', 'wan', '-j', 'MASQUERADE');
    // Only replies come in from the internet (a home router without port forwards).
    inNs(router, 'iptables', '-A', 'FORWARD', '-i', 'wan', '-m', 'conntrack', '--ctstate', 'ESTABLISHED,RELATED', '-j', 'ACCEPT');
    inNs(router, 'iptables', '-A', 'FORWARD', '-i', 'wan', '-j', 'DROP');
    // Unsolicited packets to the router itself are dropped before conntrack
    // keeps them: a kept one would take the public port a later outgoing
    // flow from the computer needs, and so change that flow's mapping.
    inNs(router, 'iptables', '-t', 'raw', '-A', 'PREROUTING', '-i', 'wan', '-d', wan, '-m', 'conntrack', '--ctstate', 'NEW,INVALID,UNTRACKED', '-j', 'ACCEPT');
    inNs(router, 'iptables', '-A', 'INPUT', '-i', 'wan', '-m', 'conntrack', '--ctstate', 'NEW,INVALID', '-j', 'DROP');
  };
  home(NS.rd, NS.d, '203.0.113.11', '10.1.0');
  home(NS.re, NS.e, '203.0.113.12', '10.2.0');
}

// Black-hole one TCP flow at the internet end, both ways, for good: every
// packet between INET_IP and the home's public `ip`:`port` (its router's
// mapping of that connection) is dropped, so neither end ever hears from
// the other again, not even a FIN or a reset. A connection that died
// quietly, as one to a redeployed Worker rendezvous can.
export function blackhole(ip, port) {
  inNs(NS.inet, 'iptables', '-I', 'INPUT', '-p', 'tcp', '-s', ip, '--sport', String(port), '-j', 'DROP');
  inNs(NS.inet, 'iptables', '-I', 'OUTPUT', '-p', 'tcp', '-d', ip, '--dport', String(port), '-j', 'DROP');
}

// The routers of the two homes, by home ('d', 'e').
export const ROUTER = { d: NS.rd, e: NS.re };

// Faults at a home's router, on its link to the internet (lib/world.mjs's
// nemesis). Each returns the function that undoes it.
// - cut: nothing passes either way (a dropped uplink, a partition).
// - lossy: each packet either way is dropped with probability `p`.
export function cut(home) {
  return routerRules(home, [['-j', 'DROP']]);
}
export function lossy(home, p) {
  return routerRules(home, [['-m', 'statistic', '--mode', 'random', '--probability', String(p), '-j', 'DROP']]);
}
function routerRules(home, matches) {
  const ns = ROUTER[home];
  const rules = [];
  for (const m of matches) {
    for (const [chain, dir] of [['FORWARD', '-i'], ['FORWARD', '-o'], ['INPUT', '-i'], ['OUTPUT', '-o']]) {
      rules.push([chain, dir, 'wan', ...m]);
    }
  }
  for (const r of rules) inNs(ns, 'iptables', '-I', ...r);
  return () => { for (const r of rules) { try { inNs(ns, 'iptables', '-D', ...r); } catch { /* gone */ } } };
}

export function teardown() {
  for (const ns of Object.values(NS)) {
    try { run('ip', 'netns', 'del', ns); } catch { /* not there */ }
  }
}

// argv prefix that runs a command inside `ns`.
export const exec = (ns) => ['ip', 'netns', 'exec', ns];

// An executable that runs Playwright's Chromium (`exe`) inside `ns`.
export function browser(ns, exe) {
  const dir = mkdtempSync(join(tmpdir(), 'cc-netns-'));
  const path = join(dir, 'chromium-' + ns);
  writeFileSync(path, '#!/bin/sh\nexec ip netns exec ' + ns + ' "' + exe + '" "$@"\n');
  chmodSync(path, 0o755);
  return path;
}
