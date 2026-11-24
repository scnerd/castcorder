// Leave on a phone (cast-abe): in a phone-sized viewport, joined, the Leave
// call button is shown, enabled, wholly on screen and nothing covers it (a
// trial click runs Playwright's actionability checks without leaving). The
// viewport is put back afterwards.
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
  // The Devices dialog fits the phone (cast-lg7) and Escape closes it (cast-3u9).
  await page.setViewportSize(PHONES[0]);
  await page.click('#devices');
  const dlg = await page.locator('#devices-dialog').boundingBox();
  const close = await page.locator('#devices-close').boundingBox();
  const inside = (b) => !!b && b.x >= 0 && b.y >= 0 && b.x + b.width <= PHONES[0].width && b.y + b.height <= PHONES[0].height;
  check(inside(dlg) && inside(close), 'guest ' + n + ' Devices dialog fits 390x844: ' + JSON.stringify(dlg) + ' close ' + JSON.stringify(close));
  await page.keyboard.press('Escape');
  check((await page.locator('#devices-dialog').count()) === 0, 'guest ' + n + ' Escape closes the Devices dialog');
  if (before) await page.setViewportSize(before);
}
