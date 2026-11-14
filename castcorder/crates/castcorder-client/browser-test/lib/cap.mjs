import { spawn } from 'node:child_process';
import { rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { waitFor, check, decoding, audioRising } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// The low layer through real browsers (cast-riw), chromium and firefox: a
// fourth host whose video allocation assumes at most CAP kbit/s per guest
// (--bandwidth-cap: a configured cap on the estimate, not network shaping;
// the policy itself runs against real capacity limits in castcorder-transport's
// simulation tests). CAP admits "l" and not "h" (alloc.rs: 90 % of the cap,
// audio first, 25 % margin to step up), so each guest decodes the other at
// 320x180 while audio flows.
export async function capChecks() {
  const { MODE, exe, dist, RESULTS, host, logs, firefox } = ctx;
  // The fake cameras settle near l 120-150 and h 200-250 kbit/s (Chromium
  // starts lower): at 250 (about 190 left after audio) l stays even at its
  // 150 kbit/s ceiling, h is admitted only below about 140 and kept only
  // below about 170 kbit/s, so the guests settle on l.
  const CAP = 250;
  const LIB4 = join(RESULTS, 'library-cap-' + MODE);
  rmSync(LIB4, { recursive: true, force: true });
  ctx.host4 = spawn(exe, ['--dist', dist, '--library', LIB4, '--call', 'cap-test', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--open', '--bandwidth-cap', String(CAP)], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host4.stderr.on('data', (d) => { ctx.host4Log += d.toString(); });
  const HTTPS4 = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('cap host did not print HOST_READY')), 20000);
    ctx.host4.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve(m[2]); }
    });
  });
  const P = await openGuest('P', [], HTTPS4);
  const Q = await openGuest('Q', [], HTTPS4);
  for (const p of [P, Q]) await p.click('#join');
  for (const [rx, n, from] of [[Q, 'Q', 'P'], [P, 'P', 'Q']]) {
    // Decoding, and settled on the low layer (the first frames may be "h").
    const v = await waitFor(async () => {
      const d = await decoding(rx, from, n + ' decoding ' + from, 30000);
      return d.w > 0 && d.w <= 320 ? d : null;
    }, n + ' decoding ' + from + '\x27s low layer', 40000, 0);
    const audio = await audioRising(rx, from);
    check(v.fps >= 3 && audio > 0, 'capped at ' + CAP + ' kbit/s, ' + n + ' decodes ' + from + '\x27s low layer (' + v.w + 'x' + v.h + ', '
      + v.fps.toFixed(1) + ' fps) and gets their audio (' + audio + ' packets in 2 s)');
  }
  const chosen = ctx.host4Log.split('\n').filter((l) => l.includes('video layer chosen')).map((l) => (l.match(/to=(\S+).*layer_kbps=(\S+)/) || []).slice(1).join(' '));
  console.log('info cap host layer choices: ' + chosen.join(' | '));
  check(/video layer chosen.*to=l/.test(ctx.host4Log), 'the cap host logs choosing layer l');
  for (const p of [P, Q]) await p.click('#leave').catch(() => {});
}
