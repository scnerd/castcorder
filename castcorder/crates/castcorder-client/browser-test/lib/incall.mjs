import { spawn } from 'node:child_process';
import { rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check } from './helpers.mjs';
import { openGuest } from './guest.mjs';
import { sampleTile } from './levels.mjs';

// In-call fixes from the UX walkthrough, chromium and firefox, on their own
// host: X is the producer (the host machine's localhost page), Y a guest over
// HTTPS.
// - cast-d13: a muted guest's tile shows no level (a flat waveform).
// - cast-5tm: the first-mute explanation closes on unmute and on any other
//   action.
// - cast-8g4: a guest who leaves loses their tile on the others' pages; a
//   rejoin shows exactly one tile, not a second beside the old one.
export async function inCallChecks() {
  const { MODE, exe, dist, RESULTS, INPUT_WAV } = ctx;
  const LIB = join(RESULTS, 'library-incall-' + MODE);
  rmSync(LIB, { recursive: true, force: true });
  ctx.host10 = spawn(exe, ['--dist', dist, '--library', LIB, '--room', 'incall-test', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host10Log = '';
  ctx.host10.stderr.on('data', (d) => { ctx.host10Log += d.toString(); });
  const [HTTP, HTTPS] = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('in-call host did not print HOST_READY')), 20000);
    ctx.host10.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
    });
  });
  // Y speaks continuously (the test WAV; Firefox's fake tone is continuous too).
  const X = await openGuest('X', [], HTTP);
  const Y = await openGuest('Y', ['--use-file-for-fake-audio-capture=' + INPUT_WAV], HTTPS);
  for (const p of [X, Y]) await p.click('#join');
  const tiles = (p) => p.evaluate(() => [...document.querySelectorAll('#remote .tile')].map((t) => t.dataset.origin));
  const first = await waitFor(async () => {
    const t = await tiles(X);
    return t.length === 1 && t[0].startsWith('Y-') ? t[0] : null;
  }, 'X shows one tile for Y', 40000);

  // cast-d13: Y muted shows no level on X (Y's audio was arriving first).
  await waitFor(async () => {
    const r = await sampleTile(X, 'Y', 1500);
    return r.n > 0 && r.tallest > 2 ? r : null;
  }, 'X sees Y\x27s level move', 30000, 0);
  // cast-n56.5: Y's own tile shows Y's level too: the newest 12 of the
  // dock waveform's readings (a steady test tone sits on the adaptive noise
  // floor, so both are flat here; the heights are compared, not the motion).
  const ownWave = () => Y.evaluate(() => ({
    own: [...document.querySelectorAll('#self-level .tile-wave span')].map((s) => s.style.height),
    dock: [...document.querySelectorAll('.dock .wave span')].slice(-12).map((s) => s.style.height),
  }));
  const ow = await ownWave();
  check(ow.own.length === 12 && JSON.stringify(ow.own) === JSON.stringify(ow.dock) && await Y.isVisible('#self-level'),
    'the own tile shows the own microphone level (the dock waveform\x27s newest readings): ' + JSON.stringify(ow));
  await Y.click('#mute');
  // The waveform keeps about 1.3 s of readings: let the unmuted ones scroll out.
  await waitFor(async () => (await sampleTile(X, 'Y', 500)).muted > 0, 'X sees Y muted', 10000, 0);
  await sleep(2000);
  const m = await sampleTile(X, 'Y', 2500);
  console.log('X samples Y muted: ' + JSON.stringify(m));
  check(m.n > 0 && m.muted === m.n && m.tallest <= 2 && m.speaking === 0,
    'a muted guest\x27s tile shows the slashed mic and no level: tallest bar ' + m.tallest + ' px (2 is flat) in '
    + m.n + ' samples, still reporting a raw level of ' + m.micMax.toFixed(3));
  await Y.click('#mute');

  // cast-5tm: the first-mute explanation closes on unmute, and on another action.
  const tip = (p) => p.locator('.tip').count();
  await X.click('#mute');
  await waitFor(async () => (await tip(X)) === 1, 'first mute explains itself', 5000);
  await X.click('#mute');
  await sleep(300);
  check((await tip(X)) === 0 && (await X.getAttribute('#mute', 'aria-pressed')) === 'false',
    'unmuting closes the "Muted for the others" explanation');
  await X.click('#mute');
  await waitFor(async () => (await tip(X)) === 1, 'a second mute explains again (not yet acknowledged)', 5000);
  await X.click('#self-view');
  await sleep(300);
  // Hidden self view says so while the camera is on; camera off keeps the avatar.
  const camOn = (await text(X, '#camera-state')) === 'on';
  check((await X.isVisible('#self-hidden-note')) === camOn,
    'Hide me shows "Self view hidden" only while the camera is on (camera ' + (camOn ? 'on' : 'off') + ')');
  check((await tip(X)) === 0 && (await X.getAttribute('#mute', 'aria-pressed')) === 'true',
    'another action (Hide me) closes the explanation; still muted');
  await X.click('#self-view');
  await X.click('#mute');

  // cast-8g4: Y leaves, X's tile for Y goes; Y rejoins, X shows one tile.
  const tLeave = Date.now();
  await Y.click('#leave');
  let after = null;
  try {
    await waitFor(async () => { after = await tiles(X); return after.length === 0; }, 'X drops Y\x27s tile after Y leaves', 40000);
  } catch { /* reported below */ }
  check(after !== null && after.length === 0, 'Y left: X shows no tile for Y after '
    + ((Date.now() - tLeave) / 1000).toFixed(1) + ' s (tiles: ' + JSON.stringify(after) + ')');
  await Y.click('#join');
  await waitFor(async () => (await tiles(X)).some((o) => o.startsWith('Y-') && o !== first), 'X shows Y\x27s new tile', 40000);
  await sleep(2000);
  const back = await tiles(X);
  check(back.length === 1 && back[0] !== first, 'Y rejoined: X shows exactly one tile for Y ' + JSON.stringify(back)
    + ' (was ' + first + ')');
  for (const p of [X, Y]) await p.click('#leave').catch(() => {});
}
