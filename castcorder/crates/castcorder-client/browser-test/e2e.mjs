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
//    on again is decoded again, with no renegotiation. Each guest's tile for the
//    other shows the level of the audio arriving and its speaking ring
//    (lib/levels.mjs).
// 3. A's producer controls start a take; ~8 s later they stop it. Mid-take A
//    mutes and unmutes in the call (lib/levels.mjs muteChecks); its track is
//    still checked bit-exact below.
// 4. Each guest's outbox syncs its track and JSON sidecar into the host's
//    library (--library test-results/library), verified; only then is the
//    OPFS copy deleted, and the test waits until both guests' OPFS is empty.
//    Mid-take, a second tab of A's profile loads: its startup scan must list
//    A's recording track as busy (files held by A's worker), with no error.
// 5. A's track, as the host received it, must be bit-exact against the input
//    (Chrome's int16 -> float scaling, every frame aligned, no dropped,
//    duplicated or inserted frames), about 8 s long, 44.1 kHz stereo.
//
// 6. Take 2 (every mode): guest B's RTCPeerConnection is closed mid-take; B
//    keeps recording into the same file, shows "reconnecting", rejoins, then
//    its microphone tracks are stopped (as a phone's system may) and it
//    reopens them and continues in a new segment. Both segments reach the host.
//    Meanwhile the pages say where the tracks are (lib/recordings.mjs): the
//    host machine's page shows the call folder and each track's state, B's
//    finished segment waits then is delivered, and B made a producer sees the
//    states but not the folder.
// 6b. Take 3 (chromium, firefox): the producer turns video recording on;
//    webcams are recorded as WebM, sync after the audio, and B, made unable
//    to record video in chromium, reports it: see lib/videorec.mjs.
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

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { makeInput, writeWav16 } from './gen-wav.mjs';
import { parseWav } from './analyze.mjs';
import { fileURLToPath } from 'node:url';
import { ctx } from './lib/ctx.mjs';
import { sleep, waitFor, text, buttons, json, noErrors, audioFrom, checks, check, opfsNames } from './lib/helpers.mjs';
import { takeChecks } from './lib/take.mjs';
import { liveLevelChecks } from './lib/levels.mjs';
import { videoChecks } from './lib/video.mjs';
import { videoRecChecks } from './lib/videorec.mjs';
import { securityChecks } from './lib/security.mjs';
import { identityChecks } from './lib/identity.mjs';
import { capChecks } from './lib/cap.mjs';
import { namesChecks } from './lib/names.mjs';
import { takeInfoChecks } from './lib/takeinfo.mjs';
import { openGuest } from './lib/guest.mjs';
import { recordingsWhileRecording, recordingsAfterSync } from './lib/recordings.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '..');
const WORKSPACE = join(CRATE, '..', '..');
const RESULTS = join(HERE, 'test-results');
const TAKE_SECONDS = 8;
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


const host = spawn(exe, ['--dist', dist, '--library', LIBRARY, '--call', 'browser-test', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
  '--port', '0', '--http-port', '0', '--udp-port', '0',
  // Always open: the leave/rejoin checks rejoin B after A (the producer) left.
  '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
ctx.hostLog = '';
host.stderr.on('data', (d) => { ctx.hostLog += d.toString(); });
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
  // Automated runs stay silent. The fallback run's 8 s take still records
  // bit-exact with this flag; longer-run render-clock effects are not covered.
  '--mute-audio',
];
const logs = { A: [], B: [] };
const browsers = [];
Object.assign(ctx, { MODE, FULL, exe, dist, RESULTS, LIBRARY, HTTP, HTTPS, host, logs, browsers, BASE_ARGS, input, INPUT_WAV, TAKE_SECONDS, HERE, CRATE, WORKSPACE, chromium, firefox });



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
  // Live levels (every mode): each tile's waveform and speaking ring follow the audio arriving.
  await liveLevelChecks(A, B);

  await videoChecks(A, B);

  await takeChecks(A, B);

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
  const seg1Name = await recordingsWhileRecording(A, B, seg1);
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
  await recordingsAfterSync(A, B, seg1Name);
  // Video recording is off by default: takes 1 and 2 have no video track.
  check(['take01', 'take02'].every((t) => !readdirSync(join(LIBRARY, 'browser-test', t)).some((f) => f.includes('_cam'))),
    'with video recording off (the default) takes 1 and 2 have no video track');
  // Take 3 records video (chromium and firefox): see lib/videorec.mjs.
  if (MODE !== 'fallback') await videoRecChecks(A, B);

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
  if (MODE !== 'fallback') await capChecks();
  if (MODE !== 'fallback') await namesChecks();
  if (MODE !== 'fallback') await takeInfoChecks();
  passed = checks.every((c) => c.ok);

} catch (e) {
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  host.kill();
  if (ctx.host2) ctx.host2.kill();
  if (ctx.host3) ctx.host3.kill();
  if (ctx.host4) ctx.host4.kill();
  if (ctx.host5) ctx.host5.kill();
  if (ctx.host6) ctx.host6.kill();
}
writeFileSync(join(RESULTS, 'host' + (FULL ? '' : '-' + MODE) + '.log'), ctx.hostLog);
if (ctx.host2) writeFileSync(join(RESULTS, 'host-security.log'), ctx.host2Log);
if (ctx.host3) writeFileSync(join(RESULTS, 'host-identity-' + MODE + '.log'), ctx.host3Log);
if (ctx.host4) writeFileSync(join(RESULTS, 'host-cap-' + MODE + '.log'), ctx.host4Log);
for (const n of ['A', 'B']) writeFileSync(join(RESULTS, 'guest-' + n + (FULL ? '' : '-' + MODE) + '.log'), logs[n].join('\n'));
if (!passed) {
  console.log('host log tail:');
  console.log(ctx.hostLog.split('\n').slice(-30).join('\n'));
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
