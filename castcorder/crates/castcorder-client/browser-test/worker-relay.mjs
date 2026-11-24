// The Worker's relay credentials (castcorder/worker, ADR 0004) under
// `wrangler dev`, with a real castcorder-host and raw WebSocket guests. No
// browser, no Docker, no Cloudflare account for the first part.
//
// 1. RELAY=coturn (the shared-secret REST scheme) with RELAY_DAILY_CAP=3:
//    the host's registration mints one, two guest hellos naming the host mint
//    two more; each is `<expiry>:<host id>` with the HMAC-SHA1 password and an
//    `expires` field. The fourth is refused with relay_quota. Then wrangler is
//    restarted on the same port and state dir: the count lives in the host
//    key's Durable Object storage, so the reconnecting host and a new guest are
//    still refused (a hibernating or restarted object does not reset the cap).
// 2. RELAY=cloudflare without a key: the host is told relay_unavailable.
// 3. RELAY=cloudflare, for real, when CF_TURN_KEY_ID and CF_TURN_API_TOKEN are
//    set (a Cloudflare Realtime TURN key): a host registers and must get
//    Cloudflare credentials with an expiry and allocate on turn.cloudflare.com
//    with them; a guest hello must carry the TURN URLs including TLS on 443.
//    Without those variables this part prints SKIP and passes. That is the one
//    skip CASTCORDER_CI allows: CI has no Cloudflare secrets until the deploy
//    work (cast-lxt.4) adds them. The key and token go to wrangler in an env
//    file, never on a command line, and are never printed.
//
// Every port is picked by the OS. CARGO_TARGET_DIR selects other build dirs.
// Run from the repo root with: task test:worker:relay
// Everything printed is ASCII. Logs go to browser-test/test-results/worker-relay-*.

import { spawn, spawnSync } from 'node:child_process';
import { createHmac, randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '..');
const WORKSPACE = join(CRATE, '..', '..');
const RESULTS = join(HERE, 'test-results');
const HOST_EXE = join(process.env.CARGO_TARGET_DIR || join(WORKSPACE, 'target'), 'release',
  process.platform === 'win32' ? 'castcorder-host.exe' : 'castcorder-host');
const WORKER_DIR = join(WORKSPACE, 'worker');
const WRANGLER = join(WORKER_DIR, 'node_modules', 'wrangler', 'bin', 'wrangler.js');
for (const f of [HOST_EXE, WRANGLER, join(WORKER_DIR, 'build', 'index.js')]) {
  if (!existsSync(f)) { console.log('FAIL missing ' + f); process.exit(1); }
}
mkdirSync(RESULTS, { recursive: true });
// wrangler needs the static assets directory to exist (no client is served here).
mkdirSync(join(WORKER_DIR, 'assets'), { recursive: true });
// The host serves no client here; any directory with an index.html will do.
const DIST = join(RESULTS, 'worker-relay-dist');
mkdirSync(DIST, { recursive: true });
writeFileSync(join(DIST, 'index.html'), '<!doctype html><title>unused</title>\n');

const SECRET = randomBytes(18).toString('base64url');
const CAP = 3;
const TTL = 600;
const CF = process.env.CF_TURN_KEY_ID && process.env.CF_TURN_API_TOKEN
  ? { id: process.env.CF_TURN_KEY_ID, token: process.env.CF_TURN_API_TOKEN } : null;

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
const freePort = () => new Promise((resolve, reject) => {
  const srv = createServer();
  srv.on('error', reject);
  srv.listen(0, '127.0.0.1', () => { const { port } = srv.address(); srv.close(() => resolve(port)); });
});

