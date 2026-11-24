import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// Join failures a non-technical guest meets, chromium mode, on the main host:
// cast-3rw an unknown room is refused once, in words, and the page stops
// asking (no endless "Reconnecting"); a bad path shows a plain page;
// cast-uok a blocked microphone or camera is explained, with Try again, and
// a blocked camera alone still joins with sound only.
// (cast-puo, the locked call, is checked in security.mjs.)
export async function joinFailChecks() {
  const { HTTP, BASE_ARGS, chromium, browsers, logs } = ctx;

  // An unknown room: refused for good.
  const U = await openGuest('U', [], HTTP, undefined, 'U', '&room=doesnotexist123');
  await U.click('#join');
  const note = await waitFor(async () => { const t = await text(U, '#join-note > span'); return t.length > 0 ? t : null; }, 'U told the room does not exist', 20000);
  check(note.includes("doesn't exist") && !note.includes('Reconnecting') && !(await U.isVisible('#join-retry')),
    'unknown room: the guest is told in plain words: ' + note);
  const state = await text(U, '#state');
  const enabled0 = await U.locator('#join').isEnabled();
  await sleep(4000);
  const later = await text(U, '#state');
  check(state === 'this call or room does not exist' && later === state && (await U.locator('#join').isEnabled()),
    'unknown room: the page stops (no reconnect attempts), Join is available again: ' + state + ' / ' + later + ' / ' + enabled0);

  // A bad path: a plain page, not a bare "not found".
  const bad = await U.request.get(HTTP + '/no/such/page');
  const body = await bad.text();
  check(bad.status() === 404 && body.includes("This page doesn't exist") && body.includes('page-missing'),
    'a bad path shows a plain page with the way on (404)');

  // Microphone and camera blocked: no fake-ui flag and no granted permission,
  // so the browser itself denies them (the real NotAllowedError; the new
  // headless mode, channel chromium, is the one that has permission prompts).
  const args = [...BASE_ARGS.filter((a) => a !== '--use-fake-ui-for-media-stream'), '--deny-permission-prompts'];
  const denied = async (name, permissions) => {
    logs[name] = logs[name] || [];
    const browser = await chromium.launch({ headless: true, channel: 'chromium', args });
    browsers.push(browser);
    const c = await browser.newContext({ permissions, ignoreHTTPSErrors: true });
    const page = await c.newPage();
    page.on('console', (m) => logs[name].push(m.text()));
    page.on('pageerror', (e) => logs[name].push('[pageerror] ' + e.message));
    await page.goto(HTTP + '/?name=' + name);
    await page.waitForSelector('#join');
    return page;
  };
  const M = await denied('M', []);
  await M.click('#join');
  const help = await waitFor(async () => { const t = await text(M, '#join-note > span'); return t.length > 0 ? t : null; }, 'M told how to allow the microphone', 20000);
  check(help.includes('address bar') && help.includes('microphone') && !help.includes('NotAllowedError')
    && await M.isVisible('#join-retry') && !(await M.isVisible('#participant')),
    'blocked microphone: plain help with Try again, back in the lobby: ' + help);
  check(await M.locator('#join').isEnabled(), 'blocked microphone: Join is available again');

  // Only the camera blocked (microphone allowed): in the call with sound only, and told.
  const K = await denied('K', ['microphone']);
  await K.check('input[type=checkbox]').catch(() => {});
  await K.click('#join');
  await waitFor(async () => (await text(K, '#participant')).length > 0, 'K joined without a camera', 30000);
  const cam = await waitFor(async () => { const t = await text(K, '#call-notes'); return t.length > 0 ? t : null; }, 'K told the camera is blocked', 10000);
  check(cam.includes('sound only') && cam.includes('address bar') && await K.isVisible('#call-notes'),
    'blocked camera: joins with sound only and says so: ' + cam);
}
