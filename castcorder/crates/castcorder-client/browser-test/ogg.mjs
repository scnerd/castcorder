// A small Ogg Opus reader for the safety tracks: checks every page's CRC and
// the page sequence, and returns the packets and granule positions.

const crcTable = (() => {
  const t = new Uint32Array(256);
  for (let i = 0; i < 256; i++) {
    let r = (i << 24) >>> 0;
    for (let j = 0; j < 8; j++) r = ((r & 0x80000000) ? ((r << 1) ^ 0x04c11db7) : (r << 1)) >>> 0;
    t[i] = r;
  }
  return t;
})();

function crc(buf) {
  let c = 0;
  for (const b of buf) c = (((c << 8) >>> 0) ^ crcTable[((c >>> 24) ^ b) & 0xff]) >>> 0;
  return c;
}

export function parseOggOpus(buf) {
  const pages = [];
  let pos = 0;
  while (pos + 27 <= buf.length) {
    if (buf.toString('latin1', pos, pos + 4) !== 'OggS') throw new Error('no page at ' + pos);
    const nseg = buf[pos + 26];
    const lacing = buf.subarray(pos + 27, pos + 27 + nseg);
    const body = lacing.reduce((a, b) => a + b, 0);
    const total = 27 + nseg + body;
    if (pos + total > buf.length) break; // torn tail
    const page = Buffer.from(buf.subarray(pos, pos + total));
    const want = page.readUInt32LE(22);
    page.writeUInt32LE(0, 22);
    if (crc(page) !== want) throw new Error('bad CRC on page at ' + pos);
    pages.push({ flags: page[5], granule: Number(page.readBigUInt64LE(6)), seq: page.readUInt32LE(18), packets: lacing.length ? [...lacing].filter((l) => l < 255).length : 0 });
    pos += total;
  }
  pages.forEach((p, i) => { if (p.seq !== i) throw new Error('page sequence broken at ' + i); });
  if (pages.length < 2 || !buf.toString('latin1', 28, 36).startsWith('OpusHead')) throw new Error('not Ogg Opus');
  const audio = pages.slice(2);
  const last = audio[audio.length - 1];
  return {
    pages: audio.length,
    packets: audio.reduce((a, p) => a + p.packets, 0),
    seconds: last ? last.granule / 48000 : 0,
    finished: !!last && (last.flags & 4) !== 0,
  };
}
