// Real-browser test for the Browser Guest (castcorder-client), ADR 0003's real
// boundary: the castcorder-transport browser_host example (real UDP socket,
// system clock) serving the trunk build of the client, plus two headless
// Chromium guests.
//
// 1. Guest A is fed a deterministic WAV (--use-file-for-fake-audio-capture);
//    guest B uses Chromium's default fake microphone. Both join.
// 2. B receives A's forwarded audio (inbound RTP from A's stream rising).
// 3. The host starts a take, waits ~5 s, stops it.
// 4. A's track is read back out of OPFS and must be bit-exact against the input
//    (Chrome's int16 -> float scaling, every frame aligned, no dropped,
//    duplicated or inserted frames), about 5 s long, 44.1 kHz stereo.
//
// Run from the repo root with:  task test:browser:client  (or task test:browser)
// Everything printed is ASCII. Logs go to browser-test/test-results/.

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeInput, writeWav16, INPUT_RATE } from './gen-wav.mjs';
import { parseWav, deinterleave, analyzeRecording } from './analyze.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '..');
const WORKSPACE = join(CRATE, '..', '..');
const RESULTS = join(HERE, 'test-results');
const TAKE_SECONDS = 5;
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const { chromium } = await import('playwright');

const exe = join(WORKSPACE, 'target', 'release', 'examples',
  process.platform === 'win32' ? 'browser_host.exe' : 'browser_host');
const dist = join(CRATE, 'dist');
if (!existsSync(exe)) { console.log('FAIL host binary missing: ' + exe); process.exit(1); }
if (!existsSync(join(dist, 'index.html'))) { console.log('FAIL client not built: ' + dist); process.exit(1); }

mkdirSync(RESULTS, { recursive: true });
const input = makeInput();
const INPUT_WAV = join(RESULTS, 'input-44k-s16-stereo.wav');
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

const host = spawn(exe, ['--dist', dist], { stdio: ['ignore', 'pipe', 'pipe'] });
let hostLog = '';
host.stderr.on('data', (d) => { hostLog += d.toString(); });
const HTTP = await new Promise((resolve, reject) => {
  let buf = '';
  const t = setTimeout(() => reject(new Error('host did not print HOST_READY')), 20000);
  host.stdout.on('data', (d) => {
    buf += d.toString();
    const m = buf.match(/HOST_READY http=(\S+)/);
    if (m) { clearTimeout(t); resolve(m[1]); }
  });
  host.on('exit', (c) => reject(new Error('host exited early with ' + c)));
});
console.log('host up at ' + HTTP);

async function hostPost(path) {
  const r = await fetch(HTTP + path, { method: 'POST' });
  const body = await r.text();
  if (!r.ok) throw new Error(path + ' -> ' + r.status + ' ' + body);
  return JSON.parse(body);
}

const BASE_ARGS = [
  '--use-fake-device-for-media-stream',
  '--use-fake-ui-for-media-stream',
  '--allow-loopback-in-peer-connection',
  '--autoplay-policy=no-user-gesture-required',
  '--disable-background-timer-throttling',
];
const logs = { A: [], B: [] };
const browsers = [];

async function openGuest(name, extraArgs) {
  const browser = await chromium.launch({ headless: true, args: [...BASE_ARGS, ...extraArgs] });
  browsers.push(browser);
  const ctx = await browser.newContext({ permissions: ['microphone', 'camera'] });
  const page = await ctx.newPage();
  page.on('console', (m) => logs[name].push(m.text()));
  page.on('pageerror', (e) => logs[name].push('[pageerror] ' + e.message));
  await page.goto(HTTP + '/?name=' + name);
  await page.waitForSelector('#join');
  return page;
}

const text = async (page, sel) => (await page.textContent(sel)) || '';
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
  const A = await openGuest('A', ['--use-file-for-fake-audio-capture=' + INPUT_WAV]);
  const B = await openGuest('B', []);
  console.log('chromium ' + browsers[0].version());

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

  // The raw path is warm (read since join; the spike's 500 ms warm-up lesson).
  await waitFor(async () => (await text(A, '#level-db')) !== '-inf dBFS', 'A level meter');
  await sleep(1000);

  const started = await hostPost('/take/start');
  check(started.take === 1, 'host started take ' + started.take);
  for (const [n, p] of [['A', A], ['B', B]]) {
    await waitFor(async () => { await noErrors(p, n); return (await text(p, '#recording')).includes('recording take'); },
      'guest ' + n + ' recording', 10000);
  }
  await sleep(TAKE_SECONDS * 1000);
  const stopped = await hostPost('/take/stop');
  check(stopped.take === 1, 'host stopped take ' + stopped.take);

  const listed = async (p) => {
    const t = await json(p, '#debug-tracks');
    return t && t.length && !t.some((x) => x.recording) && t.some((x) => x.take === 1) ? t : null;
  };
  const tracksA = await waitFor(() => listed(A), 'A track list after stop', 15000);
  const tracksB = await waitFor(() => listed(B), 'B track list after stop', 15000);
  const trackA = tracksA.find((x) => x.take === 1);
  const trackB = tracksB.find((x) => x.take === 1);
  console.log('A track ' + JSON.stringify(trackA));
  console.log('B track ' + JSON.stringify(trackB));
  check(trackB && trackB.seconds > TAKE_SECONDS - 1, 'B also recorded a local track (' + (trackB && trackB.seconds.toFixed(2)) + ' s)');

  const wav = parseWav(await readOpfs(A, 'tracks', trackA.name + '.wav'));
  writeFileSync(join(RESULTS, 'A-track.wav'), await readOpfs(A, 'tracks', trackA.name + '.wav'));
  const sidecar = JSON.parse((await readOpfs(A, 'tracks', trackA.name + '.json')).toString('utf8'));
  writeFileSync(join(RESULTS, 'A-track.json'), JSON.stringify(sidecar, null, 2));
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
  for (const [n, p] of [['A', A], ['B', B]]) await noErrors(p, n);
  passed = checks.every((c) => c.ok);
  for (const p of [A, B]) await p.click('#leave');
  await sleep(500);
} catch (e) {
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  host.kill();
}
writeFileSync(join(RESULTS, 'host.log'), hostLog);
for (const n of ['A', 'B']) writeFileSync(join(RESULTS, 'guest-' + n + '.log'), logs[n].join('\n'));
if (!passed) {
  console.log('host log tail:');
  console.log(hostLog.split('\n').slice(-30).join('\n'));
  for (const n of ['A', 'B']) {
    console.log('guest ' + n + ' console tail:');
    console.log(logs[n].slice(-20).join('\n'));
  }
}
console.log('logs: crates/castcorder-client/browser-test/test-results/');
console.log(passed ? 'PASS browser guest joins, forwards audio, records a bit-exact take to OPFS' : 'FAIL');
process.exit(passed ? 0 : 1);
