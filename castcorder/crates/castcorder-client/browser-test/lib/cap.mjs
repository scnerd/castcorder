import { spawn } from 'node:child_process';
import { rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, check, decoding, audioRising, json, videoFrom } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// The low layer through real browsers (cast-riw, cast-18o), chromium and
// firefox: a fourth host whose video allocation assumes at most CAP kbit/s
// per guest (--bandwidth-cap: a configured cap on the estimate, not network
// shaping; the policy itself, smoothing included, runs against real capacity
// limits in castcorder-transport's simulation tests). CAP admits "l" and not
// "h" (alloc.rs: 90 % of the cap, audio first, 25 % margin to step up, an
// upgrade held 3 s), so each guest keeps decoding the other at 320x180 while
// audio flows. Checked by what the host chose for each receiver (its log)
// and by frames arriving over a long window, not by a frame rate: a headless
// encoder under CI load may run at a few fps.
export async function capChecks() {
  const { MODE, exe, dist, RESULTS, host, logs, firefox } = ctx;
  // The fake cameras settle near l 120-150 and h 185-250 kbit/s (h ramps up
  // from lower): at 300 (about 225 left after audio) l is admitted from no
  // video up to about 160 and kept up to about 200, while h is admitted only
  // below about 160, so the guests settle on l.
  const CAP = 300;
  const LIB4 = join(RESULTS, 'library-cap-' + MODE);
  rmSync(LIB4, { recursive: true, force: true });
  ctx.host4 = spawn(exe, ['--dist', dist, '--library', LIB4, '--room', 'cap-test', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
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
  // Display names with a dash: the stream ids carry only letters and digits
  // ("PatLee"), the tiles show the names as given (host Event::Participants).
  const P = await openGuest('P', [], HTTPS4, undefined, 'Pat-Lee');
  const Q = await openGuest('Q', [], HTTPS4, undefined, 'Quinn-Ray');
  for (const p of [P, Q]) await p.click('#join');
  const layerLines = (rx, origin) => ctx.host4Log.split('\n')
    .filter((l) => l.includes('video layer chosen') && l.includes('device=' + rx + ' ') && l.includes('origin=' + origin + ' '));
  for (const [rx, n, rxId, from, fromName] of [[Q, 'Q', 'QuinnRay', 'PatLee', 'Pat-Lee'], [P, 'P', 'PatLee', 'QuinnRay', 'Quinn-Ray']]) {
    // Decoding, and settled on the low layer (the first frames may be "h").
    await waitFor(async () => {
      const d = await decoding(rx, from, n + ' decoding ' + from, 30000);
      return d.w > 0 && d.w <= 320 ? d : null;
    }, n + ' decoding ' + from + '\x27s low layer', 40000, 0);
    // Frames keep coming over a long window (no tight fps threshold: a
    // headless encoder under load may run slow), still the low layer.
    const x = videoFrom(await json(rx, '#debug-inbound'), from);
    await sleep(12000);
    const y = videoFrom(await json(rx, '#debug-inbound'), from);
    const audio = await audioRising(rx, from);
    check(y.frames - x.frames >= 10 && y.w <= 320 && audio > 0, 'capped at ' + CAP + ' kbit/s, ' + n + ' keeps decoding ' + from
      + '\x27s low layer (' + (y.frames - x.frames) + ' frames in 12 s, ' + y.w + 'x' + y.h + ') and gets their audio (' + audio + ' packets in 2 s)');
    // The host's own choice for this receiver: its last word is layer l.
    const tile = await rx.evaluate((from) => {
      const t = [...document.querySelectorAll('#remote .tile')].find((e) => e.dataset.origin.startsWith(from + '-'));
      return t ? { origin: t.dataset.origin, name: t.querySelector('.tile-name').textContent } : null;
    }, from);
    const origin = tile ? tile.origin.split('-').pop() : '?';
    const lines = layerLines(rxId, origin);
    const last = lines.length ? (lines[lines.length - 1].match(/ to=(\S+)/) || [])[1] : undefined;
    console.log('info cap host choices for ' + n + ' from ' + from + ': ' + lines.map((l) => (l.match(/to=(\S+).*layer_kbps=(\S+)/) || []).slice(1).join(' ')).join(' | '));
    check(last === 'l', 'the cap host\x27s last layer choice for ' + n + ' receiving ' + from + ' is l (' + last + ', ' + lines.length + ' choices)');
    check(!!tile && tile.name === fromName, n + '\x27s tile for ' + from + ' shows their display name: ' + JSON.stringify(tile));
  }
  for (const p of [P, Q]) await p.click('#leave').catch(() => {});
}
