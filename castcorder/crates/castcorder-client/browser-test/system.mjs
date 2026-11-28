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
// Then B leaves mid-take and its page delivers the take over a sync-only
// connection, signalled through the mailbox (background sync), and a browser
// profile and a second host install link to the host's person with link
// requests through the mailbox (lib/personlink.mjs), and two installs'
// persons become contacts on the home through the mailbox (lib/contacts.mjs).
//
// Every port is picked by the OS (none collide with a running `task host`).
// CARGO_TARGET_DIR and CASTCORDER_DIST select other build dirs.
// Run from the repo root with: task test:system
//
// CASTCORDER_RENDEZVOUS=worker runs the same test against the Cloudflare
// Worker (castcorder/worker, ADR 0004) under `wrangler dev` instead of
// castcorder-rendezvous: the client build is staged as its static assets, the
// host and guests reach their host key's Durable Object, and there is no STUN
// port. With Docker it also runs the Worker's relay path against a real
// coturn (lib/coturn.mjs; RELAY=coturn, the shared-secret REST scheme, and
// STUN_URLS naming coturn): a second host with --relay-only and a guest D with
// ?iceTransportPolicy=relay must connect through relayed candidates minted by
// the Worker, and that host must learn its server-reflexive address from the
// STUN server the hello names (it has no STUN port). That phase runs its own
// Worker minting credentials that last RELAY_TTL (30 s) seconds: D must renew
// its own before they expire (cast-lxt.11) without dropping the call, and
// gather a relay candidate after they would have expired. The Worker mode
// also runs the rejoin checks (status queries, lib/rejoin.mjs) through the
// Worker (cast-lxt.8). Without Docker the relay
// phase is skipped with a message, except under CASTCORDER_CI, where it
// fails. Build it first (task worker:build); task test:system:worker does both.
// Everything printed is ASCII. Logs go to browser-test/test-results/system-*.

import { spawn, spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeInput, writeWav16 } from './gen-wav.mjs';
import { VERSION } from './lib/version.mjs';
import { callDir, check, checks } from './lib/helpers.mjs';
import { ctx } from './lib/ctx.mjs';
import { rejoinChecks } from './lib/rejoin.mjs';
import { personLinkChecks } from './lib/personlink.mjs';
import { contactsChecks } from './lib/contacts.mjs';
import { COTURN_IMAGE, dockerProblem, localAddressToward, startCoturn, stopCoturn } from './lib/coturn.mjs';

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
// Every browser here shares 127.0.0.1, so the per-address rate limit meant for
// one person's devices (120/min, burst 60) would be spent by the whole cast:
// a page reloaded late in the run was refused ("rate limited") and never got
// its lobby. The rate limit itself has unit tests in castcorder-mailbox.
const IP_RATE = '6000/600';
const WORKER_DIR = join(WORKSPACE, 'worker');
const WRANGLER = join(WORKER_DIR, 'node_modules', 'wrangler', 'bin', 'wrangler.js');
const needed = WORKER ? [join(WORKER_DIR, 'build', 'index.js'), WRANGLER] : [RV_EXE];
for (const f of [HOST_EXE, ...needed, join(dist, 'index.html')]) {
  if (!existsSync(f)) { console.log('FAIL missing ' + f); process.exit(1); }
}

mkdirSync(RESULTS, { recursive: true });
const INPUT_WAV = join(RESULTS, 'input-44k-s16-stereo.wav');
writeWav16(INPUT_WAV, makeInput());
const LIBRARY = join(RESULTS, 'library-system');
rmSync(LIBRARY, { recursive: true, force: true });
const LIBRARY_RELAY = join(RESULTS, 'library-system-relay');
// TURN_TTL of the relay phase's Worker (s): D renews within the test.
const RELAY_TTL = Number(process.env.CASTCORDER_RELAY_TTL || 30);
rmSync(LIBRARY_RELAY, { recursive: true, force: true });
const TURN_SECRET = randomBytes(18).toString('base64url');
const CONTAINER = 'castcorder-system-coturn-' + process.pid;

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

const logs = { A: [], B: [], C: [], D: [] };
const browsers = [];
const BASE_ARGS = ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
  '--allow-loopback-in-peer-connection', '--autoplay-policy=no-user-gesture-required',
  '--disable-background-timer-throttling', '--mute-audio'];
