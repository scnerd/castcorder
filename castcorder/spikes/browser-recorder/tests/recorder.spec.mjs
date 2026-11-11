// Automated proof for the browser-recorder spike. Run with `npm test` (see README in FINDINGS.md).
// Every test writes its measured numbers to work/results/<test>.json.
import { test, expect } from '@playwright/test';
import fs from 'node:fs';
import path from 'node:path';
import * as h from './harness.mjs';
import * as a from '../scripts/analyze.mjs';
import { makeInput, writeWav16 } from '../scripts/gen-wav.mjs';

test.describe.configure({ mode: 'serial' });

const input = makeInput();
const RESULTS = path.join(h.WORK, 'results');
const PROFILES = path.join(h.WORK, 'profiles');
const SOAK_SECONDS = Number(process.env.SOAK_SECONDS || 150);
// The app waits 500 ms after building the audio graph before the first recorded frame,
// so recorded takes must have no glitch at all. The warmup=0 test tolerates (and
// measures) Chrome's start-up zero insertions within this window.
const STARTUP_GRACE_S = 0.25;
let server, base;

test.beforeAll(async () => {
  writeWav16(h.INPUT_WAV, input);
  fs.rmSync(PROFILES, { recursive: true, force: true });
  fs.mkdirSync(RESULTS, { recursive: true });
  ({ server, url: base } = await h.serve({ coi: true }));
});
test.afterAll(async () => { server?.close(); });

function save(name, obj) {
  fs.writeFileSync(path.join(RESULTS, `${name}.json`), JSON.stringify(obj, null, 2));
  console.log(`[${name}] ${JSON.stringify(obj.summary ?? obj)}`);
}

function summarise(per, rate) {
  return per.map((c) => ({
    ch: c.against, verdict: c.verdict, frames: c.frames, leadingZeroFrames: c.leadingZeroFrames,
    maxAbsError: c.maxAbsError, droppedFrames: c.droppedFrames, duplicatedFrames: c.duplicatedFrames,
    insertedFrames: c.insertedFrames, discontinuities: c.discontinuities,
    lateGlitches: c.firstDiscontinuities.filter((d) => d.outputFrame > STARTUP_GRACE_S * rate).length,
  }));
}

function expectClean(per, rate, graceS = 0) {
  for (const c of per) {
    expect(c.verdict, `channel ${c.against}`).toMatch(/^bit-exact/);
    expect(c.droppedFrames).toBe(0);
    expect(c.duplicatedFrames).toBe(0);
    expect(c.discontinuities).toBeLessThan(50);
    for (const d of c.firstDiscontinuities) expect(d.outputFrame, 'glitch outside start-up grace').toBeLessThan(graceS * rate);
  }
}

async function analyseTake(page, take) {
  const buf = await h.readOpfs(page, `${take.name}.wav`);
  const w = a.parseWav(buf);
  const per = a.analyzeRecording(a.deinterleave(w.samples, w.channels), input);
  return { wav: { rate: w.rate, channels: w.channels, frames: w.frames, bytes: buf.length, dataDeclared: w.dataDeclared }, per, buf };
}

async function sidecar(page, name) { return JSON.parse((await h.readOpfs(page, `${name}.json`)).toString('utf8')); }

