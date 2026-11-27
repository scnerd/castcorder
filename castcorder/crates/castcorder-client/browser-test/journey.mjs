// Two people, two installs, two home networks: the whole contact journey in
// real browsers (docs/design.md "Contacts", "Calling a contact"). Each person
// has a castcorder-host install on a computer behind its own router
// (lib/netns.mjs: Linux network namespaces with masquerading routers, so it
// runs as root; CI uses sudo). Their browsers run inside their computers'
// namespaces, so each home is that computer's own localhost page.
//
//  1. Dana (D) makes a contact link on Dana's home; Eli (E) pastes it on Eli's.
//     Each lists the other, online.
//  2. Dana adds Eli to Dana's room "Show" as a producer. Eli sees "Show" on Eli's
//     home, shared by Dana, and asks Dana's install for the rooms
//     they share over a contact link (it crosses both routers).
//  3. Dana invites Eli to the call; the invite shows on Eli's home. Both join:
//     Dana from the room card, Eli from the invite.
//  4. Dana records a take, stops it; Eli's track reaches Dana's install.
//  5. Both leave and press "Back to your rooms" to get home.
//
// Needs: target/release/castcorder-host and castcorder-rendezvous, the
// client's trunk build in ../dist. Everything printed is ASCII.

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, rmSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as net from './lib/netns.mjs';
import { check, checks, sleep, text, waitFor } from './lib/helpers.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '..');
const WORKSPACE = join(CRATE, '..', '..');
const RESULTS = join(HERE, 'test-results', 'journey');
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const { chromium } = await import('playwright');
const bin = (n) => join(process.env.CARGO_TARGET_DIR || join(WORKSPACE, 'target'), 'release', n);
const HOST_EXE = bin('castcorder-host');
const RV_EXE = bin('castcorder-rendezvous');
const dist = process.env.CASTCORDER_DIST || join(CRATE, 'dist');
const VERSION = '0.0.0';

const problem = net.available();
if (problem) {
  if (process.env.CASTCORDER_CI) { console.log('FAIL journey needs network namespaces: ' + problem); process.exit(1); }
  console.log('SKIP journey: ' + problem);
  process.exit(0);
}
for (const f of [HOST_EXE, RV_EXE, join(dist, 'index.html')]) {
  if (!existsSync(f)) { console.log('FAIL missing ' + f); process.exit(1); }
}
rmSync(RESULTS, { recursive: true, force: true });
mkdirSync(RESULTS, { recursive: true });

const procs = [];
function start(ns, exe, args, name) {
  const p = spawn('ip', [...net.exec(ns).slice(1), exe, ...args], { stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, RUST_LOG: process.env.JOURNEY_LOG || 'info' } });
  const st = { p, out: '', err: '', name };
  p.stdout.on('data', (d) => { st.out += d.toString(); });
  p.stderr.on('data', (d) => { st.err += d.toString(); });
  p.on('exit', (c) => { st.exited = c; });
  procs.push(st);
  return st;
}
const line = (st, re, what, timeout = 30000) => waitFor(() => {
  if (st.exited !== undefined) throw new Error(what + ': ' + st.name + ' exited with ' + st.exited + '\n' + st.err.slice(-2000));
  return st.out.match(re);
}, what, timeout);

const browsers = [];
const logs = { D: [], E: [], DC: [], EC: [] };
const ARGS = ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
  '--autoplay-policy=no-user-gesture-required', '--disable-background-timer-throttling', '--mute-audio',
  // The rendezvous serves plain http here (https in production): the call
  // page needs a secure context for WebCrypto.
  '--unsafely-treat-insecure-origin-as-secure=http://' + net.INET_IP + ':8080', '--no-proxy-server'];
