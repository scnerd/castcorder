// Leave on a phone (cast-abe): in a phone-sized viewport, joined, the Leave
// call button is shown, enabled, wholly on screen and nothing covers it (a
// trial click runs Playwright's actionability checks without leaving).
// The phone layout (cast-n56.7): a one-row header, a labelled dock of Mute,
// Camera, People (Recordings for a guest), More and Leave, the own tile as an
// inset once someone else is in, and a side panel that opens as a sheet below
// the header so it never covers the REC pill. The viewport is put back
// afterwards.
import { check } from './helpers.mjs';

const PHONES = [{ width: 390, height: 844 }, { width: 360, height: 640 }];

export async function phoneLeaveChecks(page, n) {
  const before = page.viewportSize();
  for (const size of PHONES) {
    await page.setViewportSize(size);
    const leave = page.locator('#leave');
    const visible = await leave.isVisible();
    const enabled = await leave.isEnabled();
    const box = await leave.boundingBox();
    const onScreen = !!box && box.x >= 0 && box.y >= 0
      && box.x + box.width <= size.width && box.y + box.height <= size.height;
    let clickable = true;
    try {
      await leave.click({ trial: true, timeout: 3000 });
    } catch {
      clickable = false;
    }
    check(visible && enabled && onScreen && clickable, 'guest ' + n + ' at ' + size.width + 'x' + size.height
      + ' can leave: Leave call visible ' + visible + ', enabled ' + enabled + ', on screen ' + onScreen
      + ' (' + JSON.stringify(box) + '), clickable ' + clickable);
  }
  await page.setViewportSize(PHONES[0]);
  const W = PHONES[0].width;
  const dock = await page.evaluate(() => ['#mute', '#camera', '#dock-people', '#dock-more', '#leave'].map((sel) => {
    const el = document.querySelector(sel);
    const r = el && el.getBoundingClientRect();
    const label = el && (el.parentElement.querySelector('.dock-label') || el);
    return { sel, shown: !!r && r.width > 0 && r.height > 0, label: label ? label.innerText.trim() : '' };
  }));
  const hidden = await page.evaluate(() => ['#self-view', '#devices', '#open-people', '#open-recordings', '#open-transcript', '#transcript-side']
    .filter((sel) => { const el = document.querySelector(sel); return el && el.getBoundingClientRect().width > 0; }));
  check(dock.every((d) => d.shown && d.label.length > 0) && hidden.length === 0,
    'guest ' + n + ' phone dock: five labelled buttons ' + JSON.stringify(dock) + ', desktop-only controls shown: ' + JSON.stringify(hidden));
  const bar = await page.locator('.topbar').boundingBox();
  const pill = await page.locator('.rec-slot').boundingBox();
  check(!!bar && bar.height <= 60 && !!pill && pill.x + pill.width <= W,
    'guest ' + n + ' phone header is one row with the REC pill in view: ' + JSON.stringify(bar) + ' pill ' + JSON.stringify(pill));
  const inset = await page.evaluate(() => {
    if (!document.querySelector('#remote .tile')) return null;
    const r = document.querySelector('.stage > .tile').getBoundingClientRect();
    return { w: r.width, h: r.height };
  });
  if (inset) check(inset.w <= 120, 'guest ' + n + ' phone: own tile is an inset beside the others: ' + JSON.stringify(inset));
  // The side panel opens as a sheet below the header: the REC pill stays uncovered.
  const wasOpen = (await page.getAttribute('#dock-people', 'aria-pressed')) === 'true';
  if (!wasOpen) await page.click('#dock-people');
  await page.waitForSelector('.side', { timeout: 5000 });
  const side = await page.locator('.side').boundingBox();
  const pillFree = await page.evaluate(() => {
    const r = document.querySelector('.rec-slot').getBoundingClientRect();
    const el = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
    return !!el && !!el.closest('.topbar');
  });
  check(!!side && !!bar && side.y >= bar.y + bar.height - 1 && pillFree,
    'guest ' + n + ' phone: the side panel opens below the header, not over the REC pill: ' + JSON.stringify(side));
  if (!wasOpen) await page.click('#dock-people');
  // The Devices dialog fits the phone (cast-lg7), opened from More, and Escape closes it (cast-3u9).
  await page.click('#dock-more');
  await page.click('#more-devices');
  const dlg = await page.locator('#devices-dialog').boundingBox();
  const close = await page.locator('#devices-close').boundingBox();
  const inside = (b) => !!b && b.x >= 0 && b.y >= 0 && b.x + b.width <= PHONES[0].width && b.y + b.height <= PHONES[0].height;
  check(inside(dlg) && inside(close), 'guest ' + n + ' Devices dialog fits 390x844: ' + JSON.stringify(dlg) + ' close ' + JSON.stringify(close));
  await page.keyboard.press('Escape');
  check((await page.locator('#devices-dialog').count()) === 0, 'guest ' + n + ' Escape closes the Devices dialog');
  if (before) await page.setViewportSize(before);
}
