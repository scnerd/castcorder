// Where tracks are saved and whether they reached the host (bead cast-dal),
// checked during and after take 2 of e2e.mjs (every mode):
// - the host machine's page (A) shows the call's folder (<library>/<call>)
//   and take 2's tracks: B "recording" while the take records, then every
//   track "verified" (or "aligned");
// - B's "Your recordings" shows the segment its lost microphone ended as
//   "waiting" while the take records (nothing syncs then), and "delivered"
//   once the host has it;
// - B made a producer sees the same takes and states but not the folder.
import { resolve } from 'node:path';
import { ctx } from './ctx.mjs';
import { waitFor, check, sleep } from './helpers.mjs';

const rows = (p, take) => p.evaluate((take) => [...document.querySelectorAll('#recordings li.take[data-take="' + take + '"] li.track')]
  .map((li) => ({ who: li.dataset.participant, file: li.dataset.file, state: li.dataset.state, text: li.textContent })), take);

const mine = (p) => p.evaluate(() => [...document.querySelectorAll('#your-recordings li')]
  .map((li) => ({ name: li.dataset.name, state: li.dataset.state, text: li.textContent })));

const norm = (s) => s.replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();

// While take 2 records: B's finished first segment waits, and A sees B recording.
export async function recordingsWhileRecording(A, B, seg1) {
  const name = (seg1.match(/take[0-9]+-[0-9]+-[A-Za-z0-9-]+/) || [seg1])[0];
  const row = await waitFor(async () => (await mine(B)).find((r) => r.name === name && r.state === 'waiting'),
    'B lists its first take 2 segment as waiting to sync', 15000).catch(() => null);
  check(!!row, 'B: "Your recordings" shows ' + name + ' waiting to sync while the take records'
    + (row ? '' : ' (saw ' + JSON.stringify(await mine(B)) + ')'));
  const rec = await waitFor(async () => (await rows(A, 2)).find((r) => r.who === 'B' && r.state === 'recording'),
    'host page shows B recording take 2', 10000).catch(() => null);
  check(!!rec, 'host page: take 2 lists B as recording while the take records');
  return name;
}

// After take 2 synced.
export async function recordingsAfterSync(A, B, seg1Name) {
  const want = norm(resolve(ctx.LIBRARY, 'browser-test'));
  const folder = await waitFor(async () => {
    const t = await A.locator('#recordings-folder').textContent({ timeout: 1000 }).catch(() => '');
    return t && norm(t) === want ? t : null;
  }, 'host page shows the call folder', 10000).catch(() => null);
  check(!!folder, 'host page shows the call folder ' + (folder || '(missing; want ' + want + ')'));
  check((await A.locator('#open-folder').count()) === 1, 'host page offers "Open recordings folder"');
  const done = (rs) => rs.length >= 3 && rs.every((r) => r.state === 'verified' || r.state === 'aligned');
  const take2 = await waitFor(async () => { const r = await rows(A, 2); return done(r) ? r : null; },
    'host page: every take 2 track verified', 20000).catch(() => null);
  check(!!take2, 'host page: take 2 tracks all reach verified: '
    + JSON.stringify(take2 || await rows(A, 2)));
  check(!!take2 && ['A', 'B'].every((w) => take2.some((r) => r.who === w && r.file.startsWith('take02_'))),
    'host page: take 2 lists A\'s and B\'s track files');
  const del = await waitFor(async () => (await mine(B)).find((r) => r.name === seg1Name && r.state === 'delivered'),
    'B lists its first segment as delivered', 15000).catch(() => null);
  check(!!del, 'B: "Your recordings" shows ' + seg1Name + ' delivered to the host (removed here)');
  check((await mine(B)).every((r) => r.state === 'delivered'), 'B: every recording delivered after the sync');

  // A producer on another device: same takes, no host path.
  const grant = A.locator('#guests li[data-name="B"] button.grant');
  await grant.click();
  const seen = await waitFor(async () => { const r = await rows(B, 2); return done(r) ? r : null; },
    'B, made producer, sees take 2', 15000).catch(() => null);
  check(!!seen, 'remote producer B sees take 2\'s tracks and their states');
  await sleep(1000);
  check((await B.locator('#recordings-folder').count()) === 0 && (await B.locator('#open-folder').count()) === 0,
    'remote producer B is not told the host\'s folder path');
  await A.locator('#guests li[data-name="B"] button.grant').click();
  await waitFor(async () => (await B.locator('#take-start').count()) === 0, 'B no longer a producer', 10000);
}