test('two opens of one device + MediaStreamTrackProcessor probe', async () => {
  const b = await h.launch(path.join(PROFILES, 'probe'));
  try {
    await b.page.goto(base + '/');
    const r = await b.page.evaluate(async () => {
      const md = navigator.mediaDevices;
      const raw = await md.getUserMedia({ audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false, channelCount: { ideal: 2 } } });
      const rawTrack = raw.getAudioTracks()[0];
      const live = await md.getUserMedia({ audio: { deviceId: { exact: rawTrack.getSettings().deviceId }, echoCancellation: true, noiseSuppression: true, autoGainControl: true } });
      const liveTrack = live.getAudioTracks()[0];
      const out = { mstpOnWindow: typeof MediaStreamTrackProcessor !== 'undefined', rawSettings: rawTrack.getSettings(), liveSettings: liveTrack.getSettings() };
      if (!out.mstpOnWindow) return out;
      async function grab(track, seconds) {
        const reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
        const chunks = []; let frames = 0, fmt = null, rate = 0, chans = 0, sizes = new Set();
        while (true) {
          const { value } = await reader.read();
          fmt = value.format; rate = value.sampleRate; chans = value.numberOfChannels; sizes.add(value.numberOfFrames);
          const n = value.numberOfFrames, c0 = new Float32Array(n);
          value.copyTo(c0, { planeIndex: 0, format: 'f32-planar' });
          chunks.push(c0); frames += n; value.close();
          if (frames >= seconds * rate) break;
        }
        reader.cancel();
        const all = new Float32Array(frames); let o = 0;
        for (const c of chunks) { all.set(c, o); o += c.length; }
        const bytes = new Uint8Array(all.buffer); let s = '';
        for (let i = 0; i < bytes.length; i += 8192) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 8192));
        return { format: fmt, sampleRate: rate, channels: chans, frameSizes: [...sizes], b64: btoa(s) };
      }
      const [x, y] = await Promise.all([grab(rawTrack, 3), grab(liveTrack, 3)]);
      out.raw = x; out.live = y;
      return out;
    });
    const dec = (b64) => new Float32Array(new Uint8Array(Buffer.from(b64, 'base64')).buffer);
    const res = { rawSettings: r.rawSettings, liveSettings: r.liveSettings, mstpOnWindow: r.mstpOnWindow };
    if (r.raw) {
      const rawS = dec(r.raw.b64), liveS = dec(r.live.b64);
      const ra = a.analyzeChannel(rawS, input.L), la = a.analyzeChannel(liveS, input.L);
      res.mstpRaw = { format: r.raw.format, sampleRate: r.raw.sampleRate, channels: r.raw.channels, frameSizes: r.raw.frameSizes, verdict: ra.verdict, droppedFrames: ra.droppedFrames, insertedFrames: ra.insertedFrames, rms: a.rms(rawS.subarray(4410)) };
      res.mstpLive = { format: r.live.format, sampleRate: r.live.sampleRate, channels: r.live.channels, frameSizes: r.live.frameSizes, verdict: la.verdict, rms: a.rms(liveS.subarray(4800)) };
      res.liveToRawRmsRatioDb = 20 * Math.log10(res.mstpLive.rms / res.mstpRaw.rms);
    }
    res.summary = {
      raw: `${r.rawSettings.sampleRate} Hz x${r.rawSettings.channelCount} ec=${r.rawSettings.echoCancellation} ns=${r.rawSettings.noiseSuppression} agc=${r.rawSettings.autoGainControl}`,
      live: `${r.liveSettings.sampleRate} Hz x${r.liveSettings.channelCount} ec=${r.liveSettings.echoCancellation} ns=${r.liveSettings.noiseSuppression} agc=${r.liveSettings.autoGainControl}`,
      mstpOnWindow: r.mstpOnWindow, mstpRaw: res.mstpRaw?.verdict, mstpLive: res.mstpLive?.verdict, liveToRawRmsRatioDb: res.liveToRawRmsRatioDb,
    };
    save('probe', res);
    expect(r.rawSettings.echoCancellation).toBe(false);
    expect(r.liveSettings.echoCancellation).toBe(true);
    if (r.raw) {
      expect(res.mstpRaw.verdict).toMatch(/^bit-exact/);
      expect(res.mstpLive.verdict).toMatch(/^not-aligned/);
    }
  } finally { await b.kill(); }
});

