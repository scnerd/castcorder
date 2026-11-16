// Track status on the tiles and the producer's roll-up (cast-2y4), every
// mode. Each device acknowledges its recorded sources to the host, which
// shares each participant's recording with producers (every tile) and with
// the device itself (its own tile):
// - take 1: every chip goes Ready -> Starting -> Recording at the take start
//   and back to Ready after the stop; the producer's top bar reads
//   "2 of 2 recording"; a guest who is not a producer sees only its own chip.
// - take 2 (micLostChecks): a guest's microphone track ends mid-take and is
//   held lost (test hook window.__castcorderHoldMic): the producer sees
//   "Mic lost" on its tile and "1 of 2 recording"; released, the microphone
//   reopens, a new segment records and its chip is Recording again.
import { waitFor, check } from './helpers.mjs';

// Every track chip's state on `page`, by whose tile: 'self', or the stream
// id's name part of a remote tile ('B' for 'B-7'). Hidden chips are skipped.
export function chips(page) {
  return page.evaluate(() => {
    const out = {};
    const own = document.querySelector('#self-track');
    if (own) out.self = [own.getAttribute('data-state')];
    for (const t of document.querySelectorAll('#remote .tile')) {
      const c = t.querySelector('.tile-track');
      if (!c || c.hasAttribute('hidden')) continue;
      const who = (t.getAttribute('data-origin') || '').replace(/-[0-9]+$/, '');
      (out[who] = out[who] || []).push(c.getAttribute('data-state'));
    }
    const r = document.querySelector('#track-rollup');
    out.rollup = r ? r.textContent.trim() : '';
    return out;
  });
}

// Log every state each chip shows on `page` (a MutationObserver plus a 50 ms
// scan), so a Starting that lasts a moment is still seen.
export function watchChips(page) {
  return page.evaluate(() => {
    const log = window.__chipLog = {};
    const scan = () => {
      const seen = [];
      const own = document.querySelector('#self-track');
      if (own) seen.push(['self', own.getAttribute('data-state')]);
      for (const t of document.querySelectorAll('#remote .tile')) {
        const c = t.querySelector('.tile-track');
        if (!c || c.hasAttribute('hidden')) continue;
        seen.push([(t.getAttribute('data-origin') || '').replace(/-[0-9]+$/, ''), c.getAttribute('data-state')]);
      }
      for (const [who, s] of seen) {
        const l = log[who] = log[who] || [];
        if (s && l[l.length - 1] !== s) l.push(s);
      }
    };
    new MutationObserver(scan).observe(document.body, { subtree: true, childList: true, attributes: true, attributeFilter: ['data-state'] });
    window.__chipScan = setInterval(scan, 50);
    scan();
  });
}

export const chipLog = (page) => page.evaluate(() => window.__chipLog || {});

// `seq` appears in `log` in order (other states may come between).
const inOrder = (log, seq) => {
  let i = 0;
  for (const s of log || []) if (s === seq[i]) i++;
  return i === seq.length;
};

const has = (c, who, state) => (c[who] || []).includes(state);

// Before take 1: both chips Ready, logging every change.
export async function chipsBeforeTake(A, B) {
  await waitFor(async () => { const a = await chips(A); return has(a, 'self', 'ready') && has(a, 'B', 'ready') ? a : null; },
    'producer A sees Ready on its own tile and on B\x27s', 15000);
  await waitFor(async () => has(await chips(B), 'self', 'ready'), 'guest B sees Ready on its own tile', 15000);
  await watchChips(A);
  await watchChips(B);
  check(true, 'before the take every track chip reads Ready');
}

