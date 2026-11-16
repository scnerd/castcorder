import { spawn } from 'node:child_process';
import { existsSync, readdirSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check, callDirs } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// Rooms and calls (cast-brj), chromium and firefox, on a fourth host whose
// default room is producer-gated:
// - the host machine's home (its page with no query) lists the rooms; a new,
//   empty room is made there and its call link copied;
// - a guest opening that room's link before anyone opened it waits in the
//   lobby; the producer joins the room from the home and the guest gets in;
// - everyone leaves and rejoins: the same call (same folder, take numbers
//   continue);
// - a producer starts a new call from the call: a new folder, takes from 01,
//   and the guest's next take lands there; the home lists both calls.
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
  const take = async (n) => {
    await waitFor(async () => P.isEnabled('#take-start'), 'P can start a take', 15000);
    await P.click('#take-start');
    await waitFor(async () => (await text(P, '#take-result')).includes('"take":' + n), 'take ' + n + ' started', 10000);
    await sleep(2500);
    await P.click('#take-stop');
    await waitFor(async () => (await text(P, '#take-result')).includes('"take":' + n) && (await text(P, '#take-status')).startsWith('idle'), 'take ' + n + ' stopped', 10000);
  };
  await take(1);
  let dirs = callDirs(LIB, ROOM);
  check(dirs.length === 1 && dirs[0].endsWith(first), 'the call has one folder: ' + dirs.join(' '));
  await waitFor(async () => synced(dirs[0], 'take01', 2), 'take 1 synced into the call folder', 40000);

  // Everyone leaves and rejoins: the same call, take numbers continue.
  for (const p of [P, G]) await p.click('#leave');
  await waitFor(async () => !(await live()), 'the room empties', 20000);
  await P.click('#join');
  await G.click('#join');
  await waitFor(async () => (await text(G, '#state')) === 'connected' && (await text(P, '#state')) === 'connected', 'both rejoin', 40000);
  await waitFor(async () => (await text(P, '#current-call')) === first, 'the rejoin lands in the same call', 15000);
  check(true, 'everyone left and rejoined: the same call ' + first);
  await take(2);
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
}
