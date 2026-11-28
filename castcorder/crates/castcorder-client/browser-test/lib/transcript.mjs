import { spawn } from 'node:child_process';
import { join } from 'node:path';
import { rmSync, writeFileSync, mkdirSync } from 'node:fs';
import { ctx } from './ctx.mjs';
import { waitFor, text, check } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// The live transcript (cast-7hm.2, .3) end to end in Chromium, on a host of
// its own run with `--transcribe sim`: the transcription port's simulation,
// which hears its own tone code (castcorder-port-transcribe `sim::speak`).
// Guest G's fake microphone "says" "hello there" in that code; once the
// producer P starts a take, the line reaches P's Transcript panel under G's
// name, and the panel's search finds it or not.

// sim::speak at 48 kHz: each letter a 60 ms tone at 400 + 80 * index Hz
// (5 ms fades), then 20 ms of silence; a space is 120 ms of silence.
function speak(textIn, rate) {
  const out = [];
  const ms = (m) => Math.round(rate * m / 1000);
  for (const c of textIn.toLowerCase()) {
    if (c === ' ') { for (let i = 0; i < ms(120); i++) out.push(0); continue; }
    const k = c.charCodeAt(0) - 97;
    if (k < 0 || k > 25) continue;
    const hz = 400 + 80 * k, n = ms(60), fade = ms(5);
    for (let i = 0; i < n; i++) {
      const ramp = Math.min(Math.min(i, n - 1 - i) / fade, 1);
      out.push(0.3 * ramp * Math.sin(2 * Math.PI * hz * i / rate));
    }
    for (let i = 0; i < ms(20); i++) out.push(0);
  }
  return out;
}

function writeMonoWav(file, samples, rate) {
  const data = Buffer.alloc(samples.length * 2);
  samples.forEach((s, i) => data.writeInt16LE(Math.max(-32768, Math.min(32767, Math.round(s * 32767))), i * 2));
  const h = Buffer.alloc(44);
  h.write('RIFF', 0); h.writeUInt32LE(36 + data.length, 4); h.write('WAVE', 8);
  h.write('fmt ', 12); h.writeUInt32LE(16, 16); h.writeUInt16LE(1, 20); h.writeUInt16LE(1, 22);
  h.writeUInt32LE(rate, 24); h.writeUInt32LE(rate * 2, 28); h.writeUInt16LE(2, 32); h.writeUInt16LE(16, 34);
  h.write('data', 36); h.writeUInt32LE(data.length, 40);
  writeFileSync(file, Buffer.concat([h, data]));
}

export async function transcriptChecks() {
  const { MODE, exe, dist, RESULTS } = ctx;
  const LIB = join(RESULTS, 'library-transcript-' + MODE);
  rmSync(LIB, { recursive: true, force: true });
  mkdirSync(RESULTS, { recursive: true });
  const WAV = join(RESULTS, 'transcript-input.wav');
  // A phrase long enough to be drafted while it is said (a draft comes per
  // second of speech, cast-veeq), then 2 s of silence (Chromium loops the file).
  const rate = 48000;
  const PHRASE = 'hello there how are you today';
  writeMonoWav(WAV, [...new Array(rate).fill(0), ...speak(PHRASE, rate), ...new Array(2 * rate).fill(0)], rate);
  ctx.host12 = spawn(exe, ['--dist', dist, '--library', LIB, '--room', 'transcript-test', '--bind', '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--open', '--transcribe', 'sim'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host12Log = '';
  ctx.host12.stderr.on('data', (d) => { ctx.host12Log += d.toString(); });
  const [HTTP, HTTPS] = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('transcript host did not print HOST_READY')), 20000);
    ctx.host12.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
    });
  });
  const P = await openGuest('P', [], HTTP);
  const G = await openGuest('G', ['--use-file-for-fake-audio-capture=' + WAV], HTTPS);
  for (const p of [P, G]) await p.click('#join');
  await waitFor(async () => P.isEnabled('#take-start'), 'P can start a take', 30000);
  await waitFor(async () => (await text(G, '#participant')).length > 0, 'G joined', 30000);

  // The Transcript panel is open from the start beside People (cast-2wqp),
  // with its own button on the top bar: empty before any take.
  const shown = (sel) => P.locator(sel).isVisible();
  check(await shown('#transcript-side') && await shown('#guest-count'),
    'a wide page shows the Transcript panel and People side by side without asking');
  check((await text(P, '#transcript-count')).startsWith('Nothing yet'), 'the panel is empty before a take: ' + await text(P, '#transcript-count'));
  await P.click('#open-transcript');
  check(!(await shown('#transcript-side')), 'the top bar\'s Transcript button closes it');
  await P.click('#open-transcript');
  check(await shown('#transcript-side'), 'and opens it again');
  // Where only one panel fits, the transcript is kept: People does not open
  // by itself, opening it takes the place, and closing it brings the transcript back.
  const wide = P.viewportSize();
  await P.setViewportSize({ width: 1000, height: wide.height });
  await waitFor(async () => !(await shown('#guest-count')) && await shown('#transcript-side'),
    'on a narrower page the transcript stays and People steps aside', 5000);
  check(true, 'where one panel fits, the transcript is the one shown');
  await P.click('#open-people');
  await waitFor(async () => await shown('#guest-count') && !(await shown('#transcript-side')), 'People opens in its place', 5000);
  await P.click('#open-people');
  await waitFor(async () => await shown('#transcript-side'), 'closing People brings the transcript back', 5000);
  check(true, 'opening People takes the transcript\'s place, closing it brings the transcript back');
  await P.setViewportSize(wide);

  // Every draft P is shown, as it appears.
  await P.evaluate(() => {
    window.__drafts = [];
    new MutationObserver(() => {
      for (const d of document.querySelectorAll('.transcript-line.draft')) {
        const t = d.querySelector('.said')?.textContent;
        if (t && window.__drafts.at(-1) !== t) window.__drafts.push(t);
      }
    }).observe(document.body, { subtree: true, childList: true, characterData: true });
  });
  await P.click('#take-start');
  const said = await waitFor(async () => {
    const lines = await P.evaluate(() => [...document.querySelectorAll('.transcript-line:not(.draft)')]
      .map((l) => [l.querySelector('.who')?.textContent, l.querySelector('.said')?.textContent]));
    return lines.find(([who, s]) => who === 'G' && s === PHRASE) ? lines : null;
  }, 'G is heard saying "' + PHRASE + '" in P\'s Transcript panel', 40000);
  const drafts = await P.evaluate(() => window.__drafts);
  check(drafts.some((d) => d.startsWith('hello') && d !== PHRASE),
    'while G was still talking, P saw drafts of what G had said so far: ' + JSON.stringify(drafts));
  check(said.every(([who]) => who === 'G'), 'only G spoke: ' + JSON.stringify(said));
  check(await P.evaluate(() => document.querySelector('.transcript-take h4')?.textContent) === 'Take 1', 'lines sit under their take');

  await P.fill('#transcript-search', 'THERE');
  const found = await text(P, '#transcript-count');
  check(/^\d+ of \d+ lines$/.test(found) && !found.startsWith('0 '), 'search finds the line: ' + found);
  await P.fill('#transcript-search', 'nobody said this');
  check((await text(P, '#transcript-count')).startsWith('0 of '), 'search hides what does not match');
  check(await P.locator('.transcript-line:not(.draft)').count() === 0, 'no lines shown for a search that matches none');
  await P.click('#take-stop');
  console.log('transcript: ' + said.length + ' lines heard');
}
