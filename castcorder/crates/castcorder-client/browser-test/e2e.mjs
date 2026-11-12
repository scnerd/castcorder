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
// 3. A's producer controls start a take; ~5 s later they stop it.
// 4. Each guest's outbox syncs its track and JSON sidecar into the host's
//    library (--library test-results/library), verified; only then is the
//    OPFS copy deleted, and the test waits until both guests' OPFS is empty.
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
// Run from the repo root with:  task test:browser:client  (or task test:browser)
// Everything printed is ASCII. Logs go to browser-test/test-results/.

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeInput, writeWav16, INPUT_RATE } from './gen-wav.mjs';
import { parseWav, deinterleave, analyzeRecording } from './analyze.mjs';

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
  '--port', '0', '--http-port', '0', '--udp-port', '0'], { stdio: ['ignore', 'pipe', 'pipe'] });
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

async function openGuest(name, extraArgs, base) {
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
  return (inbound || []).filter((i) => i.origin.startsWith(origin + '-'))
    .reduce((a, i) => ({ packets: a.packets + i.packetsReceived, bytes: a.bytes + i.bytesReceived }), { packets: 0, bytes: 0 });
}

const checks = [];
function check(ok, what) {
  checks.push({ ok, what });
  console.log((ok ? 'ok   ' : 'BAD  ') + what);
}

let passed = false;
try {
  const A = await openGuest('A', ['--use-file-for-fake-audio-capture=' + INPUT_WAV], HTTP);
  const B = await openGuest('B', [], HTTPS);
  console.log((MODE === 'firefox' ? 'firefox ' : 'chromium ') + browsers[0].version());

  // Producer controls: only on the page from the host machine's localhost.
  await A.waitForSelector('#take-start', { timeout: 10000 });
  await sleep(1000);
  check((await B.locator('#take-start').count()) === 0, 'producer controls on the localhost page only, not on the HTTPS page');
  const refused = await B.evaluate(async () => (await fetch('/take/start', { method: 'POST' })).status);
  check(refused === 403, 'host refuses take control over HTTPS (' + refused + ')');

  for (const [n, p] of [['A', A], ['B', B]]) {
    await p.click('#join');
    await waitFor(async () => { await noErrors(p, n); return (await text(p, '#participant')).length > 0; },
      'guest ' + n + ' joined');
    console.log('guest ' + n + ' joined as participant ' + (await text(p, '#participant')));
  }

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
  await sleep(TAKE_SECONDS * 1000);
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
  // Timing metadata (cast-whr): each guest's file parses, and its mapping
  // places the track's first frame just after the host's take start (loopback:
  // clocks are shared, so this checks the plumbing, not the estimator).
  for (const n of ['A', 'B']) {
    const t = parseTiming(readFileSync(join(TAKE_DIR, 'take01_' + n + '_mic.timing.jsonl'), 'utf8'));
    const start = t.hostUsAt(0);
    const delta = (start - take1StartUs) / 1000;
    console.log('info ' + n + ' timing: ' + t.refs.length + ' refs, ' + t.sync.length + ' clock samples, last estimate '
      + JSON.stringify(t.lastEstimate) + '; frame 0 at ' + delta.toFixed(1) + ' ms after the take start');
    check(t.header && t.header.version === 1 && t.header.take === 1 && t.segments === 1 && t.end !== null && t.sync.length > 0
      && t.refs.length >= TAKE_SECONDS - 1 && Math.abs(delta) < 50,
      n + '\x27s timing metadata places frame 0 within 50 ms of the host\x27s take start (' + delta.toFixed(1) + ' ms)');
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
  passed = checks.every((c) => c.ok);

} catch (e) {
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  host.kill();
}
writeFileSync(join(RESULTS, 'host' + (FULL ? '' : '-' + MODE) + '.log'), hostLog);
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
