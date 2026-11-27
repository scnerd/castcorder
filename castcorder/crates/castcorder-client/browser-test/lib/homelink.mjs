import { spawn } from 'node:child_process';
import { rmSync, writeFileSync } from 'node:fs';
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
  // The rooms the app's call window joined elsewhere, as it hands them to the
  // app (cast-n56.9; the host keeps the list in its library): shown as cards.
  writeFileSync(join(LIB, 'joined-rooms.json'), JSON.stringify([
    { url: 'https://rv.example.org/v/1/#hostid.secret', host: 'hostid', room: 'Their-show', last_ms: 1700000000000,
      calls: [{ id: 'c1', started_ms: 1699990000000, takes: 1 }, { id: 'c2', started_ms: 1700000000000, takes: 2 }] },
    { url: 'https://rv.example.org/v/1/#gone.secret', host: 'gone', room: 'Old-show', removed: true, calls: [] },
  ]));
  await P.goto(HTTP11 + '/');
  await P.waitForSelector('#joined-cards li[data-joined-room="Their-show"]', { timeout: 15000 });
  const theirs = P.locator('#joined-cards li[data-joined-room="Their-show"]');
  check((await theirs.textContent()).includes('2 calls'), 'the home lists a room the app joined elsewhere as a card with its calls');
  check((await theirs.locator('a').getAttribute('href')) === 'castcorder://rv.example.org/v/1/#hostid.secret',
    'its card opens the room in the app\x27s call window');
  const old = P.locator('#joined-cards li[data-joined-room="Old-show"]');
  check((await old.textContent()).includes('Removed') && (await old.locator('a').count()) === 0,
    'a room the device was removed from says so and offers no way in');
  // Replacing a room's link asks first (cast-7jv): it breaks every copy of the
  // old link, so one click on the menu item does nothing until confirmed.
  const mainCard = P.locator('#rooms li[data-room="main-show"]');
  await mainCard.locator('.room-menu summary').click();
  await mainCard.locator('.regen-room-link').click();
  await waitFor(async () => await mainCard.locator('.room-replace').isVisible(), 'the room asks before replacing its link', 5000);
  check((await mainCard.locator('.room-replace').textContent()).includes('old link stops working'),
    'the question says the old link stops working');
  check(!(await text(P, '#home-message')).includes('New call link made'), 'nothing was replaced by asking');
  await mainCard.locator('.cancel-replace').click();
  await waitFor(async () => !(await mainCard.locator('.room-replace').isVisible()), 'Cancel closes the question', 5000);
  check(true, 'Cancel leaves the link alone');
  // The You card is in Settings, a place of the home's left nav (cast-n56.1).
  await P.click('#nav-settings');
  await P.waitForSelector('#person #link-device', { timeout: 15000 });
  // The card fills its fields from the host once; type after that.
  await waitFor(async () => (await P.inputValue('#device-name')).length > 0, 'the You card loaded the person', 10000);
  await P.fill('#person-name', 'Pat Person');
  await P.click('#save-person');
  await waitFor(async () => (await text(P, '#person-message')) === 'Saved.', 'the You card says it saved the name', 10000);
  check((await text(P, '#linked-devices .this-device .small')).split('fingerprint ')[1]?.trim().length > 0,
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
  // cast-40e.6: Remove asks once more, then revokes the device.
  const lab = P.locator('#linked-devices li[data-device]:has-text("Lab browser")');
  await lab.locator('.revoke-device').click();
  check((await lab.locator('.revoke-device').innerText()).trim() === 'Remove for good?', 'Remove asks once more before revoking');
  check((await lab.count()) === 1, 'the first press of Remove keeps the device');
  await lab.locator('.revoke-device').click();
  await waitFor(async () => (await lab.count()) === 0, 'the revoked browser leaves the You card', 10000);
  check((await text(P, '#person-message')) === 'Device removed.', 'the You card says the device was removed');
  check(errors.length === 0, 'the link page raised no page errors: ' + errors.join('; '));

  // The Rooms explanation is a dismissible tip that stays gone (cast-n56.12).
  await P.click('#nav-rooms');
  const tip = P.locator('[data-hint="rooms-link"]');
  check(await tip.isVisible(), 'the Rooms page shows its explanation as a tip');
  await tip.locator('.hint-dismiss').click();
  check(!(await tip.isVisible()), 'Got it hides the tip');
  await P.reload();
  await P.click('#nav-rooms');
  check(!(await P.locator('[data-hint="rooms-link"]').isVisible()), 'a dismissed tip stays gone after a reload');
  // The left nav is pinned: scrolling the page's content leaves it in place.
  const navTop = () => P.evaluate(() => Math.round(document.querySelector('.home-nav').getBoundingClientRect().top));
  await P.evaluate(() => { const m = document.querySelector('.home-main'); m.scrollTop = m.scrollHeight; window.scrollTo(0, document.body.scrollHeight); });
  check((await navTop()) === 0, 'the left nav stays pinned at the top while the page scrolls');

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
  // cast-qhv: a click after that link still works (in Firefox the page went deaf).
  await P.fill('#new-room-name', 'after-the-link');
  await P.click('#create-room');
  await waitFor(async () => (await text(P, '#rooms')).includes('after-the-link'),
    'the home still takes clicks after "Rooms you joined"', 10000);
}
