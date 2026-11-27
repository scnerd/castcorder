import { spawn } from 'node:child_process';
import { rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { check, text, waitFor } from './helpers.mjs';

// Link requests (persons and linked devices, docs/design.md) through a
// rendezvous's mailbox (cast-lxt.8; system.mjs runs this against
// castcorder-rendezvous and against the Worker under wrangler dev):
// - the host machine makes a one-time link; its rendezvous form opens in a
//   fresh browser profile, which asks to be linked (sealed to the host key,
//   through the mailbox, asking again every 1.5 s); the request shows on the
//   host machine, is approved there, and the browser says it is linked;
// - a second host install links itself to the first one's person through
//   the same mailbox (castcorder-host's own mailbox client), with a second
//   link: approved on the first, it says linked.
// `HTTP`: the first host's localhost listener (http://...); `rendezvous`: host:port.
export async function personLinkChecks({ HTTP, rendezvous }) {
  const { exe, dist, RESULTS, chromium, browsers, logs } = ctx;
  const native = { 'X-Castcorder-Native': '1', 'Content-Type': 'application/json' };
  const person = async (base, path = '', body) => {
    const r = await fetch(base + '/person' + path, body === undefined
      ? { headers: native } : { method: 'POST', headers: native, body: JSON.stringify(body) });
    if (!r.ok) throw new Error('/person' + path + ' answered ' + r.status + ': ' + (await r.text()));
    return r.json();
  };
  const newLink = async () => {
    const code = await person(HTTP, '/code', {});
    return code.links.find((l) => l.startsWith('http://' + rendezvous + '/'));
  };
  await person(HTTP, '', { name: 'Pat Person' });

  // A browser profile.
  const link = await newLink();
  check(!!link && link.includes('#link.'), 'the host machine makes a link naming the rendezvous: ' + (link || 'none'));
  if (!link) return;
  const browser = await chromium.launch({ headless: true });
  browsers.push(browser);
  const page = await (await browser.newContext()).newPage();
  logs.L = [];
  page.on('console', (m) => logs.L.push(m.text()));
  page.on('pageerror', (e) => logs.L.push('[pageerror] ' + e.message));
  await page.goto(link);
  await page.waitForSelector('#link-this:not([disabled])');
  await page.fill('#link-name', 'Lab browser');
  await page.click('#link-this');
  const req = await waitFor(async () => (await person(HTTP)).requests.find((r) => r.name === 'Lab browser'),
    'the link request reaches the host machine through the mailbox', 30000, 500);
  check(true, 'the browser\x27s link request reached the host machine through the mailbox (' + req.kind + ', ' + req.fingerprint + ')');
  check((await text(page, '#link-fingerprint')).includes(req.fingerprint), 'the browser shows the fingerprint the host machine lists');
  await person(HTTP, '/decide', { device: req.device, approve: true });
  const state = await waitFor(async () => { const t = await text(page, '#link-state'); return t.startsWith('Linked') || t.startsWith('Not linked') ? t : null; },
    'the browser hears the approval', 30000);
  check(state === 'Linked to Pat Person.', 'the browser was linked after approval, asking through the mailbox: ' + state);
  check((await person(HTTP)).devices.some((d) => d.name === 'Lab browser'), 'the host machine lists the linked browser');
  check(!logs.L.some((l) => l.startsWith('[pageerror]')), 'the link page raised no page errors');

  // A link works once (docs/v1-manual-test.md, Linking): the same link opened again
  // in another profile is refused, and nothing waits for approval on the host machine.
  const again = await (await browser.newContext()).newPage();
  await again.goto(link);
  await again.waitForSelector('#link-this:not([disabled])');
  await again.fill('#link-name', 'Second try');
  await again.click('#link-this');
  const refused = await waitFor(async () => { const t = await text(again, '#link-state'); return t.startsWith('Not linked') ? t : null; },
    'the used link is refused', 30000, 500).catch(() => null);
  check(refused !== null, 'a used link is refused the second time: ' + (refused || (await text(again, '#link-state'))));
  check(!(await person(HTTP)).requests.some((r) => r.name === 'Second try'), 'the host machine lists no request from the used link');
  await again.context().close();

  // Removing a linked device takes it off the person's list (the app's "Remove for good?").
  const added = (await person(HTTP)).devices.find((d) => d.name === 'Lab browser');
  await person(HTTP, '/revoke', { device: added.id });
  check(!(await person(HTTP)).devices.some((d) => d.name === 'Lab browser'), 'a revoked device is gone from the person\x27s devices');


  const LIB = join(RESULTS, 'library-person-2');
  rmSync(LIB, { recursive: true, force: true });
  ctx.host9 = spawn(exe, ['--dist', dist, '--library', LIB, '--room', 'person-2', '--bind', '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--rendezvous', 'http://' + rendezvous], { stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  ctx.host9Log = '';
  ctx.host9.stdout.on('data', (d) => { out += d.toString(); });
  ctx.host9.stderr.on('data', (d) => { ctx.host9Log += d.toString(); });
  const HTTP2 = (await waitFor(() => out.match(/HOST_READY http=(\S+)/), 'second host ready', 20000))[1];
  await waitFor(() => ctx.host9Log.includes('registered with the rendezvous'), 'second host registered', 30000);
  await person(HTTP2, '/join', { code: await newLink() });
  const req2 = await waitFor(async () => (await person(HTTP)).requests[0], 'the second install\x27s link request', 30000, 500);
  check(req2.kind === 'app', 'the second install\x27s link request reached the first through the mailbox (' + req2.kind + ')');
  await person(HTTP, '/decide', { device: req2.device, approve: true });
  const joined = await waitFor(async () => {
    const j = (await person(HTTP2)).joining;
    return j && j.state !== 'waiting' ? j : null;
  }, 'the second install hears the approval', 30000, 500);
  check(joined.state === 'linked', 'the second install linked itself to the first one\x27s person through the mailbox: ' + JSON.stringify(joined));
  const [one, two] = [await person(HTTP), await person(HTTP2)];
  check(one.id === two.id, 'both installs now share one person (' + one.id + ')');
  ctx.host9.kill();
}
