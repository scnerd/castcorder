import { spawn } from 'node:child_process';
import { join } from 'node:path';
import { rmSync } from 'node:fs';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// What the pages tell about a take beyond its number, on a host of its own
// (chromium and firefox): the REC pill's running time (cast-wuk), whose start
// comes from the host's take status so a guest joining mid-take shows the
// same time, and the free-disk warnings (cast-ycn).
const secs = (t) => t.split(':').map(Number).reduce((a, n) => a * 60 + n, 0);

export async function takeInfoChecks() {
  const { MODE, exe, dist, RESULTS } = ctx;
  const LIB = join(RESULTS, 'library-takeinfo-' + MODE);
  rmSync(LIB, { recursive: true, force: true });
  ctx.host6 = spawn(exe, ['--dist', dist, '--library', LIB, '--call', 'takeinfo-test', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host6Log = '';
  ctx.host6.stderr.on('data', (d) => { ctx.host6Log += d.toString(); });
  const [HTTP6, HTTPS6] = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('takeinfo host did not print HOST_READY')), 20000);
    ctx.host6.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
    });
  });
  // P is the producer (localhost page); G is a guest from the start.
  const P = await openGuest('P', [], HTTP6);
  const G = await openGuest('G', [], HTTPS6);
  for (const p of [P, G]) await p.click('#join');
  await waitFor(async () => P.isEnabled('#take-start'), 'P can start a take', 30000);
  await waitFor(async () => (await text(G, '#participant')).length > 0, 'G joined', 30000);

  // The pill shows a running time that advances.
  const pill = (p) => p.evaluate(() => {
    const e = document.querySelector('#rec-elapsed');
    return e && !e.hidden && e.offsetParent !== null ? e.textContent : '';
  });
  check((await pill(P)) === '', 'no take time on the pill while idle');
  await P.click('#take-start');
  const first = await waitFor(async () => (await pill(P)) || null, 'the REC pill shows the take time', 15000);
  check(/^\d+:\d\d$/.test(first), 'the REC pill shows the take time (' + first + ')');
  await sleep(3000);
  const later = await pill(P);
  check(secs(later) - secs(first) >= 2 && secs(later) - secs(first) <= 5,
    'the pill\x27s take time runs (' + first + ' then ' + later + ' after 3 s)');

  // A guest joining mid-take shows the same time (from the host's start time).
  const Z = await openGuest('Z', [], HTTPS6);
  await Z.click('#join');
  await waitFor(async () => (await pill(Z)) || null, 'a mid-take joiner\x27s pill shows the take time', 40000);
  const [pz, pp, pg] = [await pill(Z), await pill(P), await pill(G)];
  check(Math.abs(secs(pz) - secs(pp)) <= 2 && Math.abs(secs(pg) - secs(pp)) <= 2 && secs(pz) >= 4,
    'a mid-take joiner\x27s time (' + pz + ') is within 2 s of the producer\x27s (' + pp + ') and the guest\x27s (' + pg + ')');
  await P.click('#take-stop');
  await waitFor(async () => (await pill(P)) === '' && (await pill(Z)) === '', 'the take time leaves the pill after the stop', 15000);
  check(true, 'the take time leaves the pill when the take stops');
  await Z.click('#leave').catch(() => {});

  // Free disk (cast-ycn): a guest with little room (the page's own override,
  // ?freebytes=) is flagged to the producer, in a banner and in People; the
  // take still starts and the guest records it.
  const L = await openGuest('L', [], HTTPS6, undefined, 'L', '&freebytes=800000000');
  await L.click('#join');
  await waitFor(async () => (await text(L, '#participant')).length > 0, 'L joined', 30000);
  await waitFor(async () => P.evaluate(() => {
    const e = document.querySelector('#disk-warning');
    return e && !e.hidden && e.textContent.includes('L:');
  }), 'the producer is warned that L is low on disk', 30000);
  check(true, 'the producer\'s banner warns that L has little room: ' + (await text(P, '#disk-warning')));
  const row = await P.evaluate(() => {
    const e = document.querySelector('#guests li[data-name="L"] .pdisk');
    return e ? e.className + ' | ' + e.textContent : 'missing';
  });
  check(/low/.test(row) && /800 MB free/.test(row), 'People shows L flagged with its free space: ' + row);
  await waitFor(async () => (await text(P, '#library-disk')) !== 'not known', 'the host library\'s room is shown', 15000);
  check(true, 'the producer sees the host library\'s room: ' + (await text(P, '#library-disk')));
  check(await P.isEnabled('#take-start'), 'a low-disk warning leaves Start take enabled');
  await P.click('#take-start');
  await waitFor(async () => (await text(L, '#recording')).includes('recording take'), 'L records the take despite the warning', 15000);
  check((await text(P, '#take-status')).startsWith('recording'), 'the take started with a device low on disk');
  await sleep(1500);
  await P.click('#take-stop');
  await waitFor(async () => (await text(P, '#take-status')).startsWith('idle'), 'the take stopped', 15000);
  for (const p of [P, G, L]) await p.click('#leave').catch(() => {});
}
