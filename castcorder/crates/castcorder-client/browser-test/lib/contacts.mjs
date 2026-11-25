import { spawn } from 'node:child_process';
import { rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { check, text, waitFor } from './helpers.mjs';

// Contacts (docs/design.md "Contacts", cast-40e.2) through a rendezvous's
// mailbox, on the host machine's home in a real browser:
// - "Add a contact" on the first host's home makes a one-time contact code
//   naming the rendezvous; a second host install (another person) asks with
//   it through the mailbox; the request shows on the home by name and
//   fingerprint, is accepted there, and each install lists the other;
// - presence: each install sees the other online through the mailbox, and
//   the home shows Bob online by device name; appear offline toggles there;
// - a nickname given on the home shows in the list;
// - the home trusts the contact in a room with the room card's picker, and
//   stops trusting them;
// - one-sided: the second install removes the first; the first still lists
//   it. Asking again, the home declines: the second install is told so and
//   lists nobody.
// `HTTP`: the first host's localhost listener (http://...); `rendezvous`: host:port.
export async function contactsChecks({ HTTP, rendezvous }) {
  const { exe, dist, RESULTS, chromium, browsers, logs } = ctx;
  const native = { 'X-Castcorder-Native': '1', 'Content-Type': 'application/json' };
  const call = async (base, path, body) => {
    const r = await fetch(base + path, body === undefined
      ? { headers: native } : { method: 'POST', headers: native, body: JSON.stringify(body) });
    if (!r.ok) throw new Error(path + ' answered ' + r.status + ': ' + (await r.text()));
    return r.json();
  };

  // A second install, of its own person.
  const LIB = join(RESULTS, 'library-contacts-2');
  rmSync(LIB, { recursive: true, force: true });
  ctx.host10 = spawn(exe, ['--dist', dist, '--library', LIB, '--room', 'contacts-2', '--bind', '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--rendezvous', 'http://' + rendezvous], { stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  ctx.host10Log = '';
  ctx.host10.stdout.on('data', (d) => { out += d.toString(); });
  ctx.host10.stderr.on('data', (d) => { ctx.host10Log += d.toString(); });
  const HTTP2 = (await waitFor(() => out.match(/HOST_READY http=(\S+)/), 'contacts: second host ready', 20000))[1];
  await waitFor(() => ctx.host10Log.includes('registered with the rendezvous'), 'contacts: second host registered', 30000);
  await call(HTTP, '/person', { name: 'Ann Host' });
  await call(HTTP2, '/person', { name: 'Bob Guest' });
  const [ann, bob] = [await call(HTTP, '/person'), await call(HTTP2, '/person')];
  check(ann.id !== bob.id, 'contacts: the two installs are two persons');

  // The first host's home, in a browser.
  const browser = await chromium.launch({ headless: true });
  browsers.push(browser);
  const page = await (await browser.newContext()).newPage();
  logs.K = [];
  page.on('console', (m) => logs.K.push(m.text()));
  page.on('pageerror', (e) => logs.K.push('[pageerror] ' + e.message));
  await page.goto(HTTP + '/');
  await page.waitForSelector('#add-contact');
  await page.click('#add-contact');
  const link = await waitFor(async () => {
    const all = await page.locator('#contact-codes code').allTextContents();
    return all.find((l) => l.startsWith('http://' + rendezvous + '/'));
  }, 'contacts: the home shows a contact code naming the rendezvous', 15000, 250);
  check(link.includes('#contact.'), 'contacts: the home made a contact code: ' + link);

  // Bob's install asks with it, through the mailbox.
  await call(HTTP2, '/contacts/add', { code: link });
  await page.waitForSelector('#contact-requests li .accept-contact', { timeout: 30000 });
  const asked = await text(page, '#contact-requests li');
  check(asked.includes('Bob Guest wants to add you as a contact') && asked.includes(bob.fingerprint),
    'contacts: the request reached the home through the mailbox, by name and fingerprint: ' + asked);
  check((await call(HTTP2, '/contacts')).adding.state === 'waiting', 'contacts: Bob waits for Ann to accept');
  await page.click('#contact-requests li .accept-contact');
  const added = await waitFor(async () => {
    const a = (await call(HTTP2, '/contacts')).adding;
    return a && a.state !== 'waiting' ? a : null;
  }, 'contacts: Bob hears the answer', 30000, 500);
  check(added.state === 'added' && added.detail === 'Ann Host', 'contacts: Bob was added: ' + JSON.stringify(added));
  check((await call(HTTP2, '/contacts')).contacts.some((c) => c.id === ann.id && c.name === 'Ann Host'),
    'contacts: Bob lists Ann');
  await waitFor(async () => (await text(page, '#contact-list')).includes('Bob Guest'), 'contacts: the home lists Bob', 10000);
  check((await call(HTTP, '/contacts')).contacts.some((c) => c.id === bob.id), 'contacts: Ann lists Bob');

  // Presence (cast-40e.3): sealed pings through the mailbox, sent once they
  // pair, show each online to the other; the home says so on Bob's row.
  const seen = await waitFor(async () => {
    const t = await text(page, '#contact-list li .contact-presence');
    return t.startsWith('Online on ') ? t : null;
  }, 'contacts: the home shows Bob online', 30000, 250);
  check(seen === 'Online on ' + bob.device_name, 'contacts: Bob online by his device name: ' + seen);
  await waitFor(async () => {
    const c = (await call(HTTP2, '/contacts')).contacts.find((x) => x.id === ann.id);
    return c && c.online.length === 1;
  }, 'contacts: Bob sees Ann online', 30000, 250);
  check(true, 'contacts: Bob sees Ann online through the mailbox');
  // Appear offline, on the home, is kept (and taken back).
  await page.click('#appear-offline');
  await waitFor(async () => (await call(HTTP, '/contacts')).appear_offline, 'contacts: Ann appears offline', 10000);
  await page.click('#appear-offline');
  await waitFor(async () => !(await call(HTTP, '/contacts')).appear_offline, 'contacts: Ann appears online again', 10000);
  check(true, 'contacts: appear offline toggles on the home');

  // A nickname, given on the home.
  await page.fill('#contact-list li .contact-nickname', 'Bobby');
  await page.click('#contact-list li .save-nickname');
  await waitFor(async () => (await text(page, '#contact-list li .contact-name')) === 'Bobby (Bob Guest)',
    'contacts: the home shows the nickname', 10000);
  check(true, 'contacts: the nickname shows beside the name');

  // Room trust for Bob, picked on a room's card.
  const card = page.locator('#rooms li.room-card').first();
  await card.locator('.trust-pick').selectOption(bob.id);
  await card.locator('.trust-person').click();
  await waitFor(async () => (await card.locator('.trusted-person').count()) === 1, 'contacts: the room lists Bob as trusted', 15000);
  const room = await card.getAttribute('data-room');
  const trusted = (await call(HTTP, '/rooms')).rooms.find((r) => r.name === room).persons;
  check(trusted.length === 1 && trusted[0] === bob.id, 'contacts: the room trusts Bob\x27s person: ' + JSON.stringify(trusted));
  // Contact links (cast-40e.4): Bob's install opens one to Ann's through
  // the mailbox and learns the room she trusts him in, with its call link.
  const shared = await call(HTTP2, '/contacts/rooms?person=' + encodeURIComponent(ann.id));
  const rooms = shared.installs.flatMap((i) => i.rooms);
  check(shared.installs.length === 1 && rooms.some((r) => r.room === room && r.link),
    'contacts: over a contact link Bob learns Ann shares ' + room + ' with its call link: ' + JSON.stringify(shared));
  // And the home asks Bob's install the same way, from his row.
  await page.click('#contact-list li .shared-rooms');
  const line = await waitFor(async () => {
    const t = await text(page, '#contact-list li .contact-shared');
    return t && t !== 'Asking...' ? t : null;
  }, 'contacts: the home shows the rooms Bob shares', 30000, 250);
  check(line.startsWith('No rooms you share') || line.startsWith('Rooms you share'),
    'contacts: the home asked Bob over a contact link: ' + line);
  await card.locator('.untrust-person').click();
  await waitFor(async () => (await card.locator('.trusted-person').count()) === 0, 'contacts: the room no longer trusts Bob', 15000);
  check(true, 'contacts: trust taken back on the room card');

  // One-sided: Bob removes Ann; Ann still lists Bob.
  await call(HTTP2, '/contacts/remove', { person: ann.id });
  check((await call(HTTP2, '/contacts')).contacts.length === 0, 'contacts: Bob removed Ann');
  check((await call(HTTP, '/contacts')).contacts.some((c) => c.id === bob.id), 'contacts: removing is one-sided: Ann still lists Bob');
  // Bob asks again with a new code; the home declines.
  const again = (await call(HTTP, '/contacts/code', {})).links.find((l) => l.startsWith('http://' + rendezvous + '/'));
  await call(HTTP2, '/contacts/add', { code: again });
  await page.waitForSelector('#contact-requests li .decline-contact', { timeout: 30000 });
  await page.click('#contact-requests li .decline-contact');
  const declined = await waitFor(async () => {
    const a = (await call(HTTP2, '/contacts')).adding;
    return a && a.state !== 'waiting' ? a : null;
  }, 'contacts: Bob hears the decline', 30000, 500);
  check(declined.state === 'failed' && declined.detail === 'declined', 'contacts: declined: ' + JSON.stringify(declined));
  check((await call(HTTP2, '/contacts')).contacts.length === 0, 'contacts: after a decline Bob lists nobody');
  check(!logs.K.some((l) => l.startsWith('[pageerror]')), 'contacts: the home raised no page errors');
  ctx.host10.kill();
}
