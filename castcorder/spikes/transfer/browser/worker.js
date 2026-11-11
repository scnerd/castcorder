// Dedicated worker: OPFS FileSystemSyncAccessHandle is only available here.
function wrap(h) {
  return {
    read_at(buf, at) { return h.read(buf, { at }); },
    write_at(buf, at) { return h.write(buf, { at }); },
    size() { return h.getSize(); },
    truncate(n) { h.truncate(n); },
    flush() { h.flush(); },
  };
}

// Raw FileSystemSyncAccessHandle call costs, no wasm involved.
async function opfsMicro(dir) {
  const fh = await dir.getFileHandle('micro.bin', { create: true });
  const h = await fh.createSyncAccessHandle();
  const out = {};
  try {
    for (const [label, size, count, random] of [
      ['write_64B_random', 64, 2000, true],
      ['write_16KiB_seq', 16384, 2000, false],
      ['write_1MiB_seq', 1 << 20, 64, false],
      ['read_16KiB_seq', 16384, 2000, false],
      ['read_1MiB_seq', 1 << 20, 64, false],
    ]) {
      const buf = new Uint8Array(size);
      const isRead = label.startsWith('read');
      const t = performance.now();
      for (let i = 0; i < count; i++) {
        const at = random ? Math.floor(Math.random() * 1000) * 4096 : (i * size) % (64 << 20);
        if (isRead) h.read(buf, { at }); else h.write(buf, { at });
      }
      const ms = performance.now() - t;
      out[label] = { us_per_call: Math.round((ms * 1000) / count), mbps: Math.round((size * count) / 1e6 / (ms / 1000)) };
    }
    const t = performance.now();
    for (let i = 0; i < 20; i++) { h.write(new Uint8Array(64), { at: 0 }); h.flush(); }
    out.write_plus_flush_us = Math.round(((performance.now() - t) * 1000) / 20);
  } finally {
    h.close();
    await dir.removeEntry('micro.bin');
  }
  return out;
}

onmessage = async (ev) => {
  const { pkg, memMb, opfsMb, cuts } = ev.data;
  const result = { pkg, userAgent: navigator.userAgent };
  try {
    const mod = await import(`./${pkg}/castcorder_transfer_spike.js`);
    await mod.default();
    result.memory = JSON.parse(mod.memory_bench(memMb));
    const dir = await navigator.storage.getDirectory();
    result.opfs_micro = await opfsMicro(dir);
    const names = ['src.data', 'recv.data', 'recv.obao', 'recv.journal'];
    const handles = [];
    for (const n of names) {
      const fh = await dir.getFileHandle(n, { create: true });
      handles.push(await fh.createSyncAccessHandle());
    }
    try {
      const w = handles.map(wrap);
      result.opfs = JSON.parse(mod.opfs_transfer(w[0], w[1], w[2], w[3], opfsMb, cuts));
    } finally {
      for (const h of handles) h.close();
      for (const n of names) await dir.removeEntry(n);
    }
  } catch (e) {
    result.error = String(e && e.stack || e);
  }
  postMessage(result);
};
