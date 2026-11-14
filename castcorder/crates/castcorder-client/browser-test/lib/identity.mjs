import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { waitFor, text, check } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// Device identity (cast-p2f), chromium and firefox modes: a third, always-open
// host whose room.json already remembers a pre-key device (a UUID, trusted).
// - M is that profile after the upgrade: its localStorage still holds the
//   UUID and its OPFS a finished take-1 track recorded under the UUID (seeded
//   in the layout the outbox reads). It joins with its new key: the host binds
//   the UUID to the key once (the record, trust included, moves), and the
//   pending track syncs under the UUID and leaves OPFS.
// - N presents the same UUID with another key: the binding is refused and N
//   enters as a new, untrusted device.
// - An impostor that knows M's key-derived id is refused: unsigned, or signed
//   by its own key; with M's id in its old-id slot it takes over nothing.
export async function identityChecks() {
  const { MODE, exe, dist, RESULTS, host, logs, firefox } = ctx;
  const LIB3 = join(RESULTS, 'library-identity-' + MODE);
  rmSync(LIB3, { recursive: true, force: true });
  const UUID = '0b6a3c2e-5f1d-4e8a-9c7b-2d4e6f8a1c3e';
  mkdirSync(join(LIB3, 'identity-test'), { recursive: true });
  writeFileSync(join(LIB3, 'identity-test', 'room.json'),
    JSON.stringify({ open: true, devices: { [UUID]: { name: 'M', trusted: true } } }));
  ctx.host3 = spawn(exe, ['--dist', dist, '--library', LIB3, '--call', 'identity-test', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host3.stderr.on('data', (d) => { ctx.host3Log += d.toString(); });
  const HTTPS3 = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('identity host did not print HOST_READY')), 20000);
    ctx.host3.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve(m[2]); }
    });
  });
  const joined = (p) => text(p, '#participant').then((s) => s.length > 0);
  const room = () => JSON.parse(readFileSync(join(LIB3, 'identity-test', 'room.json'), 'utf8'));
  const count = (re) => (ctx.host3Log.match(re) || []).length;
  const withOldId = (id) => async (ctx) => {
    await ctx.addInitScript((id) => {
      if (!localStorage.getItem('castcorder.device')) localStorage.setItem('castcorder.device', id);
    }, id);
  };

  // M: the upgraded profile, with a pending track from before the upgrade.
  // The track is seeded before the app first loads in M's context, as it
  // would be on disk after an upgrade: seeding into a running app races the
  // storage worker's startup scan, which opens each track's sidecar with a
  // sync access handle (Firefox, whose worker starts later, then refuses
  // createWritable with NoModificationAllowedError). The seed page is an
  // empty document on the app's origin (same OPFS), served by the test.
  const seedLegacyTrack = async (ctx) => {
    await withOldId(UUID)(ctx);
    const seed = await ctx.newPage();
    await seed.route(HTTPS3 + '/seed', (r) => r.fulfill({ contentType: 'text/html', body: '<!doctype html><title>seed</title>' }));
    await seed.goto(HTTPS3 + '/seed');
    await seedTrack(seed);
    await seed.close();
  };
  // Beside it (cast-dao): `torn-` is a track killed while its sidecar was
  // being rewritten (an empty sidecar, a header still counting 0 frames, the
  // timing metadata's header intact): the scan rebuilds the sidecar from the
  // timing header and the track must sync like any other. `orphan-` is audio
  // with no sidecar at all: the scan must not create one and must keep the
  // audio.
  const seedTrack = (page) => page.evaluate(async ([uuid, call]) => {
    const frames = 4800, ch = 1, rate = 48000;
    const wav = (freq, headerFrames) => {
      const buf = new ArrayBuffer(58 + frames * ch * 4);
      const v = new DataView(buf);
      const str = (o, s) => { for (let i = 0; i < s.length; i++) v.setUint8(o + i, s.charCodeAt(i)); };
      str(0, 'RIFF'); v.setUint32(4, 50 + headerFrames * ch * 4, true); str(8, 'WAVE'); str(12, 'fmt ');
      v.setUint32(16, 18, true); v.setUint16(20, 3, true); v.setUint16(22, ch, true); v.setUint32(24, rate, true);
      v.setUint32(28, rate * ch * 4, true); v.setUint16(32, ch * 4, true); v.setUint16(34, 32, true); v.setUint16(36, 0, true);
      str(38, 'fact'); v.setUint32(42, 4, true); v.setUint32(46, headerFrames, true); str(50, 'data'); v.setUint32(54, headerFrames * ch * 4, true);
      for (let i = 0; i < frames; i++) v.setFloat32(58 + i * 4, 0.25 * Math.sin(i / freq), true);
      return buf;
    };
    const meta = { name: 'legacy-take0001', file: 'legacy-take0001.wav', take: 1, device: uuid, displayName: 'M', call,
      source: 'mic', finalised: true, frames, sampleRate: rate, channels: ch };
    const timingHeader = { type: 'header', version: 1, call, device: uuid, take: 1, track: 'torn-take0001', source: 'mic',
      sample_rate: rate, channels: ch, device_clock: 'test', host_clock: 'test', host_timeline: 0 };
    const root = await navigator.storage.getDirectory();
    const d = await root.getDirectoryHandle('tracks', { create: true });
    for (const [name, data] of [['legacy-take0001.wav', wav(10, frames)], ['legacy-take0001.json', JSON.stringify(meta)],
      ['torn-take0001.wav', wav(7, 0)], ['torn-take0001.json', ''],
      ['torn-take0001.timing.jsonl', JSON.stringify(timingHeader) + '\n{"type":"segment","segment":1,"file":"torn-take0001.wav"}\n'],
      ['orphan-take0001.wav', wav(13, frames)]]) {
      const w = await (await d.getFileHandle(name, { create: true })).createWritable();
      await w.write(data);
      await w.close();
    }
  }, [UUID, HTTPS3 + '/offer']);
  const M = await openGuest('M', [], HTTPS3, seedLegacyTrack);
  await M.click('#join');
  await waitFor(() => joined(M), 'M joined with its key', 30000);
  await waitFor(async () => /device alias bound/.test(ctx.host3Log), 'host bound M\x27s old id', 10000);
  const r1 = room();
  const mId = r1.aliases && r1.aliases[UUID];
  check(/^k[0-9a-f]{40}$/.test(mId || '') && !r1.devices[UUID] && r1.devices[mId] && r1.devices[mId].trusted,
    'M joined with its key (' + mId + '); the host bound its old UUID once and moved its trusted record to the key id');
  const TAKE = join(LIB3, 'identity-test', 'take01');
  await waitFor(() => {
    const m = join(TAKE, 'manifest.jsonl');
    return existsSync(m) && readFileSync(m, 'utf8').split('\n').some((l) => l.startsWith('{"Verified"') && l.includes(UUID));
  }, 'M\x27s pre-upgrade track verified on the host', 60000, 250);
  const opfsLeft = await waitFor(async () => {
    const names = await M.evaluate(async () => {
      const d = await (await navigator.storage.getDirectory()).getDirectoryHandle('tracks', { create: true });
      const out = [];
      for await (const k of d.keys()) out.push(k);
      return out;
    });
    return names.some((n) => n.startsWith('legacy-') || n.startsWith('torn-')) ? null : names;
  }, 'M\x27s OPFS emptied of the old and the torn track', 30000);
  check(opfsLeft.every((n) => !n.startsWith('legacy-')),
    'M\x27s pending track from before the upgrade synced (kept under its old UUID in the manifest) and left OPFS');
  check(opfsLeft.every((n) => !n.startsWith('torn-')),
    'a track whose sidecar a crash left empty was recovered from its timing header, synced and left OPFS');
  check(opfsLeft.includes('orphan-take0001.wav') && !opfsLeft.includes('orphan-take0001.json'),
    'the startup scan created no sidecar for audio that has none, and kept the audio: ' + opfsLeft.join(' '));
  // The seed page's sync access handles are released asynchronously (Firefox
  // does it after the page is gone), so the first scan may list the track busy;
  // the worker re-lists busy tracks, and the listing settles (cast-9c9).
  const orphan = await waitFor(async () => {
    const listed = JSON.parse((await M.textContent('#debug-tracks')) || '[]');
    const t = listed.find((x) => x.name === 'orphan-take0001');
    return t && !t.busy ? t : null;
  }, 'the orphan track listed, no longer busy', 30000);
  check(orphan && orphan.noSidecar && !orphan.error && orphan.frames === 4800,
    'the audio without a sidecar is listed (noSidecar, 4800 frames): ' + JSON.stringify(orphan));

  // N: another profile presenting the same old UUID: no takeover.
  const refusedBefore = count(/device alias binding refused/g);
  const N = await openGuest('N', [], HTTPS3, withOldId(UUID));
  await N.click('#join');
  await waitFor(() => joined(N), 'N joined', 30000);
  const r2 = room();
  const nId = Object.keys(r2.devices).find((k) => r2.devices[k].name === 'N');
  check(count(/device alias binding refused/g) > refusedBefore && r2.aliases[UUID] === mId
    && nId && nId !== mId && !r2.devices[nId].trusted,
    'a second profile presenting the same old UUID is refused the binding and enters as a new, untrusted device');

  // The impostor knows M's id but not M's key.
  const I = await openGuest('I', [], HTTPS3, withOldId(mId));
  const statuses = await I.evaluate(async (mId) => {
    const post = async (body) => (await fetch('/offer?name=I', { method: 'POST', body: JSON.stringify(body) })).status;
    const sdp = 'v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n';
    const unsigned = await post({ type: 'offer', sdp, device: mId });
    // A correct signature (castcorder-link identity::offer_message, the
    // host id from GET /role as audience, no alias) by the impostor's own
    // key, claiming M's id.
    const host = (await (await fetch('/role')).json()).host;
    const pair = await crypto.subtle.generateKey({ name: 'Ed25519' }, false, ['sign', 'verify']);
    const key = new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey));
    const ts = Date.now();
    const enc = new TextEncoder();
    const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', enc.encode(sdp)));
    const parts = [enc.encode('castcorder-link device offer v1\0' + host + '\0'), key, enc.encode('\0' + ts + '\0'), hash];
    const msg = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    let o = 0;
    for (const p of parts) { msg.set(p, o); o += p.length; }
    const sig = new Uint8Array(await crypto.subtle.sign('Ed25519', pair.privateKey, msg));
    const b64 = (b) => btoa(String.fromCharCode(...b)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    const ownKey = await post({ type: 'offer', sdp, device: mId, proof: { key: b64(key), ts, sig: b64(sig) } });
    return { unsigned, ownKey };
  }, mId);
  check(statuses.unsigned === 403 && statuses.ownKey === 403,
    'an offer claiming M\x27s id without M\x27s key is refused (unsigned ' + statuses.unsigned + ', signed by another key ' + statuses.ownKey + ')');
  check(/offer not signed by a device key/.test(ctx.host3Log) && /claimed device id is not the fingerprint of the signing key/.test(ctx.host3Log),
    'the host logs why it refused the impostor');
  // Through the app, with M's id in its old-id slot: it joins only as itself.
  await I.click('#join');
  await waitFor(() => joined(I), 'I joined as itself', 30000);
  const r3 = room();
  const iId = Object.keys(r3.devices).find((k) => r3.devices[k].name === 'I');
  check(iId && iId !== mId && !r3.devices[iId].trusted && r3.devices[mId].name === 'M' && r3.devices[mId].trusted,
    'a page holding M\x27s id in its old-id slot takes over nothing: it enters as its own untrusted device');
  for (const p of [M, N, I]) await p.click('#leave').catch(() => {});
}
