import { spawn } from 'node:child_process';
import { join } from 'node:path';
import { rmSync } from 'node:fs';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check, noErrors } from './helpers.mjs';
import { openGuest } from './guest.mjs';
import { chips } from './trackchips.mjs';

// Guests on phones (cast-b07), Chromium with Playwright's device emulation
// (a phone's user agent, viewport, touch and isMobile), on a host of its own:
// - the lobby on an iPhone and on an Android phone says, in its own notice,
//   what pauses a recording on that device; a computer gets no such notice;
//   the lobby fits the screen (no sideways scroll), Join is on screen (name
//   and Join come before the preview on a phone) and the
//   name field is at least 16px (iOS Safari zooms into smaller fields);
// - joined on the Android phone, the call fits the screen too;
// - mid-take the system pauses the phone's microphone (its track fires
//   "mute", as when another app or the lock screen takes the audio): the
//   producer sees "Mic lost" on the phone's tile; back ("unmute"), the chip
//   is Recording again and the phone's person gets a note saying how long
//   was missed.
// WebKit itself is not run (its Linux build has no fake microphone); the
// iPhone check is the page's own reading of an iPhone's user agent.
const ON_PHONE = 'On a phone, keep this page open and on screen';
const ON_IPHONE = 'On an iPhone or iPad, keep this page open and on screen';

async function lobbyFits(page, n, expect) {
  const notice = await page.locator('#phone-notice').isVisible() ? await text(page, '#phone-notice') : '';
  check(notice.includes(expect), n + ' lobby warns what pauses a recording on it: ' + JSON.stringify(notice));
  const m = await page.evaluate(() => {
    const join = document.querySelector('#join').getBoundingClientRect();
    const name = getComputedStyle(document.querySelector('#name')).fontSize;
    return { scroll: document.documentElement.scrollWidth, width: innerWidth, height: innerHeight,
      join: { left: join.left, right: join.right, top: join.top, bottom: join.bottom }, name };
  });
  check(m.scroll <= m.width, n + ' lobby fits the screen sideways (' + m.scroll + ' <= ' + m.width + ')');
  check(m.join.left >= 0 && m.join.right <= m.width, n + ' Join is within the screen\x27s width: ' + JSON.stringify(m.join));
  check(m.join.bottom <= m.height, n + ' Join is on screen without scrolling (its bottom at ' + Math.round(m.join.bottom) + ' of ' + m.height + ')');
  check(parseFloat(m.name) >= 16, n + ' name field is ' + m.name + ' (16px or more: no zoom on focus)');
  await page.locator('#join').scrollIntoViewIfNeeded();
  await page.locator('#join').click({ trial: true, timeout: 5000 });
  check(true, n + ' Join is reachable and clickable');
}

export async function mobileChecks() {
  const { exe, dist, RESULTS } = ctx;
  const { devices } = await import('playwright');
  const LIB = join(RESULTS, 'library-mobile');
  rmSync(LIB, { recursive: true, force: true });
  ctx.host13 = spawn(exe, ['--dist', dist, '--library', LIB, '--room', 'mobile-test', '--bind', '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--open'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host13Log = '';
  ctx.host13.stderr.on('data', (d) => { ctx.host13Log += d.toString(); });
  const [HTTP, HTTPS] = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('mobile host did not print HOST_READY')), 20000);
    ctx.host13.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
    });
  });
  const phone = (d) => ({ userAgent: d.userAgent, viewport: d.viewport, deviceScaleFactor: d.deviceScaleFactor, isMobile: true, hasTouch: true });
  // The microphones the page opened, so the test can fire the system's
  // "mute" and "unmute" on them.
  const keepMics = (c) => c.addInitScript(() => {
    const mics = window.__mics = [];
    const gum = MediaDevices.prototype.getUserMedia;
    MediaDevices.prototype.getUserMedia = async function (...a) {
      const st = await gum.apply(this, a);
      mics.push(...st.getAudioTracks());
      return st;
    };
  });

  const I = await openGuest('MI', [], HTTPS, undefined, 'Ivy', '', phone(devices['iPhone 14']));
  await lobbyFits(I, 'iPhone', ON_IPHONE);
  const P = await openGuest('MP', [], HTTP, undefined, 'Pat');
  check((await P.locator('#phone-notice').count()) === 0, 'a computer\x27s lobby has no phone notice');
  const M = await openGuest('MM', [], HTTPS, keepMics, 'Max', '', phone(devices['Pixel 7']));
  await lobbyFits(M, 'Android phone', ON_PHONE);

  for (const p of [P, M]) await p.click('#join');
  await waitFor(async () => P.isEnabled('#take-start'), 'P can start a take', 30000);
  await waitFor(async () => (await text(M, '#participant')).length > 0, 'the phone joined', 30000);
  const joined = await M.evaluate(() => ({ scroll: document.documentElement.scrollWidth, width: innerWidth }));
  check(joined.scroll <= joined.width, 'joined, the call fits the phone\x27s screen sideways (' + joined.scroll + ' <= ' + joined.width + ')');

  await P.click('#take-start');
  await waitFor(async () => (await text(M, '#recording')).includes('recording take0001'), 'the phone records take 1', 15000);
  await waitFor(async () => ((await chips(P)).Max || []).includes('recording') || null, 'P sees the phone recording', 15000);
  await sleep(1500);
  const fire = (type) => M.evaluate((type) => {
    const live = window.__mics.filter((t) => t.readyState === 'live');
    for (const t of live) t.dispatchEvent(new Event(type));
    return live.length;
  }, type);
  const muted = await fire('mute');
  check(muted > 0, 'the system pauses the phone\x27s microphone mid-take (' + muted + ' track)');
  const lost = await waitFor(async () => { const c = await chips(P); return (c.Max || []).includes('mic-lost') ? c : null; },
    'P sees the phone\x27s microphone lost', 10000).catch(() => null);
  check(!!lost, 'the producer sees "Mic lost" on the phone\x27s tile while it is paused: ' + JSON.stringify(lost || await chips(P)));
  await sleep(3000);
  await fire('unmute');
  const back = await waitFor(async () => { const c = await chips(P); return (c.Max || []).includes('recording') ? c : null; },
    'P sees the phone recording again', 10000).catch(() => null);
  check(!!back, 'back from the pause, the producer sees the phone Recording again: ' + JSON.stringify(back || await chips(P)));
  const note = await waitFor(async () => {
    const t = await M.locator('#call-notes').isVisible() ? await text(M, '#call-notes') : '';
    return t.includes('microphone was paused for') ? t : null;
  }, 'the phone\x27s pause note', 10000).catch(() => '');
  check(/paused for (3|4|5) seconds/.test(note), 'the phone\x27s person is told how long the microphone was paused: ' + JSON.stringify(note));

  await P.click('#take-stop');
  await waitFor(async () => (await text(P, '#take-status')).startsWith('idle'), 'mobile take stopped', 10000);
  for (const [n, p] of [['MP', P], ['MM', M], ['MI', I]]) await noErrors(p, n);
  for (const p of [P, M]) await p.click('#leave').catch(() => {});
}
