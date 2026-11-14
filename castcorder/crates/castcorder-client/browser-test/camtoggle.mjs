// Camera toggle stress (cast-47x): one headless Chromium guest A, fed the
// deterministic WAV, records takes with video on and turns its camera off and
// on several times in each take; before that, its storage worker (the thread
// reading the raw audio) is stalled for a while. Every take's raw audio, as
// the host received it, must be bit-exact against the input with no dropped,
// duplicated or inserted frame, and its timing metadata must list no
// discontinuity. If audio is lost anyway, the timing metadata must account
// for exactly the frames the analysis finds dropped (never silent).
//
//   node camtoggle.mjs [takes=3] [toggles=4] [stall_ms=500]
//
// Uses the same builds as e2e.mjs (CARGO_TARGET_DIR, CASTCORDER_DIST); logs
// and the host library go to test-results/camtoggle/. Prints ASCII only.
import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { makeInput, writeWav16 } from './gen-wav.mjs';
import { parseWav, deinterleave, analyzeRecording } from './analyze.mjs';
import { ctx } from './lib/ctx.mjs';
import { sleep, waitFor, text, buttons, opfsNames } from './lib/helpers.mjs';
import { openGuest } from './lib/guest.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '..');
const WORKSPACE = join(CRATE, '..', '..');
const TAKES = Number(process.argv[2] || 3);
const TOGGLES = Number(process.argv[3] || 4);
const STALL_MS = Number(process.argv[4] || 500);
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const { chromium, firefox } = await import('playwright');

const exe = join(process.env.CARGO_TARGET_DIR || join(WORKSPACE, 'target'), 'release',
  process.platform === 'win32' ? 'castcorder-host.exe' : 'castcorder-host');
const dist = process.env.CASTCORDER_DIST || join(CRATE, 'dist');
if (!existsSync(exe)) { console.log('FAIL host binary missing: ' + exe); process.exit(1); }
if (!existsSync(join(dist, 'index.html'))) { console.log('FAIL client not built: ' + dist); process.exit(1); }
const RESULTS = join(HERE, 'test-results', 'camtoggle');
const LIBRARY = join(RESULTS, 'library');
rmSync(RESULTS, { recursive: true, force: true });
mkdirSync(RESULTS, { recursive: true });
const input = makeInput();
const INPUT_WAV = join(RESULTS, 'input-44k-s16-stereo.wav');
writeWav16(INPUT_WAV, input);

