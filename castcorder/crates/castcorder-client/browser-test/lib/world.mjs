// The world a story test (a journey) runs in: the services it reaches,
// handed out only here, and only implementations that keep the protocol.
// A journey never starts a server itself (a pre-commit hook enforces it), so
// it cannot pass against a stand-in that behaves differently from production.
//
// rendezvous(): the Cloudflare Worker under `wrangler dev` by default (real
// workerd and Durable Objects, ADR 0004), with STUN from a separate server,
// as production's stun.cloudflare.com is. CASTCORDER_RENDEZVOUS=native picks
// castcorder-rendezvous instead (the self-hosted one; no node needed). Both
// are held to one contract by castcorder-rendezvous/tests/conformance.rs
// (`task test:conformance`), which is what makes either a fair world.
//
// The Worker needs castcorder/worker built (task worker:build) and wrangler
// installed (task worker:tools); under CASTCORDER_CI a missing one fails.

import { cpSync, existsSync, mkdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { waitFor } from './helpers.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '..', '..');
const WORKSPACE = join(CRATE, '..', '..');
const WORKER_DIR = join(WORKSPACE, 'worker');
const WRANGLER = join(WORKER_DIR, 'node_modules', 'wrangler', 'bin', 'wrangler.js');
const bin = (n) => join(process.env.CARGO_TARGET_DIR || join(WORKSPACE, 'target'), 'release', n);
export const RV_EXE = bin('castcorder-rendezvous');

/// Which rendezvous this run uses, or why it cannot run.
export function rendezvousKind() {
  const kind = process.env.CASTCORDER_RENDEZVOUS || 'worker';
  if (kind === 'native') return { kind, missing: existsSync(RV_EXE) ? null : RV_EXE };
  if (kind !== 'worker') return { kind, missing: 'CASTCORDER_RENDEZVOUS must be worker or native' };
  for (const f of [join(WORKER_DIR, 'build'), WRANGLER, RV_EXE]) {
    if (!existsSync(f)) return { kind, missing: f + ' (task worker:build, task worker:tools)' };
  }
  return { kind, missing: null };
}

/// The rendezvous at `ip`:`port` in network namespace `ns` (STUN on
/// `stunPort`), serving the client build `dist` as version `version`.
/// `start(ns, exe, args, name, opts)` is the journey's process starter
/// (its logs, its cleanup); opts may carry `cwd` and `detached`.
/// Returns { kind, http, restart(), stop() }: restart() is a redeploy (the
/// process killed outright and started again on the same port and state).
export async function rendezvous({ start, ns, ip, port, stunPort, dist, version }) {
  const { kind, missing } = rendezvousKind();
  if (missing) throw new Error('rendezvous (' + kind + ') unavailable: ' + missing);
  const http = 'http://' + ip + ':' + port;
  if (kind === 'native') return native({ start, ns, ip, port, stunPort, dist, version, http });
  return worker({ start, ns, ip, port, stunPort, dist, version, http });
}

async function ready(st, re, what, timeout) {
  await waitFor(() => {
    if (st.exited !== undefined) throw new Error(what + ': ' + st.name + ' exited with ' + st.exited + '\n' + (st.out + st.err).slice(-2000));
    return re.test(st.out + st.err);
  }, what, timeout, 250);
}

function kill(st) {
  // Detached processes lead their own group (wrangler runs workerd as a child).
  try { process.kill(-st.p.pid, 'SIGKILL'); } catch { try { st.p.kill('SIGKILL'); } catch { /* gone */ } }
}

async function gone(st) {
  await waitFor(() => st.exited !== undefined, st.name + ' gone', 10000, 100);
}

async function native({ start, ns, ip, port, stunPort, dist, version, http }) {
  const args = ['--listen', ip + ':' + port, '--stun', ip + ':' + stunPort, '--client', version + '=' + dist];
  let n = 0;
  const up = async () => {
    n += 1;
    const st = start(ns, RV_EXE, args, n === 1 ? 'rendezvous' : 'rendezvous-' + n, { detached: true });
    await ready(st, /RENDEZVOUS_READY/, 'rendezvous ready', 20000);
    return st;
  };
  let st = await up();
  return {
    kind: 'native',
    http,
    async restart() { kill(st); await gone(st); st = await up(); },
    async stop() { kill(st); },
  };
}

async function worker({ start, ns, ip, port, stunPort, dist, version, http }) {
  // STUN on its own (production: stun.cloudflare.com); its HTTP side is unused.
  const stun = start(ns, RV_EXE, ['--listen', ip + ':' + (port + 1), '--stun', ip + ':' + stunPort], 'stun', { detached: true });
  await ready(stun, /RENDEZVOUS_READY/, 'STUN server ready', 20000);
  const assets = join(WORKER_DIR, 'assets', 'v', version);
  rmSync(assets, { recursive: true, force: true });
  mkdirSync(dirname(assets), { recursive: true });
  cpSync(dist, assets, { recursive: true });
  const state = join(tmpdir(), 'castcorder-world-' + process.pid);
  rmSync(state, { recursive: true, force: true });
  mkdirSync(state, { recursive: true });
  const args = [WRANGLER, 'dev', '--ip', ip, '--port', String(port), '--inspector-port', String(port + 2),
    '--persist-to', state, '--var', 'STUN_URLS:stun:' + ip + ':' + stunPort, '--var', 'RELAY:none'];
  let n = 0;
  const up = async () => {
    n += 1;
    const st = start(ns, process.execPath, args, n === 1 ? 'rendezvous' : 'rendezvous-' + n,
      { cwd: WORKER_DIR, detached: true, env: { WRANGLER_SEND_METRICS: 'false', NO_COLOR: '1' } });
    await ready(st, /Ready on http/, 'wrangler dev ready', 120000);
    return st;
  };
  let st = await up();
  return {
    kind: 'worker',
    http,
    async restart() { kill(st); await gone(st); st = await up(); },
    async stop() { kill(st); kill(stun); rmSync(state, { recursive: true, force: true }); },
  };
}

// ---------------------------------------------------------------- nemesis
//
// Faults by default, in the Jepsen manner: with CASTCORDER_NEMESIS_SEED set,
// every step boundary a journey marks (`await nem.step('name')`) may start
// one fault, chosen by a PRNG from that seed, so a failing run replays
// exactly with the same seed. Faults are ones the product claims to
// survive, each healed before the next boundary or within seconds:
//
// - redeploy: the rendezvous killed outright and started again;
// - partition: one home's uplink cut for 3 to 10 s, then restored;
// - loss: one home's uplink drops 2 to 8 % of packets until the next step.
//
// The journey's checks are invariants with a time budget: they must hold
// once faults heal. A step whose precondition a fault would break (a take
// needs its guests in the call when it starts) passes { healed: true }:
// a partition then runs and heals before step() returns, and the journey
// waits for the call to settle again. Without a seed (the clean run)
// step() does nothing.

function mulberry32(a) {
  return () => {
    a |= 0; a = (a + 0x6D2B79F5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/// The nemesis for a journey on network `net` (lib/netns.mjs) with
/// rendezvous `rv` (from rendezvous()). `log(line)` records what it did.
export function nemesis({ net, rv, log = console.log }) {
  const raw = process.env.CASTCORDER_NEMESIS_SEED;
  const seed = raw ? Number.parseInt(raw, 10) >>> 0 : null;
  const pending = [];
  const timers = [];
  if (seed === null) return { seed, async step() {}, async heal() {} };
  log('nemesis: seed ' + seed + ' (replay with CASTCORDER_NEMESIS_SEED=' + seed + ')');
  const rnd = mulberry32(seed);
  const pick = (xs) => xs[Math.floor(rnd() * xs.length)];
  const heal = async () => {
    for (const t of timers.splice(0)) clearTimeout(t);
    for (const undo of pending.splice(0)) undo();
  };
  return {
    seed,
    heal,
    async step(name, { healed = false } = {}) {
      await heal();
      if (rnd() < 0.35) { log('nemesis before ' + name + ': none'); return; }
      const home = pick(['d', 'e']);
      const fault = pick(['redeploy', 'partition', 'loss']);
      if (fault === 'redeploy') {
        log('nemesis before ' + name + ': redeploy the rendezvous');
        await rv.restart();
      } else if (fault === 'partition') {
        const ms = 3000 + Math.floor(rnd() * 7000);
        log('nemesis before ' + name + ': cut home ' + home + ' for ' + ms + ' ms');
        const undo = net.cut(home);
        if (healed) {
          await new Promise((r) => setTimeout(r, ms));
          undo();
          log('nemesis: home ' + home + ' back before ' + name);
          return;
        }
        pending.push(undo);
        timers.push(setTimeout(() => { const i = pending.indexOf(undo); if (i >= 0) pending.splice(i, 1); undo(); }, ms));
      } else {
        const p = Math.round((0.02 + rnd() * 0.06) * 100) / 100;
        log('nemesis before ' + name + ': home ' + home + ' loses ' + Math.round(p * 100) + '% of packets');
        pending.push(net.lossy(home, p));
      }
    },
  };
}
