// Castcorder browser-recorder spike: AudioWorklet tap for the raw recording path.
// Unavoidable JS: an AudioWorkletProcessor must be registered from a JS module.
// It does no processing. It copies each 128-frame render quantum, interleaved,
// to the storage worker, either through a SharedArrayBuffer ring ("sab") or by
// transferring batched Float32Arrays over a MessagePort ("msg").
// Ring layout (Int32 header, 16 slots): 0 write frames, 1 read frames,
// 2 overrun-dropped frames, 3 capacity frames, 4 ended flag, 5 silent quanta.
const HDR = 16;
class Recorder extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = options.processorOptions;
    this.ch = o.channels; this.transport = o.transport;
    this.batch = o.batchFrames || 1024; this.on = false; this.done = false;
    this.frames = 0; this.silent = 0; this.out = null; this.fill = 0;
    if (o.sab) {
      this.hdr = new Int32Array(o.sab, 0, HDR);
      this.ring = new Float32Array(o.sab, HDR * 4);
      this.cap = this.hdr[3];
    }
    this.port.onmessage = (e) => {
      const m = e.data;
      if (m.cmd === 'port') this.data = m.port;
      else if (m.cmd === 'start') { this.on = true; this.startFrame = currentFrame;
        this.port.postMessage({ type: 'started', contextFrame: currentFrame, contextTime: currentTime }); }
      else if (m.cmd === 'stop') { this.on = false; this.finish(); }
    };
  }
  finish() {
    if (this.done) return; this.done = true;
    if (this.transport === 'sab') {
      Atomics.store(this.hdr, 4, 1);
    } else {
      if (this.fill > 0) this.post();
      this.data.postMessage({ end: true, frames: this.frames, silent: this.silent });
    }
  }
  post() {
    const d = this.fill === this.batch ? this.out : this.out.slice(0, this.fill * this.ch);
    this.data.postMessage({ seq: this.frames - this.fill, data: d, silent: this.silent }, [d.buffer]);
    this.out = null; this.fill = 0;
  }
  process(inputs) {
    if (!this.on) return !this.done;
    const inp = inputs[0], ch = this.ch, n = 128;
    if (!inp || inp.length === 0) this.silent++;
    if (this.transport === 'sab') {
      const w = Atomics.load(this.hdr, 0), r = Atomics.load(this.hdr, 1);
      if (this.cap - (w - r) < n) { Atomics.add(this.hdr, 2, n); return true; }
      let p = (w % this.cap) * ch;
      for (let i = 0; i < n; i++) {
        for (let c = 0; c < ch; c++) this.ring[p++] = inp && inp[c] ? inp[c][i] : 0;
        if (p >= this.cap * ch) p = 0;
      }
      Atomics.store(this.hdr, 5, this.silent);
      Atomics.store(this.hdr, 0, w + n);
    } else {
      if (!this.out) this.out = new Float32Array(this.batch * ch);
      let p = this.fill * ch;
      for (let i = 0; i < n; i++)
        for (let c = 0; c < ch; c++) this.out[p++] = inp && inp[c] ? inp[c][i] : 0;
      this.fill += n; this.frames += n;
      if (this.fill === this.batch) this.post();
    }
    return true;
  }
}
registerProcessor('castcorder-recorder', Recorder);