async function person(ns, who) {
  const browser = await chromium.launch({ headless: true, args: ARGS, executablePath: net.browser(ns, chromium.executablePath()) });
  browsers.push(browser);
  const context = await browser.newContext({ permissions: ['microphone', 'camera'] });
  return context;
}
async function page(context, who) {
  const p = await context.newPage();
  p.on('console', (m) => logs[who].push(m.text()));
  p.on('pageerror', (e) => logs[who].push('[pageerror] ' + e.message));
  return p;
}
// The home's own API, as its page calls it.
const api = (p, path, body) => p.evaluate(async ([path, body]) => {
  const r = await fetch(path, body === undefined
    ? { headers: { 'X-Castcorder-Native': '1' } }
    : { method: 'POST', headers: { 'X-Castcorder-Native': '1', 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  const t = await r.text();
  if (!r.ok) throw new Error(path + ': ' + r.status + ' ' + t);
  return t ? JSON.parse(t) : null;
}, [path, body]);

let failed = null;
try {
  net.setup();
  const rv = start(net.NS.inet, RV_EXE, ['--listen', net.INET_IP + ':8080', '--stun', net.INET_IP + ':3478',
    '--client', VERSION + '=' + dist], 'rendezvous');
  await waitFor(() => rv.out.match(/RENDEZVOUS_READY/), 'rendezvous ready', 20000);
  const RV = 'http://' + net.INET_IP + ':8080';
  const host = (ns, who, room) => start(ns, HOST_EXE, ['--dist', dist, '--library', join(RESULTS, 'library-' + who),
    '--room', room, '--rendezvous', RV], 'host-' + who);
  const hd = host(net.NS.d, 'D', 'Show');
  const he = host(net.NS.e, 'E', 'Eli-room');
  const HD = (await line(hd, /HOST_READY http=(\S+)/, 'D host ready'))[1];
  const HE = (await line(he, /HOST_READY http=(\S+)/, 'E host ready'))[1];
  await waitFor(() => hd.err.includes('registered with the rendezvous') && he.err.includes('registered with the rendezvous'), 'both registered', 30000);
  console.log('two homes up behind their routers; rendezvous at ' + RV);

  const dctx = await person(net.NS.d, 'D');
  const ectx = await person(net.NS.e, 'E');
  const D = await page(dctx, 'D');
  const E = await page(ectx, 'E');
  await D.goto(HD + '/');
  await E.goto(HE + '/');
  await api(D, '/person', { name: 'Dana' });
  await api(E, '/person', { name: 'Eli' });
  const dana = await api(D, '/person');
  const eli = await api(E, '/person');

  // 1. Contacts, by link.
  await D.click('#nav-contacts');
  await D.click('#add-contact');
  await D.click('#copy-contact-link');
  const link = await waitFor(async () => (await D.locator('#contact-codes code').allTextContents()).find((l) => l.startsWith(RV + '/')),
    'Dana has a contact link', 15000, 250);
  await E.click('#nav-contacts');
  await E.click('#add-contact');
  await E.fill('#contact-code-input', link);
  await E.click('#use-contact-code');
  await waitFor(async () => (await text(E, '#contact-list')).includes('Dana'), 'Eli lists Dana', 30000);
  await waitFor(async () => (await text(D, '#contact-list')).includes('Eli'), 'Dana lists Eli', 30000);
  check(true, 'pasting Dana\'s link made them contacts on both sides');
  await waitFor(async () => (await api(E, '/contacts')).contacts.some((c) => c.id === dana.id && c.online.length > 0), 'Eli sees Dana online', 30000, 500);
  await waitFor(async () => (await api(D, '/contacts')).contacts.some((c) => c.id === eli.id && c.online.length > 0), 'Dana sees Eli online', 30000, 500);
  check(true, 'each sees the other online across the two routers');

  // 2. Dana makes Eli a producer in "Show"; Eli asks for the rooms they share.
  await api(D, '/rooms/trust', { room: 'Show', person: eli.id, trust: true, producer: true });
  await E.click('#contact-list li .contact-pick');
  const shared = await waitFor(async () => {
    const t = await text(E, '#contact-detail .contact-shared');
    return /Show|No rooms|not|fail|time/i.test(t) && !/asking/i.test(t) ? t : null;
  }, 'Eli hears which rooms Eli shares with Dana', 40000, 500).catch(() => null);
  check(shared && shared.includes('Show'), 'Eli asked Dana\'s install over a contact link and hears they share Show: ' + shared);

  // Eli's home lists Show under "Shared with you", with Join: no need to
  // look under the contact.
  await E.click('#nav-rooms');
  const sharedCard = E.locator('#shared-rooms li.shared-room[data-room="Show"]');
  await waitFor(async () => (await sharedCard.count()) === 1 && await sharedCard.isVisible(), 'Show under Shared with you', 45000, 500).catch(() => null);
  check(await sharedCard.count() === 1 && (await sharedCard.textContent()).includes("Dana's room"),
    'Eli\'s home lists Show under "Shared with you" as Dana\'s room');
  const sharedHref = (await sharedCard.count()) ? await sharedCard.locator('.join-shared').getAttribute('href') : '';
  check(sharedHref.startsWith('castcorder://'), 'its Join opens the room in the app: ' + sharedHref.replace(/#.*/, '#...'));

  // 3. Dana invites Eli to the call from the room card; the invite reaches
  // Eli's home over a contact link.
  await D.click('#nav-rooms');
  const card = D.locator('#rooms li[data-room="Show"]');
  await card.locator('.room-menu summary').click();
  await card.locator('.invite-toggle').click();
  await card.locator('.invite-contact .send-invite').first().click();
  await waitFor(async () => (await text(D, '#rooms li[data-room="Show"] .invite-state')) === 'Invited', 'the invite reaches Eli', 30000, 500);
  await E.click('#nav-rooms');
  const invite = E.locator('#invites li.invite');
  await waitFor(async () => (await invite.count()) === 1, 'Eli\'s home shows the invite', 15000, 500);
  check((await invite.textContent()).includes('Dana invites you to Show'), 'Eli\'s home says Dana invites Eli to Show');
  // In the app Join opens castcorder://, which the app maps back to the link.
  const href = await invite.locator('.join-invite').getAttribute('href');
  const callLink = href.replace(/^castcorder:\/\//, 'http://');
  check(callLink.startsWith(RV + '/v/'), 'the invite carries Dana\'s call link: ' + callLink.replace(/#.*/, '#...'));

  // Both join: Dana from the room card, Eli from the invite.
  const DC = await page(dctx, 'DC');
  await DC.goto(HD + '/?room=Show&name=Dana');
  const EC = await page(ectx, 'EC');
  await EC.goto(callLink.replace('/#', '/?name=Eli#'));
  const state = (p) => text(p, '#state');
  await DC.waitForSelector('#join');
  await DC.click('#join');
  await waitFor(async () => (await state(DC)) === 'connected', 'Dana in the call', 40000);
  await EC.waitForSelector('#join');
  await EC.click('#join');
  await waitFor(async () => (await state(EC)) === 'connected', 'Eli in the call (across both routers)', 60000);
  check(true, 'Dana and Eli are both in the call, each from their own home network');
  check(EC.url().startsWith(RV), 'Eli joined on the call link from the invite');

  // 4. Dana records a take; Eli's track reaches Dana's install.
  await waitFor(async () => DC.isEnabled('#take-start'), 'Dana can start a take', 20000);
  await DC.click('#take-start');
  await waitFor(async () => (await text(DC, '#take-status')).startsWith('recording'), 'take 1 recording', 15000);
  await sleep(4000);
  await DC.click('#take-stop');
  await waitFor(async () => (await text(DC, '#take-status')).startsWith('idle'), 'take 1 stopped', 15000);
  const lib = join(RESULTS, 'library-D', 'Show');
  const eliTrack = await waitFor(() => {
    for (const call of existsSync(lib) ? readdirSync(lib) : []) {
      const take = join(lib, call, 'take01');
      if (!existsSync(take)) continue;
      const f = readdirSync(take).find((n) => n.startsWith('take01_Eli') && n.endsWith('_mic.wav'));
      if (f) return join(call, 'take01', f);
    }
    return null;
  }, 'Eli\'s take 1 track on Dana\'s install', 60000, 1000).catch(() => null);
  check(eliTrack !== null, 'Eli\'s recording of take 1 reached Dana\'s install: ' + eliTrack);

  // 5. Both leave and go home.
  for (const [p, who] of [[EC, 'Eli'], [DC, 'Dana']]) {
    await p.click('#leave');
    await waitFor(async () => (await state(p)) === 'left', who + ' left', 15000);
  }
  check(true, 'both left the call');
  // The way back is on the screen (#160): "Back to your rooms" on the host
  // machine's own page, which also goes there by itself after 20 s. Eli's
  // browser here stands in for the app's call window, whose button calls the
  // app (show_home) to bring its home window forward; a plain browser has no
  // home to go back to, so Eli opens Eli's install's home page as the app would.
  const shown = await waitFor(async () => (await DC.locator('#left-home').count()) === 1, 'Dana sees Back to your rooms', 10000).then(() => true, () => false);
  check(shown, 'Dana sees "Back to your rooms" after leaving');
  if (shown) await DC.click('#left-home');
  await waitFor(async () => (await DC.locator('#nav-rooms').count()) === 1, 'Dana on the home', 15000);
  await EC.goto(HE + '/');
  await waitFor(async () => (await DC.locator('#rooms li[data-room="Show"]').count()) === 1, 'Dana back home with Show', 15000);
  await EC.click('#nav-contacts');
  await waitFor(async () => (await text(EC, '#contact-list')).includes('Dana'), 'Eli back home with Dana', 15000);
  check(true, 'both are back on their homes');
} catch (e) {
  failed = e;
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  for (const b of browsers) await b.close().catch(() => {});
  for (const st of procs) { try { st.p.kill('SIGTERM'); } catch { /* gone */ } }
  await sleep(500);
  for (const st of procs) {
    const { writeFileSync } = await import('node:fs');
    writeFileSync(join(RESULTS, st.name + '.log'), st.out + '\n--- stderr ---\n' + st.err);
  }
  for (const [k, v] of Object.entries(logs)) {
    const { writeFileSync } = await import('node:fs');
    writeFileSync(join(RESULTS, 'browser-' + k + '.log'), v.join('\n'));
  }
  net.teardown();
}
const bad = checks.filter((c) => !c.ok);
console.log('logs: ' + RESULTS);
console.log(failed || bad.length ? 'FAIL journey (' + bad.length + ' checks failed)' : 'PASS journey: two people on two home networks, contacts to call and back');
process.exit(failed || bad.length ? 1 : 0);
