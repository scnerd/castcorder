import { spawn } from 'node:child_process';
import { createServer } from 'node:net';
import { createSocket } from 'node:dgram';
import { existsSync, readdirSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check, callDirs, opfsNames } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// A guest's finished take is delivered later (cast-u9z, cast-7bc). The host restarts while a guest still holds a finished take (cast-7bc),
// chromium and firefox, on a host of its own with fixed ports (the guest's
// storage belongs to the page's origin, so the restarted host must answer on
// the same one): the guest leaves holding its take, the host process is
// killed and started again on the same library and ports, and the guest's
// background sync then delivers the take into the call it was recorded in.
const freeTcp = () => new Promise((resolve) => {
  const s = createServer().listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); });
});
const freeUdp = () => new Promise((resolve) => {
  const s = createSocket('udp4');
  s.bind(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); });
});

export async function hostRestartChecks() {
  const { MODE, exe, dist, RESULTS } = ctx;
  const LIB = join(RESULTS, 'library-restart-' + MODE);
  rmSync(LIB, { recursive: true, force: true });
  const [httpsPort, httpPort, udpPort] = [await freeTcp(), await freeTcp(), await freeUdp()];
  const start = async () => {
    const p = spawn(exe, ['--dist', dist, '--library', LIB, '--room', 'restart-show', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
      '--port', String(httpsPort), '--http-port', String(httpPort), '--udp-port', String(udpPort), '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
    ctx.host13 = p;
    p.stderr.on('data', (d) => { ctx.host13Log = (ctx.host13Log || '') + d.toString(); });
    const urls = await new Promise((resolve, reject) => {
      let buf = '';
      const t = setTimeout(() => reject(new Error('restart host did not print HOST_READY')), 20000);
      p.stdout.on('data', (d) => {
        buf += d.toString();
        const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
        if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
      });
    });
    return { p, urls };
  };
  let { p: host, urls: [HTTP, HTTPS] } = await start();
  const P = await openGuest('RSP', [], HTTP, undefined, 'Pat');
  const G = await openGuest('RSG', [], HTTPS, undefined, 'Gil');
  const [pUrl, gUrl] = [P.url(), G.url()];
  for (const x of [P, G]) await x.click('#join');
  await waitFor(async () => P.isEnabled('#take-start'), 'P can start a take', 30000);
  await waitFor(async () => (await text(G, '#participant')).length > 0, 'G joined', 30000);
  const dirOf = () => callDirs(LIB, 'restart-show')[0];
  // Two microphone tracks: the producer's own and the guest's.
  const synced = (take) => {
    const dir = dirOf();
    const d = dir && join(dir, take);
    return !!d && existsSync(d) && readdirSync(d).filter((f) => f.endsWith('_mic.wav')).length >= 2
      && !readdirSync(d).some((f) => f.endsWith('.journal') || f.endsWith('.obao'));
  };
  const heldTake = (n) => waitFor(async () => (await opfsNames(G)).some((f) => f.startsWith('take000' + n) && f.endsWith('.json')), 'G kept its take ' + n, 15000);
  const rejoin = async () => {
    for (const x of [P, G]) await x.click('#join');
    await waitFor(async () => P.isEnabled('#take-start'), 'P can start a take', 30000);
    await waitFor(async () => (await text(G, '#participant')).length > 0, 'G joined', 30000);
  };

  // cast-u9z: a guest that leaves while a take still records transfers
  // nothing until the take stops, then it does.
  await P.click('#take-start');
  await waitFor(async () => (await text(P, '#take-result')).includes('"take":1'), 'take 1 started', 10000);
  await sleep(2500);
  await G.click('#leave');
  await sleep(8000);
  check(!synced('take01') && (await opfsNames(G)).some((f) => f.startsWith('take0001')),
    'a guest that left mid-take has transferred nothing 8 s later, and still holds its track');
  await P.click('#take-stop');
  await waitFor(async () => (await text(P, '#take-status')).startsWith('idle'), 'take 1 stopped', 10000);
  await waitFor(async () => synced('take01'), 'G\x27s take 1 arrives once the take stopped', 60000);
  await waitFor(async () => !(await opfsNames(G)).some((f) => f.startsWith('take0001')), 'G\x27s take 1 left its storage', 30000);
  check(true, 'sync waited for the take to stop, then delivered it');

  // cast-7bc: the same, but the host is restarted before the delivery.
  await P.click('#leave').catch(() => {});
  await sleep(500);
  await P.goto(pUrl);
  await G.goto(gUrl);
  await rejoin();
  await P.click('#take-start');
  await waitFor(async () => (await text(P, '#take-result')).includes('"take":2'), 'take 2 started', 10000);
  await sleep(2500);
  await G.evaluate(() => { window.__castcorderHoldSync = true; });
  await G.click('#leave');
  await sleep(500);
  await P.click('#take-stop');
  await waitFor(async () => (await text(P, '#take-status')).startsWith('idle'), 'take 2 stopped', 10000);
  await heldTake(2);
  check(!synced('take02'), 'G\x27s take 2 is not on the host before the restart');

  // The host process dies and comes back on the same library and ports.
  await P.click('#leave').catch(() => {});
  await new Promise((resolve) => { host.once('exit', resolve); host.kill(); });
  ({ p: host } = await start());
  await waitFor(async () => (await fetch(HTTP + '/rooms', { headers: { 'X-Castcorder-Native': '1' } }).then((r) => r.ok).catch(() => false)), 'the restarted host answers', 15000);
  check((await opfsNames(G)).some((f) => f.startsWith('take0002')), 'G still holds its take after the host restarted');
  await G.evaluate(() => { window.__castcorderHoldSync = false; });
  await waitFor(async () => synced('take02'), 'G\x27s take reaches the restarted host', 60000);
  await waitFor(async () => !(await opfsNames(G)).some((f) => f.startsWith('take0002')), 'G\x27s take left its storage', 30000);
  check(callDirs(LIB, 'restart-show').length === 1, 'the take went into the call it was recorded in, not a new one');
  check(true, 'a guest\x27s take recorded before a host restart syncs to the restarted host');
}
