// Video recording (M4 part 2, chromium and firefox modes): take 3 with the
// call's recording profile set to record video by the producer's control.
// - Each guest with a camera records it with MediaRecorder (WebM) into OPFS;
//   A turns its camera off and on mid-take, which ends one video track and
//   starts another.
// - chromium: B's MediaRecorder claims no codec (isTypeSupported replaced),
//   so B reports "cannot meet profile": A's producer panel shows it, B's
//   audio still records and syncs, and B has no video track.
// - Every video track, its JSON sidecar and timing metadata sync to the host
//   as takeNN_<who>_cam.*; per device the host verified all audio before any
//   video; each .webm is valid WebM (EBML header, DocType webm; ffprobe, when
//   present and always on CI, decodes a VP8/VP9 stream of plausible
//   duration); the timing file maps the recorder start near the take start
//   and its chunks tile the file. A's take 3 audio stays bit-exact (chromium).
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { parseWav, deinterleave, analyzeRecording } from '../analyze.mjs';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, buttons, check, opfsNames, parseTiming, inputZeroHostUs, loopDiffUs } from './helpers.mjs';
import { INPUT_RATE, INPUT_SECONDS } from '../gen-wav.mjs';

// How far two takes' timing metadata may disagree about the same input
// instant: each places frames by their earliest delivery over a second, so
// the spread of the shortest delivery delay (a few ms) bounds it.
const INPUT_TRUTH_TOL_MS = 15;

// A minimal EBML look at a WebM: the magic, the DocType, and the largest
// Cluster Timecode (ms at the default TimecodeScale) as a rough duration.
export function inspectWebm(buf) {
  const magic = buf.length > 4 && buf[0] === 0x1a && buf[1] === 0x45 && buf[2] === 0xdf && buf[3] === 0xa3;
  const head = buf.subarray(0, 64).toString('latin1');
  const docType = head.includes('webm') ? 'webm' : head.includes('matroska') ? 'matroska' : '';
  let clusters = 0, maxTimecode = -1;
  for (let i = 0; i + 8 < buf.length; i++) {
    if (buf[i] !== 0x1f || buf[i + 1] !== 0x43 || buf[i + 2] !== 0xb6 || buf[i + 3] !== 0x75) continue;
    // Cluster: an EBML size (1-8 bytes), then usually the Timecode (0xE7).
    const first = buf[i + 4];
    let len = 1;
    while (len <= 8 && !(first & (0x80 >> (len - 1)))) len++;
    const j = i + 4 + len;
    if (len > 8 || buf[j] !== 0xe7) continue;
    const sz = buf[j + 1] & 0x7f;
    if (!(buf[j + 1] & 0x80) || sz < 1 || sz > 8) continue;
    let tc = 0;
    for (let k = 0; k < sz; k++) tc = tc * 256 + buf[j + 2 + k];
    clusters++;
    maxTimecode = Math.max(maxTimecode, tc);
  }
  return { magic, docType, clusters, seconds: maxTimecode / 1000 };
}

function ffprobe(file) {
  const r = spawnSync('ffprobe', ['-v', 'error', '-select_streams', 'v:0', '-show_entries',
    'stream=codec_name,width,height:packet=pts_time', '-of', 'json', file], { encoding: 'utf8', maxBuffer: 64 << 20 });
  if (r.error || r.status === null) return null;
  if (r.status !== 0) return { error: (r.stderr || '').trim() || 'ffprobe exit ' + r.status };
  const j = JSON.parse(r.stdout);
  const pts = (j.packets || []).map((p) => Number(p.pts_time)).filter((x) => Number.isFinite(x));
  const s = (j.streams || [])[0] || {};
  return { codec: s.codec_name, width: s.width, height: s.height, packets: pts.length, seconds: pts.length ? Math.max(...pts) - Math.min(...pts) : 0 };
}

