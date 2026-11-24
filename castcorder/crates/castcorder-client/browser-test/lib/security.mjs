import { spawn } from 'node:child_process';
import { readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { sleep, waitFor, text, check } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// Producers and room security (cast-xne), chromium mode: a second host with
// a fresh library and the default producer-gated call, and new browsers
// (each a new device): C waits until the host machine's page H joins; C, a
// non-producer, is refused StartTake on the control channel; H grants C the
// producer role and C starts and stops a take; a locked call refuses a new
// untrusted device D; a wrong password refuses E and the right one admits
// it; H kicks E, who cannot rejoin; H regenerates the link.
export async function securityChecks() {
  const { exe, dist, RESULTS, host, logs } = ctx;
  const LIB2 = join(RESULTS, 'library-security');
  rmSync(LIB2, { recursive: true, force: true });
  ctx.host2 = spawn(exe, ['--dist', dist, '--library', LIB2, '--room', 'security-test', '--bind', '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0'], { stdio: ['ignore', 'pipe', 'pipe'] });
  ctx.host2.stderr.on('data', (d) => { ctx.host2Log += d.toString(); });
  const [HTTP2, HTTPS2] = await new Promise((resolve, reject) => {
    let buf = '';
    const t = setTimeout(() => reject(new Error('security host did not print HOST_READY')), 20000);
    ctx.host2.stdout.on('data', (d) => {
      buf += d.toString();
      const m = buf.match(/HOST_READY http=(\S+) https=(\S+)/);
      if (m) { clearTimeout(t); resolve([m[1], m[2]]); }
    });
  });
  const joined = (p) => text(p, '#participant').then((s) => s.length > 0);
  const state = (p) => text(p, '#state');

  // Gated call: C waits until a producer is in the call.
  const C = await openGuest('C', [], HTTPS2);
  await C.click('#join');
  await waitFor(async () => (await state(C)).includes('waiting for the host or a producer'), 'C waiting for a producer', 15000);
  await sleep(3000);
  check(!(await joined(C)) && (await state(C)).includes('waiting'), 'gated call: guest C waits while no producer is in the call');
  const H = await openGuest('H', [], HTTP2);
  await H.click('#join');
  await waitFor(() => joined(H), 'host machine page H joined', 30000);
  await waitFor(() => joined(C), 'C admitted once the producer joined', 30000);
  check(true, 'gated call: C entered once the host machine\x27s page joined');

  // A non-producer cannot start a take, even by sending the request itself.
  await H.waitForSelector('#take-start', { timeout: 10000 });
  check((await C.locator('#take-start').count()) === 0, 'non-producer C has no producer panel');
  await C.evaluate(() => document.getElementById('debug-start-take').click());
  const refusedStart = await waitFor(async () => { const r = await text(C, '#debug-result'); return r ? r : null; }, 'C StartTake answered', 10000);
  check(refusedStart.includes('refused') && refusedStart.includes('producer'), 'host refuses StartTake from a non-producer on the control channel: ' + refusedStart);
  check((await text(H, '#take-status')).startsWith('idle'), 'no take started by the non-producer');

  // The host machine grants the producer role to trusted C, who starts and stops a take.
  await waitFor(async () => (await text(H, '#guests')).includes('C (guest'), 'H lists C', 10000);
  check(/C \(guest \d+\) \[trusted\]/.test(await text(H, '#guests')), 'C joined with a producer present and is trusted');
  await H.click('#guests li[data-name="C"] button.grant');
  await C.waitForSelector('#take-start', { timeout: 10000 });
  check((await C.locator('button.grant').count()) === 0, 'granted producer C gets the panel but no grant/revoke controls');
  await waitFor(async () => C.isEnabled('#take-start'), 'C can start a take', 10000);
  await C.click('#take-start');
  await waitFor(async () => (await text(C, '#take-result')).includes('"take":1'), 'C started take 1', 10000);
  await waitFor(async () => (await text(H, '#take-status')).startsWith('recording'), 'H sees take 1 recording', 10000);
  await sleep(1500);
  await waitFor(async () => C.isEnabled('#take-stop'), 'C can stop the take', 10000);
  await C.click('#take-stop');
  await waitFor(async () => (await text(C, '#take-status')).startsWith('idle'), 'C stopped take 1', 10000);
  check(true, 'producer C (granted by the host machine) started and stopped take 1');

  // Gated or always open: only the host machine's page has the toggle; the
  // change shows on every producer's panel.
  check((await text(C, '#room-open')) === 'producer-gated', 'the call starts producer-gated');
  check((await C.locator('#open-toggle').count()) === 0, 'granted producer C has no gated/open toggle');
  await H.click('#open-toggle');
  await waitFor(async () => (await text(C, '#room-open')) === 'always open' && (await text(H, '#room-open')) === 'always open', 'call made always open for everyone', 10000);
  await H.click('#open-toggle');
  await waitFor(async () => (await text(C, '#room-open')) === 'producer-gated' && (await text(H, '#room-open')) === 'producer-gated', 'call made producer-gated again', 10000);
  check(true, 'host machine flipped open and gated; producer C saw both and has no toggle');

  // Lock: a new, untrusted device is refused.
  await H.click('#lock');
  await waitFor(async () => (await text(H, '#room-locked')) === 'locked', 'call locked', 10000);
  const D = await openGuest('D', [], HTTPS2);
  await D.click('#join');
  await waitFor(async () => (await state(D)) === 'the call is locked', 'D refused by the lock', 20000);
  check((await text(D, '#errors')).includes('locked') && !(await joined(D)), 'a locked call refuses a new untrusted device: ' + (await text(D, '#errors')));
  // cast-puo: the reason is in plain words beside Join, with Try again, and
  // the producer on the host machine is told someone tried.
  const note = await waitFor(async () => { const t = await text(D, '#join-note'); return t.includes('locked') ? t : null; }, 'D sees the plain reason beside Join', 10000);
  check(await D.isVisible('#join-note') && await D.isVisible('#join-retry') && !note.includes('rejected by the host')
    && await D.evaluate(() => document.getElementById('join-note').getBoundingClientRect().top < window.innerHeight),
    'locked call: the plain reason shows in view beside Join, with Try again: ' + note);
  await waitFor(async () => (await text(H, '#call-notes')).includes('D tried to join'), 'H is told D tried to join the locked call', 10000);
  check(await H.isVisible('#call-notes'), 'locked call: the producer is told that D tried to join');
  await H.click('#lock');
  await waitFor(async () => (await text(H, '#room-locked')) === 'unlocked', 'call unlocked', 10000);

  // Password: wrong refused, right admitted.
  await H.fill('#new-password', 'pw-1');
  await H.click('#set-password');
  await waitFor(async () => (await text(H, '#room-password')) === 'password set', 'password set', 10000);
  const E = await openGuest('E', [], HTTPS2);
  await E.fill('#password', 'wrong');
  await E.click('#join');
  await waitFor(async () => (await state(E)) === 'wrong password', 'E refused: wrong password', 20000);
  check((await text(E, '#errors')).includes('wrong password') && !(await joined(E)), 'a wrong password is refused: ' + (await text(E, '#errors')));
  await E.fill('#password', 'pw-1');
  await E.click('#join');
  await waitFor(() => joined(E), 'E joined with the right password', 30000);
  check(true, 'the right password is admitted');

  // Kick: E is removed and cannot rejoin.
  await waitFor(async () => (await text(H, '#guests')).includes('E (guest'), 'H lists E', 10000);
  await H.click('#guests li[data-name="E"] button.kick');
  await waitFor(async () => (await state(E)) === 'removed from the call by a producer', 'E removed', 30000);
  const kicks = () => (ctx.host2Log.match(/device was kicked/g) || []).length;
  const before = kicks();
  await E.click('#join');
  await waitFor(async () => kicks() > before && (await state(E)) === 'removed from the call by a producer', 'E rejoin refused', 30000);
  check(!(await joined(E)), 'a kicked device cannot rejoin (even with the password)');

  // Regenerate link (no rendezvous here: the secret rotates on disk).
  const secretFile = join(LIB2, 'security-test', 'call-secret');
  const oldSecret = readFileSync(secretFile, 'utf8');
  await H.click('#regen-link');
  await waitFor(async () => readFileSync(secretFile, 'utf8') !== oldSecret, 'call secret rotated', 10000);
  check((await joined(C)) && (await joined(H)), 'regenerating the link rotates the call secret; everyone in the call stays');

  const room = JSON.parse(readFileSync(join(LIB2, 'security-test', 'room.json'), 'utf8'));
  const devs = Object.values(room.devices);
  check(devs.some((d) => d.name === 'E' && d.banned) && devs.some((d) => d.name === 'C' && d.producer && d.trusted)
    && !JSON.stringify(room).includes('pw-1'),
    'room.json in the call folder remembers devices (C producer and trusted, E banned), password only hashed');
  check(/security decision/.test(ctx.host2Log) && /allowed=false/.test(ctx.host2Log), 'the host logs its security decisions');
}
