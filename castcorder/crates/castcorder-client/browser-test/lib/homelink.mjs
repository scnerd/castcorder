import { spawn } from 'node:child_process';
import { rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { waitFor, text, check } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// The home's You card and linking a browser profile to a person (cast-40e.8,
// cast-x4j), chromium and firefox, on a host of its own:
// - the host machine's home shows its person (name, this device's fingerprint);
// - "Rooms you joined" and "Link a device" each show a visible result: a
//   message inside the viewport, and for the link a code to open elsewhere;
// - a second browser profile opens that link over the LAN and asks to be
//   linked; the home lists the request with the fingerprint the browser shows,
//   approves it, and the browser says it is linked to the person; the You card
//   then lists the browser among the person's devices.
export async function homeLinkChecks() {
  const { MODE, exe, dist, RESULTS, chromium, firefox, browsers } = ctx;
  const LIB = join(RESULTS, 'library-homelink-' + MODE);
  rmSync(LIB, { recursive: true, force: true });
  ctx.host11 = spawn(exe, ['--dist', dist, '--library', LIB, '--room', 'main-show', '--bind', MODE === 'firefox' ? '0.0.0.0' : '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host11Log = '';
  ctx.host11.stderr.on('data', (d) => { ctx.host11Log += d.toString(); });
  const HTTP11 = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('home-link host did not print HOST_READY')), 20000);
    ctx.host11.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+)/);
      if (m) { clearTimeout(t); resolve(m[1]); }
    });
  });
  const inView = (page, sel) => page.evaluate((s) => {
    const r = document.querySelector(s).getBoundingClientRect();
    return r.width > 0 && r.height > 0 && r.top >= 0 && r.bottom <= window.innerHeight;
  }, sel);

  const P = await openGuest('H', [], HTTP11, undefined, 'Pat');
  await P.setViewportSize({ width: 1000, height: 500 });
  await P.goto(HTTP11 + '/');
  // The You card is in Settings, a place of the home's left nav (cast-n56.1).
  await P.click('#nav-settings');
  await P.waitForSelector('#person #link-device', { timeout: 15000 });
  // The card fills its fields from the host once; type after that.
  await waitFor(async () => (await P.inputValue('#device-name')).length > 0, 'the You card loaded the person', 10000);
  await P.fill('#person-name', 'Pat Person');
  await P.click('#save-person');
  await waitFor(async () => (await text(P, '#person-message')) === 'Saved.', 'the You card says it saved the name', 10000);
  check((await text(P, '#person .small.muted:has-text("This device:")')).replace('This device:', '').trim().length > 0,
    'the You card shows this device\x27s fingerprint');

  // "Link a device": a link to open on the other browser, shown in view.
  await P.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
  await P.locator('#link-device').scrollIntoViewIfNeeded();
  await P.click('#link-device');
  await waitFor(async () => (await P.locator('#link-codes li code').count()) > 0, '"Link a device" shows a link', 10000);
  await waitFor(() => inView(P, '#person-message'), 'the message beside "Link a device" is in view', 5000);
  check((await text(P, '#person-message')).includes('Open this link on the new device'), '"Link a device" says what to do with the link');
  const link = (await P.locator('#link-codes li code').first().textContent()).trim();
  check(link.includes('#link.'), 'the link carries a one-time code: ' + link.slice(0, 40));

  // A second browser profile asks to be linked over the LAN.
  const browser = await (MODE === 'firefox' ? firefox : chromium).launch({ headless: true });
  browsers.push(browser);
  const L = await (await browser.newContext({ ignoreHTTPSErrors: true })).newPage();
  const errors = [];
  L.on('pageerror', (e) => errors.push(e.message));
  await L.goto(link);
  await L.waitForSelector('#link-this:not([disabled])', { timeout: 15000 });
  await L.fill('#link-name', 'Lab browser');
  await L.click('#link-this');
  const row = P.locator('#link-requests li[data-device]');
  await waitFor(async () => (await row.count()) === 1, 'the home lists the link request', 30000, 500);
  const fp = (await text(P, '#link-requests li[data-device] .muted')).replace('Fingerprint', '').trim();
  check(fp.length > 0 && (await text(L, '#link-fingerprint')).includes(fp), 'the home and the browser show the same fingerprint (' + fp + ')');
  await P.click('#link-requests .approve-link');
  await waitFor(async () => { const t = await text(L, '#link-state'); return t.startsWith('Linked') || t.startsWith('Not linked') ? t : null; },
    'the browser hears the approval', 30000);
  check((await text(L, '#link-state')) === 'Linked to Pat Person.', 'the browser says it is linked to Pat Person: ' + (await text(L, '#link-state')));
  await waitFor(async () => (await P.locator('#linked-devices li[data-device]:has-text("Lab browser")').count()) === 1,
    'the You card lists the linked browser', 10000);
  check((await row.count()) === 0, 'the approved request leaves the list');
  check(errors.length === 0, 'the link page raised no page errors: ' + errors.join('; '));

  // Last: in Firefox a custom-scheme link leaves the page deaf to later clicks.
  // "Rooms you joined": say what it does, where the click happened.
  await P.click('#nav-rooms');
  await P.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
  await P.locator('#joined-rooms').scrollIntoViewIfNeeded();
  await P.click('#joined-rooms');
  await waitFor(async () => (await text(P, '#home-message')).includes('Rooms you joined'),
    '"Rooms you joined" says what it opens', 5000);
  await waitFor(() => inView(P, '#home-message'), 'the message beside "Rooms you joined" is in view', 5000);
  check(P.url().startsWith(HTTP11), 'the home stays open after "Rooms you joined"');
}
