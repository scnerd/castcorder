// Real-browser test for the Browser Guest (castcorder-client), ADR 0003's real
// boundary: the castcorder-host binary (real UDP socket, system clock, bound to
// loopback here) serving the trunk build of the client, plus two headless
// Chromium guests.
//
// 1. Guest A is fed a deterministic WAV (--use-file-for-fake-audio-capture) and
//    loads the page from the host's localhost HTTP listener, so it is the
//    producer; guest B uses Chromium's default fake microphone and loads the
//    page over HTTPS (the self-signed certificate, errors ignored), so it gets
//    no producer controls and the host refuses it the take endpoints. Both join.
// 2. B receives A's forwarded audio (inbound RTP from A's stream rising).
//    Live video (every mode, see videoChecks): each guest decodes the other's
//    fake camera in a named tile, A sends two simulcast layers, and A's camera
//    turned off is released, stops B's frames while audio goes on, and turned
//    on again is decoded again, with no renegotiation.
// 3. A's producer controls start a take; ~5 s later they stop it.
// 4. Each guest's outbox syncs its track and JSON sidecar into the host's
//    library (--library test-results/library), verified; only then is the
//    OPFS copy deleted, and the test waits until both guests' OPFS is empty.
//    Mid-take, a second tab of A's profile loads: its startup scan must list
//    A's recording track as busy (files held by A's worker), with no error.
// 5. A's track, as the host received it, must be bit-exact against the input
//    (Chrome's int16 -> float scaling, every frame aligned, no dropped,
//    duplicated or inserted frames), about 5 s long, 44.1 kHz stereo.
//
// 6. Take 2 (every mode): guest B's RTCPeerConnection is closed mid-take; B
//    keeps recording into the same file, shows "reconnecting", rejoins, then
//    its microphone tracks are stopped (as a phone's system may) and it
//    reopens them and continues in a new segment. Both segments reach the host.
// 7. Producer buttons offer only the valid action (start when idle, stop when
//    recording). After Leave every local MediaStreamTrack is "ended" and every
//    AudioContext "closed"; leave/rejoin cycles raise no page errors or
//    unhandled rejections.
//
// 8. Each guest's closed log files arrive in <call>/logs on the host (lowest priority,
//    after the tracks), are valid JSON lines with the call and device fields, and
//    the merge-logs example prints them with the host's own log merged by time.
//
// Modes (first argument; default chromium, everything above):
//   fallback - as above, but MediaStreamTrackProcessor is deleted from window in
//              both Chromium guests before the app loads, so recording must fall
//              back (worker MSTP if it works, else the AudioWorklet tap). Reduced
//              checks: join, each hears the other, a take records on both, both
//              tracks and sidecars arrive on the host; A's track is analysed
//              against the input and the result printed, not required bit-exact.
//   firefox  - the same reduced checks with two headless Firefox guests (A the
//              producer over localhost HTTP, B over HTTPS: the owner's desktop
//              and phone cases); recording must use the AudioWorklet path. The
//              host binds 0.0.0.0 and advertises this machine's LAN address, as
//              `task host` does: Firefox on Windows gathers no loopback ICE
//              candidates, so it cannot reach a host bound to 127.0.0.1.
//
// 9. (chromium) Producers and room security on a second, producer-gated host:
//    see securityChecks() at the end.
// 10. (chromium, firefox) Device identity: every join above proves its device
//    key; on a third host an upgraded pre-key profile keeps its trust and its
//    pending track, and an impostor with another device's id is refused: see
//    identityChecks() at the end. The same profile also holds a track with an
//    empty sidecar (recovered from its timing header and synced) and audio
//    with no sidecar (kept; the scan creates no file).
//
// Run from the repo root with:  task test:browser:client  (or task test:browser)
// Everything printed is ASCII. Logs go to browser-test/test-results/.

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeInput, writeWav16, INPUT_RATE } from './gen-wav.mjs';
import { parseWav, deinterleave, analyzeRecording } from './analyze.mjs';
import { parseOggOpus } from './ogg.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '..');
const WORKSPACE = join(CRATE, '..', '..');
const RESULTS = join(HERE, 'test-results');
const TAKE_SECONDS = 5;
const MODE = process.argv[2] || 'chromium';
if (!['chromium', 'fallback', 'firefox'].includes(MODE)) { console.log('FAIL unknown mode ' + MODE); process.exit(1); }
const FULL = MODE === 'chromium';
console.log('mode ' + MODE);
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const { chromium, firefox } = await import('playwright');

// CARGO_TARGET_DIR and CASTCORDER_DIST let a run use other build dirs (e.g.
// while a host from `task host` holds target/ and serves dist/).
const exe = join(process.env.CARGO_TARGET_DIR || join(WORKSPACE, 'target'), 'release',
  process.platform === 'win32' ? 'castcorder-host.exe' : 'castcorder-host');
const dist = process.env.CASTCORDER_DIST || join(CRATE, 'dist');
if (!existsSync(exe)) { console.log('FAIL host binary missing: ' + exe); process.exit(1); }
if (!existsSync(join(dist, 'index.html'))) { console.log('FAIL client not built: ' + dist); process.exit(1); }

mkdirSync(RESULTS, { recursive: true });
const input = makeInput();
const INPUT_WAV = join(RESULTS, 'input-44k-s16-stereo.wav');
const LIBRARY = join(RESULTS, FULL ? 'library' : 'library-' + MODE);
rmSync(LIBRARY, { recursive: true, force: true });
writeWav16(INPUT_WAV, input);

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

