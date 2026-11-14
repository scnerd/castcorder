import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { INPUT_RATE } from '../gen-wav.mjs';
import { parseWav, deinterleave, analyzeRecording } from '../analyze.mjs';
import { parseOggOpus } from '../ogg.mjs';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, buttons, json, noErrors, checks, check, opfsNames, parseTiming } from './helpers.mjs';

export async function takeChecks(A, B) {
  const { MODE, FULL, RESULTS, LIBRARY, HTTP, host, logs, input, TAKE_SECONDS, WORKSPACE, chromium, firefox } = ctx;
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
  const takeT0 = Date.now();
  // A second tab of A's profile (same OPFS) loads mid-take: its worker's
  // startup scan meets the files A's worker holds open (one sync handle per
  // file) and must list the track as busy, without an error and without
  // touching it (A's take is checked bit-exact below).
  {
    const second = await A.context().newPage();
    const secondLog = [];
    second.on('console', (m) => secondLog.push(m.text()));
    second.on('pageerror', (e) => secondLog.push('[pageerror] ' + e.message));
    await second.goto(HTTP + '/?name=A2');
    let listed;
    try {
      listed = await waitFor(async () => { const t = await json(second, '#debug-tracks'); return t && t.length ? t : null; },
        'second tab lists A\x27s tracks', 10000);
    } catch { listed = []; }
    const errs = await text(second, '#errors');
    check(listed.length > 0 && listed.every((t) => t.busy) && errs === '',
      'a second tab\x27s startup scan skips the track another tab is recording (busy, no error): '
      + JSON.stringify(listed) + (errs ? ' errors: ' + errs : ''));
    await second.close();
    writeFileSync(join(RESULTS, 'guest-A-second-tab' + (FULL ? '' : '-' + MODE) + '.log'), secondLog.join('\n'));
  }
  await sleep(Math.max(0, TAKE_SECONDS * 1000 - (Date.now() - takeT0)));
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
  // Safety tracks (cast-706): the host kept each guest's live Opus stream in
  // take01/safety/, one Ogg Opus per guest with a timing sidecar, none of it
  // in the take directory proper (so the outbox and aligner never see it).
  {
    const safetyDir = join(TAKE_DIR, 'safety');
    const safety = existsSync(safetyDir) ? readdirSync(safetyDir).sort() : [];
    console.log('host take01/safety: ' + safety.join(' '));
    check(['A', 'B'].every((n) => safety.includes('take01_' + n + '_safety.opus') && safety.includes('take01_' + n + '_safety.timing.jsonl'))
      && safety.filter((f) => f.endsWith('.opus')).length === 2,
      'safety/ holds one .opus and one timing file per guest');
    for (const n of ['A', 'B']) {
      const o = parseOggOpus(readFileSync(join(safetyDir, 'take01_' + n + '_safety.opus')));
      check(o.finished && o.packets > 0 && o.seconds > TAKE_SECONDS - 1.5 && o.seconds < TAKE_SECONDS + 1.5,
        n + '\x27s safety track is valid Ogg Opus: ' + o.packets + ' packets, ' + o.seconds.toFixed(2) + ' s for a ~' + TAKE_SECONDS + ' s take');
      const tl = readFileSync(join(safetyDir, 'take01_' + n + '_safety.timing.jsonl'), 'utf8').trim().split('\n').map((l) => JSON.parse(l));
      check(tl[0].type === 'header' && tl[0].source === 'safety' && tl.some((l) => l.type === 'ref' && typeof l.host_us === 'number'),
        n + '\x27s safety timing file is marked as a safety track and maps RTP time to host time');
    }
    check(!hostFiles.some((f) => f.includes('safety') && f !== 'safety'), 'no safety file in the take directory itself');
  }
  // Timing metadata (cast-whr): each guest's file parses, and its mapping
  // places the track's first frame just after the host's take start (loopback:
  // clocks are shared, so this checks the plumbing, not the estimator).
  // The worklet capture path (fallback, firefox) starts on the audio thread's own schedule,
  // and on a loaded CI runner Firefox was seen 44-72 ms late, so those modes allow 150 ms.
  const FRAME0_TOL_MS = MODE === 'chromium' ? 50 : 150;
  for (const n of ['A', 'B']) {
    const t = parseTiming(readFileSync(join(TAKE_DIR, 'take01_' + n + '_mic.timing.jsonl'), 'utf8'));
    const start = t.hostUsAt(0);
    const delta = (start - take1StartUs) / 1000;
    console.log('info ' + n + ' timing: ' + t.refs.length + ' refs, ' + t.sync.length + ' clock samples, last estimate '
      + JSON.stringify(t.lastEstimate) + '; frame 0 at ' + delta.toFixed(1) + ' ms after the take start');
    check(t.header && t.header.version === 1 && t.header.take === 1 && t.segments === 1 && t.end !== null && t.sync.length > 0
      && t.refs.length >= TAKE_SECONDS - 1 && Math.abs(delta) < FRAME0_TOL_MS,
      n + '\x27s timing metadata places frame 0 within ' + FRAME0_TOL_MS + ' ms of the host\x27s take start (' + delta.toFixed(1) + ' ms)');
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
}
