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
// Then the credentials renew (minted with a short TTL, CASTCORDER_RELAY_TTL,
// default 60 s): the host allocates again and restarts ICE; each guest must
// answer with its own new relay candidate, the call moves to the new
// allocation without a reconnect, audio keeps flowing once the old
// allocation is deleted, and a second take syncs.
// Then relay metering: the rendezvous scrapes coturn's Prometheus exporter
// (--prometheus-username-labels, published on a Docker-picked TCP port) and,
// once the guests leave and their sessions end, its admin /usage (and the
// `usage` subcommand on its data dir) shows bytes relayed for the host key.
// Restarted on the same port and data dir with --relay-budget 1, it refuses
// relay credentials with relay_quota: the reconnecting host logs it, and a
// guest's hello for that host carries it with no turn: server.
//
// Every port is picked by the OS. CARGO_TARGET_DIR and CASTCORDER_DIST select
// other build dirs; DOCKER_HOST is honoured by the docker CLI.
// Run from the repo root with: task test:relay
// Everything printed is ASCII. Logs go to browser-test/test-results/relay-*.

import { spawn, execFileSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeInput, writeWav16 } from './gen-wav.mjs';
import { VERSION } from './lib/version.mjs';
import { callDir } from './lib/helpers.mjs';
import { COTURN_IMAGE, docker, localAddressToward, startCoturn, stopCoturn } from './lib/coturn.mjs';

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
// Lifetime of minted TURN credentials (s). The host renews at 80% of it, so
// the first renewal lands mid-test. The guests renew theirs at 80% too
// (cast-lxt.11, an ICE restart they ask the host for), so their restarts
// land beside the host's; system.mjs checks a guest's renewal on its own,
// against the Worker.
const TTL = Number(process.env.CASTCORDER_RELAY_TTL || 60);

mkdirSync(RESULTS, { recursive: true });
const INPUT_WAV = join(RESULTS, 'input-44k-s16-stereo.wav');
writeWav16(INPUT_WAV, makeInput());
const LIBRARY = join(RESULTS, 'library-relay');
const RV_DATA = join(RESULTS, 'rendezvous-data-' + process.pid);
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
const audioFrom = (inbound, origin) => (inbound || []).filter((i) => i.kind === 'audio' && i.origin.startsWith(origin + '-'))
  .reduce((a, i) => ({ packets: a.packets + i.packetsReceived, bytes: a.bytes + i.bytesReceived }), { packets: 0, bytes: 0 });

