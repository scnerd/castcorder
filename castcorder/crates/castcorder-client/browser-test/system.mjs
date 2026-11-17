// Tier 3 system test: a guest joins by call link only, through the rendezvous.
//
// Real processes on this machine (no Docker: simpler and reliable on Windows):
//   castcorder-rendezvous  WebSocket mailbox + STUN + the client build at /v/<version>/
//   castcorder-host        --rendezvous http://127.0.0.1:<port>, bound to loopback
//   headless Chromium      guest A: the producer, on the host's localhost page
//                          (LAN mode, POST /offer), fed a deterministic WAV;
//                          guest B: opens only the CALL_LINK the host printed;
//                          guest C: the same link with a wrong call secret.
// Checks: the host registers and learns its server-reflexive address over
// STUN; B loads the page from the rendezvous and joins through it; C is
// rejected; B hears A; a take records and B's track syncs into the host's
// library; the rendezvous log never contains the call secret or any SDP.
//
// Every port is picked by the OS (none collide with a running `task host`).
// CARGO_TARGET_DIR and CASTCORDER_DIST select other build dirs.
// Run from the repo root with: task test:system
//
// CASTCORDER_RENDEZVOUS=worker runs the same test against the Cloudflare
// Worker (castcorder/worker, ADR 0004) under `wrangler dev` instead of
// castcorder-rendezvous: the client build is staged as its static assets, the
// host and guests reach their host key's Durable Object, and there is no STUN
// port (the host-srflx check is skipped; loopback ICE needs none). Build it
// first (task worker:build); task test:system:worker does both.
// Everything printed is ASCII. Logs go to browser-test/test-results/system-*.

import { spawn, spawnSync } from 'node:child_process';
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeInput, writeWav16 } from './gen-wav.mjs';
import { callDir } from './lib/helpers.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '..');
const WORKSPACE = join(CRATE, '..', '..');
const RESULTS = join(HERE, 'test-results');
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const { chromium } = await import('playwright');

const bin = (n) => join(process.env.CARGO_TARGET_DIR || join(WORKSPACE, 'target'), 'release',
  process.platform === 'win32' ? n + '.exe' : n);
const HOST_EXE = bin('castcorder-host');
const RV_EXE = bin('castcorder-rendezvous');
const dist = process.env.CASTCORDER_DIST || join(CRATE, 'dist');
const WORKER = process.env.CASTCORDER_RENDEZVOUS === 'worker';
const WORKER_DIR = join(WORKSPACE, 'worker');
const WRANGLER = join(WORKER_DIR, 'node_modules', 'wrangler', 'bin', 'wrangler.js');
const needed = WORKER ? [join(WORKER_DIR, 'build', 'index.js'), WRANGLER] : [RV_EXE];
for (const f of [HOST_EXE, ...needed, join(dist, 'index.html')]) {
  if (!existsSync(f)) { console.log('FAIL missing ' + f); process.exit(1); }
}
const VERSION = '0.0.0';

mkdirSync(RESULTS, { recursive: true });
const INPUT_WAV = join(RESULTS, 'input-44k-s16-stereo.wav');
writeWav16(INPUT_WAV, makeInput());
const LIBRARY = join(RESULTS, 'library-system');
rmSync(LIBRARY, { recursive: true, force: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(fn, what, timeout = 30000, interval = 200) {
  const t0 = Date.now();
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() - t0 > timeout) throw new Error('timeout waiting for ' + what);
    await sleep(interval);
  }
}

const procs = [];
function start(exe, args, name, opts = {}) {
  // Detached on POSIX so the whole group (wrangler and its workerd) can be killed.
  const p = spawn(exe, args, { stdio: ['ignore', 'pipe', 'pipe'], detached: process.platform !== 'win32', ...opts });
  const st = { p, out: '', err: '' };
  p.stdout.on('data', (d) => { st.out += d.toString(); });
  p.stderr.on('data', (d) => { st.err += d.toString(); });
  p.on('exit', (c) => { st.exited = c; });
  procs.push([name, st]);
  return st;
}
const line = (st, re, what, timeout = 20000) => waitFor(() => {
  if (st.exited !== undefined) throw new Error(what + ': process exited with ' + st.exited);
  return st.out.match(re);
}, what, timeout);

