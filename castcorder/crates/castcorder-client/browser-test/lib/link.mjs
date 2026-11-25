// Call link (cast-s5g): the host machine's page shows the call link in the
// lobby before anyone joins (no rendezvous in this run, so the host's LAN
// https link), copyable; once joined, the producer's top bar shows it too.
import { check, waitFor } from './helpers.mjs';
import { ctx } from './ctx.mjs';

// The host prints its https URL with or without the trailing slash.
const HTTPS_LINK_OF = () => ctx.HTTPS.replace(/\/?$/, '/');

export async function lobbyLinkChecks(A) {
  const lobby = A.locator('#call-link-lobby');
  const shown = await waitFor(async () => (await lobby.textContent()) || '', 'the lobby shows a call link', 15000);
  check(shown === HTTPS_LINK_OF(), 'the host machine\'s lobby shows the host\'s https link (' + shown + ')');
  await A.locator('.lobby [data-copy-link]').click();
  check((await A.locator('.lobby [data-copy-link]').textContent()).includes('Copied'), 'Copy in the lobby answers Copied');
  if (ctx.MODE !== 'firefox') {
    const clip = await A.evaluate(() => navigator.clipboard.readText());
    check(clip === HTTPS_LINK_OF(), 'the lobby Copy puts the link on the clipboard');
  }
}

export async function topBarLinkChecks(A, B) {
  const top = A.locator('#call-link-top');
  // The link sits behind Invite (cast-n56.5).
  check(!(await top.isVisible()), 'the top bar keeps the call link behind Invite');
  await A.click('#invite');
  const shown = await waitFor(async () => ((await top.isVisible()) && (await top.textContent())) || '', 'the top bar shows a call link', 15000);
  check(shown === HTTPS_LINK_OF(), 'the producer\'s top bar shows the call link (' + shown + ')');
  check((await B.locator('#call-link-top').count()) === 0 && (await B.locator('#invite').count()) === 0,
    'a guest\'s top bar has no Invite and no link');
  await A.click('#invite');
}