const SECRET = randomBytes(18).toString('base64url');
const CONTAINER = 'castcorder-relay-test-' + process.pid;
let coturnLog = '';
let passed = false;
let rv, host;
try {
  const turn = await startCoturn(CONTAINER, SECRET, {
    publish: ['9641/tcp'],
    extra: ['--prometheus', '--prometheus-username-labels', '--user-quota=32', '--max-bps=2500000'],
  });
  const [TURN_IP, TURN_PORT, TURN_URL] = [turn.ip, turn.port, turn.url];
  console.log('coturn (' + COTURN_IMAGE + ', Docker) answering STUN at ' + TURN_IP + ':' + TURN_PORT + '/udp');

  const METRICS_PORT = Number(docker(['port', CONTAINER, '9641/tcp']).split('\n')[0].split(':').pop());
  const METRICS_URL = 'http://' + TURN_IP + ':' + METRICS_PORT + '/metrics';
  const rvArgs = (listen) => ['--listen', listen, '--stun', '127.0.0.1:0', '--client', VERSION + '=' + dist,
    '--turn', TURN_URL, '--turn-ttl', String(TTL), '--turn-metrics', METRICS_URL, '--turn-metrics-interval', '2',
    '--data-dir', RV_DATA, '--admin', '127.0.0.1:0'];
  rv = start(RV_EXE, rvArgs('127.0.0.1:0'), 'rendezvous', { CASTCORDER_TURN_SECRET: SECRET });
  const [, RV_HTTP, , RV_ADMIN] = await line(rv, /RENDEZVOUS_READY http=(\S+) stun=(\S+) admin=(\S+)/, 'RENDEZVOUS_READY');
  console.log('rendezvous at ' + RV_HTTP);

  // The host binds the local address that routes to the TURN server (loopback
  // cannot send to a WSL2 VM address).
  const HOST_IP = await localAddressToward(TURN_IP, TURN_PORT);
  host = start(HOST_EXE, ['--dist', dist, '--library', LIBRARY, '--room', 'relay-test', '--bind', HOST_IP,
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
  // Both guests share one address here, so the second reuses the first's
  // credential (cast-lxt.12) instead of minting its own.
  const reused = (rv.err.match(/relay credentials reused/g) || []).length;
  check(minted >= 2 && minted + reused >= 3, 'the rendezvous gave relay credentials to the host and both guests (' + minted + ' minted, ' + reused + ' reused)');

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

  const TAKE_DIR = join(callDir(LIBRARY, 'relay-test'), 'take01');
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

  // Credential renewal (cast-dns): at 80% of the TTL the host fetches new
  // credentials, allocates again beside the live allocation, offers every
  // guest the new relayed address with an ICE restart, and deletes the old
  // allocation a few seconds later. Each browser guest's answer to that
  // restart must carry its own new relay candidate (no trickle), and the call
  // must carry on over the new allocation without a reconnect.
  const participants = {};
  for (const [n, p] of [['A', A], ['B', B]]) participants[n] = await text(p, '#participant');
  const SWITCH = /relayed address switched to the renewed allocation; restarting ICE.*? new=(\S+)/;
  const sw = await waitFor(() => host.err.match(SWITCH), 'the host renewing its relay credentials', (TTL + 30) * 1000, 250);
  const at = host.err.indexOf(sw[0]);
  const NEW_RELAYED = sw[1];
  check(NEW_RELAYED !== RELAYED, 'the host renewed its credentials and switched to a new allocation: ' + RELAYED + ' -> ' + NEW_RELAYED);
  const answers = await waitFor(() => {
    const last = {};
    for (const n of ['A', 'B']) {
      const l = logs[n].filter((x) => x.includes('answered host renegotiation')).pop() || '';
      const relay = Number((l.match(/relay_candidates = (\d+)/) || [])[1] || 0);
      if (relay < 1) return null;
      last[n] = l;
    }
    return last;
  }, 'each guest answering the ICE restart with a relay candidate', 15000).catch(() => null);
  check(answers !== null, 'each guest answered the ICE restart with its own new relay candidate'
    + (answers ? '' : ': ' + ['A', 'B'].map((n) => n + ' ' + (logs[n].filter((x) => x.includes('answered host renegotiation')).pop() || 'none')).join(' | ')));
  const newPairs = await waitFor(() => {
    const m = [...host.err.slice(at).matchAll(/chosen candidate pair candidate_pair=Udp (\S+) -> (\S+)/g)];
    return m.filter((x) => x[1] === NEW_RELAYED).length >= 2 ? m : null;
  }, 'host chosen pairs on the new relayed address', 20000).catch(() => null);
  check(newPairs !== null, 'after the ICE restart both guests\x27 chosen pairs on the host use the new relayed address'
    + (newPairs ? ': ' + newPairs.map((m) => m[1] + ' -> ' + m[2]).join(', ') : ''));
  await waitFor(() => host.err.slice(at).includes('old relay allocation deleted'), 'the old allocation deleted', 20000);
  const after = await waitFor(async () => {
    const x = audioFrom(await json(B, '#debug-inbound'), 'A');
    await sleep(2000);
    const y = audioFrom(await json(B, '#debug-inbound'), 'A');
    return y.packets > x.packets ? { x, y } : null;
  }, 'B receiving audio from A after the renewal', 15000, 0).catch(() => null);
  check(after !== null, 'B still hears A once the old allocation is gone'
    + (after ? ' (' + Math.round((after.y.packets - after.x.packets) / 2) + ' pkt/s)' : ''));
  const lost = ['A', 'B'].filter((n) => logs[n].some((l) => l.includes('connection to the host lost')));
  const same = [];
  for (const [n, p] of [['A', A], ['B', B]]) if ((await text(p, '#participant')) === participants[n]) same.push(n);
  check(lost.length === 0 && same.length === 2, 'no guest lost its connection or rejoined across the renewal'
    + (lost.length ? ' (lost: ' + lost.join(', ') + ')' : ''));
  const started2 = await post('/take/start');
  check(started2[0] === 200, 'take 2 started after the renewal: ' + started2.join(' '));
  await sleep(4000);
  const stopped2 = await post('/take/stop');
  check(stopped2[0] === 200, 'take 2 stopped: ' + stopped2.join(' '));
  const TAKE2 = join(callDir(LIBRARY, 'relay-test'), 'take02');
  const verified2 = () => {
    const m = join(TAKE2, 'manifest.jsonl');
    return existsSync(m) ? readFileSync(m, 'utf8').split('\n').filter((l) => l.startsWith('{"Verified"')).length : 0;
  };
  const synced2 = await waitFor(() => verified2() >= 6, 'take 2 verified on the host', 60000, 250).catch(() => false);
  check(synced2, 'take 2, recorded after the renewal, synced over the new allocation (' + verified2() + ' items verified)');
  const minted2 = (rv.err.match(/relay credentials minted/g) || []).length;
  check(minted2 >= 3 && minted2 > minted, 'the rendezvous minted again for the renewal (' + minted2 + ' in all)');

  // Relay metering. The guests leave, so their coturn sessions end and coturn
  // reports their bytes (it counts a session when it ends).
  const HOST_ID = link.split('#')[1].split('.')[0];
  for (const p of [A, B]) await p.close().catch(() => {});
  const usageOf = async () => {
    const r = await fetch('http://' + RV_ADMIN + '/usage').then((x) => x.json()).catch(() => null);
    return ((r && r.hosts) || []).find((h) => h.host === HOST_ID);
  };
  const used = await waitFor(async () => { const u = await usageOf(); return u && u.bytes > 0 ? u : null; },
    'relay usage for the host key', 180000, 1000).catch(() => null);
  check(!!used, 'the rendezvous counted bytes relayed for the host key: ' + (used ? used.bytes : 'none'));
  const cli = JSON.parse(execFileSync(RV_EXE, ['usage', '--data-dir', RV_DATA, '--json'], { encoding: 'utf8' }));
  const stored = cli.hosts.find((h) => h.host === HOST_ID);
  check(!!stored && stored.bytes > 0, 'the usage subcommand reads it from the data dir: ' + (stored ? stored.bytes : 'none'));
  // Restart with a tiny budget on the same port and data dir.
  rv.p.kill();
  await waitFor(() => rv.exited !== undefined || rv.p.signalCode, 'rendezvous stopped', 10000);
  const rv2 = start(RV_EXE, [...rvArgs(RV_HTTP), '--relay-budget', '1'], 'rendezvous-budget', { CASTCORDER_TURN_SECRET: SECRET });
  await line(rv2, /RENDEZVOUS_READY/, 'RENDEZVOUS_READY (budget)');
  const quotaLogged = await waitFor(() => /relay_quota/.test(host.err), 'host logs relay_quota', 60000).then(() => true, () => false);
  check(quotaLogged, 'the host, reconnected, logged that the rendezvous refused relay credentials with relay_quota');
  const guestHello = await new Promise((resolve, reject) => {
    const ws = new WebSocket('ws://' + RV_HTTP + '/ws?host=' + HOST_ID);
    const t = setTimeout(() => { ws.close(); reject(new Error('no hello')); }, 10000);
    ws.onmessage = (ev) => { clearTimeout(t); ws.close(); resolve(JSON.parse(String(ev.data))); };
    ws.onerror = () => { clearTimeout(t); reject(new Error('websocket error')); };
  });
  const turnGiven = (guestHello.ice_servers || []).some((s) => s.urls.some((u) => u.startsWith('turn:')));
  check(String(guestHello.relay_error || '').startsWith('relay_quota') && !turnGiven,
    'a guest calling the host over budget gets no relay, with relay_error ' + guestHello.relay_error);

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
  const last = stopCoturn(CONTAINER);
  if (!coturnLog) coturnLog = last;
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