const checks = [];
function check(ok, what) {
  checks.push({ ok, what });
  console.log((ok ? 'ok   ' : 'BAD  ') + what);
}

const logs = { A: [], B: [], C: [] };
const browsers = [];
async function openGuest(name, url, extraArgs = []) {
  const browser = await chromium.launch({
    headless: true,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
      '--allow-loopback-in-peer-connection', '--autoplay-policy=no-user-gesture-required',
      '--disable-background-timer-throttling', '--mute-audio', ...extraArgs],
  });
  browsers.push(browser);
  const ctx = await browser.newContext({ permissions: ['microphone', 'camera'] });
  const page = await ctx.newPage();
  page.on('console', (m) => logs[name].push(m.text()));
  page.on('pageerror', (e) => logs[name].push('[pageerror] ' + e.message));
  await page.goto(url);
  await page.waitForSelector('#join');
  return page;
}
const text = async (page, sel) => (await page.textContent(sel)) || '';
const json = async (page, sel) => { const t = await text(page, sel); return t ? JSON.parse(t) : null; };
const audioFrom = (inbound, origin) => (inbound || []).filter((i) => i.kind === 'audio' && i.origin.startsWith(origin + '-'))
  .reduce((a, i) => ({ packets: a.packets + i.packetsReceived, bytes: a.bytes + i.bytesReceived }), { packets: 0, bytes: 0 });

function killTree(p) {
  if (p.exitCode !== null) return;
  if (process.platform === 'win32') spawnSync('taskkill', ['/pid', String(p.pid), '/T', '/F'], { stdio: 'ignore' });
  else { try { process.kill(-p.pid, 'SIGTERM'); } catch { p.kill(); } }
}
const freePort = () => new Promise((resolve, reject) => {
  const srv = createServer();
  srv.on('error', reject);
  srv.listen(0, '127.0.0.1', () => { const { port } = srv.address(); srv.close(() => resolve(port)); });
});

// The Worker under `wrangler dev`: the client build staged as its assets at
// /v/<VERSION>/, local state in a fresh temp dir, no STUN servers in hellos.
async function startWorker() {
  const assets = join(WORKER_DIR, 'assets', 'v', VERSION);
  rmSync(assets, { recursive: true, force: true });
  cpSync(dist, assets, { recursive: true });
  const [port, inspector] = [await freePort(), await freePort()];
  const state = join(tmpdir(), 'castcorder-wrangler-' + process.pid);
  rmSync(state, { recursive: true, force: true });
  const st = start(process.execPath, [WRANGLER, 'dev', '--ip', '127.0.0.1', '--port', String(port),
    '--inspector-port', String(inspector), '--persist-to', state, '--var', 'STUN_URLS:'],
  'worker', { cwd: WORKER_DIR, env: { ...process.env, WRANGLER_SEND_METRICS: 'false', NO_COLOR: '1' } });
  const http = '127.0.0.1:' + port;
  await waitFor(async () => {
    if (st.exited !== undefined) throw new Error('wrangler dev exited with ' + st.exited);
    try { return (await fetch('http://' + http + '/healthz')).ok; } catch { return false; }
  }, 'wrangler dev ready', 120000, 500);
  return [st, http];
}