for (const transport of ['sab', 'msg']) {
  test(`record 10 s via ${transport}, read back from OPFS, FLAC, download`, async () => {
    const b = await h.launch(path.join(PROFILES, `rec-${transport}`));
    try {
      await h.openApp(b.page, `${base}/?transport=${transport}`);
      const mics = await h.openMics(b.page);
      await b.page.click('#persist');
      await h.waitFor(async () => (await h.text(b.page, '#storage')).startsWith('persist() -> t') || (await h.text(b.page, '#storage')).startsWith('persist() -> f'), { what: 'persist' });
      const storage = await h.text(b.page, '#storage');
      await h.startTake(b.page);
      const rate = await h.recordFor(b.page, 10);
      const { stopped, takes } = await h.stopTake(b.page);
      const take = takes.find((t) => t.name === stopped.name);
      const { wav, per, buf } = await analyseTake(b.page, take);
      const meta = await sidecar(b.page, take.name);

      await b.page.click(`tr[data-name="${take.name}"] .flac`);
      const flac = await h.waitFor(async () => (await h.json(b.page, '#flac-result')), { what: 'flac', timeout: 120000 });

      let download = { ok: false };
      try {
        const [dl] = await Promise.all([b.page.waitForEvent('download', { timeout: 15000 }), b.page.click(`tr[data-name="${take.name}"] .download`)]);
        const to = path.join(h.WORK, `download-${transport}.wav`);
        await dl.saveAs(to);
        download = { ok: Buffer.compare(fs.readFileSync(to), buf) === 0, suggestedFilename: dl.suggestedFilename(), bytes: fs.statSync(to).size };
      } catch (e) { download = { ok: false, error: String(e.message).split('\n')[0] }; }

      const res = {
        transport, rate, mics, storage, stopped, wav, sidecar: meta, flac, download, per,
        summary: { transport, rate, frames: wav.frames, channels: wav.channels, channelsResult: summarise(per, rate), flac: { ms: flac.encodeAndVerifyMs, realtimeX: flac.realtimeFactor, bits: flac.bitsPerSample, ratioVsFloat: flac.flacBytes / flac.float32Bytes, ratioVsInt16: flac.flacBytes / flac.int16PcmBytes, roundtripOk: flac.roundtripOk, lossless: flac.losslessFromFloat }, download, storage, maxFlushMs: stopped.maxFlushMs, maxWriteMs: stopped.maxWriteMs },
      };
      save(`record-${transport}`, res);
      expect(wav.frames).toBeGreaterThanOrEqual(10 * rate);
      expect(wav.dataDeclared).toBe(wav.frames * wav.channels * 4);
      expect(meta.finalised).toBe(true);
      expect(stopped.seqGaps).toBe(0);
      expect(stopped.ringDroppedFrames).toBe(0);
      expectClean(per, rate);
      expect(flac.roundtripOk).toBe(true);
      expect(flac.losslessFromFloat).toBe(true);
      expect(flac.bitsPerSample).toBe(16);
      expect(download.ok).toBe(true);
    } finally { await b.kill(); }
  });
}

test('without warm-up: Chrome inserts zero blocks at source start (measured)', async () => {
  const runs = [];
  for (let i = 0; i < 3; i++) {
    const b = await h.launch(path.join(PROFILES, `nowarm-${i}`));
    try {
      await h.openApp(b.page, `${base}/?warmup=0`);
      await h.openMics(b.page);
      await h.startTake(b.page);
      const rate = await h.recordFor(b.page, 3);
      const { stopped, takes } = await h.stopTake(b.page);
      const { per } = await analyseTake(b.page, takes.find((t) => t.name === stopped.name));
      runs.push({ rate, leadingZeroFrames: per[0].leadingZeroFrames, insertedFrames: per[0].insertedFrames, glitches: per[0].firstDiscontinuities.map((d) => ({ atFrame: d.outputFrame, zeroFrames: d.foreignZeroFrames })) });
      expectClean(per, rate, STARTUP_GRACE_S);
    } finally { await b.kill(); }
  }
  save('no-warmup', { summary: runs });
});

test('AudioContext at a different rate than the track is resampled (detected)', async () => {
  const b = await h.launch(path.join(PROFILES, 'rate'));
  try {
    await h.openApp(b.page, `${base}/?rate=48000`);
    const mics = await h.openMics(b.page);
    await h.startTake(b.page);
    const rate = await h.recordFor(b.page, 4);
    const { stopped, takes } = await h.stopTake(b.page);
    const { wav, per } = await analyseTake(b.page, takes.find((t) => t.name === stopped.name));
    save('rate-mismatch', { summary: { trackRate: mics.raw.sampleRate, contextRate: rate, verdicts: per.map((c) => c.verdict) }, wav });
    expect(rate).not.toBe(mics.raw.sampleRate);
    for (const c of per) expect(c.verdict).toMatch(/^not-aligned/);
  } finally { await b.kill(); }
});

