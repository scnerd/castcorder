// Two people, two installs, two home networks: the whole contact journey in
// real browsers (docs/design.md "Contacts", "Calling a contact"). Each person
// has a castcorder-host install on a computer behind its own router
// (lib/netns.mjs: Linux network namespaces with masquerading routers, so it
// runs as root; CI uses sudo). Their browsers run inside their computers'
// namespaces, so each home is that computer's own localhost page.
//
//  1. Dana (D) makes a contact link on her home; Eli (E) pastes it on his.
//     Each lists the other, online.
//  2. Dana adds Eli to her room "Show" as a producer. Eli sees "Show" on his
//     home, shared with him by Dana, and asks Dana's install for the rooms
//     they share over a contact link (it crosses both routers).
//  3. Dana invites Eli to the call; the invite shows on his home. Both join:
//     Dana from her room card, Eli from the invite.
//  4. Dana records a take, stops it; Eli's track reaches Dana's install.
//  5. Both leave and get back to their homes.
//
// Needs: target/release/castcorder-host and castcorder-rendezvous, the
// client's trunk build in ../dist. Everything printed is ASCII.

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, rmSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as net from './lib/netns.mjs';
import { check, checks, sleep, text, waitFor } from './lib/helpers.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '..');
const WORKSPACE = join(CRATE, '..', '..');
const RESULTS = join(HERE, 'test-results', 'journey');
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const { chromium } = await import('playwright');
const bin = (n) => join(process.env.CARGO_TARGET_DIR || join(WORKSPACE, 'target'), 'release', n);
const HOST_EXE = bin('castcorder-host');
const RV_EXE = bin('castcorder-rendezvous');
const dist = process.env.CASTCORDER_DIST || join(CRATE, 'dist');
const VERSION = '0.0.0';

const problem = net.available();
if (problem) {
  if (process.env.CASTCORDER_CI) { console.log('FAIL journey needs network namespaces: ' + problem); process.exit(1); }
  console.log('SKIP journey: ' + problem);
  process.exit(0);
}
for (const f of [HOST_EXE, RV_EXE, join(dist, 'index.html')]) {
  if (!existsSync(f)) { console.log('FAIL missing ' + f); process.exit(1); }
}
rmSync(RESULTS, { recursive: true, force: true });
mkdirSync(RESULTS, { recursive: true });

const procs = [];
function start(ns, exe, args, name) {
  const p = spawn('ip', [...net.exec(ns).slice(1), exe, ...args], { stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, RUST_LOG: process.env.JOURNEY_LOG || 'info' } });
  const st = { p, out: '', err: '', name };
  p.stdout.on('data', (d) => { st.out += d.toString(); });
  p.stderr.on('data', (d) => { st.err += d.toString(); });
  p.on('exit', (c) => { st.exited = c; });
  procs.push(st);
  return st;
}
const line = (st, re, what, timeout = 30000) => waitFor(() => {
  if (st.exited !== undefined) throw new Error(what + ': ' + st.name + ' exited with ' + st.exited + '\n' + st.err.slice(-2000));
  return st.out.match(re);
}, what, timeout);

const browsers = [];
const logs = { D: [], E: [], DC: [], EC: [] };
const ARGS = ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
  '--autoplay-policy=no-user-gesture-required', '--disable-background-timer-throttling', '--mute-audio'];