let passed = false;
let rv, host, link = '';
// The rendezvous log: castcorder-rendezvous logs to stderr, wrangler to stdout.
const rvLog = () => rv.out + rv.err;
try {
  let RV_HTTP;
  if (WORKER) {
    [rv, RV_HTTP] = await startWorker();
    console.log('worker (wrangler dev) at ' + RV_HTTP);
  } else {
    rv = start(RV_EXE, ['--listen', '127.0.0.1:0', '--stun', '127.0.0.1:0', '--client', VERSION + '=' + dist], 'rendezvous');
    let RV_STUN;
    [, RV_HTTP, RV_STUN] = await line(rv, /RENDEZVOUS_READY http=(\S+) stun=(\S+)/, 'RENDEZVOUS_READY');
    console.log('rendezvous at ' + RV_HTTP + ', stun ' + RV_STUN);
  }

  host = start(HOST_EXE, ['--dist', dist, '--library', LIBRARY, '--room', 'system-test', '--bind', '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--rendezvous', 'http://' + RV_HTTP], 'host');
  const [, HTTP] = await line(host, /HOST_READY http=(\S+)/, 'HOST_READY');
  link = (await line(host, /CALL_LINK (\S+)/, 'CALL_LINK'))[1];
  console.log('host up at ' + HTTP + '; call link ' + link.replace(/#.*/, '#<host-id>.<secret>'));
  check(link.startsWith('http://' + RV_HTTP + '/v/' + VERSION + '/#'), 'the call link points at the rendezvous site, pinned to /v/' + VERSION + '/');
  if (WORKER) {
    await waitFor(() => host.err.includes('registered with the rendezvous'), 'host registered with the Worker', 30000);
    check(true, 'host registered with its Durable Object (no STUN port on the Worker; srflx check skipped)');
    // Leave the host idle long enough for its Durable Object to hibernate
    // (about 10 s without events) and longer than the host allows silence
    // (45 s; its own pings are answered by the
    // runtime without waking it). Guests then wake it, and it must restore
    // the host's connection from its WebSocket attachment.
    const idle = Number(process.env.CASTCORDER_WORKER_IDLE_MS || 50000);
    console.log('host idle for ' + idle + ' ms so its Durable Object can hibernate');
    await sleep(idle);
  } else {
    const learned = await waitFor(() => host.err.match(/server-reflexive address learned reflexive=(\S+)/), 'host srflx via STUN', 15000);
    check(true, 'host learned its server-reflexive address over STUN through its own UDP socket: ' + learned[1]);
  }

  const A = await openGuest('A', HTTP + '/?name=A', ['--use-file-for-fake-audio-capture=' + INPUT_WAV]);
  const B = await openGuest('B', link.replace('/#', '/?name=B#'));
  check(new URL(B.url()).host === RV_HTTP, 'guest B loaded the client from the rendezvous, not the host');
  const signalB = await B.inputValue('#signal');
  check(signalB === 'ws://' + RV_HTTP + '/ws', 'guest B signals through the rendezvous mailbox (' + signalB + ')');

  // A wrong call secret is rejected by the host, sealed, through the mailbox.
  const frag = link.split('#')[1];
  const badSecret = frag.split('.')[0] + '.' + 'A'.repeat(21) + 'w';
  const C = await openGuest('C', link.split('#')[0] + '?name=C#' + badSecret);
  await C.click('#join');
  await waitFor(async () => (await text(C, '#state')) === 'rejected by the host', 'guest C rejected', 20000);
  const cErr = await text(C, '#errors');
  check(cErr.includes('wrong call secret') && (await text(C, '#participant')) === '' && !(await C.isEnabled('#leave')),
    'a wrong call secret is rejected and the guest stops retrying: ' + cErr);
  await sleep(3000);
  check((rvLog().match(/offer relayed/g) || []).length === 1, 'the rejected guest sent exactly one offer');

  for (const [n, p] of [['A', A], ['B', B]]) {
    await p.click('#join');
    await waitFor(async () => (await text(p, '#participant')).length > 0, 'guest ' + n + ' joined', 30000);
    console.log('guest ' + n + ' joined as participant ' + (await text(p, '#participant')));
  }
  check(logs.B.some((l) => l.includes('offer sealed to the host through the rendezvous')), 'guest B joined through the rendezvous');
  check(!logs.A.some((l) => l.includes('through the rendezvous')), 'guest A (LAN page) still joins by POST /offer');

  const flowing = await waitFor(async () => {
    const x = audioFrom(await json(B, '#debug-inbound'), 'A');
    await sleep(2000);
    const y = audioFrom(await json(B, '#debug-inbound'), 'A');
    return y.packets > x.packets && y.bytes > x.bytes ? { x, y } : null;
  }, 'B receiving audio from A', 30000, 0);
  check(true, 'B hears A (' + Math.round((flowing.y.packets - flowing.x.packets) / 2) + ' pkt/s)');

  await waitFor(async () => (await text(A, '#level-db')) !== '-inf dBFS', 'A level meter');
  await A.waitForSelector('#take-start:not([disabled])', { timeout: 10000 });
  await A.click('#take-start');
  for (const [n, p] of [['A', A], ['B', B]]) {
    await waitFor(async () => (await text(p, '#recording')).includes('recording take'), 'guest ' + n + ' recording', 10000);
  }
  await sleep(4000);
  await A.click('#take-stop');
  await waitFor(async () => (await text(A, '#take-status')).startsWith('idle'), 'take stopped', 10000);
  check(true, 'producer recorded take 1 with both guests');

  const TAKE_DIR = join(callDir(LIBRARY, 'system-test'), 'take01');
  const verified = () => {
    const m = join(TAKE_DIR, 'manifest.jsonl');
    return existsSync(m) ? readFileSync(m, 'utf8').split('\n').filter((l) => l.startsWith('{"Verified"')).length : 0;
  };
  await waitFor(() => verified() >= 6, 'both guests\' tracks, sidecars and timing metadata verified on the host', 60000, 250);
  const files = readdirSync(TAKE_DIR).sort();
  console.log('host take01: ' + files.join(' '));
  const bWav = files.find((f) => /^take01_B.*\.wav$/.test(f));
  check(!!bWav && statSync(join(TAKE_DIR, bWav)).size > 100000, 'guest B\'s take synced into the host library (' + bWav + ')');

  // The server saw only routing metadata.
  const secret = frag.split('.')[1];
  check(rvLog().includes('offer relayed') && rvLog().includes('answer relayed'), 'the rendezvous relayed the offer and the answer');
  check(!rvLog().includes(secret) && !rvLog().includes('v=0') && !rvLog().includes('a=candidate'),
    'the rendezvous log holds no call secret and no SDP');
  if (WORKER) {
    check(rvLog().includes('mailbox woke from hibernation'),
      "the host's Durable Object hibernated while the host was idle and restored its connection on waking");
    check((host.err.match(/registered with the rendezvous/g) || []).length === 1,
      'the host kept its one connection throughout (its pings, answered by the runtime, kept it alive)');
  }
  for (const n of ['A', 'B']) {
    const bad = logs[n].filter((l) => l.startsWith('[pageerror]'));
    check(bad.length === 0, 'guest ' + n + ': no page errors' + (bad.length ? ': ' + bad.slice(0, 3).join(' | ') : ''));
  }
  passed = checks.every((c) => c.ok);
} catch (e) {
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  for (const [, st] of procs) killTree(st.p);
}
for (const [name, st] of procs) writeFileSync(join(RESULTS, 'system-' + name + '.log'), st.out + '\n--- stderr ---\n' + st.err);
for (const n of Object.keys(logs)) writeFileSync(join(RESULTS, 'system-guest-' + n + '.log'), logs[n].join('\n'));
if (!passed) {
  for (const [name, st] of procs) { console.log(name + ' log tail:'); console.log(st.err.split('\n').slice(-25).join('\n')); }
  for (const n of Object.keys(logs)) { console.log('guest ' + n + ' console tail:'); console.log(logs[n].slice(-15).join('\n')); }
}
console.log('logs: crates/castcorder-client/browser-test/test-results/system-*');
console.log(passed ? 'PASS a guest with only the call link joined through the rendezvous, heard another guest, and its take synced to the host'
  : 'FAIL (system)');
process.exit(passed ? 0 : 1);
