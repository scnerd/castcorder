import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check, callDirs, opfsNames, readOpfs } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// Rooms and calls (cast-brj), chromium and firefox, on a fourth host whose
// default room is producer-gated:
// - the host machine's home (its page with no query) lists the rooms; a new,
//   empty room is made there and its call link copied;
// - a guest opening that room's link before anyone opened it waits in the
//   lobby; the producer joins the room from the home and the guest gets in;
// - everyone leaves and rejoins: the same call (same folder, take numbers
//   continue); the guest reloads and joins mid-take, its first take status
//   a recording one, and its track still syncs (cast-83e);
// - a producer starts a new call from the call: a new folder, takes from 01,
//   and the guest's next take lands there; the home lists both calls;
// - a guest leaves mid-take and joins a later call: its take syncs into the
//   call it was recorded in and leaves OPFS; a track for a call it was never
//   in is refused and stays on the device (cast-6qm);
// - a guest leaves mid-take and stays on the page: once another room's call
//   is live, its take syncs over a sync-only connection into the call it was
//   recorded in, the lobby showing progress and then what it kept; a guest
//   kicked mid-take is refused sync-only and keeps its take (cast-xsh).
export async function roomsChecks() {
  const { MODE, exe, dist, RESULTS } = ctx;
  const LIB = join(RESULTS, 'library-rooms-' + MODE);
  rmSync(LIB, { recursive: true, force: true });
  ctx.host7 = spawn(exe, ['--dist', dist, '--library', LIB, '--room', 'main-show', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host7Log = '';
  ctx.host7.stderr.on('data', (d) => { ctx.host7Log += d.toString(); });
  const [HTTP7, HTTPS7] = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('rooms host did not print HOST_READY')), 20000);
    ctx.host7.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
    });
  });
  const rooms = async () => {
    const r = await fetch(HTTP7 + '/rooms', { headers: { 'X-Castcorder-Native': '1' } });
    return (await r.json()).rooms;
  };
  const live = async () => (await rooms()).some((r) => r.live);
  const ROOM = 'Late-show';
  const synced = (dir, take, n) => {
    const d = join(dir, take);
    const wavs = existsSync(d) ? readdirSync(d).filter((f) => f.endsWith('_mic.wav')) : [];
    return wavs.length >= n && !readdirSync(d).some((f) => f.endsWith('.journal') || f.endsWith('.obao'));
  };

  // The home: the host machine's own page, no query.
  const P = await openGuest('P', [], HTTP7, undefined, 'Pat');
  await P.goto(HTTP7 + '/');
  await waitFor(async () => (await P.locator('#rooms li[data-room="main-show"]').count()) === 1, 'the home lists the default room', 15000);
  check((await P.locator('#join').count()) === 0, 'the home is not a lobby');
  await P.fill('#new-room-name', 'Late show');
  await P.click('#create-room');
  const card = P.locator('#rooms li[data-room="' + ROOM + '"]');
  await waitFor(async () => (await card.count()) === 1, 'the new room appears on the home', 10000);
  check((await card.locator('.call-row').count()) === 0 && (await text(P, '#rooms li[data-room="' + ROOM + '"] .next-call')).includes('starts a new call'),
    'a new room has no calls yet; joining starts one');
  const link = (await card.locator('.room-link').textContent()).trim();
  check(link.startsWith(HTTPS7) && link.endsWith('?room=' + ROOM), 'the room\x27s LAN call link names the room: ' + link);
  await card.locator('.copy-room-link').click();
  await waitFor(async () => (await card.locator('.copy-room-link').textContent()).includes('Copied'), 'Copy says copied', 5000);
  if (MODE !== 'firefox') {
    check((await P.evaluate(() => navigator.clipboard.readText())) === link, 'Copy put the room\x27s link on the clipboard');
  }

  // A guest opens the link while the room is not live: the waiting lobby.
  const G = await openGuest('G', [], link.replace(/\/?\?room=.*$/, ''), undefined, 'Gil', '&room=' + ROOM);
  await G.click('#join');
  await waitFor(async () => (await text(G, '#state')).includes('waiting for the host or a producer'), 'G waits in the lobby', 20000);
  check(true, 'a guest opening a room that is not live waits in the lobby');
  check(!(await live()), 'nothing is live while the guest waits');

  // The producer joins the room from the home; the guest gets in.
  await card.locator('.join-room').click();
  await P.waitForSelector('#join');
  check((await text(P, '#lobby-room')) === ROOM, 'Join on the home opens that room\x27s lobby');
  await P.click('#join');
  await waitFor(async () => (await text(G, '#state')) === 'connected', 'G gets in once the producer opened the room', 40000);
  await waitFor(async () => (await text(P, '#current-call')).length > 0, 'the producer sees which call this is', 15000);
  const first = await text(P, '#current-call');
  const startTake = async (n) => {
    await waitFor(async () => P.isEnabled('#take-start'), 'P can start a take', 15000);
    await P.click('#take-start');
    await waitFor(async () => (await text(P, '#take-result')).includes('"take":' + n), 'take ' + n + ' started', 10000);
  };
  const stopTake = async (n) => {
    await sleep(2500);
    await P.click('#take-stop');
    await waitFor(async () => (await text(P, '#take-result')).includes('"take":' + n) && (await text(P, '#take-status')).startsWith('idle'), 'take ' + n + ' stopped', 10000);
  };
  const take = async (n) => {
    await startTake(n);
    await stopTake(n);
  };
  await take(1);
  let dirs = callDirs(LIB, ROOM);
  check(dirs.length === 1 && dirs[0].endsWith(first), 'the call has one folder: ' + dirs.join(' '));
  await waitFor(async () => synced(dirs[0], 'take01', 2), 'take 1 synced into the call folder', 40000);
  // Everyone leaves and rejoins: the same call, take numbers continue.
  for (const p of [P, G]) await p.click('#leave');
  await waitFor(async () => !(await live()), 'the room empties', 20000);
  // P is back first and starts take 2. G reloads (a fresh page and storage
  // worker) and joins at once, mid-take: the first take status it sees is
  // Recording, never Idle. Its track must still be offered once the take
  // stops (cast-83e).
  await P.click('#join');
  await waitFor(async () => (await text(P, '#state')) === 'connected', 'P rejoins', 40000);
  await waitFor(async () => (await text(P, '#current-call')) === first, 'the rejoin lands in the same call', 15000);
  await startTake(2);
  await G.reload();
  await G.click('#join');
  await waitFor(async () => (await text(G, '#state')) === 'connected', 'G rejoins mid-take', 40000);
  await waitFor(async () => (await text(G, '#recording')).includes('recording take'), 'G records the take it joined', 15000);
  check(true, 'everyone left and rejoined: the same call ' + first + '; G joined mid-take and records it');
  await stopTake(2);
  dirs = callDirs(LIB, ROOM);
  await waitFor(async () => synced(dirs[0], 'take02', 2), 'take 2 synced into the same folder', 40000);
  check(dirs.length === 1, 'still one call folder, take numbering continued');

  // A producer starts a new call: a new folder, takes from 01.
  // (Refused while a device is still syncing: asked again.)
  await P.click('#new-call');
  await waitFor(async () => {
    const c = await text(P, '#current-call');
    if (c.length > 0 && c !== first) return true;
    if ((await text(P, '#take-result')).startsWith('refused')) await P.click('#new-call');
    return false;
  }, 'P is moved to a new call', 30000, 1000);
  const second = await text(P, '#current-call');
  await take(1);
  dirs = callDirs(LIB, ROOM);
  check(dirs.length === 2 && dirs[1].endsWith(second), 'the new call has its own folder: ' + dirs.join(' '));
  await waitFor(async () => synced(dirs[1], 'take01', 2), 'the guest\x27s take of the new call lands in its folder', 40000);
  check(!existsSync(join(dirs[0], 'take03')), 'the old call got no take of the new one');
  for (const p of [P, G]) await p.click('#leave');
  await waitFor(async () => !(await live()), 'the room empties again', 20000);

  // The home lists both calls with their takes, the newest first.
  await P.goto(HTTP7 + '/');
  const rows = P.locator('#rooms li[data-room="' + ROOM + '"] .call-row');
  await waitFor(async () => (await rows.count()) === 2, 'the home lists both calls', 10000);
  const takes = await rows.locator('.call-takes').allTextContents();
  check(takes[0] === '1 take' && takes[1] === '2 takes', 'each call shows its takes: ' + takes.join(', '));
  check((await rows.first().getAttribute('data-call')) === second && (await rows.first().getAttribute('class')).includes('next'),
    'the newest call is where the next join lands');
  // Continue the first call from the home: it becomes the next join's.
  await rows.nth(1).locator('.continue-call').click();
  await waitFor(async () => (await rooms()).find((r) => r.name === ROOM).current === first, 'Continue makes the old call the current one', 10000);
  check(true, 'Continue this call on the home picks the call the next join lands in');

  // Every call's tracks sync whenever the host is connected (cast-6qm): the
  // guest leaves mid-take, so its take 3 of the first call never syncs; the
  // producer moves on to a new call and the guest joins that one. Its take
  // 3 lands in the first call's folder and leaves OPFS; a track it claims
  // for a call it was never in (made on disk here) is refused and kept.
  const FOREIGN = '2001-01-01';
  const foreignDir = join(LIB, ROOM, FOREIGN);
  mkdirSync(foreignDir, { recursive: true });
  writeFileSync(join(foreignDir, 'call.json'), JSON.stringify({ started_ms: 978307200000, last_active_ms: 978307200000 }));
  await card.locator('.join-room').click();
  await P.waitForSelector('#join');
  await P.click('#join');
  await G.click('#join');
  await waitFor(async () => (await text(G, '#state')) === 'connected' && (await text(P, '#state')) === 'connected', 'both join the continued call', 40000);
  await waitFor(async () => (await text(P, '#current-call')) === first, 'the join lands in the continued call', 15000);
  await waitFor(async () => P.isEnabled('#take-start'), 'P can start take 3', 15000);
  await P.click('#take-start');
  await waitFor(async () => (await text(P, '#take-result')).includes('"take":3'), 'take 3 started', 10000);
  await sleep(2500);
  // Held: this leave must not sync it sync-only (cast-xsh); a later join does.
  await G.evaluate(() => { window.__castcorderHoldSync = true; });
  await G.click('#leave');
  await sleep(500);
  await P.click('#take-stop');
  await waitFor(async () => (await text(P, '#take-status')).startsWith('idle'), 'take 3 stopped', 10000);
  const pending = await waitFor(async () => {
    const n = (await opfsNames(G)).filter((f) => f.startsWith('take0003') && f.endsWith('.json'));
    return n.length > 0 ? n : null;
  }, 'G kept its take 3 track', 15000);
  const meta = JSON.parse((await readOpfs(G, 'tracks', pending[0])).toString('utf8'));
  check(meta.finalised && meta.call.endsWith('/' + ROOM + '/' + first), 'G\x27s take 3 is finalised and belongs to the first call: ' + meta.call);
  check(!synced(dirs[0], 'take03', 2), 'G\x27s take 3 is not on the host yet');
  // A track for a call G was never in.
  await G.evaluate(async (m) => {
    const d = await (await navigator.storage.getDirectory()).getDirectoryHandle('tracks', { create: true });
    for (const [name, data] of [['foreign-take0001.wav', 'RIFF-not-a-real-wav-' + 'x'.repeat(5000)],
      ['foreign-take0001.json', JSON.stringify(m)]]) {
      const w = await (await d.getFileHandle(name, { create: true })).createWritable();
      await w.write(data);
      await w.close();
    }
  }, { ...meta, name: 'foreign-take0001', file: 'foreign-take0001.wav', take: 1,
    call: meta.call.replace(/[^/]+$/, FOREIGN) });
  // What a cut connection leaves behind (cast-22g): a track whose wav was
  // delivered but whose sidecar and timing were not, and one whose timing
  // alone was not. No wav beside them: they must still be offered.
  await G.evaluate(async (m) => {
    const d = await (await navigator.storage.getDirectory()).getDirectoryHandle('tracks', { create: true });
    const header = (track, source) => JSON.stringify({ type: 'header', version: 1, call: m.call, device: m.device,
      take: m.take, track, source, sample_rate: 48000, channels: 1, device_clock: 'test', host_clock: 'test' }) + '\n';
    for (const [name, data] of [
      ['leftover-take0003.json', JSON.stringify({ ...m, name: 'leftover-take0003', file: 'leftover-take0003.wav', source: 'leftover' })],
      ['leftover-take0003.timing.jsonl', header('leftover-take0003', 'leftover')],
      ['lone-take0003.timing.jsonl', header('lone-take0003', 'lone')]]) {
      const w = await (await d.getFileHandle(name, { create: true })).createWritable();
      await w.write(data);
      await w.close();
    }
  }, meta);
  // The producer moves on to a new call (G is gone, so nothing holds it).
  await P.click('#new-call');
  await waitFor(async () => {
    const c = await text(P, '#current-call');
    if (c.length > 0 && c !== first && c !== second) return true;
    if ((await text(P, '#take-result')).startsWith('refused')) await P.click('#new-call');
    return false;
  }, 'P is moved to a third call', 30000, 1000);
  const third = await text(P, '#current-call');
  await G.click('#join');
  await waitFor(async () => (await text(G, '#state')) === 'connected', 'G joins the third call', 40000);
  await waitFor(async () => synced(dirs[0], 'take03', 2), 'G\x27s take 3 lands in the first call\x27s folder while G is in the third', 60000);
  const m3 = readFileSync(join(dirs[0], 'take03', 'manifest.jsonl'), 'utf8');
  check(m3.split('\n').some((l) => l.startsWith('{"Verified"') && l.includes(meta.device)),
    'the first call\x27s manifest has G\x27s take 3 verified');
  const left = await waitFor(async () => {
    const n = await opfsNames(G);
    return n.some((f) => f.startsWith('take0003')) ? null : n;
  }, 'G\x27s take 3 left OPFS after verified receipt', 30000);
  check(true, 'G\x27s take 3 of call ' + first + ' synced from call ' + third + ' and left the device');
  const leftovers = await waitFor(async () => {
    const d = join(dirs[0], 'take03');
    const f = readdirSync(d);
    const done = [/_leftover\.json$/, /_leftover\.timing\.jsonl$/, /_lone\.timing\.jsonl$/].every((r) => f.some((x) => r.test(x)))
      && !f.some((x) => x.endsWith('.journal') || x.endsWith('.obao'));
    const n = await opfsNames(G);
    return done && !n.some((x) => x.startsWith('leftover-') || x.startsWith('lone-')) ? f : null;
  }, 'the sidecar and timing a cut connection left behind land and leave the device (cast-22g)', 30000);
  check(leftovers.length > 0, 'what was left of a delivered track was offered again and landed');
  await waitFor(async () => /track for another call refused/.test(ctx.host7Log), 'the host refused the foreign track', 15000);
  check(left.includes('foreign-take0001.wav') && left.includes('foreign-take0001.json'),
    'a track for a call G was never in stays on the device: ' + left.join(' '));
  check(!existsSync(join(foreignDir, 'take01')), 'nothing was written into a call G was never in');
  check(!existsSync(join(callDirs(LIB, ROOM).find((d) => d.endsWith(third)), 'take03')), 'the third call got no take 3');
  // Background sync (cast-xsh): G records take 1 of the third call and
  // leaves mid-take; its page keeps the track and shows it waiting (held
  // by the test hook). P leaves and opens the other room, main-show. G's
  // page then delivers the take over a sync-only connection into the third
  // call while main-show is live, and says what it kept (the foreign track).
  const thirdDir = callDirs(LIB, ROOM).find((d) => d.endsWith(third));
  const syncState = () => text(G, '#sync-state');
  await waitFor(async () => P.isEnabled('#take-start'), 'P can start take 1 of the third call', 15000);
  await P.click('#take-start');
  await waitFor(async () => (await text(P, '#take-result')).includes('"take":1'), 'take 1 of the third call started', 10000);
  await sleep(2500);
  await G.evaluate(() => { window.__castcorderHoldSync = true; });
  await G.click('#leave');
  await sleep(500);
  await P.click('#take-stop');
  await waitFor(async () => (await text(P, '#take-status')).startsWith('idle'), 'take 1 of the third call stopped', 10000);
  await waitFor(async () => /file\(s\) left/.test(await syncState()), 'G shows its recordings waiting to sync', 30000);
  check(!synced(thirdDir, 'take01', 2), 'G\x27s take is not on the host while held: ' + (await syncState()));
  check((await opfsNames(G)).some((f) => f.startsWith('take0001')), 'G kept its take 1 after Leave');
  await P.click('#leave');
  await waitFor(async () => !(await live()), 'the late show empties', 20000);
  await P.goto(HTTP7 + '/?room=main-show');
  await P.click('#join');
  await waitFor(async () => (await rooms()).find((r) => r.name === 'main-show').live, 'main-show is live', 40000);
  const accepted = () => (ctx.host7Log.match(/sync-only connection accepted/g) || []).length;
  const before = accepted();
  await G.evaluate(() => { window.__castcorderHoldSync = false; });
  await waitFor(async () => synced(thirdDir, 'take01', 2), 'G\x27s take lands in the third call over a sync-only connection', 60000);
  const now = await rooms();
  check(accepted() > before && now.find((r) => r.name === 'main-show').live && !now.find((r) => r.name === ROOM).live,
    'the host accepted the sync-only connection beside main-show\x27s live call; the late show stayed closed');
  await waitFor(async () => !(await opfsNames(G)).some((f) => f.startsWith('take0001')), 'G\x27s take left OPFS', 30000);
  await waitFor(async () => (await syncState()).includes('could not be delivered'), 'G says what it kept', 30000);
  check((await opfsNames(G)).includes('foreign-take0001.wav'), 'the refused foreign track stays on G: ' + (await syncState()));
  check(true, 'a guest that left synced over a sync-only connection, beside another room\x27s live call');

  // Kicked: G joins main-show, records, and is kicked mid-take; its
  // sync-only connection is refused and the take stays on the device.
  await G.goto(G.url().replace('room=' + ROOM, 'room=main-show'));
  await G.evaluate(() => { window.__castcorderHoldSync = true; });
  await G.click('#join');
  await waitFor(async () => (await text(G, '#state')) === 'connected', 'G joins main-show', 40000);
  await waitFor(async () => P.isEnabled('#take-start'), 'P can start a main-show take', 15000);
  await P.click('#take-start');
  await waitFor(async () => (await text(P, '#take-result')).includes('"take":'), 'a main-show take started', 10000);
  await sleep(2500);
  await waitFor(async () => (await text(P, '#guests')).includes('Gil'), 'P lists G', 10000);
  await P.click('#guests li[data-name="Gil"] button.kick');
  await waitFor(async () => (await text(G, '#state')) === 'removed from the call by a producer', 'G removed', 30000);
  await P.click('#take-stop');
  await waitFor(async () => (await text(P, '#take-status')).startsWith('idle'), 'the main-show take stopped', 10000);
  const kept = await waitFor(async () => {
    const n = (await opfsNames(G)).filter((f) => f.startsWith('take0001') && f.endsWith('.json'));
    return n.length > 0 ? n : null;
  }, 'G kept its main-show take', 15000);
  await G.evaluate(() => { window.__castcorderHoldSync = false; });
  await waitFor(async () => (await syncState()).includes('did not take your recordings'), 'G\x27s sync-only connection is refused', 30000);
  check((await opfsNames(G)).includes(kept[0]), 'a kicked device keeps its take: ' + (await syncState()));
  await P.click('#leave');
  await waitFor(async () => !(await live()), 'main-show empties', 20000);
}
