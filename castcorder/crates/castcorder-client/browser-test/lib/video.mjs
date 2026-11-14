import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, json, check, videoFrom, decoding, audioRising } from './helpers.mjs';

// Live video (cast-riw): A and B each decode the other's forwarded camera
// (fake devices) and show it in a tile named after them; A sends two
// simulcast layers where the browser does (Chromium); A's camera turned off
// is released and B's frames from A stop while the audio goes on; turned on
// again, B decodes it again.
export async function videoChecks(A, B) {
  const { host } = ctx;
  for (const [rx, n, from] of [[B, 'B', 'A'], [A, 'A', 'B']]) {
    const v = await decoding(rx, from, n + ' decoding video from ' + from);
    check(v.fps >= 3, n + ' decodes ' + from + '\x27s forwarded video (' + v.fps.toFixed(1) + ' fps, ' + v.w + 'x' + v.h + ')');
    const tile = await rx.evaluate((from) => {
      const t = [...document.querySelectorAll('#remote .tile')].find((e) => e.dataset.origin.startsWith(from + '-'));
      const el = t && t.querySelector('video');
      return el ? { w: el.videoWidth, h: el.videoHeight, ready: el.readyState, name: t.querySelector('.tile-name').textContent } : null;
    }, from);
    check(!!tile && tile.w > 0 && tile.ready >= 2 && tile.name === from,
      n + ' shows ' + from + '\x27s video playing in a tile named after them: ' + JSON.stringify(tile));
  }
  const outbound = await A.evaluate(async () => {
    const pc = window.__pcs[window.__pcs.length - 1];
    const out = [];
    (await pc.getStats()).forEach((s) => {
      if (s.type === 'outbound-rtp' && s.kind === 'video') out.push({ rid: s.rid || '', framesEncoded: s.framesEncoded || 0, w: s.frameWidth || 0, h: s.frameHeight || 0 });
    });
    return out;
  });
  console.log('info A outbound video: ' + JSON.stringify(outbound));
  const layers = outbound.filter((o) => o.rid && o.framesEncoded > 0).map((o) => o.rid).sort();
  check(layers.join(',') === 'h,l', 'A sends two simulcast layers (' + layers.join(',') + ')');
  // Camera off: released, B's frames from A stop, audio goes on.
  const offers = () => (ctx.hostLog.match(/sent offer/g) || []).length;
  const offersBefore = offers();
  await A.click('#camera');
  await waitFor(async () => (await text(A, '#camera-state')) === 'off', 'A camera off', 5000);
  const live = await A.evaluate(() => window.__tracks.filter((t) => t.kind === 'video' && t.readyState !== 'ended').length);
  check(live === 0, 'turning the camera off releases it (' + live + ' live video tracks)');
  await sleep(1500);
  const x = videoFrom(await json(B, '#debug-inbound'), 'A');
  const audio = await audioRising(B, 'A');
  await sleep(1000);
  const y = videoFrom(await json(B, '#debug-inbound'), 'A');
  check(y.frames - x.frames <= 2 && audio > 0,
    'with A\x27s camera off B decodes no new frames from A (' + (y.frames - x.frames) + ' in 3 s) and still gets A\x27s audio (' + audio + ' packets in 2 s)');
  await A.click('#camera');
  await waitFor(async () => (await text(A, '#camera-state')) === 'on', 'A camera on again', 10000);
  const again = await decoding(B, 'A', 'B decoding A\x27s camera again', 20000);
  check(again.fps >= 3 && offers() === offersBefore, 'A\x27s camera back on: B decodes it again (' + again.fps.toFixed(1)
    + ' fps); the host sent ' + (offers() - offersBefore) + ' offers meanwhile (no renegotiation)');
}
