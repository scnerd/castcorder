// Castcorder capture tap: the recording fallback where MediaStreamTrackProcessor
// is missing (Firefox, Safari). Unavoidable JS: an AudioWorkletProcessor must be
// registered from a JS module. It does no processing: it copies each render
// quantum into planar batches (plane c at [c * BATCH, (c + 1) * BATCH)) and
// transfers each full batch to the storage worker over a MessagePort, with
// seq (frames before this batch), frame (currentFrame at the batch end) and
// silent (render quanta that had no input, i.e. zeros the graph inserted).
// Ported from spikes/browser-recorder/js/recorder-worklet.js, "msg" transport.
const BATCH = 1024;
class Tap extends AudioWorkletProcessor {
  constructor(options) {
    super();
    this.ch = options.processorOptions.channels;
    this.seq = 0; this.fill = 0; this.silent = 0; this.out = null; this.data = null; this.done = false;
    this.port.onmessage = (e) => {
      if (e.data.cmd === 'port') this.data = e.data.port;
      else if (e.data.cmd === 'stop') { this.done = true; this.data = null; }
    };
  }
  process(inputs) {
    if (this.done) return false;
    if (!this.data) return true;
    const inp = inputs[0], ch = this.ch;
    const n = inp && inp[0] ? inp[0].length : 128;
    if (!inp || inp.length === 0) this.silent++;
    if (!this.out) this.out = new Float32Array(BATCH * ch);
    for (let c = 0; c < ch; c++) {
      const at = c * BATCH + this.fill;
      if (inp && inp[c]) this.out.set(inp[c], at); else this.out.fill(0, at, at + n);
    }
    this.fill += n;
    if (this.fill >= BATCH) {
      const d = this.out;
      this.data.postMessage({ seq: this.seq, frames: this.fill, data: d, silent: this.silent, frame: currentFrame + n }, [d.buffer]);
      this.seq += this.fill; this.fill = 0; this.out = null;
    }
    return true;
  }
}
registerProcessor('castcorder-capture', Tap);