test('crash mid-take: kill the browser process tree, reopen, recover', async () => {
  const profile = path.join(PROFILES, 'crash');
  let b = await h.launch(profile);
  let last = null, name;
  try {
    await h.openApp(b.page, `${base}/`);
    await h.openMics(b.page);
    await h.startTake(b.page);
    await h.recordFor(b.page, 6.5, (s) => { last = s; });
    name = last.name;
  } finally { await b.kill(); }
  const killedAt = Date.now();
  b = await h.launch(profile);
  try {
    await h.openApp(b.page, `${base}/`);
    let polls = 0;
    const takes = await h.waitFor(async () => {
      const t = await h.json(b.page, '#debug-takes');
      if (t?.find((x) => x.name === name && x.recovered !== undefined)) return t;
      if (++polls % 20 === 0) await b.page.click('#refresh'); // retry recovery every ~2 s
      return null;
    }, { what: 'recovered take', timeout: 30000 }).catch(async (e) => {
      const opfs = await b.page.evaluate(async () => { const d = await navigator.storage.getDirectory(); const o = []; for await (const k of d.keys()) o.push(k); return o; });
      throw new Error(`${e.message}; errors=[${await h.errorsOf(b.page)}] takes=${await h.text(b.page, '#debug-takes')} opfs=${JSON.stringify(opfs)}`);
    });
    const listRetries = Math.floor(polls / 20);
    const take = takes.find((x) => x.name === name);
    const { wav, per } = await analyseTake(b.page, take);
    const meta = await sidecar(b.page, name);
    const rate = wav.rate;
    const res = {
      take, sidecar: meta, lastStatsBeforeKill: last, wav, per,
      summary: {
        recovered: take.recovered, recoveredFrames: wav.frames, recoveredSeconds: wav.frames / rate,
        lastSeenFramesBeforeKill: last.frames, lastFlushedFramesBeforeKill: last.framesAtLastFlush,
        headerFramesBeforeRecovery: take.headerFramesBeforeRecovery, journalFramesBeforeRecovery: take.journalFramesBeforeRecovery,
        framesBeyondLastSeen: wav.frames - last.frames, channelsResult: summarise(per, rate), reopenMs: Date.now() - killedAt, listRetries,
      },
    };
    save('crash', res);
    expect(take.recovered).toBe(true);
    expect(meta.finalised).toBe(true);
    expect(wav.dataDeclared).toBe(wav.frames * wav.channels * 4);
    expect(wav.frames).toBeGreaterThanOrEqual(last.framesAtLastFlush);
    expect(wav.frames).toBeGreaterThanOrEqual(last.frames);
    expectClean(per, rate);
  } finally { await b.kill(); }
});

for (const transport of ['sab', 'msg']) {
  test(`soak ${SOAK_SECONDS} s via ${transport}`, async () => {
    test.setTimeout((SOAK_SECONDS * 2 + 240) * 1000);
    const b = await h.launch(path.join(PROFILES, `soak-${transport}`));
    try {
      await h.openApp(b.page, `${base}/?transport=${transport}${process.env.KEEPALIVE ? "&keepalive=1" : ""}`);
      await h.openMics(b.page);
      await h.startTake(b.page);
      const startedAt = Date.now();
      const rate = await h.recordFor(b.page, SOAK_SECONDS);
      const { stopped, takes } = await h.stopTake(b.page);
      const { wav, per } = await analyseTake(b.page, takes.find((t) => t.name === stopped.name));
      const loopWraps = Math.floor(wav.frames / input.n);
      const wallSeconds = (Date.now() - startedAt) / 1000;
      save(`soak-${transport}`, { stopped, wav, per, summary: { transport, seconds: wav.frames / rate, wallSeconds, inputLoopWraps: loopWraps, channelsResult: summarise(per, rate), seqGaps: stopped.seqGaps, ringDroppedFrames: stopped.ringDroppedFrames, maxFlushMs: stopped.maxFlushMs, maxWriteMs: stopped.maxWriteMs } });
      expect(stopped.seqGaps).toBe(0);
      expect(stopped.ringDroppedFrames).toBe(0);
      expectClean(per, rate);
    } finally { await b.kill(); }
  });
}