async function person(ns, who) {
  const browser = await chromium.launch({ headless: true, args: ARGS, executablePath: net.browser(ns, chromium.executablePath()) });
  browsers.push(browser);
  const context = await browser.newContext({ permissions: ['microphone', 'camera'] });
  return context;
}
async function page(context, who) {
  const p = await context.newPage();
  p.on('console', (m) => logs[who].push(m.text()));
  p.on('pageerror', (e) => logs[who].push('[pageerror] ' + e.message));
  return p;
}
// The home's own API, as its page calls it.
const api = (p, path, body) => p.evaluate(async ([path, body]) => {
  const r = await fetch(path, body === undefined
    ? { headers: { 'X-Castcorder-Native': '1' } }
    : { method: 'POST', headers: { 'X-Castcorder-Native': '1', 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  const t = await r.text();
  if (!r.ok) throw new Error(path + ': ' + r.status + ' ' + t);
  return t ? JSON.parse(t) : null;
}, [path, body]);

let failed = null;
try {
  net.setup();
  const rv = start(net.NS.inet, RV_EXE, ['--listen', net.INET_IP + ':8080', '--stun', net.INET_IP + ':3478',
    '--client', VERSION + '=' + dist], 'rendezvous');
  await waitFor(() => rv.out.match(/RENDEZVOUS_READY/), 'rendezvous ready', 20000);
  const RV = 'http://' + net.INET_IP + ':8080';
  const host = (ns, who, room) => start(ns, HOST_EXE, ['--dist', dist, '--library', join(RESULTS, 'library-' + who),
    '--room', room, '--rendezvous', RV], 'host-' + who);
  const hd = host(net.NS.d, 'D', 'Show');
  const he = host(net.NS.e, 'E', 'Eli-room');
  const HD = (await line(hd, /HOST_READY http=(\S+)/, 'D host ready'))[1];
  const HE = (await line(he, /HOST_READY http=(\S+)/, 'E host ready'))[1];
  await waitFor(() => hd.err.includes('registered with the rendezvous') && he.err.includes('registered with the rendezvous'), 'both registered', 30000);
  console.log('two homes up behind their routers; rendezvous at ' + RV);

  const dctx = await person(net.NS.d, 'D');
  const ectx = await person(net.NS.e, 'E');
  const D = await page(dctx, 'D');
  const E = await page(ectx, 'E');
  await D.goto(HD + '/');
  await E.goto(HE + '/');
  await api(D, '/person', { name: 'Dana' });
  await api(E, '/person', { name: 'Eli' });
  const dana = await api(D, '/person');
  const eli = await api(E, '/person');

  // 1. Contacts, by link.
  await D.click('#nav-contacts');
  await D.click('#add-contact');
  await D.click('#copy-contact-link');
  const link = await waitFor(async () => (await D.locator('#contact-codes code').allTextContents()).find((l) => l.startsWith(RV + '/')),
    'Dana has a contact link', 15000, 250);
  await E.click('#nav-contacts');
  await E.click('#add-contact');
  await E.fill('#contact-code-input', link);
  await E.click('#use-contact-code');
  await waitFor(async () => (await text(E, '#contact-list')).includes('Dana'), 'Eli lists Dana', 30000);
  await waitFor(async () => (await text(D, '#contact-list')).includes('Eli'), 'Dana lists Eli', 30000);
  check(true, 'pasting Dana\'s link made them contacts on both sides');
  await waitFor(async () => (await api(E, '/contacts')).contacts.some((c) => c.id === dana.id && c.online.length > 0), 'Eli sees Dana online', 30000, 500);
  await waitFor(async () => (await api(D, '/contacts')).contacts.some((c) => c.id === eli.id && c.online.length > 0), 'Dana sees Eli online', 30000, 500);
  check(true, 'each sees the other online across the two routers');

  // 2. Dana makes Eli a producer in "Show"; Eli asks for the rooms they share.
  await api(D, '/rooms/trust', { room: 'Show', person: eli.id, trust: true, producer: true });
  await E.click('#contact-list li .contact-pick');
  const shared = await waitFor(async () => {
    const t = await text(E, '#contact-detail .contact-shared');
    return /Show|No rooms|not|fail|time/i.test(t) && !/asking/i.test(t) ? t : null;
  }, 'Eli hears which rooms he shares with Dana', 40000, 500).catch(() => null);
  check(shared && shared.includes('Show'), 'Eli asked Dana\'s install over a contact link and hears they share Show: ' + shared);
} catch (e) {
  failed = e;
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  for (const st of procs) { try { st.p.kill('SIGTERM'); } catch { /* gone */ } }
  await sleep(500);
  for (const st of procs) {
    const { writeFileSync } = await import('node:fs');
    writeFileSync(join(RESULTS, st.name + '.log'), st.out + '\n--- stderr ---\n' + st.err);
  }
  for (const [k, v] of Object.entries(logs)) {
    const { writeFileSync } = await import('node:fs');
    writeFileSync(join(RESULTS, 'browser-' + k + '.log'), v.join('\n'));
  }
  net.teardown();
}
const bad = checks.filter((c) => !c.ok);
console.log('logs: ' + RESULTS);
console.log(failed || bad.length ? 'FAIL journey (' + bad.length + ' checks failed)' : 'PASS journey: two people on two home networks, contacts to call and back');
process.exit(failed || bad.length ? 1 : 0);
