import { ctx } from './ctx.mjs';
import { waitFor, text, check } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// The lobby (cast-n56.3), on the main host: a name is required (no
// GuestXXXX default), the password field stays hidden until the host asks
// for one (security.mjs), and the microphone and camera test shows the
// camera and a moving meter, then closes every track and audio context it
// opened before Join opens the call's own.
export async function lobbyChecks() {
  const { HTTP, INPUT_WAV } = ctx;
  // A steady test signal (the default fake microphone only beeps now and then).
  const L = await openGuest('L', ['--use-file-for-fake-audio-capture=' + INPUT_WAV], HTTP, undefined, '');
  check(!(await L.locator('#join').isEnabled()) && (await L.isVisible('#name-needed'))
    && (await L.inputValue('#name')) === '',
    'lobby: Join waits for a name (no default guest name)');
  check(!(await L.isVisible('#password')), 'lobby: no password field until the call asks for one');

  const live = (p) => p.evaluate(() => ({
    tracks: window.__tracks.filter((t) => t.readyState !== 'ended').map((t) => t.kind),
    contexts: window.__audioContexts.filter((c) => c.state !== 'closed').length,
  }));
  check((await live(L)).tracks.length === 0, 'lobby: no device is open before the test');

  // The test, camera on: the preview plays and the meter moves.
  if (!(await L.isChecked('#lobby-camera'))) await L.check('#lobby-camera');
  await L.click('#test-devices');
  let seen = '';
  await waitFor(async () => {
    seen = (await text(L, '#mic-heard')) + ' (level ' + (await L.getAttribute('#mic-meter', 'data-level')) + ')';
    return seen.startsWith('We hear you');
  }, 'L: the meter hears the microphone', 15000).catch(async (e) => {
    const dbg = await L.evaluate(() => JSON.stringify({
      ctx: window.__audioContexts.map((c) => c.state),
      tracks: window.__tracks.map((t) => t.kind + ':' + t.readyState + ':' + t.muted + ':' + t.enabled + ':' + t.label),
    }));
    throw new Error(e.message + ': ' + seen + ' ' + dbg);
  });
  const level = Number(await L.getAttribute('#mic-meter', 'data-level'));
  await waitFor(() => L.evaluate(() => {
    const v = document.querySelector('#lobby-video');
    return !v.hidden && v.videoWidth > 0;
  }), 'L: the camera preview plays', 10000);
  const open = await live(L);
  check(level > 0 && open.tracks.includes('audio') && open.tracks.includes('video') && open.contexts === 1,
    'lobby: the test shows the camera and a moving microphone meter (level ' + level + ', ' + open.tracks.join('+') + ')');

  // Camera off for joining: the test follows, with the microphone only.
  await L.uncheck('#lobby-camera');
  await waitFor(async () => { const o = await live(L); return o.tracks.length === 1 && o.tracks[0] === 'audio'; },
    'L: the test drops the camera', 10000);
  check(await L.isHidden('#lobby-video'), 'lobby: camera off for joining, the test shows no preview');

  // Stop test: everything it opened is closed.
  await L.click('#test-devices');
  await waitFor(async () => { const o = await live(L); return o.tracks.length === 0 && o.contexts === 0; }, 'L: the test closes its devices', 10000);
  check((await text(L, '#test-devices')) === 'Test mic and camera', 'lobby: Stop test closes every track and audio context');

  // Join while testing: the test's tracks close, the call opens its own.
  await L.click('#test-devices');
  await waitFor(async () => (await live(L)).tracks.length === 1, 'L: the test runs again', 10000);
  const before = await L.evaluate(() => window.__tracks.length);
  await L.fill('#name', 'Lee');
  check(await L.locator('#join').isEnabled(), 'lobby: a typed name enables Join');
  await L.click('#join');
  await waitFor(async () => (await text(L, '#participant')).length > 0, 'L joined', 30000);
  const testTracks = await L.evaluate((n) => window.__tracks.slice(0, n).every((t) => t.readyState === 'ended'), before);
  check(testTracks && (await text(L, '#test-devices')) === 'Test mic and camera',
    'lobby: Join closes the test before the call opens the microphone');

  // Leave lands on "You left" (cast-n56.4), not the lobby; Rejoin goes back in.
  await L.click('#leave');
  await waitFor(() => L.isVisible('#left-call'), 'L sees the You left page', 10000);
  const summary = await text(L, '#left-summary');
  check(summary.startsWith('You were in the call for') && summary.includes('no takes recorded')
    && !(await L.isVisible('#name')) && (await text(L, '#join')) === 'Rejoin',
    'leaving shows You left with the time in the call and the takes, and Rejoin: ' + summary);
  await L.click('#join');
  await waitFor(async () => (await text(L, '#participant')).length > 0 && !(await L.isVisible('#left-call')), 'L rejoined from You left', 30000);
  check(true, 'Rejoin on the You left page joins the call again');
  await L.click('#leave');
  await waitFor(() => L.isVisible('#left-call'), 'L left again', 10000);
  await L.click('#left-settings');
  check(await L.isVisible('#name') && (await L.inputValue('#name')) === 'Lee',
    'Change name or devices goes back to the lobby with the name kept');
  // Back from You left: obvious on the host machine's page, and automatic after a while (cast-n56.12).
  await L.click('#join');
  await waitFor(async () => (await text(L, '#participant')).length > 0 && !(await L.isVisible('#left-call')), 'L joined again', 30000);
  await L.click('#leave');
  await waitFor(() => L.isVisible('#left-call'), 'L left a third time', 10000);
  check(await L.isVisible('#left-home'), 'You left offers Back to your rooms on the host machine');
  await waitFor(async () => (await L.locator('#nav-rooms').count()) === 1,
    'You left returns to the rooms by itself after a while', 40000);
  await L.close();
}