const procs = [];
function start(exe, args, name, opts = {}) {
  const p = spawn(exe, args, { stdio: ['ignore', 'pipe', 'pipe'], detached: process.platform !== 'win32', ...opts });
  const st = { p, out: '', err: '' };
  p.stdout.on('data', (d) => { st.out += d.toString(); });
  p.stderr.on('data', (d) => { st.err += d.toString(); });
  p.on('exit', (c) => { st.exited = c; });
  procs.push([name, st]);
  return st;
}
function killTree(p) {
  if (p.exitCode !== null) return;
  if (process.platform === 'win32') spawnSync('taskkill', ['/pid', String(p.pid), '/T', '/F'], { stdio: 'ignore' });
  else { try { process.kill(-p.pid, 'SIGTERM'); } catch { p.kill(); } }
}

const checks = [];
function check(ok, what) {
  checks.push({ ok, what });
  console.log((ok ? 'ok   ' : 'BAD  ') + what);
}

// `wrangler dev` on `port` with `vars` (--var) and `secrets` (an env file in
// `state`, which also holds the Durable Object storage).
async function startWorker(name, port, state, vars, secrets) {
  mkdirSync(state, { recursive: true });
  const envFile = join(state, 'secrets.env');
  writeFileSync(envFile, Object.entries(secrets).map(([k, v]) => k + '=' + v + '\n').join(''));
  const inspector = await freePort();
  const st = start(process.execPath, [WRANGLER, 'dev', '--ip', '127.0.0.1', '--port', String(port),
    '--inspector-port', String(inspector), '--persist-to', state, '--env-file', envFile,
    ...Object.entries(vars).flatMap(([k, v]) => ['--var', k + ':' + v])],
  name, { cwd: WORKER_DIR, env: { ...process.env, WRANGLER_SEND_METRICS: 'false', NO_COLOR: '1' } });
  await waitFor(async () => {
    if (st.exited !== undefined) throw new Error('wrangler dev exited with ' + st.exited);
    try { return (await fetch('http://127.0.0.1:' + port + '/healthz')).ok; } catch { return false; }
  }, 'wrangler dev ready', 120000, 500);
  return st;
}

// bind: 127.0.0.1 keeps a host off the network; a host that must reach a
// public TURN server binds 0.0.0.0 (a socket bound to loopback cannot send
// beyond it) and still advertises only loopback.
function startHost(name, port, bind = '127.0.0.1') {
  const library = join(RESULTS, 'library-' + name);
  rmSync(library, { recursive: true, force: true });
  return start(HOST_EXE, ['--dist', DIST, '--library', library, '--room', name, '--bind', bind,
    '--advertise', '127.0.0.1', '--port', '0', '--http-port', '0', '--udp-port', '0',
    '--rendezvous', 'http://127.0.0.1:' + port], name);
}

// The host id from the call link the host prints.
async function hostId(h) {
  const m = await waitFor(() => h.out.match(/CALL_LINK \S+#([^.\s]+)\./), 'CALL_LINK');
  return m[1];
}

// A guest's hello for `host` (`/ws?mailbox=HOST&host=HOST`), as a joining
// guest asks for relay credentials.
function guestHello(port, host) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket('ws://127.0.0.1:' + port + '/ws?mailbox=' + host + '&host=' + host);
    const t = setTimeout(() => { ws.close(); reject(new Error('no hello')); }, 15000);
    ws.onmessage = (e) => { clearTimeout(t); ws.close(); resolve(JSON.parse(e.data)); };
    ws.onerror = () => { clearTimeout(t); reject(new Error('WebSocket error')); };
  });
}

const count = (s, re) => (s.match(re) || []).length;
let passed = false;
const secretsSeen = () => procs.some(([, st]) => (st.out + st.err).includes(SECRET)
  || (CF && (st.out + st.err).includes(CF.token)));
