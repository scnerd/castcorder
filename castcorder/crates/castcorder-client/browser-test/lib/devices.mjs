import { spawn } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check, callDir, noErrors, audioRising, parseTiming } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// Device selection (cast-94c) on a host of its own. Chromium's fake devices
// list three microphones (Fake Default Audio Input, Fake Audio Input 1 and 2)
// and three outputs.
//
// Chromium: guest Y picks Fake Audio Input 1 and Fake Audio Output 1 in the
// lobby, and both are still picked after a reload. Y's storage worker starts
// late (its wasm is served 2.5 s late), so Y joins before that worker
// listens; its main microphone still records through the first capture path
// (cast-n1s). In the call Y's Devices
// dialog shows them, every participant's <audio> plays through the picked
// output (sinkId). The producer X sees Y's devices by name in its People
// panel and there adds Fake Audio Input 2 as a backup microphone on Y's
// device (cast-swn); Y is told. X starts a take; Y's backup records beside
// its main microphone. Mid-take X switches Y's live microphone to the system
// default from the People panel (Firefox: Y in its Devices dialog): no new
// RTCPeerConnection, still connected, X still hears Y, and Y's main track
// continues in a new segment. X then adds a backup whose device cannot be
// opened (getUserMedia replaced to refuse that one device, as a busy device
// would): it is listed as unavailable, X's page warns that its main
// microphone still records, and the producers see that it cannot meet the
// recording profile. After the take the host holds, verified: Y's two main
// segments (named for their devices in order, the second starting after the
// first ends, each with timing on the host timeline, the first holding the
// audio from when Y said it recorded until the switch), Y's backup track
// (source mic-2), and X's track. After Leave nothing is left open.
//
// fallback (MediaStreamTrackProcessor deleted from window): the same, with
// the main and backup microphones on the AudioWorklet path.
//
// Firefox (one fake microphone): the pickers and the dialog work, there is
// no other microphone to add as a backup, and a take still lands.
export async function devicesChecks() {
  const { MODE, exe, dist, RESULTS } = ctx;
  const ROOM = 'devices-test';
  const LIB = join(RESULTS, 'library-devices-' + MODE);
  rmSync(LIB, { recursive: true, force: true });
  ctx.host9 = spawn(exe, ['--dist', dist, '--library', LIB, '--room', ROOM, '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host9Log = '';
  ctx.host9.stderr.on('data', (d) => { ctx.host9Log += d.toString(); });
  const [HTTP9, HTTPS9] = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('devices host did not print HOST_READY')), 20000);
    ctx.host9.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
    });
  });
  const X = await openGuest('X', [], HTTP9);
  // Y's storage worker starts late (its wasm is served 2.5 s late), so Y
  // joins before that worker listens: the join's capture must still reach
  // it (cast-n1s: the command was lost and the take's first segment held
  // almost no audio).
  const slowWorker = (c) => c.route('**/worker_bg.wasm', async (r) => { await sleep(2500); await r.continue(); });
  const Y = await openGuest('Y', [], HTTPS9, slowWorker);
  const chosen = (p, sel) => p.evaluate((s) => { const e = document.querySelector(s); return e ? e.options[e.selectedIndex].textContent : null; }, sel);
  const options = (p, sel) => p.evaluate((s) => [...document.querySelectorAll(s + ' option')].map((o) => o.textContent), sel);
  const firefox = MODE === 'firefox';

  // The Join button stays where it was drawn while the lobby learns about the
  // devices (cast-9n7: Firefox's "Show device names" row appeared above it
  // a moment after load and moved it from under a click, so a fresh guest's
  // Join was sometimes lost).
  const joinY = (p) => p.evaluate(() => Math.round(document.querySelector('#join').getBoundingClientRect().y));
  const y0 = await joinY(X);
  await waitFor(async () => (await options(X, '#pick-mic')).length >= 2 || (await X.locator('#devices-allow').isVisible()),
    'X lobby lists microphones or asks to name them', 10000);
  const y1 = await joinY(X);
  check(y1 === y0, 'the lobby\x27s Join button stays put once the devices are known (y ' + y0 + ' then ' + y1 + ')');

  // The lobby's pickers, remembered across a reload. A browser that names
  // no device before one is allowed (Firefox) offers "Show device names".
  const named = async () => (await options(Y, '#pick-mic')).length >= 2;
  await waitFor(async () => (await named()) || (await Y.locator('#devices-allow').isVisible()), 'Y lobby lists microphones or asks to name them', 10000);
  if (!(await named())) {
    await Y.click('#devices-allow');
    check(true, 'the lobby asks to show device names before the browser names them');
  }
  await waitFor(named, 'Y lobby lists microphones', 10000);
  const mics = await options(Y, '#pick-mic');
  check(mics[0] === 'System default', 'the lobby microphone picker offers the system default first: ' + JSON.stringify(mics));
  const MIC1 = firefox ? mics[1] : 'Fake Audio Input 1';
  const MIC2 = 'Fake Audio Input 2';
  if (!firefox) check(mics.includes(MIC1) && mics.includes(MIC2), 'the lobby lists every fake microphone by name');
  await Y.selectOption('#pick-mic', { label: MIC1 });
  const sinks = !firefox;
  if (sinks) await Y.selectOption('#pick-out', { label: 'Fake Audio Output 1' });
  await Y.reload();
  await Y.waitForSelector('#join');
  await waitFor(async () => (await chosen(Y, '#pick-mic')) === MIC1, 'Y lobby still picks ' + MIC1 + ' after a reload', 10000);
  check(true, 'the lobby microphone pick is remembered across a reload');
  if (sinks) check((await chosen(Y, '#pick-out')) === 'Fake Audio Output 1', 'the lobby speakers pick is remembered across a reload');

  // Only what this section logs: X and Y are names other sections use too.
  const mark = { X: ctx.logs.X.length, Y: ctx.logs.Y.length };
  const since = (n) => ctx.logs[n].slice(mark[n]);
  for (const p of [X, Y]) await p.click('#join');
  for (const [n, p] of [['X', X], ['Y', Y]]) {
    await waitFor(async () => (await text(p, '#participant')).length > 0, n + ' joined', 40000);
  }
  await Y.click('#devices');
  await Y.waitForSelector('#devices-dialog');
  check((await chosen(Y, '#dev-mic')) === MIC1, 'the Devices dialog shows the picked microphone');
  await waitFor(async () => (await text(Y, '#dev-recorded li[data-source="mic"] .dev-name')) === MIC1,
    'the main recorded source is the picked microphone', 10000);
  if (sinks) {
    const want = await Y.evaluate(() => document.querySelector('#dev-out').value);
    await waitFor(async () => Y.evaluate((w) => { const a = [...document.querySelectorAll('#remote audio')]; return a.length > 0 && a.every((e) => e.sinkId === w); }, want),
      'every participant plays through the picked speakers', 15000);
    check(true, 'every participant\x27s audio plays through the picked speakers (setSinkId)');
  }

  // The producer X sees Y's devices by name in the People panel (cast-swn).
  const yId = await text(Y, '#participant');
  const yRow = '.psources[data-participant="' + yId + '"]';
  await waitFor(async () => (await X.locator(yRow).count()) === 1, 'X sees Y\x27s devices in the People panel', 15000);
  await X.click(yRow + ' summary');
  await waitFor(async () => (await chosen(X, yRow + ' .remote-mic')) === MIC1, 'X sees Y\x27s picked microphone by name', 15000);
  check(true, 'a producer sees a guest\x27s microphones by name and which one is picked');
  if (!firefox) {
    // A backup microphone before the take, added by the producer X on Y's
    // device: it records from the take start.
    check((await options(X, yRow + ' .add-remote-backup')).includes(MIC2), 'X is offered Y\x27s other microphones as a backup');
    await X.selectOption(yRow + ' .add-remote-backup', { label: MIC2 });
    await waitFor(async () => (await Y.locator('#dev-recorded li[data-source="mic-2"][data-state="ready"]').count()) === 1,
      'Y backup microphone ready', 15000);
    await waitFor(async () => (await X.locator(yRow + ' .remote-backup[data-source="mic-2"][data-state="ready"]').count()) === 1,
      'X sees the backup it added on Y ready', 15000);
    check((await text(Y, '#call-notes')).includes('X (producer) added ' + MIC2 + ' as your backup microphone'),
      'Y is told the producer added a backup microphone');
    check(true, 'a producer adds a backup microphone on a guest\x27s device');
  } else {
    check(await Y.isDisabled('#dev-backup-add'), 'firefox: no other microphone to add as a backup');
  }

  // Both record from the take start (a capture path is chosen; a page whose
  // first path gave no audio in time has fallen back by then).
  // The page logs the chosen path across several console lines.
  const chosenPath = (n) => {
    const l = since(n);
    const i = l.findIndex((x) => x.includes('recording capture path chosen'));
    if (i < 0) return null;
    const m = l.slice(i, i + 4).join(' ').match(/capture_path = "([^"]+)"/);
    return m ? m[1] : '?';
  };
  for (const n of ['X', 'Y']) {
    await waitFor(async () => chosenPath(n), n + ' chose a capture path', 20000);
  }
  const yPath = chosenPath('Y');
  // The first path that can work there (in fallback a Chromium with
  // MediaStreamTrackProcessor in workers takes it there).
  const wantPaths = firefox ? ['worklet'] : MODE === 'fallback' ? ['mstp-worker', 'worklet'] : ['mstp-window'];
  check(wantPaths.includes(yPath), 'Y, joined before its storage worker listened, records its main microphone through '
    + wantPaths.join(' or ') + ': ' + yPath);
  await waitFor(async () => X.isEnabled('#take-start'), 'X can start a take', 15000);
  await X.click('#take-start');
  await waitFor(async () => (await text(X, '#take-result')).includes('"take":1'), 'take 1 started', 10000);
  const seg1 = await waitFor(async () => { const r = await text(Y, '#recording'); return r.includes('recording take0001') ? r : null; }, 'Y recording', 15000);
  const recordingAt = Date.now();
  if (!firefox) {
    await waitFor(async () => (await Y.locator('#dev-recorded li[data-source="mic-2"][data-state="recording"]').count()) === 1,
      'Y backup microphone recording', 15000);
    check(true, 'a backup microphone records beside the main one');
  }
  await sleep(3000);

  // Switch the live microphone mid-take.
  const pcs = await Y.evaluate(() => window.__pcs.length);
  const switchAt = Date.now();
  if (firefox) {
    await Y.selectOption('#dev-mic', { value: '' });
  } else {
    // The producer X switches it, on Y's device (cast-swn).
    await X.selectOption(yRow + ' .remote-mic', { value: '' });
  }
  const seg2 = await waitFor(async () => { const r = await text(Y, '#recording'); return r.includes('recording take0001') && r !== seg1 ? r : null; },
    'Y main track continues in a new segment after the switch', 20000);
  check(seg2 !== seg1, 'a microphone switch mid-take starts a new segment (' + seg1 + ' then ' + seg2 + ')');
  check((await Y.evaluate(() => window.__pcs.length)) === pcs, 'the switch swaps the sent track without a new connection');
  const yState = await waitFor(async () => { const s = await text(Y, '#state'); return s === 'connected' ? s : null; }, 'Y connected after the switch', 5000);
  check(yState === 'connected', 'Y stays connected through the switch');
  if (!firefox) {
    await waitFor(async () => (await chosen(Y, '#dev-mic')) === 'System default', 'Y\x27s Devices dialog shows the switch X made', 10000);
    await waitFor(async () => (await chosen(X, yRow + ' .remote-mic')) === 'System default', 'X sees the switch it made on Y', 10000);
    check((await text(Y, '#call-notes')).includes('X (producer) switched your microphone to the system default'),
      'Y is told the producer switched its microphone');
  }
  check((await audioRising(X, 'Y')) > 0, 'X still receives Y\x27s audio after the switch');

  if (!firefox) {
    // A backup that cannot be opened: listed as unavailable, warned, reported.
    await X.evaluate((label) => {
      const gum = MediaDevices.prototype.getUserMedia;
      MediaDevices.prototype.getUserMedia = async function (c) {
        const want = c && c.audio && c.audio.deviceId && c.audio.deviceId.exact;
        const all = await navigator.mediaDevices.enumerateDevices();
        const d = all.find((x) => x.deviceId === want);
        if (d && d.label === label) throw new DOMException('Could not start audio source', 'NotReadableError');
        return gum.call(this, c);
      };
    }, MIC2);
    await X.click('#devices');
    await X.selectOption('#dev-backup', { label: MIC2 });
    await X.click('#dev-backup-add');
    await waitFor(async () => (await X.locator('#dev-recorded li[data-source="mic-2"][data-state="unavailable"]').count()) === 1,
      'X backup microphone listed as unavailable', 15000);
    check((await text(X, '#warning')).includes('Your main microphone still records'), 'X is warned and told its main microphone still records');
    await waitFor(async () => (await text(X, '#profile-warnings')).includes('mic-2'), 'producers see the backup cannot meet the recording profile', 15000);
    check(true, 'producers are told a backup microphone cannot meet the recording profile');
    await X.click('#devices-close');
  }

  await sleep(2000);
  await X.click('#take-stop');
  await waitFor(async () => (await text(X, '#take-status')).startsWith('idle'), 'take 1 stopped', 10000);
  const TAKE = join(callDir(LIB, ROOM), 'take01');
  const files = () => (existsSync(TAKE) ? readdirSync(TAKE) : []);
  const wavs = () => files().filter((f) => f.endsWith('.wav'));
  const want = firefox ? 3 : 4;
  await waitFor(async () => wavs().length >= want && !files().some((f) => f.endsWith('.journal') || f.endsWith('.obao'))
    && files().filter((f) => f.endsWith('.timing.jsonl')).length >= want ? true : null,
  'the host verified every track of take 1', 60000, 250);
  console.log('devices take01: ' + files().join(' '));
  const base = (f) => f.replace(/\.wav$/, '');
  const meta = (f) => JSON.parse(readFileSync(join(TAKE, base(f) + '.json'), 'utf8'));
  const timing = (f) => parseTiming(readFileSync(join(TAKE, base(f) + '.timing.jsonl'), 'utf8'));
  const yMain = wavs().filter((f) => /^take01_Y(-\d+)?_mic\.wav$/.test(f)).map((f) => ({ f, m: meta(f), t: timing(f) }))
    .sort((a, b) => a.m.startedAt - b.m.startedAt);
  check(yMain.length === 2, 'Y\x27s main microphone landed as two segments: ' + yMain.map((s) => s.f).join(' '));
  if (yMain.length === 2) {
    const [a, b] = yMain;
    check(a.m.source === 'mic' && b.m.source === 'mic', 'both segments are the main source (mic)');
    check(a.m.sourceLabel === MIC1, 'the first segment names the picked microphone: ' + a.m.sourceLabel);
    if (!firefox) check(b.m.sourceLabel === 'Fake Default Audio Input', 'the second segment names the switched-to microphone: ' + b.m.sourceLabel);
    const rate = (t) => t.header.sample_rate;
    const aEnd = a.t.hostUsAt(a.t.end);
    const bStart = b.t.hostUsAt(0);
    check(a.t.end > rate(a.t) * 2 && b.t.end > rate(b.t) * 2, 'both segments hold audio (' + a.t.end + ' and ' + b.t.end + ' frames)');
    // The first segment holds all the audio from when Y said it recorded
    // until the switch (less 0.5 s of slack), not just its start.
    const aSec = a.t.end / rate(a.t);
    const heldSec = (switchAt - recordingAt) / 1000;
    check(aSec > heldSec - 0.5, 'the first segment holds the audio up to the switch ('
      + aSec.toFixed(2) + ' s recorded, ' + heldSec.toFixed(2) + ' s between recording and the switch)');
    check(Number.isFinite(aEnd) && Number.isFinite(bStart), 'both segments are placed on the host timeline');
    const gapMs = (bStart - aEnd) / 1000;
    check(gapMs > -50 && gapMs < 3000, 'the second segment starts after the first ends (gap ' + gapMs.toFixed(0) + ' ms)');
  }
  if (!firefox) {
    const backup = wavs().filter((f) => /^take01_Y(-\d+)?_mic-2\.wav$/.test(f));
    check(backup.length === 1, 'Y\x27s backup microphone landed as its own track: ' + backup.join(' '));
    if (backup.length === 1) {
      const m = meta(backup[0]);
      const t = timing(backup[0]);
      check(m.source === 'mic-2' && m.sourceLabel === MIC2, 'the backup track names its source and device: ' + m.source + ' ' + m.sourceLabel);
      check(t.end > t.header.sample_rate * 4 && Number.isFinite(t.hostUsAt(0)), 'the backup track holds the take with timing on the host timeline');
    }
    check(!wavs().some((f) => /_X(-\d+)?_mic-2\.wav$/.test(f)), 'the unavailable backup recorded nothing');
  }
  check(wavs().some((f) => /^take01_X_mic\.wav$/.test(f)), 'X\x27s own track landed');
  // Each segment and the backup are aligned as tracks of their own.
  const ALIGNED = join(TAKE, 'aligned');
  const aligned = () => (existsSync(ALIGNED) ? readdirSync(ALIGNED) : []);
  const wantAligned = [...yMain.map((s) => s.f), ...wavs().filter((f) => /_mic-2\.wav$/.test(f))];
  await waitFor(async () => wantAligned.every((f) => aligned().includes(f)) ? true : null,
    'the host aligned every segment and the backup', 30000, 250);
  check(true, 'every segment and the backup are aligned: ' + wantAligned.join(' '));

  // Leave releases the backups too.
  for (const [n, p] of [['X', X], ['Y', Y]]) {
    await noErrors(p, n);
    await p.click('#leave');
    await waitFor(async () => p.evaluate(() => window.__tracks.every((t) => t.readyState === 'ended')
      && window.__audioContexts.every((c) => c.state === 'closed')), n + ' released every device after Leave', 10000);
  }
  check(true, 'Leave releases the main and backup microphones');
}
