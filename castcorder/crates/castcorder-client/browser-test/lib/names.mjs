import { spawn } from 'node:child_process';
import { existsSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check, callDir } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// Display names (cast-2e3), chromium and firefox: a guest named with a space
// and a non-ASCII letter ("Zo\u00eb Lee") joins by URL query, so the name
// travels percent-encoded. Another guest's tile and the producer's guest list
// show the real name; room.json keeps it; its tracks land on the host under
// an ASCII slug (letters and digits kept, then a short hash, because the slug
// lost something) and the stream ids stay plain ASCII.
export async function namesChecks() {
  const { MODE, exe, dist, RESULTS, host, logs, firefox } = ctx;
  const NAME = 'Zo\u00eb Lee';
  const LIB5 = join(RESULTS, 'library-names-' + MODE);
  rmSync(LIB5, { recursive: true, force: true });
  ctx.host5 = spawn(exe, ['--dist', dist, '--library', LIB5, '--room', 'names-test', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host5Log = '';
  ctx.host5.stderr.on('data', (d) => { ctx.host5Log += d.toString(); });
  const [HTTP5, HTTPS5] = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('names host did not print HOST_READY')), 20000);
    ctx.host5.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
    });
  });
  // X is the producer (the host machine's localhost page); Y watches over HTTPS.
  const X = await openGuest('X', [], HTTP5, undefined, NAME);
  const Y = await openGuest('Y', [], HTTPS5, undefined, 'Yan');
  for (const p of [X, Y]) await p.click('#join');
  await waitFor(async () => Y.evaluate(() => [...document.querySelectorAll('#remote .tile .tile-name')].map((e) => e.textContent).includes('Zo\u00eb Lee')),
    'Y shows a tile named with the real display name', 40000);
  check(true, 'another guest\x27s tile shows the name with its space and non-ASCII letter');
  const tileOrigin = await Y.evaluate(() => [...document.querySelectorAll('#remote .tile')].map((e) => e.dataset.origin));
  check(tileOrigin.every((o) => /^[A-Za-z0-9-]+$/.test(o)), 'stream ids stay plain ASCII: ' + JSON.stringify(tileOrigin));
  await waitFor(async () => (await text(X, '#guests')).includes(NAME + ' (guest'), 'producer list shows the real name', 10000);
  check(await X.evaluate(() => [...document.querySelectorAll('#guests li')].some((e) => e.dataset.name === 'Zo\u00eb Lee')),
    'the producer\x27s guest list shows the real name');
  check(/name=Zo%C3%AB%20Lee/.test(await X.evaluate(() => performance.getEntriesByType('resource').map((r) => r.name).join('\n'))),
    'the name goes into the offer URL percent-encoded');

  // A take: the tracks land under the slug.
  await waitFor(async () => X.isEnabled('#take-start'), 'X can start a take', 15000);
  await X.click('#take-start');
  await waitFor(async () => (await text(X, '#take-result')).includes('"take":1'), 'take 1 started', 10000);
  await sleep(2500);
  await X.click('#take-stop');
  const TAKE = join(callDir(LIB5, 'names-test'), 'take01');
  const mine = () => (existsSync(TAKE) ? readdirSync(TAKE) : []).filter((f) => f.endsWith('_mic.wav'));
  await waitFor(async () => mine().length >= 2 && !readdirSync(TAKE).some((f) => f.endsWith('.journal') || f.endsWith('.obao')), 'both tracks synced', 40000);
  const files = readdirSync(TAKE);
  check(files.some((f) => /^take01_ZoLee-[0-9a-f]{6}_mic\.wav$/.test(f)) && files.some((f) => f === 'take01_Yan_mic.wav')
    && files.every((f) => /^[A-Za-z0-9._-]+$/.test(f)),
    'tracks land under an ASCII slug (hash only where the name lost something): ' + files.join(' '));
  const room = JSON.parse(readFileSync(join(LIB5, 'names-test', 'room.json'), 'utf8'));
  check(Object.values(room.devices).some((d) => d.name === NAME), 'room.json keeps the real display name');
  check(ctx.host5Log.includes('Zo\u00eb Lee'), 'the host log shows the real name');
  for (const p of [X, Y]) await p.click('#leave').catch(() => {});
}