// Take 1 recording: every chip Recording, the producer's roll-up "2 of 2".
export async function chipsWhileRecording(A, B) {
  const a = await waitFor(async () => {
    const c = await chips(A);
    return has(c, 'self', 'recording') && has(c, 'B', 'recording') && c.rollup === '2 of 2 recording' ? c : null;
  }, 'producer A sees Recording on both tiles and "2 of 2 recording"', 15000);
  check(true, 'producer A: both tracks Recording, top bar reads "' + a.rollup + '"');
  const b = await waitFor(async () => { const c = await chips(B); return has(c, 'self', 'recording') ? c : null; },
    'guest B sees Recording on its own tile', 15000);
  check(!b.A && b.rollup === '', 'guest B (not a producer) sees only its own track chip and no roll-up: ' + JSON.stringify(b));
}

// After take 1 stops: Ready again, no roll-up; every chip went
// Ready -> Starting -> Recording -> Ready.
export async function chipsAfterTake(A, B) {
  await waitFor(async () => {
    const c = await chips(A);
    return has(c, 'self', 'ready') && has(c, 'B', 'ready') && c.rollup === '';
  }, 'producer A sees Ready on both tiles after the stop, and no roll-up', 15000);
  await waitFor(async () => has(await chips(B), 'self', 'ready'), 'guest B sees Ready after the stop', 15000);
  const seq = ['ready', 'starting', 'recording', 'ready'];
  const la = await chipLog(A), lb = await chipLog(B);
  check(inOrder(la.self, seq) && inOrder(la.B, seq) && inOrder(lb.self, seq),
    'track chips went Ready -> Starting -> Recording -> Ready: A ' + JSON.stringify(la) + ' B ' + JSON.stringify(lb));
  await A.evaluate(() => clearInterval(window.__chipScan));
  await B.evaluate(() => clearInterval(window.__chipScan));
}

// Mid-take 2, B connected and recording: its microphone track ends and is held
// lost; the producer sees it; released, a new segment records. Returns once B
// lost its microphone and the hold is released, with when its tracks were
// stopped (the caller waits for the new segment, then calls
// micRecoveredChecks).
export async function micLostChecks(A, B) {
  await waitFor(async () => {
    const c = await chips(A);
    return has(c, 'B', 'recording') && c.rollup === '2 of 2 recording';
  }, 'producer A sees B Recording after B rejoined mid-take (B acknowledged its mic again)', 20000);
  check(true, 'after a reconnect mid-take, B\x27s chip is Recording again and A reads "2 of 2 recording"');
  await B.evaluate(() => { window.__castcorderHoldMic = true; for (const t of window.__tracks) t.stop(); });
  const tMicLost = Date.now();
  const a = await waitFor(async () => {
    const c = await chips(A);
    return has(c, 'B', 'mic-lost') && c.rollup === '1 of 2 recording' ? c : null;
  }, 'producer A sees Mic lost on B\x27s tile and "1 of 2 recording"', 20000);
  check(true, 'B\x27s microphone ended mid-take: producer A sees ' + JSON.stringify(a));
  await waitFor(async () => has(await chips(B), 'self', 'mic-lost'), 'guest B sees Mic lost on its own tile', 10000);
  check(true, 'guest B sees Mic lost on its own tile');
  const mlText = await A.evaluate(() => {
    for (const t of document.querySelectorAll('#remote .tile')) {
      const c = t.querySelector('.tile-track[data-state="mic-lost"]');
      if (c) return c.textContent.trim();
    }
    return '';
  });
  check(mlText === 'Mic lost', 'the chip says it in words: "' + mlText + '"');
  await B.evaluate(() => { window.__castcorderHoldMic = false; });
  return tMicLost;
}

// After B's new segment records (and B reconnected): Recording again.
export async function micRecoveredChecks(A) {
  const a = await waitFor(async () => {
    const c = await chips(A);
    return (c.B || []).length && c.B.every((s) => s === 'recording') && c.rollup === '2 of 2 recording' ? c : null;
  }, 'producer A sees B Recording again (new segment) and "2 of 2 recording"', 30000);
  check(true, 'B\x27s microphone came back in a new segment: ' + JSON.stringify(a));
}
