// Relay test: a call that connects with only TURN relay candidates allowed.
//
// Real processes on this machine plus a real TURN server:
//   coturn (Docker, coturn/coturn:4.6)  use-auth-secret with a random per-run secret;
//                          only its listening port is published (UDP, a port Docker
//                          picks, reached on 127.0.0.1 or the WSL2 VM address, which
//                          the host binds toward): with every side relayed, media
//                          moves between two allocations inside the container,
//                          so the relay port range needs no publishing
//   castcorder-rendezvous  --turn turn:<that address>:<port>: mints credentials per host key
//   castcorder-host        --rendezvous ... --relay-only: offers only its relayed candidate
//   headless Chromium      guests A (producer audio, a deterministic WAV) and B, both
//                          by call link with ?iceTransportPolicy=relay
// Checks: the host allocates on coturn with minted credentials; both guests
// connect with the host's chosen pair on its relayed address; B hears A; a
// take started through the host's producer HTTP endpoint records and both
// guests' tracks sync into the host library; nothing logs the TURN secret.
//
// Every port is picked by the OS. CARGO_TARGET_DIR and CASTCORDER_DIST select
// other build dirs; DOCKER_HOST is honoured by the docker CLI.
// Run from the repo root with: task test:relay
// Everything printed is ASCII. Logs go to browser-test/test-results/relay-*.

import { spawn, execFileSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import dgram from 'node:dgram';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeInput, writeWav16 } from './gen-wav.mjs';

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
for (const f of [HOST_EXE, RV_EXE, join(dist, 'index.html')]) {
  if (!existsSync(f)) { console.log('FAIL missing ' + f); process.exit(1); }
}
const VERSION = '0.0.0';
const COTURN_IMAGE = 'coturn/coturn:4.6';

mkdirSync(RESULTS, { recursive: true });
const INPUT_WAV = join(RESULTS, 'input-44k-s16-stereo.wav');
writeWav16(INPUT_WAV, makeInput());
const LIBRARY = join(RESULTS, 'library-relay');
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

const docker = (args) => execFileSync('docker', args, {
  encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, MSYS_NO_PATHCONV: '1' },
}).trim();

// One STUN Binding request; true if anything answers within `ms`.
function stunAnswers(ip, port, ms = 700) {
  return new Promise((resolve) => {
    const s = dgram.createSocket('udp4');
    const req = Buffer.alloc(20);
    req.writeUInt16BE(1, 0);
    req.writeUInt32BE(0x2112A442, 4);
    randomBytes(12).copy(req, 8);
    const done = (v) => { clearTimeout(t); try { s.close(); } catch { /* closed */ } resolve(v); };
    const t = setTimeout(() => done(false), ms);
    s.on('message', () => done(true));
    s.on('error', () => done(false));
    s.send(req, port, ip);
  });
}

// The local address this machine sends from toward ip:port (connecting a UDP
// socket sends nothing).
function localAddressToward(ip, port) {
  return new Promise((resolve, reject) => {
    const s = dgram.createSocket('udp4');
    s.connect(port, ip, (e) => {
      if (e) { s.close(); reject(e); return; }
      const a = s.address().address;
      s.close();
      resolve(a);
    });
  });
}

// Where this machine reaches the container's published UDP port. Docker Desktop
// forwards UDP on 127.0.0.1; a Docker Engine inside WSL2 only forwards TCP to
// Windows' localhost, so there the WSL VM's own address is used. Override with
// CASTCORDER_TURN_HOST.
async function reachableDockerAddress(port) {
  let candidates = process.env.CASTCORDER_TURN_HOST ? [process.env.CASTCORDER_TURN_HOST] : ['127.0.0.1'];
  if (!process.env.CASTCORDER_TURN_HOST) {
    const probe = CONTAINER + '-ip';
    try {
      docker(['run', '-d', '--name', probe, '--network', 'host', '--entrypoint', '/bin/cat', COTURN_IMAGE, '/proc/net/fib_trie']);
      await sleep(1500);
      const trie = docker(['logs', probe]).split('\n');
      for (let i = 1; i < trie.length; i++) {
        const m = trie[i - 1].match(/\|-- (\d+\.\d+\.\d+\.\d+)/);
        if (m && trie[i].includes('/32 host LOCAL') && !m[1].startsWith('127.')) candidates.push(m[1]);
      }
    } catch { /* not a Linux daemon we can inspect */ }
    try { docker(['rm', '-f', probe]); } catch { /* gone */ }
    candidates = [...new Set(candidates)];
  }
  for (let round = 0; round < 15; round++) {
    for (const ip of candidates) if (await stunAnswers(ip, port)) return ip;
  }
  throw new Error('coturn does not answer on port ' + port + ' at any of ' + candidates.join(', '));
}

