// Theme toggle (cast-a3h), every mode: in the lobby, Light then Dark set the
// `data-theme` attribute on <html> and change the colours; the choice survives
// a reload (localStorage, applied before first paint); System removes it.
import { check, waitFor } from './helpers.mjs';

const attr = (p) => p.evaluate(() => document.documentElement.getAttribute('data-theme'));
const bg = (p) => p.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--bg').trim());

export async function themeChecks(page) {
  const pick = (t) => page.locator('.lobby [data-theme-set="' + t + '"]').click();
  await pick('light');
  check((await attr(page)) === 'light', 'Light sets data-theme=light on <html>');
  const light = await bg(page);
  await pick('dark');
  check((await attr(page)) === 'dark', 'Dark sets data-theme=dark on <html>');
  const dark = await bg(page);
  check(light !== dark, 'the theme changes the colours (' + light + ' vs ' + dark + ')');
  await pick('light');
  await page.reload();
  check((await attr(page)) === 'light', 'the Light choice survives a reload');
  await waitFor(async () => (await page.locator('.lobby [data-theme-set="light"][aria-pressed="true"]').count()) === 1,
    'the toggle shows the remembered choice', 10000);
  await pick('system');
  check((await attr(page)) === null, 'System removes data-theme');
  await page.reload();
  check((await attr(page)) === null, 'System survives a reload');
}
