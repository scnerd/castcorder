import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

// The call folder of a room in a host library that was active last
// (<library>/<room>/<YYYY-MM-DD>[-N], holding call.json); the room folder
// itself if it has no call yet.
export function callDir(library, room) {
  const dir = join(library, room);
  if (!existsSync(dir)) return join(dir, 'no-call-yet');
  let best = null;
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const meta = join(dir, e.name, 'call.json');
    if (!e.isDirectory() || !existsSync(meta)) continue;
    const m = JSON.parse(readFileSync(meta, 'utf8'));
    const key = [m.last_active_ms, m.started_ms, e.name];
    if (!best || key[0] > best[0][0] || (key[0] === best[0][0] && (key[1] > best[0][1] || (key[1] === best[0][1] && key[2] > best[0][2])))) best = [key, e.name];
  }
  return best ? join(dir, best[1]) : join(dir, 'no-call-yet');
}

// Every call folder of a room, oldest first by start.
export function callDirs(library, room) {
  const dir = join(library, room);
  if (!existsSync(dir)) return [];
  return readdirSync(dir, { withFileTypes: true })
    .filter((e) => e.isDirectory() && existsSync(join(dir, e.name, 'call.json')))
    .map((e) => [JSON.parse(readFileSync(join(dir, e.name, 'call.json'), 'utf8')).started_ms, e.name])
    .sort((a, b) => a[0] - b[0] || (a[1] < b[1] ? -1 : 1))
    .map(([, n]) => join(dir, n));
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export async function waitFor(fn, what, timeout = 30000, interval = 200) {
  const t0 = Date.now();
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() - t0 > timeout) throw new Error('timeout waiting for ' + what);
    await sleep(interval);
  }
}

export const text = async (page, sel) => (await page.textContent(sel)) || '';
export const buttons = async (page) => ({
  start: await page.isEnabled('#take-start'), stop: await page.isEnabled('#take-stop'),
});
export const json = async (page, sel) => { const t = await text(page, sel); return t ? JSON.parse(t) : null; };
export async function noErrors(page, name) {
  const e = await text(page, '#errors');
  if (e) throw new Error('guest ' + name + ' errors: ' + e);
}

export async function readOpfs(page, dir, name) {
  const parts = [];
  let off = 0, size = Infinity;
  const CHUNK = 4 * 1024 * 1024;
  while (off < size) {
    const r = await page.evaluate(async ([dir, name, off, len]) => {
      const root = await navigator.storage.getDirectory();
      const d = await root.getDirectoryHandle(dir);
      const f = await (await d.getFileHandle(name)).getFile();
      const b = new Uint8Array(await f.slice(off, off + len).arrayBuffer());
      let s = '';
      for (let i = 0; i < b.length; i += 8192) s += String.fromCharCode.apply(null, b.subarray(i, i + 8192));
      return { size: f.size, b64: btoa(s) };
    }, [dir, name, off, CHUNK]);
    size = r.size;
    const buf = Buffer.from(r.b64, 'base64');
    parts.push(buf);
    off += buf.length;
    if (buf.length === 0) break;
  }
  return Buffer.concat(parts);
}

export function audioFrom(inbound, origin) {
  return (inbound || []).filter((i) => i.kind === 'audio' && i.origin.startsWith(origin + '-'))
    .reduce((a, i) => ({ packets: a.packets + i.packetsReceived, bytes: a.bytes + i.bytesReceived }), { packets: 0, bytes: 0 });
}

export const checks = [];
export function check(ok, what) {
  checks.push({ ok, what });
  console.log((ok ? 'ok   ' : 'BAD  ') + what);
}

export const opfsNames = (p) => p.evaluate(async () => {
  const root = await navigator.storage.getDirectory();
  const d = await root.getDirectoryHandle('tracks', { create: true });
  const out = [];
  for await (const k of d.keys()) out.push(k);
  return out;
});

// Video decoded from `origin`'s forwarded camera: frames (summed), size.
export function videoFrom(inbound, origin) {
  return (inbound || []).filter((i) => i.kind === 'video' && i.origin.startsWith(origin + '-'))
    .reduce((a, i) => ({ frames: a.frames + (i.framesDecoded || 0), w: Math.max(a.w, i.frameWidth || 0), h: Math.max(a.h, i.frameHeight || 0) }),
      { frames: 0, w: 0, h: 0 });
}

// Wait until `page` decodes new frames from `origin` over a 2 s window.
export function decoding(page, origin, what, timeout = 30000) {
  return waitFor(async () => {
    const x = videoFrom(await json(page, '#debug-inbound'), origin);
    await sleep(2000);
    const y = videoFrom(await json(page, '#debug-inbound'), origin);
    return y.frames > x.frames ? { fps: (y.frames - x.frames) / 2, w: y.w, h: y.h } : null;
  }, what, timeout, 0);
}

// Audio packets from `origin` rise over a 2 s window.
export async function audioRising(page, origin) {
  const x = audioFrom(await json(page, '#debug-inbound'), origin);
  await sleep(2000);
  const y = audioFrom(await json(page, '#debug-inbound'), origin);
  return y.packets - x.packets;
}


// A timing metadata file (castcorder-core timing): JSON lines.
export function parseTiming(text) {
  const t = { header: null, segments: 0, refs: [], sync: [], end: null, lastEstimate: null };
  for (const l of text.split('\n').filter((x) => x.length > 0)) {
    const v = JSON.parse(l);
    if (v.type === 'header') t.header = v;
    else if (v.type === 'segment') t.segments++;
    else if (v.type === 'ref') {
      t.refs.push(v);
      if (v.offset_us !== undefined) t.lastEstimate = { offset_us: v.offset_us, drift_ppm: v.drift_ppm, rtt_us: v.rtt_us };
    } else if (v.type === 'sync') t.sync.push(v);
    else if (v.type === 'end') t.end = v.frames;
  }
  t.hostUsAt = (frame) => {
    const r = t.refs.filter((x) => typeof x.host_us === 'number');
    if (r.length === 0) return NaN;
    return r[0].host_us + (frame - r[0].frame) / t.header.sample_rate * 1e6;
  };
  return t;
}

// Ground truth for timing metadata: the fake microphone plays the
// deterministic input on its own continuous clock, so a track's content
// (analyzeChannel `c`: which input frame its first aligned frame holds) and
// its timing metadata `t` together say when, on the host timeline, the input
// was at frame 0 (modulo the input's loop). Correct metadata gives every
// track of the same running input the same answer.
export function inputZeroHostUs(t, c, rate) {
  if (c.firstAlignedFrame === null || c.firstInputFrame === null) return NaN;
  return t.hostUsAt(c.firstAlignedFrame) - c.firstInputFrame / rate * 1e6;
}

// a - b in us, modulo a loop of `loopUs`, in [-loopUs/2, loopUs/2).
export function loopDiffUs(a, b, loopUs) {
  const d = ((a - b) % loopUs + loopUs) % loopUs;
  return d >= loopUs / 2 ? d - loopUs : d;
}

// The People panel (cast-n56.6): a person's action sits in their row's menu.
export async function personAction(page, name, cls) {
  const row = page.locator('#guests li[data-name="' + name + '"]');
  await row.locator('.row-menu').click();
  await row.locator('button.' + cls).click();
}

// Open the People panel's folded sections (Room settings, Calls in this
// room, Connection details) before using a control inside them.
export const unfold = (page) => page.evaluate(() => { for (const d of document.querySelectorAll('details.fold')) d.open = true; });