const procs = [];
function start(exe, args, name, env) {
  const p = spawn(exe, args, { stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, ...env } });
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

const logs = { A: [], B: [] };
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
const audioFrom = (inbound, origin) => (inbound || []).filter((i) => i.origin.startsWith(origin + '-'))
  .reduce((a, i) => ({ packets: a.packets + i.packetsReceived, bytes: a.bytes + i.bytesReceived }), { packets: 0, bytes: 0 });

const SECRET = randomBytes(18).toString('base64url');
const CONTAINER = 'castcorder-relay-test-' + process.pid;
let coturnLog = '';
let passed = false;
let rv, host;
try {
  try {
    docker(['version', '--format', '{{.Server.Version}}']);
  } catch (e) {
    throw new Error('docker is not reachable (set DOCKER_HOST?): ' + String(e.stderr || e.message).trim());
  }
  docker(['run', '-d', '--rm', '--name', CONTAINER, '-p', '3478/udp', COTURN_IMAGE,
    '-n', '--listening-port=3478', '--realm=castcorder.test', '--fingerprint',
    '--use-auth-secret', '--static-auth-secret=' + SECRET, '--min-port=49160', '--max-port=49200',
    '--no-tls', '--no-dtls', '--no-tcp-relay', '--no-cli', '--log-file=stdout', '--simple-log', '--verbose']);
  const mapped = docker(['port', CONTAINER, '3478/udp']).split('\n')[0];
  const TURN_PORT = Number(mapped.split(':').pop());
  const TURN_IP = await reachableDockerAddress(TURN_PORT);
  const TURN_URL = 'turn:' + TURN_IP + ':' + TURN_PORT + '?transport=udp';
  console.log('coturn (' + COTURN_IMAGE + ', Docker) answering STUN at ' + TURN_IP + ':' + TURN_PORT + '/udp');

  rv = start(RV_EXE, ['--listen', '127.0.0.1:0', '--stun', '127.0.0.1:0', '--client', VERSION + '=' + dist,
    '--turn', TURN_URL, '--turn-ttl', '600'], 'rendezvous', { CASTCORDER_TURN_SECRET: SECRET });
  const [, RV_HTTP] = await line(rv, /RENDEZVOUS_READY http=(\S+) stun=(\S+)/, 'RENDEZVOUS_READY');
  console.log('rendezvous at ' + RV_HTTP);

  // The host binds the local address that routes to the TURN server (loopback
  // cannot send to a WSL2 VM address).
  const HOST_IP = await localAddressToward(TURN_IP, TURN_PORT);
  host = start(HOST_EXE, ['--dist', dist, '--library', LIBRARY, '--call', 'relay-test', '--bind', HOST_IP,
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--rendezvous', 'http://' + RV_HTTP, '--relay-only',
    // Always open: no producer joins here (takes go through the host machine's HTTP endpoint).
    '--open'], 'host');
  const [, HTTP] = await line(host, /HOST_READY http=(\S+)/, 'HOST_READY');
  const link = (await line(host, /CALL_LINK (\S+)/, 'CALL_LINK'))[1];
  const alloc = await waitFor(() => host.err.match(/turn allocation created relayed=(\S+)/), 'host TURN allocation', 30000);
  const RELAYED = alloc[1];
  check(true, 'the host allocated a relayed address on coturn with minted credentials: ' + RELAYED);

  const guestUrl = (name) => link.replace('/#', '/?name=' + name + '&iceTransportPolicy=relay#');
  const A = await openGuest('A', guestUrl('A'), ['--use-file-for-fake-audio-capture=' + INPUT_WAV]);
  const B = await openGuest('B', guestUrl('B'));
  for (const [n, p] of [['A', A], ['B', B]]) {
    await p.click('#join');
    await waitFor(async () => (await text(p, '#participant')).length > 0, 'guest ' + n + ' joined', 45000);
    console.log('guest ' + n + ' joined as participant ' + (await text(p, '#participant')));
  }
  check(['A', 'B'].every((n) => logs[n].some((l) => l.includes('ice transport policy forced') && l.includes('relay'))),
    'both guests forced iceTransportPolicy=relay');
  const minted = (rv.err.match(/relay credentials minted/g) || []).length;
  check(minted >= 3, 'the rendezvous minted relay credentials for the host and both guests (' + minted + ')');

  const pairs = await waitFor(() => {
    const m = [...host.err.matchAll(/chosen candidate pair candidate_pair=Udp (\S+) -> (\S+)/g)];
    return m.length >= 2 ? m : null;
  }, 'host chosen pairs', 20000);
  check(pairs.every((m) => m[1] === RELAYED),
    'every chosen pair on the host uses its relayed candidate: ' + pairs.map((m) => m[1] + ' -> ' + m[2]).join(', '));
  check(host.err.includes('turn permission created'), 'the host created TURN permissions for its peers');

  const flowing = await waitFor(async () => {
    const x = audioFrom(await json(B, '#debug-inbound'), 'A');
    await sleep(2000);
    const y = audioFrom(await json(B, '#debug-inbound'), 'A');
    return y.packets > x.packets && y.bytes > x.bytes ? { x, y } : null;
  }, 'B receiving audio from A', 30000, 0);
  check(true, 'B hears A through the relay (' + Math.round((flowing.y.packets - flowing.x.packets) / 2) + ' pkt/s)');

  await waitFor(async () => (await text(A, '#level-db')) !== '-inf dBFS', 'A level meter');
  const post = async (path) => {
    const r = await fetch(HTTP + path, { method: 'POST' });
    return [r.status, await r.text()];
  };
  const started = await post('/take/start');
  check(started[0] === 200, 'take started through the producer endpoint: ' + started.join(' '));
  for (const [n, p] of [['A', A], ['B', B]]) {
    await waitFor(async () => (await text(p, '#recording')).includes('recording take'), 'guest ' + n + ' recording', 10000);
  }
  await sleep(4000);
  const stopped = await post('/take/stop');
  check(stopped[0] === 200, 'take stopped: ' + stopped.join(' '));

  const TAKE_DIR = join(LIBRARY, 'relay-test', 'take01');
  const verified = () => {
    const m = join(TAKE_DIR, 'manifest.jsonl');
    return existsSync(m) ? readFileSync(m, 'utf8').split('\n').filter((l) => l.startsWith('{"Verified"')).length : 0;
  };
  await waitFor(() => verified() >= 6, 'both guests\' tracks, sidecars and timing metadata verified on the host', 90000, 250);
  const files = readdirSync(TAKE_DIR).sort();
  console.log('host take01: ' + files.join(' '));
  for (const n of ['A', 'B']) {
    const wav = files.find((f) => new RegExp('^take01_' + n + '.*\\.wav$').test(f));
    check(!!wav && statSync(join(TAKE_DIR, wav)).size > 100000, 'guest ' + n + '\'s take synced into the host library over the relay (' + wav + ')');
  }

  coturnLog = docker(['logs', CONTAINER]);
  check(/session \d+: new, realm=<castcorder.test>/.test(coturnLog) || coturnLog.includes('ALLOCATE processed, success'),
    'coturn logged successful allocations');
  check(![rv.err, rv.out, host.err, host.out].some((s) => s.includes(SECRET)), 'no process logged the TURN secret');
  for (const n of ['A', 'B']) {
    const bad = logs[n].filter((l) => l.startsWith('[pageerror]'));
    check(bad.length === 0, 'guest ' + n + ': no page errors' + (bad.length ? ': ' + bad.slice(0, 3).join(' | ') : ''));
  }
  passed = checks.every((c) => c.ok);
} catch (e) {
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  for (const [, st] of procs) st.p.kill();
  try { if (!coturnLog) coturnLog = docker(['logs', CONTAINER]); } catch { /* not started */ }
  try { docker(['rm', '-f', CONTAINER]); } catch { /* not started */ }
}
for (const [name, st] of procs) writeFileSync(join(RESULTS, 'relay-' + name + '.log'), st.out + '\n--- stderr ---\n' + st.err);
writeFileSync(join(RESULTS, 'relay-coturn.log'), coturnLog.replaceAll(SECRET, '<secret>'));
for (const n of Object.keys(logs)) writeFileSync(join(RESULTS, 'relay-guest-' + n + '.log'), logs[n].join('\n'));
if (!passed) {
  for (const [name, st] of procs) { console.log(name + ' log tail:'); console.log(st.err.split('\n').slice(-25).join('\n')); }
  console.log('coturn log tail:'); console.log(coturnLog.replaceAll(SECRET, '<secret>').split('\n').slice(-25).join('\n'));
  for (const n of Object.keys(logs)) { console.log('guest ' + n + ' console tail:'); console.log(logs[n].slice(-15).join('\n')); }
}
console.log('logs: crates/castcorder-client/browser-test/test-results/relay-*');
console.log(passed ? 'PASS two relay-only guests and a relay-only host held a call through coturn; audio flowed and a take synced'
  : 'FAIL (relay)');
process.exit(passed ? 0 : 1);