try {
  // 1. coturn mode: the daily cap, kept across a restart.
  const port = await freePort();
  const state = join(tmpdir(), 'castcorder-worker-relay-' + process.pid);
  rmSync(state, { recursive: true, force: true });
  const vars = { STUN_URLS: '', RELAY: 'coturn', TURN_URLS: 'turn:127.0.0.1:9?transport=udp',
    TURN_TTL: String(TTL), RELAY_DAILY_CAP: String(CAP) };
  let w = await startWorker('worker-coturn', port, state, vars, { TURN_SECRET: SECRET });
  console.log('worker (wrangler dev, RELAY=coturn, cap ' + CAP + ') at 127.0.0.1:' + port);
  const h = startHost('host-coturn', port);
  const H = await hostId(h);
  const granted = await waitFor(() => h.err.match(/relay \(TURN\) server from the rendezvous.*expires=Some\((\d+)\)/),
    'host relay credentials', 30000);
  check(Math.abs(Number(granted[1]) - (Date.now() / 1000 + TTL)) < 60,
    'the host got coturn credentials on registering, expiring in TURN_TTL (' + granted[1] + ')');
  for (const n of [2, 3]) {
    const hello = await guestHello(port, H);
    const s = (hello.ice_servers || []).find((x) => x.username);
    const ok = !!s && !hello.relay_error && s.username === s.expires + ':' + H
      && s.credential === createHmac('sha1', SECRET).update(s.username).digest('base64')
      && s.urls.join(' ') === vars.TURN_URLS;
    check(ok, 'guest hello ' + n + ' of ' + CAP + ' carries coturn REST credentials (<expiry>:<host id>, HMAC-SHA1)');
  }
  const over = await guestHello(port, H);
  check((over.relay_error || '').startsWith('relay_quota') && !(over.ice_servers || []).some((x) => x.username),
    'the ' + (CAP + 1) + 'th credential of the day is refused: ' + over.relay_error);

  killTree(w.p);
  await waitFor(() => w.exited !== undefined || w.p.exitCode !== null, 'wrangler stopped', 15000).catch(() => {});
  await sleep(1000);
  w = await startWorker('worker-coturn-restarted', port, state, vars, { TURN_SECRET: SECRET });
  console.log('worker restarted on the same port and state');
  await waitFor(() => count(h.err, /registered with the rendezvous/g) >= 2, 'host re-registered', 90000);
  await waitFor(() => /gave no relay \(TURN\) credentials.*relay_quota/.test(h.err), 'host refused relay credentials', 15000);
  check(true, 'after the restart the reconnecting host is refused relay credentials (relay_quota): the count survived');
  const after = await guestHello(port, H);
  check((after.relay_error || '').startsWith('relay_quota'), 'a guest is still refused after the restart: ' + after.relay_error);
  killTree(h.p);

  // 2. RELAY=cloudflare without a TURN key: the relay is configured but
  // cannot mint, so the host is told why (relay_unavailable), never silently
  // given no relay.
  {
    const p2 = await freePort();
    const st2 = join(tmpdir(), 'castcorder-worker-nokey-' + process.pid);
    rmSync(st2, { recursive: true, force: true });
    await startWorker('worker-nokey', p2, st2, { STUN_URLS: '', RELAY: 'cloudflare' }, {});
    const n = startHost('host-nokey', p2);
    await waitFor(() => /gave no relay \(TURN\) credentials.*relay_unavailable.*CF_TURN_KEY_ID/.test(n.err),
      'host told the relay is unavailable', 30000);
    check(true, 'RELAY=cloudflare without CF_TURN_KEY_ID answers relay_unavailable naming what is missing');
    killTree(n.p);
  }

  // 3. Cloudflare Realtime TURN, for real, when a TURN key is given.
  if (!CF) {
    console.log('SKIP Cloudflare Realtime TURN: CF_TURN_KEY_ID and CF_TURN_API_TOKEN are not set '
      + '(the Worker\'s Cloudflare adapter only runs against the real API; cast-lxt.4 adds the CI secrets)');
  } else {
    const cfPort = await freePort();
    const cfState = join(tmpdir(), 'castcorder-worker-cf-' + process.pid);
    rmSync(cfState, { recursive: true, force: true });
    await startWorker('worker-cloudflare', cfPort, cfState,
      { STUN_URLS: 'stun:stun.cloudflare.com:3478', RELAY: 'cloudflare', TURN_TTL: String(TTL) },
      { CF_TURN_KEY_ID: CF.id, CF_TURN_API_TOKEN: CF.token });
    console.log('worker (wrangler dev, RELAY=cloudflare) at 127.0.0.1:' + cfPort);
    const c = startHost('host-cloudflare', cfPort, '0.0.0.0');
    const C = await hostId(c);
    const g = await waitFor(() => c.err.match(/relay \(TURN\) server from the rendezvous server=(\S+) alternates=\[[^\]]*\] expires=Some\((\d+)\)/),
      'host Cloudflare relay credentials', 30000);
    check(Math.abs(Number(g[2]) - (Date.now() / 1000 + TTL)) < 60,
      'the host got Cloudflare TURN credentials with their expiry (' + g[1] + ', expires ' + g[2] + ')');
    const alt = c.err.match(/relay \(TURN\) server from the rendezvous server=\S+ alternates=\[([^\]]*)\]/);
    check(!!alt && /:\d+"/.test(alt[1]),
      'the host got a fallback UDP address: [' + (alt ? alt[1] : '') + ']');
    // Some runners' flows to Cloudflare's UDP 3478 are dropped for good
    // (cast-wtf); the host then moves on to its fallback address after
    // ALLOCATE_TIMEOUT (15 s). Wait for both attempts.
    const alloc = await waitFor(() => c.err.match(/turn allocation created relayed=(\S+) server=(\S+)/), 'allocation on Cloudflare TURN', 45000);
    const fellBack = /turn allocation timed out/.test(c.err);
    check(true, 'the host allocated on Cloudflare TURN with them: relayed ' + alloc[1] + ' on ' + alloc[2]
      + (fellBack ? ' (after its first address went unanswered)' : ''));
    const hello = await guestHello(cfPort, C);
    const s = (hello.ice_servers || []).find((x) => x.username);
    check(!!s && !hello.relay_error && s.urls.some((u) => u.startsWith('turns:') && u.includes(':443'))
      && s.urls.some((u) => u.startsWith('turn:') && u.includes('transport=udp')) && !s.urls.some((u) => /:53(\?|$)/.test(u))
      && Number(s.expires) > Date.now() / 1000,
    'a guest hello carries Cloudflare TURN credentials: ' + (s ? s.urls.join(' ') : hello.relay_error));
    check((hello.ice_servers || []).some((x) => x.urls.includes('stun:stun.cloudflare.com:3478')), 'and stun.cloudflare.com');
    killTree(c.p);
  }
  check(!secretsSeen(), 'no log holds the TURN secret or the Cloudflare API token');
  passed = checks.every((x) => x.ok);
} catch (e) {
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const [, st] of procs) killTree(st.p);
}
const redact = (t) => {
  let r = t.replaceAll(SECRET, '<secret>');
  if (CF) r = r.replaceAll(CF.token, '<token>').replaceAll(CF.id, '<key id>');
  return r;
};
for (const [name, st] of procs) writeFileSync(join(RESULTS, 'worker-relay-' + name + '.log'), redact(st.out + '\n--- stderr ---\n' + st.err));
if (!passed) {
  for (const [name, st] of procs) {
    console.log(name + ' log tail:');
    console.log(redact((st.err.trim() ? st.err : st.out)).split('\n').slice(-20).join('\n'));
  }
}
console.log('logs: crates/castcorder-client/browser-test/test-results/worker-relay-*');
console.log(passed ? 'PASS the Worker mints relay credentials under a daily cap that survives a restart'
  + (CF ? ', and Cloudflare Realtime TURN credentials work' : ' (Cloudflare part skipped: no TURN key)') : 'FAIL (worker relay)');
process.exit(passed ? 0 : 1);
