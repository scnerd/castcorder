// Theme (cast-a3h; cast-n56.2 moved the choice to the home's Settings >
// Appearance): Light then Dark set the `data-theme` attribute on <html> and
// change the colours; the choice survives a reload (localStorage, applied
// before first paint) and reaches the lobby on the same origin; System
// removes it. The lobby itself has no theme control any more.
import { check, waitFor } from './helpers.mjs';

const attr = (p) => p.evaluate(() => document.documentElement.getAttribute('data-theme'));
const bg = (p) => p.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--bg').trim());

export async function themeChecks(lobby) {
  check((await lobby.locator('.lobby [data-theme-set]').count()) === 0, 'the lobby has no theme control (it is in Settings)');
  const page = await lobby.context().newPage();
  await page.goto(new URL(lobby.url()).origin + '/');
  await page.click('#nav-settings');
  const pick = (t) => page.locator('#settings-appearance [data-theme-set="' + t + '"]').click();
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
  await page.click('#nav-settings');
  await waitFor(async () => (await page.locator('#settings-appearance [data-theme-set="light"][aria-pressed="true"]').count()) === 1,
    'Settings shows the remembered choice', 10000);
  await lobby.reload();
  check((await attr(lobby)) === 'light', 'the lobby on the same origin follows the choice');
  await pick('system');
  check((await attr(page)) === null, 'System removes data-theme');
  await page.reload();
  check((await attr(page)) === null, 'System survives a reload');
  await lobby.reload();
  check((await attr(lobby)) === null, 'the lobby is back on System');
  await page.close();
}
