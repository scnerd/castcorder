// The deterministic fake-microphone input, copied from
// castcorder/spikes/browser-recorder/scripts/gen-wav.mjs: 44.1 kHz, 16-bit,
// stereo, 12 s (Chromium loops it). L = 440 Hz sine + LCG white noise; R = 660 Hz
// sine + independent LCG noise. The noise makes every position identifiable, so
// analyze.mjs can align a recording sample-exactly and see any dropped,
// duplicated or inserted frame.
import fs from 'node:fs';
import path from 'node:path';

export const INPUT_RATE = 44100;
export const INPUT_SECONDS = 12;

export function makeInput() {
  const n = INPUT_RATE * INPUT_SECONDS;
  const L = new Int16Array(n), R = new Int16Array(n);
  let a = 12345, b = 67890;
  const lcg = (s) => (Math.imul(s, 1664525) + 1013904223) >>> 0;
  for (let i = 0; i < n; i++) {
    a = lcg(a); b = lcg(b);
    const na = (a / 4294967296 - 0.5) * 0.3, nb = (b / 4294967296 - 0.5) * 0.3;
    const l = 0.25 * Math.sin(2 * Math.PI * 440 * i / INPUT_RATE) + na;
    const r = 0.2 * Math.sin(2 * Math.PI * 660 * i / INPUT_RATE) + nb;
    L[i] = Math.max(-32768, Math.min(32767, Math.round(l * 32767)));
    R[i] = Math.max(-32768, Math.min(32767, Math.round(r * 32767)));
  }
  return { L, R, n, rate: INPUT_RATE };
}

export function writeWav16(file, { L, R, n, rate }) {
  const data = Buffer.alloc(n * 4);
  for (let i = 0; i < n; i++) { data.writeInt16LE(L[i], i * 4); data.writeInt16LE(R[i], i * 4 + 2); }
  const h = Buffer.alloc(44);
  h.write('RIFF', 0); h.writeUInt32LE(36 + data.length, 4); h.write('WAVE', 8);
  h.write('fmt ', 12); h.writeUInt32LE(16, 16); h.writeUInt16LE(1, 20); h.writeUInt16LE(2, 22);
  h.writeUInt32LE(rate, 24); h.writeUInt32LE(rate * 4, 28); h.writeUInt16LE(4, 32); h.writeUInt16LE(16, 34);
  h.write('data', 36); h.writeUInt32LE(data.length, 40);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, Buffer.concat([h, data]));
}