const host = spawn(exe, ['--dist', dist, '--library', LIBRARY, '--call', 'browser-test', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
  '--port', '0', '--http-port', '0', '--udp-port', '0',
  // Always open: the leave/rejoin checks rejoin B after A (the producer) left.
  '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
let hostLog = '';
host.stderr.on('data', (d) => { hostLog += d.toString(); });
const [HTTP, HTTPS] = await new Promise((resolve, reject) => {
  let buf = '';
  const t = setTimeout(() => reject(new Error('host did not print HOST_READY')), 20000);
  host.stdout.on('data', (d) => {
    buf += d.toString();
    const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
    if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
  });
  host.on('exit', (c) => reject(new Error('host exited early with ' + c)));
});
console.log('host up at ' + HTTP + ' and ' + HTTPS);
if (MODE === 'firefox' && /\/\/127\./.test(HTTPS)) {
  // No LAN address: Firefox (no loopback ICE candidates on Windows) cannot connect.
  host.kill();
  console.log('SKIP firefox: this machine has no LAN IPv4 address for the host to advertise');
  process.exit(0);
}

const BASE_ARGS = [
  '--use-fake-device-for-media-stream',
  '--use-fake-ui-for-media-stream',
  '--allow-loopback-in-peer-connection',
  '--autoplay-policy=no-user-gesture-required',
  '--disable-background-timer-throttling',
  // Automated runs stay silent. The fallback run's 5 s take still records
  // bit-exact with this flag; longer-run render-clock effects are not covered.
  '--mute-audio',
];
const logs = { A: [], B: [] };
const browsers = [];

const FIREFOX_PREFS = {
  'media.navigator.streams.fake': true,
  'media.navigator.permission.disabled': true,
  // ICE and autoplay stay at Firefox's defaults, as on the owner's machine.
  // Automated runs stay silent: every output is scaled to 0 while the output
  // stream itself keeps running (the AudioWorklet's render clock needs it).
  'media.volume_scale': '0.0',
};

async function openGuest(name, extraArgs, base, setup) {
  logs[name] = logs[name] || [];
  let browser, ctx;
  if (MODE === 'firefox') {
    // Distinct fake tones: identical recordings would share one content root,
    // and the host library keys tracks by root (one entry for both guests).
    const prefs = { ...FIREFOX_PREFS, 'media.navigator.audio.fake_frequency': name === 'A' ? 1000 : 1500 };
    browser = await firefox.launch({ headless: true, firefoxUserPrefs: prefs });
    ctx = await browser.newContext({ ignoreHTTPSErrors: true });
  } else {
    browser = await chromium.launch({ headless: true, args: [...BASE_ARGS, ...extraArgs] });
    ctx = await browser.newContext({ permissions: ['microphone', 'camera'], ignoreHTTPSErrors: true });
  }
  browsers.push(browser);
  if (MODE === 'fallback') {
    // A browser without MediaStreamTrackProcessor on window (Firefox, Safari,
    // or a Chrome that exposes it in workers only).
    await ctx.addInitScript(() => { delete window.MediaStreamTrackProcessor; });
  }
  // Instrumentation, not a mock: the real objects the app creates are kept so
  // the test can check them after Leave and cut a connection.
  await ctx.addInitScript(() => {
    const tracks = window.__tracks = [];
    const ctxs = window.__audioContexts = [];
    const pcs = window.__pcs = [];
    const gum = MediaDevices.prototype.getUserMedia;
    MediaDevices.prototype.getUserMedia = async function (...a) {
      const st = await gum.apply(this, a);
      for (const t of st.getTracks()) if (!tracks.includes(t)) tracks.push(t);
      return st;
    };
    // Every track the app reads from a stream, too (whatever created it).
    for (const m of ['getTracks', 'getAudioTracks', 'getVideoTracks']) {
      const f = MediaStream.prototype[m];
      MediaStream.prototype[m] = function () {
        const r = f.call(this);
        for (const t of r) if (!tracks.includes(t)) tracks.push(t);
        return r;
      };
    }
    const clone = MediaStreamTrack.prototype.clone;
    MediaStreamTrack.prototype.clone = function () { const t = clone.call(this); tracks.push(t); return t; };
    const AC = window.AudioContext;
    window.AudioContext = class extends AC { constructor(...a) { super(...a); ctxs.push(this); } };
    const PC = window.RTCPeerConnection;
    window.RTCPeerConnection = class extends PC { constructor(...a) { super(...a); pcs.push(this); } };
  });
  if (setup) await setup(ctx);
  const page = await ctx.newPage();
  page.on('console', (m) => logs[name].push(m.text()));
  page.on('pageerror', (e) => logs[name].push('[pageerror] ' + e.message));
  await page.goto(base + '/?name=' + name);
  await page.waitForSelector('#join');
  return page;
}

const text = async (page, sel) => (await page.textContent(sel)) || '';
const buttons = async (page) => ({
  start: await page.isEnabled('#take-start'), stop: await page.isEnabled('#take-stop'),
});
const json = async (page, sel) => { const t = await text(page, sel); return t ? JSON.parse(t) : null; };
async function noErrors(page, name) {
  const e = await text(page, '#errors');
  if (e) throw new Error('guest ' + name + ' errors: ' + e);
}

async function readOpfs(page, dir, name) {
  const parts = [];
  let off = 0, size = Infinity;
  const CHUNK = 4 * 1024 * 1024;
  while (off < size) {
    const r = await page.evaluate(async ([dir, name, off, len]) => {
      const root = await navigator.storage.getDirectory();
      const d = await root.getDirectoryHandle(dir);
      const f = await (await d.getFileHandle(name)).getFile();
      const b = new Uint8Array(await f.slice(off, off + len).arrayBuffer());
      let s = '';
      for (let i = 0; i < b.length; i += 8192) s += String.fromCharCode.apply(null, b.subarray(i, i + 8192));
      return { size: f.size, b64: btoa(s) };
    }, [dir, name, off, CHUNK]);
    size = r.size;
    const buf = Buffer.from(r.b64, 'base64');
    parts.push(buf);
    off += buf.length;
    if (buf.length === 0) break;
  }
  return Buffer.concat(parts);
}

function audioFrom(inbound, origin) {
  return (inbound || []).filter((i) => i.kind === 'audio' && i.origin.startsWith(origin + '-'))
    .reduce((a, i) => ({ packets: a.packets + i.packetsReceived, bytes: a.bytes + i.bytesReceived }), { packets: 0, bytes: 0 });
}

const checks = [];
function check(ok, what) {
  checks.push({ ok, what });
  console.log((ok ? 'ok   ' : 'BAD  ') + what);
}

let host2 = null;
let host2Log = '';
let host3 = null;
let host3Log = '';
let passed = false;
try {
  const A = await openGuest('A', ['--use-file-for-fake-audio-capture=' + INPUT_WAV], HTTP);
  const B = await openGuest('B', [], HTTPS);
  console.log((MODE === 'firefox' ? 'firefox ' : 'chromium ') + browsers[0].version());

  check((await A.locator('#take-start').count()) === 0, 'no producer controls before joining');
  const refused = await B.evaluate(async () => (await fetch('/take/start', { method: 'POST' })).status);
  check(refused === 403, 'host refuses take control over HTTPS (' + refused + ')');

  for (const [n, p] of [['A', A], ['B', B]]) {
    await p.click('#join');
    await waitFor(async () => { await noErrors(p, n); return (await text(p, '#participant')).length > 0; },
      'guest ' + n + ' joined');
    console.log('guest ' + n + ' joined as participant ' + (await text(p, '#participant')));
  }
  // Producer controls: shown from the host's role event, to the page from the
  // host machine's localhost only.
  await A.waitForSelector('#take-start', { timeout: 10000 });
  await sleep(1000);
  check((await B.locator('#take-start').count()) === 0, 'producer controls on the localhost page only, not on the HTTPS page');

  // B receives A's forwarded audio.
  const flowing = await waitFor(async () => {
    const x = audioFrom(await json(B, '#debug-inbound'), 'A');
    await sleep(2000);
    const y = audioFrom(await json(B, '#debug-inbound'), 'A');
    return y.packets > x.packets && y.bytes > x.bytes ? { x, y } : null;
  }, 'B receiving audio from A', 30000, 0);
  check(true, 'B receives A\'s forwarded audio (' + Math.round((flowing.y.packets - flowing.x.packets) / 2) + ' pkt/s)');
  if (!FULL) {
    const back = await waitFor(async () => {
      const x = audioFrom(await json(A, '#debug-inbound'), 'B');
      await sleep(2000);
      const y = audioFrom(await json(A, '#debug-inbound'), 'B');
      return y.packets > x.packets && y.bytes > x.bytes ? { x, y } : null;
    }, 'A receiving audio from B', 30000, 0);
    check(true, 'A receives B\'s forwarded audio (' + Math.round((back.y.packets - back.x.packets) / 2) + ' pkt/s)');
    // The capture path each guest chose, from its structured join log line.
    for (const n of ['A', 'B']) {
      const line = await waitFor(async () => logs[n].find((l) => l.includes('recording capture path chosen') || l.includes('RECORDING UNAVAILABLE')),
        'guest ' + n + ' capture path log line', 15000);
      const path = (line.match(/capture_path = "?([a-z-]+)/) || [])[1];
      check(line.includes('user_agent') && line.includes('raw_settings') && path && path !== 'none',
        'guest ' + n + ' logged its user agent, raw track settings and capture path: ' + path);
      if (MODE === 'firefox') check(path === 'worklet', 'guest ' + n + ' (Firefox) records through the AudioWorklet path');
      if (MODE === 'fallback') check(path === 'worklet' || path === 'mstp-worker', 'guest ' + n + ' (no MSTP on window) fell back to ' + path);
    }
  }

  // Live video (every mode): each decodes the other's camera, simulcast, and
  // the camera turns off and on without renegotiating.
  await videoChecks(A, B);

  // The raw path is warm (read since join; the spike's 500 ms warm-up lesson).
  await waitFor(async () => (await text(A, '#level-db')) !== '-inf dBFS', 'A level meter');
  await sleep(1000);

  await waitFor(async () => buttons(A).then((b) => b.start && !b.stop), 'producer offers Start take only', 10000);
  check(true, 'idle: Start take enabled, Stop take disabled');
  await A.click('#take-start');
  await waitFor(async () => (await text(A, '#take-result')).includes('"take":1'), 'take 1 started', 10000);
  const take1StartUs = JSON.parse(await text(A, '#take-result')).hostUs;
  check(typeof take1StartUs === 'number', 'host reports take 1 start on its timeline (' + take1StartUs + ' us)');
  await waitFor(async () => buttons(A).then((b) => !b.start && b.stop), 'producer offers Stop take only', 10000);
  check(true, 'recording: Start take disabled, Stop take enabled');
  check(true, 'producer started take 1');
  for (const [n, p] of [['A', A], ['B', B]]) {
    await waitFor(async () => { await noErrors(p, n); return (await text(p, '#recording')).includes('recording take'); },
      'guest ' + n + ' recording', 10000);
  }
  const takeT0 = Date.now();
  // A second tab of A's profile (same OPFS) loads mid-take: its worker's
  // startup scan meets the files A's worker holds open (one sync handle per
  // file) and must list the track as busy, without an error and without
  // touching it (A's take is checked bit-exact below).
  {
    const second = await A.context().newPage();
    const secondLog = [];
    second.on('console', (m) => secondLog.push(m.text()));
    second.on('pageerror', (e) => secondLog.push('[pageerror] ' + e.message));
    await second.goto(HTTP + '/?name=A2');
    let listed;
    try {
      listed = await waitFor(async () => { const t = await json(second, '#debug-tracks'); return t && t.length ? t : null; },
        'second tab lists A\x27s tracks', 10000);
    } catch { listed = []; }
    const errs = await text(second, '#errors');
    check(listed.length > 0 && listed.every((t) => t.busy) && errs === '',
      'a second tab\x27s startup scan skips the track another tab is recording (busy, no error): '
      + JSON.stringify(listed) + (errs ? ' errors: ' + errs : ''));
    await second.close();
    writeFileSync(join(RESULTS, 'guest-A-second-tab' + (FULL ? '' : '-' + MODE) + '.log'), secondLog.join('\n'));
  }
  await sleep(Math.max(0, TAKE_SECONDS * 1000 - (Date.now() - takeT0)));
  await A.click('#take-stop');
  await waitFor(async () => (await text(A, '#take-status')).startsWith('idle'), 'take 1 stopped', 10000);
  check(true, 'producer stopped take 1');
  await waitFor(async () => buttons(A).then((b) => b.start && !b.stop), 'producer offers Start take again', 10000);
  check(true, 'idle again: Start take enabled, Stop take disabled');
  const guestsSeen = await text(A, '#guests');
  check(guestsSeen.includes('A (guest') && guestsSeen.includes('B (guest'), 'producer lists the connected guests: ' + guestsSeen);

  // The outbox: after the take, each guest's track (and its JSON sidecar) is
  // pulled into the host's library, verified, and only then deleted from OPFS.
  const TAKE_DIR = join(LIBRARY, 'browser-test', 'take01');
  const verifiedCount = () => {
    const m = join(TAKE_DIR, 'manifest.jsonl');
    if (!existsSync(m)) return 0;
    return readFileSync(m, 'utf8').split('\n').filter((l) => l.startsWith('{"Verified"')).length;
  };
  await waitFor(async () => verifiedCount() >= 6, 'host verified both guests\' tracks, sidecars and timing metadata', 60000, 250);
  const opfsNames = (p) => p.evaluate(async () => {
    const root = await navigator.storage.getDirectory();
    const d = await root.getDirectoryHandle('tracks', { create: true });
    const out = [];
    for await (const k of d.keys()) out.push(k);
    return out;
  });
  for (const [n, p] of [['A', A], ['B', B]]) {
    const left = await waitFor(async () => ((await opfsNames(p)).length === 0 ? [] : null), 'guest ' + n + ' OPFS emptied after sync', 20000);
    check(left.length === 0, 'guest ' + n + ' deleted its local copies after verified receipt');
  }
  // Logs (cast-zoa): each guest closed its log file when the take stopped and
  // offered it as the lowest-priority outbox item; the host stores it under
  // <call>/logs/<device>-<name>.jsonl beside its own host-<start>.jsonl.
  const LOGS_DIR = join(LIBRARY, 'browser-test', 'logs');
  if (FULL) {
  const guestLogFiles = () => (existsSync(LOGS_DIR) ? readdirSync(LOGS_DIR) : [])
    .filter((f) => f.endsWith('.jsonl') && f !== 'manifest.jsonl' && !f.startsWith('host-'));
  const devicesWithLogs = () => new Set(guestLogFiles().map((f) => f.split('-log-')[0]));
  await waitFor(async () => {
    const m = join(LOGS_DIR, 'manifest.jsonl');
    const verified = existsSync(m)
      ? readFileSync(m, 'utf8').split('\n').filter((l) => l.startsWith('{"Verified"')).length : 0;
    return verified >= 2 && devicesWithLogs().size >= 2;
  }, 'both guests\' log files verified on the host', 60000, 250);
  const logFiles = readdirSync(LOGS_DIR).sort();
  console.log('host logs: ' + logFiles.join(' '));
  check(devicesWithLogs().size >= 2 && !logFiles.some((f) => f.endsWith('.obao') || f.endsWith('.journal')),
    'log files of ' + devicesWithLogs().size + ' guest devices arrived in <call>/logs, with no transfer state left');
  check(logFiles.some((f) => f.startsWith('host-') && f.endsWith('.jsonl')), 'the host wrote its own host-<start>.jsonl');
  let withBoth = 0, parsed = 0;
  for (const f of logFiles.filter((f) => f.endsWith('.jsonl') && f !== 'manifest.jsonl')) {
    const lines = readFileSync(join(LOGS_DIR, f), 'utf8').split('\n').filter((l) => l.length > 0);
    let ok = lines.length > 0;
    for (const l of lines) {
      try {
        const v = JSON.parse(l);
        ok = ok && typeof v.ts === 'string' && /^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$/.test(v.ts)
          && typeof v.mono_ms === 'number' && typeof v.level === 'string' && typeof v.target === 'string'
          && typeof v.message === 'string' && typeof v.fields === 'object';
        if (!f.startsWith('host-') && v.fields.call && v.fields.device) withBoth++;
      } catch { ok = false; }
    }
    parsed += ok ? 1 : 0;
    if (!ok) console.log('BAD log file ' + f);
  }
  check(parsed === logFiles.filter((f) => f.endsWith('.jsonl') && f !== 'manifest.jsonl').length,
    'every log file is valid JSON lines in the shared shape');
  check(withBoth > 0, 'guest log lines carry the call and device fields (' + withBoth + ' lines)');
  const merged = spawnSync('cargo', ['run', '--quiet', '-p', 'castcorder-core', '--example', 'merge-logs', '--', LOGS_DIR],
    { cwd: join(WORKSPACE), encoding: 'utf8', shell: process.platform === 'win32' });
  const mergedLines = (merged.stdout || '').split('\n').filter((l) => l.length > 0);
  writeFileSync(join(RESULTS, 'merged-logs.txt'), merged.stdout || '');
  const stamps = mergedLines.map((l) => l.split(' ')[0]);
  check(merged.status === 0 && mergedLines.length > 10 && stamps.every((s, i) => i === 0 || stamps[i - 1] <= s)
    && mergedLines.some((l) => l.includes('[host host]')) && mergedLines.some((l) => !l.includes('[host host]')),
    'merge-logs prints host and guest lines merged by time (' + mergedLines.length + ' lines)');
  }

  const hostFiles = readdirSync(TAKE_DIR).sort();
  console.log('host take01: ' + hostFiles.join(' '));
  check(['take01_A_mic.json', 'take01_A_mic.wav', 'take01_A_mic.timing.jsonl', 'take01_B_mic.json', 'take01_B_mic.wav',
    'take01_B_mic.timing.jsonl'].every((f) => hostFiles.includes(f))
    && !hostFiles.some((f) => f.endsWith('.obao') || f.endsWith('.journal')),
    'host library holds both tracks, sidecars and timing metadata, with no transfer state left');
  // Safety tracks (cast-706): the host kept each guest's live Opus stream in
  // take01/safety/, one Ogg Opus per guest with a timing sidecar, none of it
  // in the take directory proper (so the outbox and aligner never see it).
  {
    const safetyDir = join(TAKE_DIR, 'safety');
    const safety = existsSync(safetyDir) ? readdirSync(safetyDir).sort() : [];
    console.log('host take01/safety: ' + safety.join(' '));
    check(['A', 'B'].every((n) => safety.includes('take01_' + n + '_safety.opus') && safety.includes('take01_' + n + '_safety.timing.jsonl'))
      && safety.filter((f) => f.endsWith('.opus')).length === 2,
      'safety/ holds one .opus and one timing file per guest');
    for (const n of ['A', 'B']) {
      const o = parseOggOpus(readFileSync(join(safetyDir, 'take01_' + n + '_safety.opus')));
      check(o.finished && o.packets > 0 && o.seconds > TAKE_SECONDS - 1.5 && o.seconds < TAKE_SECONDS + 1.5,
        n + '\x27s safety track is valid Ogg Opus: ' + o.packets + ' packets, ' + o.seconds.toFixed(2) + ' s for a ~' + TAKE_SECONDS + ' s take');
      const tl = readFileSync(join(safetyDir, 'take01_' + n + '_safety.timing.jsonl'), 'utf8').trim().split('\n').map((l) => JSON.parse(l));
      check(tl[0].type === 'header' && tl[0].source === 'safety' && tl.some((l) => l.type === 'ref' && typeof l.host_us === 'number'),
        n + '\x27s safety timing file is marked as a safety track and maps RTP time to host time');
    }
    check(!hostFiles.some((f) => f.includes('safety') && f !== 'safety'), 'no safety file in the take directory itself');
  }
  // Timing metadata (cast-whr): each guest's file parses, and its mapping
  // places the track's first frame just after the host's take start (loopback:
  // clocks are shared, so this checks the plumbing, not the estimator).
  // The worklet capture path (fallback, firefox) starts on the audio thread's own schedule,
  // and on a loaded CI runner Firefox was seen 44-72 ms late, so those modes allow 150 ms.
  const FRAME0_TOL_MS = MODE === 'chromium' ? 50 : 150;
  for (const n of ['A', 'B']) {
    const t = parseTiming(readFileSync(join(TAKE_DIR, 'take01_' + n + '_mic.timing.jsonl'), 'utf8'));
    const start = t.hostUsAt(0);
    const delta = (start - take1StartUs) / 1000;
    console.log('info ' + n + ' timing: ' + t.refs.length + ' refs, ' + t.sync.length + ' clock samples, last estimate '
      + JSON.stringify(t.lastEstimate) + '; frame 0 at ' + delta.toFixed(1) + ' ms after the take start');
    check(t.header && t.header.version === 1 && t.header.take === 1 && t.segments === 1 && t.end !== null && t.sync.length > 0
      && t.refs.length >= TAKE_SECONDS - 1 && Math.abs(delta) < FRAME0_TOL_MS,
      n + '\x27s timing metadata places frame 0 within ' + FRAME0_TOL_MS + ' ms of the host\x27s take start (' + delta.toFixed(1) + ' ms)');
  }
  const SUFFIX = FULL ? '' : '-' + MODE;
  const wavBytes = readFileSync(join(TAKE_DIR, 'take01_A_mic.wav'));
  writeFileSync(join(RESULTS, 'A-track' + SUFFIX + '.wav'), wavBytes);
  const wav = parseWav(wavBytes);
  const sidecar = JSON.parse(readFileSync(join(TAKE_DIR, 'take01_A_mic.json'), 'utf8'));
  writeFileSync(join(RESULTS, 'A-track' + SUFFIX + '.json'), JSON.stringify(sidecar, null, 2));
  if (!FULL) {
    for (const n of ['A', 'B']) {
      const w = parseWav(readFileSync(join(TAKE_DIR, 'take01_' + n + '_mic.wav')));
      const sc = JSON.parse(readFileSync(join(TAKE_DIR, 'take01_' + n + '_mic.json'), 'utf8'));
      const secs = w.frames / w.rate;
      let pk = 0;
      for (let i = 0; i < w.samples.length; i++) pk = Math.max(pk, Math.abs(w.samples[i]));
      check(w.tag === 3 && w.bits === 32 && w.dataDeclared === w.frames * w.channels * 4,
        n + '\'s track is float32 WAV, ' + w.rate + ' Hz, ' + w.channels + ' ch, header matches the data');
      check(secs > TAKE_SECONDS - 1 && secs < TAKE_SECONDS + 2, n + '\'s track is ' + secs.toFixed(2) + ' s for a ~' + TAKE_SECONDS + ' s take');
      check(pk > 0.001, n + '\'s track is not silent (peak ' + pk.toFixed(4) + ')');
      check(sc.finalised === true && sc.take === 1 && sc.frames === w.frames && typeof sc.capturePath === 'string'
        && Array.isArray(sc.discontinuities) && typeof sc.insertedFrames === 'number' && typeof sc.droppedFrames === 'number',
        n + '\'s sidecar: finalised, capture ' + sc.capturePath + ', ' + sc.discontinuities.length + ' discontinuities (inserted '
        + sc.insertedFrames + ', dropped ' + sc.droppedFrames + ' frames)');
    }
    if (MODE === 'fallback') {
      // Informational: how the fallback path compares with the input.
      for (const c of analyzeRecording(deinterleave(wav.samples, wav.channels), input)) {
        console.log('info A channel ' + c.channel + ' vs ' + c.against + ': ' + c.verdict + ' (aligned ' + c.aligned + '/' + c.considered
          + ', dropped ' + c.droppedFrames + ', duplicated ' + c.duplicatedFrames + ', inserted ' + c.insertedFrames
          + ', discontinuities ' + c.discontinuities + ')');
      }
    }
  }
  if (FULL) {
  const wavB = parseWav(readFileSync(join(TAKE_DIR, 'take01_B_mic.wav')));
  check(wavB.frames / wavB.rate > TAKE_SECONDS - 1, 'B\'s track arrived too (' + (wavB.frames / wavB.rate).toFixed(2) + ' s)');
  const seconds = wav.frames / wav.rate;
  check(wav.tag === 3 && wav.bits === 32, 'track is float32 WAV');
  check(wav.rate === INPUT_RATE && wav.channels === 2, 'track is ' + wav.rate + ' Hz, ' + wav.channels + ' ch (input ' + INPUT_RATE + ' Hz, 2 ch)');
  check(wav.dataDeclared === wav.frames * 8, 'header data size matches the frames on disk');
  check(seconds > TAKE_SECONDS - 1 && seconds < TAKE_SECONDS + 2, 'track is ' + seconds.toFixed(2) + ' s for a ~' + TAKE_SECONDS + ' s take');
  check(sidecar.finalised === true && sidecar.take === 1 && sidecar.frames === wav.frames,
    'sidecar finalised, take 1, frames match (' + sidecar.frames + ')');

  const per = analyzeRecording(deinterleave(wav.samples, wav.channels), input);
  for (const c of per) {
    const clean = c.verdict.startsWith('bit-exact') && c.discontinuities === 0 && c.droppedFrames === 0
      && c.duplicatedFrames === 0 && c.insertedFrames === 0 && c.mismatched === 0;
    check(clean, 'channel ' + c.channel + ' vs ' + c.against + ': ' + c.verdict + ' (aligned ' + c.aligned + '/' + c.considered
      + ', leading zeros ' + c.leadingZeroFrames + ', unlocked ' + c.unlocked + ', mismatched ' + c.mismatched
      + ', dropped ' + c.droppedFrames + ', duplicated ' + c.duplicatedFrames + ', inserted ' + c.insertedFrames + ')');
  }
  // Aligned tracks (cast-pzb): once both tracks and their timing metadata are
  // verified the host renders take01/aligned/, one file per guest, equal
  // lengths; the summary is written last.
  const ALIGNED = join(TAKE_DIR, 'aligned');
  await waitFor(async () => existsSync(join(ALIGNED, 'take01_alignment.json')), 'aligned take01', 60000);
  const alignedFiles = readdirSync(ALIGNED).filter((f) => f.endsWith('.wav') || f.endsWith('.flac')).sort();
  const summary = JSON.parse(readFileSync(join(ALIGNED, 'take01_alignment.json'), 'utf8'));
  const alignedLens = alignedFiles.filter((f) => f.endsWith('.wav')).map((f) => parseWav(readFileSync(join(ALIGNED, f))).frames);
  console.log('info aligned take01: ' + alignedFiles.join(' ') + '; frames ' + alignedLens.join(' ') + '; offsets '
    + summary.tracks.map((t) => t.who + ' ' + (t.offset_s * 1000).toFixed(1) + ' ms, drift ' + t.drift_ppm.toFixed(1) + ' ppm').join(', '));
  check(alignedFiles.length === 2 && alignedFiles.every((f, i) => f === ['take01_A_mic.wav', 'take01_B_mic.wav'][i])
    && alignedLens.length === 2 && alignedLens.every((n) => n === summary.frames && n > 0)
    && existsSync(join(ALIGNED, 'take01_markers.txt')) && existsSync(join(ALIGNED, 'take01_markers.csv')),
    'host aligned take01: one track per guest, equal lengths (' + summary.frames + ' frames), with marker exports');
  }
  // Take 2: B's connection is cut mid-take, then its microphone is lost.
  await waitFor(async () => buttons(A).then((b) => b.start && !b.stop), 'producer ready for take 2', 10000);
  await A.click('#take-start');
  await waitFor(async () => (await text(A, '#take-result')).includes('"take":2'), 'take 2 started', 10000);
  const seg1 = await waitFor(async () => {
    const r = await text(B, '#recording');
    return r.includes('recording take0002') ? r : null;
  }, 'guest B recording take 2', 10000);
  const tRec = Date.now();
  await sleep(2000);
  await B.evaluate(() => { for (const pc of window.__pcs) pc.close(); });
  const tCut = Date.now();
  await waitFor(async () => (await text(B, '#state')).includes('reconnecting'), 'B shows reconnecting', 10000);
  check((await text(B, '#recording')) === seg1, 'B keeps recording the same track while reconnecting (' + seg1 + ')');
  await waitFor(async () => (await text(B, '#state')) === 'connected' && (await text(B, '#participant')).length > 0,
    'B reconnected and rejoined', 30000);
  check(true, 'B reconnected after ' + ((Date.now() - tCut) / 1000).toFixed(1) + ' s and rejoined as participant '
    + (await text(B, '#participant')) + '; still ' + (await text(B, '#recording')));
  check((await text(B, '#recording')) === seg1, 'B continued the same track after rejoining');
  await sleep(1500);
  await B.evaluate(() => { for (const t of window.__tracks) t.stop(); });
  const tMicLost = Date.now();
  const seg2 = await waitFor(async () => {
    const r = await text(B, '#recording');
    return r.includes('recording take0002') && r !== seg1 ? r : null;
  }, 'B reopened its microphone and records a new segment', 30000);
  check(true, 'B lost its microphone and continues take 2 in a new segment (' + seg2 + ')');
  await waitFor(async () => (await text(B, '#state')) === 'connected' && (await text(B, '#participant')).length > 0,
    'B connected again after reopening its microphone', 30000);
  await sleep(2000);
  await A.click('#take-stop');
  await waitFor(async () => (await text(A, '#take-status')).startsWith('idle'), 'take 2 stopped', 10000);
  const TAKE2 = join(LIBRARY, 'browser-test', 'take02');
  const verified2 = () => {
    const m = join(TAKE2, 'manifest.jsonl');
    return existsSync(m) ? readFileSync(m, 'utf8').split('\n').filter((l) => l.startsWith('{"Verified"')).length : 0;
  };
  await waitFor(async () => verified2() >= 9, 'host verified take 2: A\'s track and both of B\'s segments, with sidecars', 60000, 250);
  await waitFor(async () => ((await opfsNames(B)).length === 0 ? true : null), 'guest B OPFS emptied after take 2 sync', 20000);
  const files2 = readdirSync(TAKE2).sort();
  console.log('host take02: ' + files2.join(' '));
  check(['take02_A_mic.wav', 'take02_B_mic.wav', 'take02_B-2_mic.wav', 'take02_B_mic.json', 'take02_B-2_mic.json'].every((f) => files2.includes(f))
    && !files2.some((f) => f.endsWith('.obao') || f.endsWith('.journal')),
    'both of B\'s take 2 segments synced to the host; B\'s OPFS is empty');
  const secs = (f) => { const w = parseWav(readFileSync(join(TAKE2, f))); return w.frames / w.rate; };
  const s1 = secs('take02_B_mic.wav'), s2 = secs('take02_B-2_mic.wav');
  const expect1 = (tMicLost - tRec) / 1000;
  check(s1 > expect1 - 1.5, 'B\'s first segment (' + s1.toFixed(2) + ' s) spans the connection drop without a gap (expected ~'
    + expect1.toFixed(1) + ' s)');
  check(s2 > 1, 'B\'s second segment holds ' + s2.toFixed(2) + ' s');

  // Leave releases every device and audio context; leave/rejoin is clean.
  const unreleased = (p) => p.evaluate(() => ({
    tracks: window.__tracks.filter((t) => t.readyState !== 'ended').map((t) => t.kind + ':' + t.label),
    total: window.__tracks.length,
    contexts: window.__audioContexts.filter((c) => c.state !== 'closed').length,
  }));
  const released = async (p, n, what) => {
    let last;
    try {
      await waitFor(async () => { last = await unreleased(p); return last.tracks.length === 0 && last.contexts === 0; }, what, 10000);
    } catch { /* reported below */ }
    check(last.tracks.length === 0 && last.contexts === 0, 'guest ' + n + ' ' + what + ': ' + last.total + ' tracks all ended'
      + (last.tracks.length ? ' (still live: ' + last.tracks.join(', ') + ')' : '') + ', open audio contexts ' + last.contexts);
  };
  for (const [n, p] of [['A', A], ['B', B]]) {
    await noErrors(p, n);
    await p.click('#leave');
    await released(p, n, 'after Leave');
  }
  for (const [n, p] of [['A', A], ['B', B]]) {
    // Leave while the join is still opening the devices, then a full rejoin.
    await p.click('#join');
    await p.click('#leave');
    await released(p, n, 'after a Leave during Join');
    await p.click('#join');
    await waitFor(async () => (await text(p, '#participant')).length > 0, 'guest ' + n + ' rejoined', 30000);
    await p.click('h1');
    await sleep(1000);
    await p.click('#leave');
    await p.click('h1');
    await released(p, n, 'after rejoin and Leave');
  }
  await sleep(500);
  for (const n of ['A', 'B']) {
    const bad = logs[n].filter((l) => l.startsWith('[pageerror]') || l.includes('unhandledrejection'));
    check(bad.length === 0, 'guest ' + n + ': no page errors or unhandled rejections' + (bad.length ? ': ' + bad.slice(0, 3).join(' | ') : ''));
  }
  for (const [n, p] of [['A', A], ['B', B]]) await noErrors(p, n);
  if (FULL) await securityChecks();
  if (MODE !== 'fallback') await identityChecks();
  passed = checks.every((c) => c.ok);

} catch (e) {
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  host.kill();
  if (host2) host2.kill();
  if (host3) host3.kill();
}
writeFileSync(join(RESULTS, 'host' + (FULL ? '' : '-' + MODE) + '.log'), hostLog);
if (host2) writeFileSync(join(RESULTS, 'host-security.log'), host2Log);
if (host3) writeFileSync(join(RESULTS, 'host-identity-' + MODE + '.log'), host3Log);
for (const n of ['A', 'B']) writeFileSync(join(RESULTS, 'guest-' + n + (FULL ? '' : '-' + MODE) + '.log'), logs[n].join('\n'));
if (!passed) {
  console.log('host log tail:');
  console.log(hostLog.split('\n').slice(-30).join('\n'));
  for (const n of ['A', 'B']) {
    console.log('guest ' + n + ' console tail:');
    console.log(logs[n].slice(-20).join('\n'));
  }
}
console.log('logs: crates/castcorder-client/browser-test/test-results/');
console.log(!passed ? 'FAIL (' + MODE + ')'
  : FULL ? 'PASS browser guest joins, forwards audio, records a bit-exact take to OPFS, syncs it to the host and deletes it locally'
    : 'PASS (' + MODE + ') guests join, hear each other, record a take without MediaStreamTrackProcessor on the page, and the tracks reach the host');
process.exit(passed ? 0 : 1);

// Video decoded from `origin`'s forwarded camera: frames (summed), size.
function videoFrom(inbound, origin) {
  return (inbound || []).filter((i) => i.kind === 'video' && i.origin.startsWith(origin + '-'))
    .reduce((a, i) => ({ frames: a.frames + (i.framesDecoded || 0), w: Math.max(a.w, i.frameWidth || 0), h: Math.max(a.h, i.frameHeight || 0) }),
      { frames: 0, w: 0, h: 0 });
}

// Wait until `page` decodes new frames from `origin` over a 2 s window.
function decoding(page, origin, what, timeout = 30000) {
  return waitFor(async () => {
    const x = videoFrom(await json(page, '#debug-inbound'), origin);
    await sleep(2000);
    const y = videoFrom(await json(page, '#debug-inbound'), origin);
    return y.frames > x.frames ? { fps: (y.frames - x.frames) / 2, w: y.w, h: y.h } : null;
  }, what, timeout, 0);
}

// Audio packets from `origin` rise over a 2 s window.
async function audioRising(page, origin) {
  const x = audioFrom(await json(page, '#debug-inbound'), origin);
  await sleep(2000);
  const y = audioFrom(await json(page, '#debug-inbound'), origin);
  return y.packets - x.packets;
}

// Live video (cast-riw): A and B each decode the other's forwarded camera
// (fake devices) and show it in a tile named after them; A sends two
// simulcast layers where the browser does (Chromium); A's camera turned off
// is released and B's frames from A stop while the audio goes on; turned on
// again, B decodes it again.
async function videoChecks(A, B) {
  for (const [rx, n, from] of [[B, 'B', 'A'], [A, 'A', 'B']]) {
    const v = await decoding(rx, from, n + ' decoding video from ' + from);
    check(v.fps >= 3, n + ' decodes ' + from + '\x27s forwarded video (' + v.fps.toFixed(1) + ' fps, ' + v.w + 'x' + v.h + ')');
    const tile = await rx.evaluate((from) => {
      const t = [...document.querySelectorAll('#remote .tile')].find((e) => e.dataset.origin.startsWith(from + '-'));
      const el = t && t.querySelector('video');
      return el ? { w: el.videoWidth, h: el.videoHeight, ready: el.readyState, name: t.querySelector('.tile-name').textContent } : null;
    }, from);
    check(!!tile && tile.w > 0 && tile.ready >= 2 && tile.name === from,
      n + ' shows ' + from + '\x27s video playing in a tile named after them: ' + JSON.stringify(tile));
  }
  const outbound = await A.evaluate(async () => {
    const pc = window.__pcs[window.__pcs.length - 1];
    const out = [];
    (await pc.getStats()).forEach((s) => {
      if (s.type === 'outbound-rtp' && s.kind === 'video') out.push({ rid: s.rid || '', framesEncoded: s.framesEncoded || 0, w: s.frameWidth || 0, h: s.frameHeight || 0 });
    });
    return out;
  });
  console.log('info A outbound video: ' + JSON.stringify(outbound));
  const layers = outbound.filter((o) => o.rid && o.framesEncoded > 0).map((o) => o.rid).sort();
  check(layers.join(',') === 'h,l', 'A sends two simulcast layers (' + layers.join(',') + ')');
  // Camera off: released, B's frames from A stop, audio goes on.
  const offers = () => (hostLog.match(/sent offer/g) || []).length;
  const offersBefore = offers();
  await A.click('#camera');
  await waitFor(async () => (await text(A, '#camera-state')) === 'off', 'A camera off', 5000);
  const live = await A.evaluate(() => window.__tracks.filter((t) => t.kind === 'video' && t.readyState !== 'ended').length);
  check(live === 0, 'turning the camera off releases it (' + live + ' live video tracks)');
  await sleep(1500);
  const x = videoFrom(await json(B, '#debug-inbound'), 'A');
  const audio = await audioRising(B, 'A');
  await sleep(1000);
  const y = videoFrom(await json(B, '#debug-inbound'), 'A');
  check(y.frames - x.frames <= 2 && audio > 0,
    'with A\x27s camera off B decodes no new frames from A (' + (y.frames - x.frames) + ' in 3 s) and still gets A\x27s audio (' + audio + ' packets in 2 s)');
  await A.click('#camera');
  await waitFor(async () => (await text(A, '#camera-state')) === 'on', 'A camera on again', 10000);
  const again = await decoding(B, 'A', 'B decoding A\x27s camera again', 20000);
  check(again.fps >= 3 && offers() === offersBefore, 'A\x27s camera back on: B decodes it again (' + again.fps.toFixed(1)
    + ' fps); the host sent ' + (offers() - offersBefore) + ' offers meanwhile (no renegotiation)');
}

// A timing metadata file (castcorder-core timing): JSON lines.
function parseTiming(text) {
  const t = { header: null, segments: 0, refs: [], sync: [], end: null, lastEstimate: null };
  for (const l of text.split('\n').filter((x) => x.length > 0)) {
    const v = JSON.parse(l);
    if (v.type === 'header') t.header = v;
    else if (v.type === 'segment') t.segments++;
    else if (v.type === 'ref') {
      t.refs.push(v);
      if (v.offset_us !== undefined) t.lastEstimate = { offset_us: v.offset_us, drift_ppm: v.drift_ppm, rtt_us: v.rtt_us };
    } else if (v.type === 'sync') t.sync.push(v);
    else if (v.type === 'end') t.end = v.frames;
  }
  t.hostUsAt = (frame) => {
    const r = t.refs.filter((x) => typeof x.host_us === 'number');
    if (r.length === 0) return NaN;
    return r[0].host_us + (frame - r[0].frame) / t.header.sample_rate * 1e6;
  };
  return t;
}

// Producers and room security (cast-xne), chromium mode: a second host with
// a fresh library and the default producer-gated call, and new browsers
// (each a new device): C waits until the host machine's page H joins; C, a
// non-producer, is refused StartTake on the control channel; H grants C the
// producer role and C starts and stops a take; a locked call refuses a new
// untrusted device D; a wrong password refuses E and the right one admits
// it; H kicks E, who cannot rejoin; H regenerates the link.
async function securityChecks() {
  const LIB2 = join(RESULTS, 'library-security');
  rmSync(LIB2, { recursive: true, force: true });
  host2 = spawn(exe, ['--dist', dist, '--library', LIB2, '--call', 'security-test', '--bind', '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0'], { stdio: ['ignore', 'pipe', 'pipe'] });
  host2.stderr.on('data', (d) => { host2Log += d.toString(); });
  const [HTTP2, HTTPS2] = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('security host did not print HOST_READY')), 20000);
    host2.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
    });
  });
  const joined = (p) => text(p, '#participant').then((s) => s.length > 0);
  const state = (p) => text(p, '#state');

  // Gated call: C waits until a producer is in the call.
  const C = await openGuest('C', [], HTTPS2);
  await C.click('#join');
  await waitFor(async () => (await state(C)).includes('waiting for the host or a producer'), 'C waiting for a producer', 15000);
  await sleep(3000);
  check(!(await joined(C)) && (await state(C)).includes('waiting'), 'gated call: guest C waits while no producer is in the call');
  const H = await openGuest('H', [], HTTP2);
  await H.click('#join');
  await waitFor(() => joined(H), 'host machine page H joined', 30000);
  await waitFor(() => joined(C), 'C admitted once the producer joined', 30000);
  check(true, 'gated call: C entered once the host machine\x27s page joined');

  // A non-producer cannot start a take, even by sending the request itself.
  await H.waitForSelector('#take-start', { timeout: 10000 });
  check((await C.locator('#take-start').count()) === 0, 'non-producer C has no producer panel');
  await C.evaluate(() => document.getElementById('debug-start-take').click());
  const refusedStart = await waitFor(async () => { const r = await text(C, '#debug-result'); return r ? r : null; }, 'C StartTake answered', 10000);
  check(refusedStart.includes('refused') && refusedStart.includes('producer'), 'host refuses StartTake from a non-producer on the control channel: ' + refusedStart);
  check((await text(H, '#take-status')).startsWith('idle'), 'no take started by the non-producer');

  // The host machine grants the producer role to trusted C, who starts and stops a take.
  await waitFor(async () => (await text(H, '#guests')).includes('C (guest'), 'H lists C', 10000);
  check(/C \(guest \d+\) \[trusted\]/.test(await text(H, '#guests')), 'C joined with a producer present and is trusted');
  await H.click('#guests li[data-name="C"] button.grant');
  await C.waitForSelector('#take-start', { timeout: 10000 });
  check((await C.locator('button.grant').count()) === 0, 'granted producer C gets the panel but no grant/revoke controls');
  await waitFor(async () => C.isEnabled('#take-start'), 'C can start a take', 10000);
  await C.click('#take-start');
  await waitFor(async () => (await text(C, '#take-result')).includes('"take":1'), 'C started take 1', 10000);
  await waitFor(async () => (await text(H, '#take-status')).startsWith('recording'), 'H sees take 1 recording', 10000);
  await sleep(1500);
  await waitFor(async () => C.isEnabled('#take-stop'), 'C can stop the take', 10000);
  await C.click('#take-stop');
  await waitFor(async () => (await text(C, '#take-status')).startsWith('idle'), 'C stopped take 1', 10000);
  check(true, 'producer C (granted by the host machine) started and stopped take 1');

  // Lock: a new, untrusted device is refused.
  await H.click('#lock');
  await waitFor(async () => (await text(H, '#room-locked')) === 'locked', 'call locked', 10000);
  const D = await openGuest('D', [], HTTPS2);
  await D.click('#join');
  await waitFor(async () => (await state(D)) === 'the call is locked', 'D refused by the lock', 20000);
  check((await text(D, '#errors')).includes('locked') && !(await joined(D)), 'a locked call refuses a new untrusted device: ' + (await text(D, '#errors')));
  await H.click('#lock');
  await waitFor(async () => (await text(H, '#room-locked')) === 'unlocked', 'call unlocked', 10000);

  // Password: wrong refused, right admitted.
  await H.fill('#new-password', 'pw-1');
  await H.click('#set-password');
  await waitFor(async () => (await text(H, '#room-password')) === 'password set', 'password set', 10000);
  const E = await openGuest('E', [], HTTPS2);
  await E.fill('#password', 'wrong');
  await E.click('#join');
  await waitFor(async () => (await state(E)) === 'wrong password', 'E refused: wrong password', 20000);
  check((await text(E, '#errors')).includes('wrong password') && !(await joined(E)), 'a wrong password is refused: ' + (await text(E, '#errors')));
  await E.fill('#password', 'pw-1');
  await E.click('#join');
  await waitFor(() => joined(E), 'E joined with the right password', 30000);
  check(true, 'the right password is admitted');

  // Kick: E is removed and cannot rejoin.
  await waitFor(async () => (await text(H, '#guests')).includes('E (guest'), 'H lists E', 10000);
  await H.click('#guests li[data-name="E"] button.kick');
  await waitFor(async () => (await state(E)) === 'removed from the call by a producer', 'E removed', 30000);
  const kicks = () => (host2Log.match(/device was kicked/g) || []).length;
  const before = kicks();
  await E.click('#join');
  await waitFor(async () => kicks() > before && (await state(E)) === 'removed from the call by a producer', 'E rejoin refused', 30000);
  check(!(await joined(E)), 'a kicked device cannot rejoin (even with the password)');

  // Regenerate link (no rendezvous here: the secret rotates on disk).
  const secretFile = join(LIB2, 'security-test', 'call-secret');
  const oldSecret = readFileSync(secretFile, 'utf8');
  await H.click('#regen-link');
  await waitFor(async () => readFileSync(secretFile, 'utf8') !== oldSecret, 'call secret rotated', 10000);
  check((await joined(C)) && (await joined(H)), 'regenerating the link rotates the call secret; everyone in the call stays');

  const room = JSON.parse(readFileSync(join(LIB2, 'security-test', 'room.json'), 'utf8'));
  const devs = Object.values(room.devices);
  check(devs.some((d) => d.name === 'E' && d.banned) && devs.some((d) => d.name === 'C' && d.producer && d.trusted)
    && !JSON.stringify(room).includes('pw-1'),
    'room.json in the call folder remembers devices (C producer and trusted, E banned), password only hashed');
  check(/security decision/.test(host2Log) && /allowed=false/.test(host2Log), 'the host logs its security decisions');
}