export async function videoRecChecks(A, B) {
  const { MODE, RESULTS, LIBRARY, input, TAKE_SECONDS } = ctx;
  const chromium = MODE === 'chromium';
  const TAKE3 = join(LIBRARY, 'browser-test', 'take03');

  // The producer turns video recording on (default off: takes 1 and 2 had none).
  check((await text(A, '#room-record-video')) === 'audio only', 'the recording profile records no video by default');
  await A.click('#record-video');
  await waitFor(async () => (await text(A, '#room-record-video')) === 'audio and video', 'producer turned video recording on', 10000);
  const roomJson = join(LIBRARY, 'browser-test', 'room.json');
  if (existsSync(roomJson)) {
    check(JSON.parse(readFileSync(roomJson, 'utf8')).record_video === true, 'the setting is saved with the call (room.json)');
  }
  if (chromium) {
    // B stands in for a browser whose MediaRecorder has no WebM VP9/VP8.
    await B.evaluate(() => { MediaRecorder.isTypeSupported = () => false; });
  }
  for (const [n, p] of [['A', A], ['B', B]]) {
    await waitFor(async () => (await text(p, '#camera-state')) === 'on', 'guest ' + n + ' camera on before take 3', 10000);
  }
  await waitFor(async () => buttons(A).then((b) => b.start && !b.stop), 'producer ready for take 3', 10000);
  await A.click('#take-start');
  await waitFor(async () => (await text(A, '#take-result')).includes('"take":3'), 'take 3 started', 10000);
  const take3StartUs = JSON.parse(await text(A, '#take-result')).hostUs;
  check((await text(A, '#take-status')).includes('with video'), 'take 3 status says video is recorded');
  const recA1 = await waitFor(async () => { const r = await text(A, '#video-recording'); return r.startsWith('recording take0003') ? r : null; },
    'A recording its camera', 15000);
  if (chromium) {
    const warn = await waitFor(async () => { const t = await text(A, '#profile-warnings'); return t.includes('B (guest') ? t : null; },
      'producer warned that B cannot record video', 15000);
    check(/cannot record cam for take 3/.test(warn), 'A\x27s producer panel: ' + warn);
    check((await text(B, '#video-recording')).includes('cannot record video'), 'B shows it cannot record video: ' + await text(B, '#video-recording'));
    check((await text(B, '#recording')).includes('recording take0003'), 'B still records its audio in take 3');
  } else {
    await waitFor(async () => (await text(B, '#video-recording')).startsWith('recording take0003'), 'B recording its camera', 15000);
  }
  // A's camera off then on mid-take: two video tracks for A, and (chromium)
  // A's raw audio stays bit-exact through it (cast-47x: opening the new
  // video track once stalled the storage worker past the audio source's
  // queue; camtoggle.mjs stresses this).
  await sleep(3000);
  await A.click('#camera');
  await waitFor(async () => (await text(A, '#video-recording')).startsWith('stopped take0003'), 'A\x27s first video track closed with its camera', 10000);
  await A.click('#camera');
  await waitFor(async () => { const r = await text(A, '#video-recording'); return r.startsWith('recording take0003') && r !== recA1; },
    'A records a new video track after the camera is back', 15000);
  await sleep(3000);
  await A.click('#take-stop');
  await waitFor(async () => (await text(A, '#take-status')).startsWith('idle'), 'take 3 stopped', 10000);

  const expected = { A: { audio: 3, video: 6 }, B: { audio: 3, video: chromium ? 0 : 3 } };
  const total = 3 + expected.A.video + 3 + expected.B.video;
  const manifest = () => {
    const m = join(TAKE3, 'manifest.jsonl');
    return existsSync(m) ? readFileSync(m, 'utf8').split('\n').filter((l) => l.length).map((l) => JSON.parse(l)) : [];
  };
  await waitFor(async () => manifest().filter((r) => r.Verified).length >= total,
    'host verified take 3: audio and video of both guests (' + total + ' items)', 180000, 500);
  for (const [n, p] of [['A', A], ['B', B]]) {
    await waitFor(async () => ((await opfsNames(p)).length === 0 ? true : null), 'guest ' + n + ' OPFS emptied after take 3 sync', 30000);
  }
  // Audio before video, per device (the guests' pullers run side by side).
  const recs = manifest();
  const kindOf = new Map();
  const who = new Map();
  for (const r of recs) if (r.Offered) { kindOf.set(r.Offered.offer.device + '/' + r.Offered.offer.name, r.Offered.offer.kind); who.set(r.Offered.offer.device, r.Offered.participant); }
  for (const dev of who.keys()) {
    const order = recs.filter((r) => r.Verified && r.Verified.device === dev).map((r) => kindOf.get(dev + '/' + r.Verified.name));
    const lastAudio = order.lastIndexOf('Audio'), firstVideo = order.indexOf('Video');
    const n = who.get(dev);
    check(order.filter((k) => k === 'Audio').length === expected[n].audio && order.filter((k) => k === 'Video').length === expected[n].video,
      n + '\x27s take 3 items verified: ' + order.join(' '));
    check(firstVideo === -1 || lastAudio < firstVideo, n + '\x27s audio was verified before any of its video');
  }
  const files = readdirSync(TAKE3).sort();
  console.log('host take03: ' + files.join(' '));
  const want = ['take03_A_mic.wav', 'take03_B_mic.wav', 'take03_A_cam.webm', 'take03_A_cam.json', 'take03_A_cam.timing.jsonl'];
  want.push('take03_A-2_cam.webm', 'take03_A-2_cam.json', 'take03_A-2_cam.timing.jsonl');
  if (!chromium) want.push('take03_B_cam.webm', 'take03_B_cam.json', 'take03_B_cam.timing.jsonl');
  check(want.every((f) => files.includes(f)) && !files.some((f) => f.endsWith('.obao') || f.endsWith('.journal')),
    'host take03 holds every audio track and each video track with its sidecar and timing metadata');
  if (chromium) check(!files.some((f) => f.startsWith('take03_B') && f.includes('_cam')), 'B, which cannot record video, has no video track');

  const probeNeeded = !!process.env.CI;
  const webms = files.filter((f) => f.endsWith('.webm'));
  for (const f of webms) {
    const base = f.replace(/\.webm$/, '');
    const buf = readFileSync(join(TAKE3, f));
    const w = inspectWebm(buf);
    const sc = JSON.parse(readFileSync(join(TAKE3, base + '.json'), 'utf8'));
    const lines = readFileSync(join(TAKE3, base + '.timing.jsonl'), 'utf8').trim().split('\n').map((l) => JSON.parse(l));
    const header = lines[0];
    const start = lines.find((l) => l.type === 'video_start');
    const chunks = lines.filter((l) => l.type === 'chunk');
    const end = lines.find((l) => l.type === 'video_end');
    let tiled = chunks.length > 0;
    let off = 0;
    for (const c of chunks) { tiled = tiled && c.offset === off; off += c.bytes; }
    const p = ffprobe(join(TAKE3, f));
    console.log('info ' + f + ': ' + buf.length + ' bytes, ' + w.clusters + ' clusters, ~' + w.seconds.toFixed(2) + ' s by clusters; '
      + (p ? 'ffprobe ' + JSON.stringify(p) : 'no ffprobe') + '; ' + chunks.length + ' chunks; recorder ' + sc.mimeType + ' '
      + sc.width + 'x' + sc.height + ' at ' + sc.videoBitsPerSecond + ' bit/s');
    check(w.magic && w.docType === 'webm' && buf.length > 10000, f + ' is WebM (EBML header, DocType webm, ' + buf.length + ' bytes)');
    check(sc.finalised === true && sc.kind === 'video' && sc.take === 3 && sc.bytes === buf.length && String(sc.mimeType).startsWith('video/webm'),
      f + '\x27s sidecar: finalised video, take 3, ' + sc.bytes + ' bytes, ' + sc.mimeType);
    check(header.type === 'header' && header.sample_rate === 0 && header.take === 3 && header.source === 'cam'
      && start && typeof start.host_us === 'number' && end && end.bytes === buf.length && tiled && off === buf.length && chunks.length >= 2,
      f + '\x27s timing metadata: header, video_start on the host clock, ' + chunks.length + ' chunks tiling the file, video_end');
    if (p === null) {
      check(!probeNeeded, 'ffprobe is installed (required on CI)');
      check(w.seconds > 1 && w.seconds < TAKE_SECONDS + 5, f + ' spans ~' + w.seconds.toFixed(2) + ' s by its cluster timecodes');
    } else {
      check(!p.error && (p.codec === 'vp8' || p.codec === 'vp9') && p.packets > 10 && p.seconds > 1 && p.seconds < TAKE_SECONDS + 5,
        'ffprobe decodes ' + f + ': ' + (p.error || p.codec + ' ' + p.width + 'x' + p.height + ', ' + p.packets + ' packets, ' + p.seconds.toFixed(2) + ' s'));
    }
    if (f === 'take03_A_cam.webm' || f === 'take03_B_cam.webm') {
      const delta = (start.host_us - take3StartUs) / 1000;
      check(delta > -100 && delta < 3000, f + ' recorder started ' + delta.toFixed(0) + ' ms after the take start (host timeline)');
    }
  }
  // Audio recorded beside video.
  const wav = parseWav(readFileSync(join(TAKE3, 'take03_A_mic.wav')));
  const secs = wav.frames / wav.rate;
  check(secs > 5, 'A\x27s take 3 audio is ' + secs.toFixed(2) + ' s');
  if (chromium) {
    const per3 = analyzeRecording(deinterleave(wav.samples, wav.channels), input);
    // Ground truth (cast-vo4): A's input kept playing on its own clock since
    // take 1, so take 3's content and timing metadata must place the input's
    // frame 0 where take 1's did (modulo its loop). Each take's mapping is
    // its own (reference points from its own deliveries), so a mapping that
    // misplaces a take shows here, however late either take started.
    const t3 = parseTiming(readFileSync(join(TAKE3, 'take03_A_mic.timing.jsonl'), 'utf8'));
    const zero3 = inputZeroHostUs(t3, per3[0], INPUT_RATE);
    const apart = loopDiffUs(zero3, ctx.inputZeroUs, INPUT_SECONDS * 1e6) / 1000;
    check(Math.abs(apart) < INPUT_TRUTH_TOL_MS,
      'A\x27s timing metadata places the same input on the same host time in take 1 and take 3 (' + apart.toFixed(1)
      + ' ms apart, within ' + INPUT_TRUTH_TOL_MS + ' ms)');
    for (const c of per3) {
      const clean = c.verdict.startsWith('bit-exact') && c.discontinuities === 0 && c.droppedFrames === 0
        && c.duplicatedFrames === 0 && c.insertedFrames === 0 && c.mismatched === 0;
      check(clean, 'take 3 (with video) channel ' + c.channel + ' vs ' + c.against + ': ' + c.verdict + ' (dropped ' + c.droppedFrames
        + ', duplicated ' + c.duplicatedFrames + ', inserted ' + c.insertedFrames + ', mismatched ' + c.mismatched + ')');
    }
    const disc = readFileSync(join(TAKE3, 'take03_A_mic.timing.jsonl'), 'utf8').trim().split('\n')
      .map((l) => JSON.parse(l)).filter((l) => l.type === 'discontinuity');
    check(disc.length === 0, 'A\x27s take 3 timing metadata lists no discontinuity: ' + JSON.stringify(disc));
  }
  // Back to audio only for what follows.
  await A.click('#record-video');
  await waitFor(async () => (await text(A, '#room-record-video')) === 'audio only', 'producer turned video recording off', 10000);
}
