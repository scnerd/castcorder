// Copied from castcorder/spikes/browser-recorder/scripts/analyze.mjs (the spike
// is outside the workspace and may change or go away).
// Compares a recorded float track with the deterministic int16 input.
// Classifies it as bit-exact (every float == s/32768 for the aligned input
// sample s), sample-aligned-but-converted, or resampled/processed, and finds
// dropped or duplicated frames as jumps in the input position.

export function parseWav(buf) {
  if (buf.toString('ascii', 0, 4) !== 'RIFF' || buf.toString('ascii', 8, 12) !== 'WAVE') throw new Error('not a WAV');
  let off = 12, fmt = null, data = null, dataDeclared = 0;
  while (off + 8 <= buf.length) {
    const id = buf.toString('ascii', off, off + 4), size = buf.readUInt32LE(off + 4);
    if (id === 'fmt ') fmt = { tag: buf.readUInt16LE(off + 8), channels: buf.readUInt16LE(off + 10), rate: buf.readUInt32LE(off + 12), bits: buf.readUInt16LE(off + 22) };
    if (id === 'data') { dataDeclared = size; data = buf.subarray(off + 8, Math.min(buf.length, off + 8 + size)); break; }
    off += 8 + size + (size & 1);
  }
  if (!fmt || !data) throw new Error('missing fmt or data');
  const bps = fmt.bits / 8, frames = Math.floor(data.length / (bps * fmt.channels));
  const ab = new ArrayBuffer(frames * fmt.channels * bps);
  new Uint8Array(ab).set(data.subarray(0, ab.byteLength));
  const samples = fmt.tag === 3 ? new Float32Array(ab) : new Int16Array(ab);
  return { ...fmt, frames, samples, dataDeclared };
}

export function deinterleave(samples, channels) {
  const n = Math.floor(samples.length / channels), out = [];
  for (let c = 0; c < channels; c++) {
    const a = new Float32Array(n);
    for (let i = 0; i < n; i++) a[i] = samples[i * channels + c];
    out.push(a);
  }
  return out;
}

const key3 = (a, b, c) => ((a + 32768) * 65536 + (b + 32768)) * 65536 + (c + 32768);
const indexCache = new WeakMap();
function indexOf(ref) {
  let m = indexCache.get(ref);
  if (m) return m;
  m = new Map();
  for (let i = 0; i < ref.length; i++) {
    const k = key3(ref[i], ref[(i + 1) % ref.length], ref[(i + 2) % ref.length]);
    if (!m.has(k)) m.set(k, i);
  }
  indexCache.set(ref, m);
  return m;
}