// Device identity (cast-p2f), chromium and firefox modes: a third, always-open
// host whose room.json already remembers a pre-key device (a UUID, trusted).
// - M is that profile after the upgrade: its localStorage still holds the
//   UUID and its OPFS a finished take-1 track recorded under the UUID (seeded
//   in the layout the outbox reads). It joins with its new key: the host binds
//   the UUID to the key once (the record, trust included, moves), and the
//   pending track syncs under the UUID and leaves OPFS.
// - N presents the same UUID with another key: the binding is refused and N
//   enters as a new, untrusted device.
// - An impostor that knows M's key-derived id is refused: unsigned, or signed
//   by its own key; with M's id in its old-id slot it takes over nothing.
async function identityChecks() {
  const LIB3 = join(RESULTS, 'library-identity-' + MODE);
  rmSync(LIB3, { recursive: true, force: true });
  const UUID = '0b6a3c2e-5f1d-4e8a-9c7b-2d4e6f8a1c3e';
  mkdirSync(join(LIB3, 'identity-test'), { recursive: true });
  writeFileSync(join(LIB3, 'identity-test', 'room.json'),
    JSON.stringify({ open: true, devices: { [UUID]: { name: 'M', trusted: true } } }));
  host3 = spawn(exe, ['--dist', dist, '--library', LIB3, '--call', 'identity-test', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
  host3.stderr.on('data', (d) => { host3Log += d.toString(); });
  const HTTPS3 = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('identity host did not print HOST_READY')), 20000);
    host3.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve(m[2]); }
    });
  });
  const joined = (p) => text(p, '#participant').then((s) => s.length > 0);
  const room = () => JSON.parse(readFileSync(join(LIB3, 'identity-test', 'room.json'), 'utf8'));
  const count = (re) => (host3Log.match(re) || []).length;
  const withOldId = (id) => async (ctx) => {
    await ctx.addInitScript((id) => {
      if (!localStorage.getItem('castcorder.device')) localStorage.setItem('castcorder.device', id);
    }, id);
  };

  // M: the upgraded profile, with a pending track from before the upgrade.
  // The track is seeded before the app first loads in M's context, as it
  // would be on disk after an upgrade: seeding into a running app races the
  // storage worker's startup scan, which opens each track's sidecar with a
  // sync access handle (Firefox, whose worker starts later, then refuses
  // createWritable with NoModificationAllowedError). The seed page is an
  // empty document on the app's origin (same OPFS), served by the test.
  const seedLegacyTrack = async (ctx) => {
    await withOldId(UUID)(ctx);
    const seed = await ctx.newPage();
    await seed.route(HTTPS3 + '/seed', (r) => r.fulfill({ contentType: 'text/html', body: '<!doctype html><title>seed</title>' }));
    await seed.goto(HTTPS3 + '/seed');
    await seedTrack(seed);
    await seed.close();
  };
  // Beside it (cast-dao): `torn-` is a track killed while its sidecar was
  // being rewritten (an empty sidecar, a header still counting 0 frames, the
  // timing metadata's header intact): the scan rebuilds the sidecar from the
  // timing header and the track must sync like any other. `orphan-` is audio
  // with no sidecar at all: the scan must not create one and must keep the
  // audio.
  const seedTrack = (page) => page.evaluate(async ([uuid, call]) => {
    const frames = 4800, ch = 1, rate = 48000;
    const wav = (freq, headerFrames) => {
      const buf = new ArrayBuffer(58 + frames * ch * 4);
      const v = new DataView(buf);
      const str = (o, s) => { for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i)); };
      str(0, 'RIFF'); v.setUint32(4, 50 + headerFrames * ch * 4, true); str(8, 'WAVE'); str(12, 'fmt ');
      v.setUint32(16, 18, true); v.setUint16(20, 3, true); v.setUint16(22, ch, true); v.setUint32(24, rate, true);
      v.setUint32(28, rate * ch * 4, true); v.setUint16(32, ch * 4, true); v.setUint16(34, 32, true); v.setUint16(36, 0, true);
      str(38, 'fact'); v.setUint32(42, 4, true); v.setUint32(46, headerFrames, true); str(50, 'data'); v.setUint32(54, headerFrames * ch * 4, true);
      for (let i = 0; i < frames; i++) v.setFloat32(58 + i * 4, 0.25 * Math.sin(i / freq), true);
      return buf;
    };
    const meta = { name: 'legacy-take0001', file: 'legacy-take0001.wav', take: 1, device: uuid, displayName: 'M', call,
      source: 'mic', finalised: true, frames, sampleRate: rate, channels: ch };
    const timingHeader = { type: 'header', version: 1, call, device: uuid, take: 1, track: 'torn-take0001', source: 'mic',
      sample_rate: rate, channels: ch, device_clock: 'test', host_clock: 'test', host_timeline: 0 };
    const root = await navigator.storage.getDirectory();
    const d = await root.getDirectoryHandle('tracks', { create: true });
    for (const [name, data] of [['legacy-take0001.wav', wav(10, frames)], ['legacy-take0001.json', JSON.stringify(meta)],
      ['torn-take0001.wav', wav(7, 0)], ['torn-take0001.json', ''],
      ['torn-take0001.timing.jsonl', JSON.stringify(timingHeader) + '\n{"type":"segment","segment":1,"file":"torn-take0001.wav"}\n'],
      ['orphan-take0001.wav', wav(13, frames)]]) {
      const w = await (await d.getFileHandle(name, { create: true })).createWritable();
      await w.write(data);
      await w.close();
    }
  }, [UUID, HTTPS3 + '/offer']);
  const M = await openGuest('M', [], HTTPS3, seedLegacyTrack);
  await M.click('#join');
  await waitFor(() => joined(M), 'M joined with its key', 30000);
  await waitFor(async () => /device alias bound/.test(host3Log), 'host bound M\x27s old id', 10000);
  const r1 = room();
  const mId = r1.aliases && r1.aliases[UUID];
  check(/^k[0-9a-f]{40}$/.test(mId || '') && !r1.devices[UUID] && r1.devices[mId] && r1.devices[mId].trusted,
    'M joined with its key (' + mId + '); the host bound its old UUID once and moved its trusted record to the key id');
  const TAKE = join(LIB3, 'identity-test', 'take01');
  await waitFor(() => {
    const m = join(TAKE, 'manifest.jsonl');
    return existsSync(m) && readFileSync(m, 'utf8').split('\n').some((l) => l.startsWith('{"Verified"') && l.includes(UUID));
  }, 'M\x27s pre-upgrade track verified on the host', 60000, 250);
  const opfsLeft = await waitFor(async () => {
    const names = await M.evaluate(async () => {
      const d = await (await navigator.storage.getDirectory()).getDirectoryHandle('tracks', { create: true });
      const out = [];
      for await (const k of d.keys()) out.push(k);
      return out;
    });
    return names.some((n) => n.startsWith('legacy-') || n.startsWith('torn-')) ? null : names;
  }, 'M\x27s OPFS emptied of the old and the torn track', 30000);
  check(opfsLeft.every((n) => !n.startsWith('legacy-')),
    'M\x27s pending track from before the upgrade synced (kept under its old UUID in the manifest) and left OPFS');
  check(opfsLeft.every((n) => !n.startsWith('torn-')),
    'a track whose sidecar a crash left empty was recovered from its timing header, synced and left OPFS');
  check(opfsLeft.includes('orphan-take0001.wav') && !opfsLeft.includes('orphan-take0001.json'),
    'the startup scan created no sidecar for audio that has none, and kept the audio: ' + opfsLeft.join(' '));
  const listed = JSON.parse((await M.textContent('#debug-tracks')) || '[]');
  const orphan = listed.find((t) => t.name === 'orphan-take0001');
  check(orphan && orphan.noSidecar && !orphan.error && orphan.frames === 4800,
    'the audio without a sidecar is listed (noSidecar, 4800 frames): ' + JSON.stringify(orphan));

  // N: another profile presenting the same old UUID: no takeover.
  const refusedBefore = count(/device alias binding refused/g);
  const N = await openGuest('N', [], HTTPS3, withOldId(UUID));
  await N.click('#join');
  await waitFor(() => joined(N), 'N joined', 30000);
  const r2 = room();
  const nId = Object.keys(r2.devices).find((k) => r2.devices[k].name === 'N');
  check(count(/device alias binding refused/g) > refusedBefore && r2.aliases[UUID] === mId
    && nId && nId !== mId && !r2.devices[nId].trusted,
    'a second profile presenting the same old UUID is refused the binding and enters as a new, untrusted device');

  // The impostor knows M's id but not M's key.
  const I = await openGuest('I', [], HTTPS3, withOldId(mId));
  const statuses = await I.evaluate(async (mId) => {
    const post = async (body) => (await fetch('/offer?name=I', { method: 'POST', body: JSON.stringify(body) })).status;
    const sdp = 'v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n';
    const unsigned = await post({ type: 'offer', sdp, device: mId });
    // A correct signature (castcorder-link identity::offer_message, the
    // host id from GET /role as audience, no alias) by the impostor's own
    // key, claiming M's id.
    const host = (await (await fetch('/role')).json()).host;
    const pair = await crypto.subtle.generateKey({ name: 'Ed25519' }, false, ['sign', 'verify']);
    const key = new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey));
    const ts = Date.now();
    const enc = new TextEncoder();
    const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', enc.encode(sdp)));
    const parts = [enc.encode('castcorder-link device offer v1\0' + host + '\0'), key, enc.encode('\0' + ts + '\0'), hash];
    const msg = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    let o = 0;
    for (const p of parts) { msg.set(p, o); o += p.length; }
    const sig = new Uint8Array(await crypto.subtle.sign('Ed25519', pair.privateKey, msg));
    const b64 = (b) => btoa(String.fromCharCode(...b)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    const ownKey = await post({ type: 'offer', sdp, device: mId, proof: { key: b64(key), ts, sig: b64(sig) } });
    return { unsigned, ownKey };
  }, mId);
  check(statuses.unsigned === 403 && statuses.ownKey === 403,
    'an offer claiming M\x27s id without M\x27s key is refused (unsigned ' + statuses.unsigned + ', signed by another key ' + statuses.ownKey + ')');
  check(/offer not signed by a device key/.test(host3Log) && /claimed device id is not the fingerprint of the signing key/.test(host3Log),
    'the host logs why it refused the impostor');
  // Through the app, with M's id in its old-id slot: it joins only as itself.
  await I.click('#join');
  await waitFor(() => joined(I), 'I joined as itself', 30000);
  const r3 = room();
  const iId = Object.keys(r3.devices).find((k) => r3.devices[k].name === 'I');
  check(iId && iId !== mId && !r3.devices[iId].trusted && r3.devices[mId].name === 'M' && r3.devices[mId].trusted,
    'a page holding M\x27s id in its old-id slot takes over nothing: it enters as its own untrusted device');
  for (const p of [M, N, I]) await p.click('#leave').catch(() => {});
}