const host = spawn(exe, ['--dist', dist, '--library', LIBRARY, '--call', 'browser-test', '--bind', '127.0.0.1',
  '--port', '0', '--http-port', '0', '--udp-port', '0', '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
let hostLog = '';
host.stderr.on('data', (d) => { hostLog += d.toString(); });
const [HTTP] = await new Promise((resolve, reject) => {
  let buf = '';
  const t = setTimeout(() => reject(new Error('host did not print HOST_READY')), 20000);
  host.stdout.on('data', (d) => {
    buf += d.toString();
    const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
    if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
  });
  host.on('exit', (c) => reject(new Error('host exited early with ' + c)));
});
const logs = { A: [] };
const browsers = [];
Object.assign(ctx, {
  MODE: 'chromium', host, logs, browsers, chromium, firefox,
  BASE_ARGS: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--allow-loopback-in-peer-connection',
    '--autoplay-policy=no-user-gesture-required', '--disable-background-timer-throttling', '--mute-audio'],
});

let failures = 0;
try {
  const A = await openGuest('A', ['--use-file-for-fake-audio-capture=' + INPUT_WAV], HTTP);
  await A.click('#join');
  await waitFor(async () => (await text(A, '#participant')).length > 0, 'A joined');
  await A.waitForSelector('#take-start', { timeout: 10000 });
  await A.click('#record-video');
  await waitFor(async () => (await text(A, '#room-record-video')) === 'audio and video', 'video recording on', 10000);
  for (let t = 1; t <= TAKES; t++) {
    const name = 'take' + String(t).padStart(2, '0');
    await waitFor(async () => (await text(A, '#camera-state')) === 'on', 'camera on before ' + name, 10000);
    await waitFor(async () => buttons(A).then((b) => b.start && !b.stop), 'ready for ' + name, 10000);
    await A.click('#take-start');
    let rec = await waitFor(async () => { const r = await text(A, '#video-recording'); return r.startsWith('recording take') ? r : null; },
      'A recording its camera in ' + name, 15000);
    await sleep(1500);
    if (STALL_MS > 0) {
      // A real stall of the storage worker, the thread that reads the raw
      // audio: its source must buffer through it, losing nothing.
      const w = A.workers().find((x) => x.url().includes('worker'));
      if (!w) throw new Error('storage worker not found');
      await w.evaluate((ms) => { const t = performance.now(); while (performance.now() - t < ms) { /* busy */ } }, STALL_MS);
      await sleep(500);
    }
    for (let k = 0; k < TOGGLES; k++) {
      await A.click('#camera');
      await waitFor(async () => (await text(A, '#video-recording')).startsWith('stopped take'), 'video track closed (' + name + ' toggle ' + (k + 1) + ')', 10000);
      await sleep(300);
      await A.click('#camera');
      rec = await waitFor(async () => { const r = await text(A, '#video-recording'); return r.startsWith('recording take') && r !== rec ? r : null; },
        'new video track (' + name + ' toggle ' + (k + 1) + ')', 15000);
      await sleep(700);
    }
    await A.click('#take-stop');
    await waitFor(async () => (await text(A, '#take-status')).startsWith('idle'), name + ' stopped', 10000);
    await waitFor(async () => ((await opfsNames(A)).length === 0 ? true : null), name + ' synced (A OPFS empty)', 120000, 500);
    const dir = join(LIBRARY, 'browser-test', name);
    const wav = parseWav(readFileSync(join(dir, name + '_A_mic.wav')));
    const timing = readFileSync(join(dir, name + '_A_mic.timing.jsonl'), 'utf8').trim().split('\n').map((l) => JSON.parse(l));
    const disc = timing.filter((l) => l.type === 'discontinuity');
    const per = analyzeRecording(deinterleave(wav.samples, wav.channels), input);
    let ok = disc.length === 0;
    const parts = [];
    for (const c of per) {
      const clean = c.verdict.startsWith('bit-exact') && c.discontinuities === 0 && c.droppedFrames === 0
        && c.duplicatedFrames === 0 && c.insertedFrames === 0 && c.mismatched === 0;
      ok = ok && clean;
      parts.push('ch' + c.channel + ' ' + c.verdict + ' dropped ' + c.droppedFrames + ' dup ' + c.duplicatedFrames
        + ' ins ' + c.insertedFrames + ' mism ' + c.mismatched
        + (c.firstDiscontinuities.length ? ' at ' + c.firstDiscontinuities.map((d) => d.outputFrame + ':' + d.deltaFrames).join(',') : ''));
    }
    if (!ok) failures++;
    // Lost audio is a failure either way; unrecorded lost audio is the worse one.
    const recorded = disc.filter((d) => d.kind === 'dropped').reduce((a, d) => a + d.frames, 0);
    const lost = Math.max(...per.map((c) => c.droppedFrames));
    const accounted = Math.abs(recorded - lost) <= Math.max(2, lost * 0.02);
    console.log((ok ? 'ok   ' : 'FAIL ') + name + ' ' + (wav.frames / wav.rate).toFixed(2) + ' s, ' + STALL_MS + ' ms worker stall, '
      + TOGGLES + ' camera toggles; ' + parts.join('; ') + '; timing metadata records ' + recorded + ' dropped frames ('
      + (accounted ? 'matches the analysis' : 'SILENT LOSS: the analysis finds ' + lost) + ') ' + JSON.stringify(disc));
  }
} catch (e) {
  failures++;
  console.log('FAIL ' + String(e && e.message || e).replace(/[^\x20-\x7e]/g, '?'));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  host.kill();
  writeFileSync(join(RESULTS, 'guest-A.log'), logs.A.join('\n'));
  writeFileSync(join(RESULTS, 'host.log'), hostLog);
}
console.log(failures ? 'FAIL camtoggle: ' + failures + ' of ' + TAKES + ' takes' : 'PASS camtoggle: ' + TAKES + ' takes x ' + TOGGLES + ' camera toggles, raw audio bit-exact');
process.exit(failures ? 1 : 0);