// out: Float32Array (one channel), ref: Int16Array (looping input channel)
export function analyzeChannel(out, ref) {
  const N = ref.length, m = out.length, idx = indexOf(ref);
  const q = (i) => Math.max(-32768, Math.min(32767, Math.round(out[i] * 32768)));
  let start = 0;
  while (start < m && out[start] === 0) start++;
  let pos = null, firstInputPos = null, aligned = 0, exact = 0, exactAsym = 0, mismatched = 0, unlocked = 0, maxErr = 0;
  let run = 0, runZeros = 0, insertedFrames = 0, insertedZeroFrames = 0;
  const disc = [];
  const f = Math.fround, inv32767 = f(1 / 32767);
  let i = start;
  while (i < m) {
    if (pos === null) {
      if (i + 2 >= m) { unlocked += m - i; break; }
      const p = idx.get(key3(q(i), q(i + 1), q(i + 2)));
      if (p === undefined) { unlocked++; i++; continue; }
      pos = p; if (firstInputPos === null) firstInputPos = p;
    }
    const cur = pos % N, s = ref[cur];
    if (q(i) === s) {
      aligned++;
      if (out[i] === s / 32768) exact++;
      else if (s > 0 && (out[i] === f(s / 32767) || out[i] === f(s * inv32767))) exactAsym++;
      maxErr = Math.max(maxErr, Math.abs(out[i] - s / 32768));
      pos = cur + 1; i++; run = 0; runZeros = 0;
      continue;
    }
    if (i + 2 < m) {
      const p = idx.get(key3(q(i), q(i + 1), q(i + 2)));
      if (p !== undefined && p !== cur) {
        let d = (p - cur + N) % N; if (d > N / 2) d -= N;
        // A run of r mismatched frames followed by a jump of d: net input skip is d + r.
        // Net 0 means r foreign frames were inserted; > 0 dropped; < 0 duplicated.
        const net = d + run;
        if (net === 0) { insertedFrames += run; insertedZeroFrames += runZeros; }
        if (disc.length < 50) disc.push({ outputFrame: i - run, inputFrameExpected: (cur - run + N) % N, foreignFrames: run, foreignZeroFrames: runZeros, netInputSkip: net, deltaFrames: net });
        else disc.push(null);
        pos = p;
        continue;
      }
    }
    mismatched++; run++; if (out[i] === 0) runZeros++;
    maxErr = Math.max(maxErr, Math.abs(out[i] - s / 32768));
    pos = cur + 1; i++;
  }
  const real = disc.filter(Boolean);
  const dropped = real.filter((d) => d.deltaFrames > 0).reduce((a, d) => a + d.deltaFrames, 0);
  const duplicated = real.filter((d) => d.deltaFrames < 0).reduce((a, d) => a - d.deltaFrames, 0);
  const considered = m - start;
  // lossless: every aligned float is exactly s/32768 (or Chrome's asymmetric s/32767 for s > 0),
  // so the original int16 is recoverable bit-for-bit.
  const lossless = aligned > 0 && exact + exactAsym === aligned;
  let verdict;
  if (considered > 0 && aligned / considered > 0.99) {
    verdict = lossless ? (exact === aligned ? 'bit-exact' : 'bit-exact-int16 (lossless asymmetric int16->float scaling)') : 'sample-aligned-but-converted';
    if (real.length) verdict += ` with ${real.length} discontinuities`;
  } else verdict = 'not-aligned (resampled or processed)';
  return {
    verdict, frames: m, leadingZeroFrames: start, considered, aligned, exactFloat: exact, exactAsymmetric: exactAsym, mismatched, unlocked,
    insertedFrames, insertedZeroFrames,
    maxAbsError: maxErr, maxErrorLsb16: maxErr * 32768, firstInputFrame: firstInputPos,
    discontinuities: disc.length, droppedFrames: dropped, duplicatedFrames: duplicated, firstDiscontinuities: real.slice(0, 10),
  };
}

// Best normalised cross-correlation of a window of `out` against the looping ref (coarse fallback).
export function bestCorrelation(out, ref, from = 4800, len = 512, step = 1) {
  const N = ref.length;
  if (out.length < from + len) return null;
  let eo = 0;
  for (let j = 0; j < len; j++) eo += out[from + j] * out[from + j];
  let best = { corr: -1, lag: -1 };
  const r = new Float32Array(N);
  for (let k = 0; k < N; k++) r[k] = ref[k] / 32768;
  for (let k = 0; k < N; k += step) {
    let dot = 0, er = 0;
    for (let j = 0; j < len; j++) { const v = r[(k + j) % N]; dot += out[from + j] * v; er += v * v; }
    const c = dot / Math.sqrt(eo * er + 1e-20);
    if (c > best.corr) best = { corr: c, lag: k };
  }
  return best;
}

export function rms(a) {
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i] * a[i];
  return Math.sqrt(s / Math.max(1, a.length));
}

// Analyse a whole recording (array of channels) against input {L, R}.
export function analyzeRecording(channels, input) {
  const refs = [input.L, input.R];
  const per = channels.map((ch, c) => {
    if (channels.length === 2) return { channel: c, against: c === 0 ? 'L' : 'R', ...analyzeChannel(ch, refs[c]) };
    const l = analyzeChannel(ch, input.L), r = analyzeChannel(ch, input.R);
    return l.aligned >= r.aligned ? { channel: c, against: 'L', ...l } : { channel: c, against: 'R', ...r };
  });
  return per;
}