// What the shared lib/ modules (guest.mjs, rejoin.mjs, personlink.mjs) read.
Object.assign(ctx, { MODE: 'chromium', exe: HOST_EXE, dist, RESULTS, logs, browsers, BASE_ARGS, chromium });
async function openGuest(name, url, extraArgs = []) {
  const browser = await chromium.launch({ headless: true, args: [...BASE_ARGS, ...extraArgs] });
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
// /v/<VERSION>/, local state in a fresh temp dir. With `turn` (coturn's
// address), hellos name coturn for STUN and the Worker mints coturn REST
// credentials (the secret in an env file, which wrangler does not print);
// without, no STUN servers and no relay. `ttl`: TURN_TTL, for a second
// instance (`name`) beside the first, with its own state.
let staged = false;
async function startWorker(turn, { name = 'worker', ttl } = {}) {
  const assets = join(WORKER_DIR, 'assets', 'v', VERSION);
  if (!staged) {
    rmSync(assets, { recursive: true, force: true });
    cpSync(dist, assets, { recursive: true });
    staged = true;
  }
  const [port, inspector] = [await freePort(), await freePort()];
  const state = join(tmpdir(), 'castcorder-wrangler-' + process.pid + '-' + name);
  rmSync(state, { recursive: true, force: true });
  mkdirSync(state, { recursive: true });
  let relay = ['--var', 'STUN_URLS:', '--var', 'RELAY:none'];
  if (turn) {
    const envFile = join(state, 'relay.env');
    writeFileSync(envFile, 'TURN_SECRET=' + TURN_SECRET + '\n');
    relay = ['--var', 'STUN_URLS:stun:' + turn.ip + ':' + turn.port, '--var', 'RELAY:coturn',
      '--var', 'TURN_URLS:' + turn.url, '--env-file', envFile];
    if (ttl) relay.push('--var', 'TURN_TTL:' + ttl);
  }
  const st = start(process.execPath, [WRANGLER, 'dev', '--ip', '127.0.0.1', '--port', String(port),
    '--inspector-port', String(inspector), '--persist-to', state, '--var', 'IP_RATE:' + IP_RATE, ...relay],
  name, { cwd: WORKER_DIR, env: { ...process.env, WRANGLER_SEND_METRICS: 'false', NO_COLOR: '1' } });
  const http = '127.0.0.1:' + port;
  await waitFor(async () => {
    if (st.exited !== undefined) throw new Error('wrangler dev exited with ' + st.exited);
    try { return (await fetch('http://' + http + '/healthz')).ok; } catch { return false; }
  }, 'wrangler dev ready', 120000, 500);
  return [st, http];
}

// The Worker's relay path through coturn: a host offering only its relayed
// candidate (bound toward coturn) and a guest that may only use relayed
// candidates, both with credentials the Worker minted (RELAY=coturn).
async function relayPhase() {
  const [wr, wrHttp] = await startWorker(turn, { name: 'worker-relay', ttl: RELAY_TTL });
  const wrLog = () => wr.out + wr.err;
  console.log('relay worker (wrangler dev, TURN_TTL=' + RELAY_TTL + ') at ' + wrHttp);
  const HOST_IP = await localAddressToward(turn.ip, turn.port);
  const h = start(HOST_EXE, ['--dist', dist, '--library', LIBRARY_RELAY, '--room', 'relay-test', '--bind', HOST_IP,
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--rendezvous', 'http://' + wrHttp, '--relay-only', '--open'],
  'host-relay');
  const relayLink = (await line(h, /CALL_LINK (\S+)/, 'relay host CALL_LINK'))[1];
  const learned = await waitFor(() => h.err.match(/server-reflexive address learned reflexive=(\S+)/),
    'relay host srflx via the STUN server in the hello', 20000);
  check(h.err.includes("STUN server from the rendezvous's ice_servers"),
    'the host took its STUN server from the Worker hello and learned its server-reflexive address: ' + learned[1]);
  const granted = await waitFor(() => h.err.match(/relay \(TURN\) server from the rendezvous.*expires=Some\((\d+)\)/),
    'relay host TURN credentials from the Worker', 20000);
  check(Number(granted[1]) > Date.now() / 1000, 'the Worker minted coturn credentials for the host on registering (expires ' + granted[1] + ')');
  const RELAYED = (await waitFor(() => h.err.match(/turn allocation created relayed=(\S+)/), 'relay host TURN allocation', 30000))[1];
  check(true, 'the relay host allocated on coturn with the Worker\'s credentials: ' + RELAYED);
  const D = await openGuest('D', relayLink.replace('/#', '/?name=D&iceTransportPolicy=relay#'));
  await D.click('#join');
  await waitFor(async () => (await text(D, '#participant')).length > 0, 'guest D joined through the relay', 45000);
  check(logs.D.some((l) => l.includes('ice transport policy forced') && l.includes('relay')), 'guest D forced iceTransportPolicy=relay');
  const pair = await waitFor(() => h.err.match(/chosen candidate pair candidate_pair=Udp (\S+) -> (\S+)/), 'relay host chosen pair', 20000);
  check(pair[1] === RELAYED, 'guest D connected through coturn: the host\'s chosen pair is on its relayed address (' + pair[1] + ' -> ' + pair[2] + ')');
  const counted = () => (wrLog().match(/relay credentials counted/g) || []).length;
  check(counted() >= 2, 'the Worker counted the relay credentials it minted against the daily cap (' + counted() + ')');

  // Renewal (cast-lxt.11): D's credentials last RELAY_TTL seconds. At 80% of
  // that, D asks the Worker for fresh ones (a new hello naming the host,
  // counted against its cap), applies them with setConfiguration and asks
  // the host to restart ICE; its answer must carry a relay candidate. Once
  // the first credentials have expired, a later ICE restart (the host's own
  // renewal, or D's next) must still gather a relay candidate, which coturn
  // grants only with unexpired credentials, and D must stay in the call
  // throughout: same participant, no reconnect.
  const participant = await text(D, '#participant');
  const planned = await waitFor(() => logs.D.find((l) => l.includes('relay credentials will be renewed')), 'D plans a renewal', 10000);
  const exp0 = Number((planned.match(/expires = (\d+)/) || [])[1]);
  // coturn credentials last one to one and a half TTLs: the expiry is rounded
  // up to a step of half the TTL, so a host key's credentials share a coturn
  // username (cast-gln).
  const left0 = exp0 - Date.now() / 1000;
  check(exp0 > 0 && left0 <= RELAY_TTL * 1.5 + 5 && exp0 % Math.max(1, Math.floor(RELAY_TTL / 2)) === 0,
    'D got relay credentials from the Worker lasting ' + RELAY_TTL + ' to ' + RELAY_TTL * 1.5 + ' s, on the step (expires ' + exp0 + ', ' + Math.round(left0) + ' s left)');
  const before = counted();
  // D renews on its own schedule and asks for an ICE restart, or, when the
  // host's own renewal restart comes near (or during) D's renewal, renews
  // inside that restart and asks for none (cast-um1).
  const ASKED = 'relay credentials renewed; asked the host to restart ICE';
  const RENEWED = /relay credentials renewed; asked the host to restart ICE|relay credentials renewed with the host's ICE restart|relay credentials renewed while the host restarted ICE/;
  await waitFor(() => logs.D.some((l) => RENEWED.test(l)), 'D renewing its relay credentials', (RELAY_TTL + 10) * 1000, 250);
  const at = logs.D.findIndex((l) => RENEWED.test(l));
  check(Date.now() / 1000 < exp0, 'D renewed its relay credentials before they expired');
  if (logs.D[at].includes(ASKED)) {
    await waitFor(() => h.err.includes('guest renewed its relay credentials; restarting ICE'), 'the host restarting ICE for D', 10000);
    check(true, 'D asked the host for an ICE restart and the host restarted ICE');
  } else {
    check(true, 'D renewed inside the host\'s own ICE restart and asked for no second one');
  }
  check(counted() > before, 'the Worker minted (and counted) D\'s renewed credentials: ' + before + ' -> ' + counted());
  const relayAnswer = (from) => logs.D.slice(from).find((l) => l.includes('answered host renegotiation')
    && Number((l.match(/relay_candidates = (\d+)/) || [])[1] || 0) >= 1);
  const ans = await waitFor(() => relayAnswer(at), 'D answering the restart with a relay candidate', 15000).catch(() => null);
  check(!!ans, 'D answered the ICE restart with a relay candidate from its renewed credentials');
  // Past the first credentials' expiry: a later restart still gathers a relay
  // candidate, so D gathers with the renewed ones.
  await waitFor(() => Date.now() / 1000 > exp0 + 2, 'D\'s first credentials expiring', RELAY_TTL * 1000, 500);
  const expiredAt = logs.D.length;
  const pairsBefore = (h.err.match(/chosen candidate pair/g) || []).length;
  const late = await waitFor(() => relayAnswer(expiredAt), 'an ICE restart after the first credentials expired', (RELAY_TTL + 10) * 1000, 250).catch(() => null);
  check(!!late, 'after D\'s first credentials expired, an ICE restart still gathered a relay candidate (renewed credentials)');
  const repaired = await waitFor(() => (h.err.match(/chosen candidate pair/g) || []).length > pairsBefore, 'the host choosing a pair after that restart', 15000).catch(() => null);
  check(!!repaired, 'the host chose a candidate pair with D after that restart');
  await sleep(5000);
  const stateD = await text(D, '#state');
  check((await text(D, '#participant')) === participant && stateD === 'connected'
    && !logs.D.some((l) => l.includes('connection to the host lost')),
  'D stayed in the call across its renewals: same participant, no reconnect (' + stateD + ')');
  check(!wrLog().includes(TURN_SECRET) && !h.err.includes(TURN_SECRET), 'no log holds the TURN secret');
}

let passed = false;
let rvHttp = '';
let rv, host, link = '', turn = null, coturnLog = '';
// The rendezvous log: castcorder-rendezvous logs to stderr, wrangler to stdout.
const rvLog = () => rv.out + rv.err;
try {
  let RV_HTTP;
  if (WORKER) {
    const problem = dockerProblem();
    if (!problem) {
      turn = await startCoturn(CONTAINER, TURN_SECRET);
      console.log('coturn (' + COTURN_IMAGE + ', Docker) at ' + turn.ip + ':' + turn.port + '/udp');
    } else if (process.env.CASTCORDER_CI) {
      throw new Error('CASTCORDER_CI is set but the Worker relay phase needs Docker: ' + problem);
    } else {
      console.log('SKIP the Worker relay phase (coturn): ' + problem);
    }
    [rv, RV_HTTP] = await startWorker(turn);
    console.log('worker (wrangler dev) at ' + RV_HTTP + (turn ? ', RELAY=coturn' : ', no relay'));
  } else {
    rv = start(RV_EXE, ['--listen', '127.0.0.1:0', '--stun', '127.0.0.1:0', '--ip-rate', IP_RATE, '--client', VERSION + '=' + dist], 'rendezvous');
    let RV_STUN;
    [, RV_HTTP, RV_STUN] = await line(rv, /RENDEZVOUS_READY http=(\S+) stun=(\S+)/, 'RENDEZVOUS_READY');
    console.log('rendezvous at ' + RV_HTTP + ', stun ' + RV_STUN);
  }

  rvHttp = RV_HTTP;
  host = start(HOST_EXE, ['--dist', dist, '--library', LIBRARY, '--room', 'system-test', '--bind', '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--rendezvous', 'http://' + RV_HTTP], 'host');
  const [, HTTP] = await line(host, /HOST_READY http=(\S+)/, 'HOST_READY');
  link = (await line(host, /CALL_LINK (\S+)/, 'CALL_LINK'))[1];
  console.log('host up at ' + HTTP + '; call link ' + link.replace(/#.*/, '#<host-id>.<secret>'));
  check(link.startsWith('http://' + RV_HTTP + '/v/' + VERSION + '/#'), 'the call link points at the rendezvous site, pinned to /v/' + VERSION + '/');
  if (WORKER) {
    await waitFor(() => host.err.includes('registered with the rendezvous'), 'host registered with the Worker', 30000);
    check(true, 'host registered with its Durable Object');
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
  // Guests' offers carry an SDP (kilobytes); the host's own reachability
  // self-checks (castcorder-host reach) are a nonce of ~100 bytes.
  const guestOffers = (rvLog().match(/offer relayed.*?bytes=(\d+)/g) || [])
    .filter((l) => Number(l.match(/bytes=(\d+)/)[1]) >= 400);
  check(guestOffers.length === 1, 'the rejected guest sent exactly one offer');

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

  // Background sync (cast-xsh, cast-lxt.8): B leaves mid-take with its sync
  // held, then its page delivers the take over a sync-only connection,
  // signalled through the mailbox like its call connection.
  await A.click('#take-start');
  await waitFor(async () => (await text(B, '#recording')).includes('recording take'), 'B recording take 2', 10000);
  await sleep(3000);
  await B.evaluate(() => { window.__castcorderHoldSync = true; });
  await B.click('#leave');
  await sleep(500);
  await A.click('#take-stop');
  await waitFor(async () => (await text(A, '#take-status')).startsWith('idle'), 'take 2 stopped', 10000);
  await waitFor(async () => /file\(s\) left/.test(await text(B, '#sync-state')), 'B shows its take waiting to sync', 30000);
  // The You left page (cast-n56.4): what was left and the delivery progress.
  const leftSummary = await text(B, '#left-summary');
  const leftFiles = await text(B, '#left-files');
  check(await B.isVisible('#left-call') && leftSummary.includes('2 takes recorded') && /^0 of [1-9]\d* files sent/.test(leftFiles),
    'B lands on You left with its takes and the files still to send: ' + leftSummary + ' / ' + leftFiles);
  const TAKE2 = join(callDir(LIBRARY, 'system-test'), 'take02');
  const synced2 = () => existsSync(TAKE2) && readdirSync(TAKE2).filter((f) => f.endsWith('_mic.wav')).length >= 2
    && !readdirSync(TAKE2).some((f) => f.endsWith('.journal') || f.endsWith('.obao'));
  check(!synced2(), 'B\'s take 2 is not on the host while its sync is held');
  const sealed = () => logs.B.filter((l) => l.includes('offer sealed to the host through the rendezvous')).length;
  const sealedBefore = sealed();
  await B.evaluate(() => { window.__castcorderHoldSync = false; });
  await waitFor(synced2, 'B\'s take 2 delivered over a sync-only connection', 60000, 250);
  check(host.err.includes('sync-only connection accepted'), 'the host accepted B\'s sync-only connection');
  check(sealed() > sealedBefore && logs.B.some((l) => l.includes('answer applied') && l.includes('sync_only = true')),
    'B\'s sync-only connection was signalled through the rendezvous mailbox');
  await waitFor(async () => !/file\(s\) left/.test(await text(B, '#sync-state')), 'B says its take was delivered', 30000);
  check((await text(B, '#sync-state')).includes('reached the host') && !(await B.isVisible('#left-files')),
    'You left says when every recording reached the host: ' + (await text(B, '#sync-state')));
  check(true, 'B\'s take 2 synced after it left, over a sync-only connection through the rendezvous');

  // Link requests through the mailbox (lib/personlink.mjs).
  await personLinkChecks({ HTTP, rendezvous: RV_HTTP });
  // Contacts through the mailbox, on the home (lib/contacts.mjs).
  await contactsChecks({ HTTP, rendezvous: RV_HTTP });

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
  // Status queries and rejoining (cast-zr8) through the Worker: the shared
  // scenario, against this Worker (the browser tier runs it against
  // castcorder-rendezvous).
  if (WORKER) await rejoinChecks({ rendezvous: RV_HTTP });
  if (turn) await relayPhase();
  for (const n of turn ? ['A', 'B', 'D'] : ['A', 'B']) {
    const bad = logs[n].filter((l) => l.startsWith('[pageerror]'));
    check(bad.length === 0, 'guest ' + n + ': no page errors' + (bad.length ? ': ' + bad.slice(0, 3).join(' | ') : ''));
  }
  passed = checks.every((c) => c.ok);
} catch (e) {
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  for (const [, st] of procs) killTree(st.p);
  for (const p of [ctx.host8, ctx.host9, ctx.host10]) if (p) p.kill();
  if (turn) coturnLog = stopCoturn(CONTAINER).replaceAll(TURN_SECRET, '<secret>');
}
if (coturnLog) writeFileSync(join(RESULTS, 'system-coturn.log'), coturnLog);
for (const [name, st] of procs) writeFileSync(join(RESULTS, 'system-' + name + '.log'), st.out + '\n--- stderr ---\n' + st.err);
for (const n of Object.keys(logs)) writeFileSync(join(RESULTS, 'system-guest-' + n + '.log'), logs[n].join('\n'));
if (ctx.host8) writeFileSync(join(RESULTS, 'system-host-rejoin.log'), ctx.host8Log || '');
if (ctx.host9) writeFileSync(join(RESULTS, 'system-host-person-2.log'), ctx.host9Log || '');
if (ctx.host10) writeFileSync(join(RESULTS, 'system-host-contacts-2.log'), ctx.host10Log || '');
if (!passed) {
  for (const [name, st] of procs) {
    console.log(name + ' log tail:');
    console.log((st.err.trim() ? st.err : st.out).split('\n').slice(-25).join('\n'));
  }
  if (coturnLog) { console.log('coturn log tail:'); console.log(coturnLog.split('\n').slice(-25).join('\n')); }
  for (const n of Object.keys(logs)) { console.log('guest ' + n + ' console tail:'); console.log(logs[n].slice(-15).join('\n')); }
}
console.log('logs: crates/castcorder-client/browser-test/test-results/system-*');
console.log(passed ? 'PASS a guest with only the call link joined through the rendezvous, heard another guest, and its take synced to the host'
  : 'FAIL (system)');
process.exit(passed ? 0 : 1);
