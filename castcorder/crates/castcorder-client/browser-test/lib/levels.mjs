// Live levels on the remote tiles (cast-vr3), every mode: each guest's tile
// for the other shows the level of the audio actually arriving (the tile's
// data-live-level, from an AnalyserNode on the received track) and lights its
// speaking ring while that guest's fake microphone plays. A's input is the
// continuous test WAV; B's is Chromium's fake beep or Firefox's fake tone.
//
// Mute (cast-xe2, muteChecks, run inside take 1 so A's track is checked
// bit-exact through it in chromium): B sees A's slashed mic and a dimmed
// waveform that keeps moving with A's reported raw level (about 10 Hz), no
// speaking ring, and silence arriving; after unmute, A is heard again.
import { sleep, waitFor, check } from './helpers.mjs';

// Above this the waveform is visibly moving (-46 dBFS).
export const LIVE = 0.005;

// Sample, for `ms`, the tile on `page` of the guest whose stream id starts
// with `origin` + '-': its live level, speaking ring, muted indicator, and
// the waveform. Returns the readings' max, min and distinct count, the
// tallest bar (px; 2 is flat), and in how many samples the ring was on, the
// muted indicator showed and the waveform was dimmed; while muted, the
// highest reported raw level and how many distinct reports arrived.
export function sampleTile(page, origin, ms = 3000) {
  return page.evaluate(async ([origin, ms]) => {
    const tile = () => document.querySelector('#remote .tile[data-origin^="' + origin + '-"]');
    const live = [], mic = [], micAt = new Set();
    let speaking = 0, muted = 0, n = 0, dimmed = 0, tallest = 0;
    const end = Date.now() + ms;
    while (Date.now() < end) {
      const t = tile();
      if (t) {
        n++;
        live.push(Number(t.getAttribute('data-live-level') || 0));
        if (t.classList.contains('speaking')) speaking++;
        if (t.classList.contains('mic-muted')) muted++;
        if (t.hasAttribute('data-mic-level')) { mic.push(Number(t.getAttribute('data-mic-level'))); micAt.add(t.getAttribute('data-mic-at')); }
        const wave = t.querySelector('.tile-wave');
        if (wave && Number(getComputedStyle(wave).opacity) < 0.9) dimmed++;
        if (wave) for (const s of wave.children) tallest = Math.max(tallest, parseFloat(s.style.height) || 0);
      }
      await new Promise((r) => setTimeout(r, 100));
    }
    return {
      n, speaking, muted, dimmed,
      max: live.length ? Math.max(...live) : 0,
      min: live.length ? Math.min(...live) : 0,
      distinct: new Set(live).size,
      tallest,
      micMax: mic.length ? Math.max(...mic) : 0,
      micUpdates: micAt.size,
    };
  }, [origin, ms]);
}

export async function liveLevelChecks(A, B) {
  for (const [viewer, vn, origin] of [[B, 'B', 'A'], [A, 'A', 'B']]) {
    let last = null;
    const s = await waitFor(async () => {
      const r = last = await sampleTile(viewer, origin, 3000);
      return r.n > 0 && r.max >= LIVE && r.speaking > 0 && r.tallest > 2 ? r : null;
    }, vn + ' sees ' + origin + '\x27s live level move and the speaking ring light', 30000, 0)
      .catch((e) => { console.log('last sample: ' + JSON.stringify(last)); throw e; });
    check(true, vn + '\x27s tile for ' + origin + ' shows the audio arriving: peak ' + s.max.toFixed(3)
      + ', tallest bar ' + s.tallest + ' px, speaking ring on in '
      + s.speaking + ' of ' + s.n + ' samples');
  }
}

// Below this the received audio counts as silence (-50 dBFS).
export const SILENT = 0.003;

export async function muteChecks(A, B) {
  await A.click('#mute');
  check((await A.getAttribute('#mute', 'aria-pressed')) === 'true', 'A muted in the call (mid-take)');
  // Let the audio already in flight play out.
  await sleep(800);
  const s = await sampleTile(B, 'A', 2500);
  console.log('B samples A muted: ' + JSON.stringify(s));
  check(s.n > 0 && s.muted === s.n && s.dimmed === s.n,
    'B sees A\x27s slashed mic and a dimmed waveform in every sample (' + s.muted + '/' + s.dimmed + ' of ' + s.n + ')');
  check(s.max < SILENT, 'B receives silence from muted A (live peak ' + s.max.toFixed(4) + ')');
  check(s.speaking === 0, 'no speaking ring on A\x27s tile while A is muted');
  check(s.micMax >= LIVE && s.micUpdates >= 10 && s.tallest > 2,
    'A\x27s dimmed waveform keeps moving with its raw microphone: ' + s.micUpdates + ' level reports in 2.5 s, peak '
    + s.micMax.toFixed(3) + ', tallest bar ' + s.tallest + ' px');
  await A.click('#mute');
  check((await A.getAttribute('#mute', 'aria-pressed')) === 'false', 'A unmuted');
  const back = await waitFor(async () => {
    const r = await sampleTile(B, 'A', 1000);
    return r.n > 0 && r.muted === 0 && r.max >= LIVE ? r : null;
  }, 'B sees A unmuted and hears A again', 10000, 0);
  check(true, 'B sees A unmuted and hears A again (live peak ' + back.max.toFixed(3) + ')');
}
