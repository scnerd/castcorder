import { spawn } from 'node:child_process';
import { existsSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { ctx } from './ctx.mjs';
import { VERSION } from './version.mjs';
import { sleep, waitFor, text, check } from './helpers.mjs';
import { openGuest } from './guest.mjs';

// Rejoining (cast-zr8), Chromium, guests through a rendezvous of their own
// (the page is served by the rendezvous, so it loads with the host offline):
// - a guest that left lists the room in its lobby ("Rooms you joined") as live,
//   with who is in it, and the list survives a reload;
// - Rejoin lands in the room's current call, and so does Rejoin this call
//   on the current call;
// - a guest naming a past call that is not current is refused (not a
//   producer) and its next Join takes the current call; the row says a
//   producer can continue it;
// - once the host machine grants it the producer role, Continue this call
//   continues the past call;
// - a kicked device's room says Removed and offers no rejoin;
// - with the host stopped, a reloaded page still lists the room, as offline.
// `rendezvous` (host:port): use that one (system.mjs passes the Worker under
// wrangler dev, cast-lxt.8) instead of starting a castcorder-rendezvous.
export async function rejoinChecks({ rendezvous } = {}) {
  const { exe, dist, RESULTS } = ctx;
  const RV_EXE = join(exe, '..', process.platform === 'win32' ? 'castcorder-rendezvous.exe' : 'castcorder-rendezvous');
  if (!rendezvous && !existsSync(RV_EXE)) { check(false, 'rejoin: missing ' + RV_EXE); return; }
  const LIB = join(RESULTS, 'library-rejoin' + (rendezvous ? '-rv' : ''));
  rmSync(LIB, { recursive: true, force: true });
  const out = (p, name) => {
    const st = { out: '', err: '' };
    p.stdout.on('data', (d) => { st.out += d.toString(); });
    p.stderr.on('data', (d) => { st.err += d.toString(); ctx[name + 'Log'] = st.err; });
    return st;
  };
  let RV = rendezvous;
  if (!RV) {
    ctx.rv8 = spawn(RV_EXE, ['--listen', '127.0.0.1:0', '--stun', '127.0.0.1:0', '--client', VERSION + '=' + dist], { stdio: ['ignore', 'pipe', 'pipe'] });
    const rv = out(ctx.rv8, 'rv8');
    RV = (await waitFor(() => rv.out.match(/RENDEZVOUS_READY http=(\S+)/), 'RENDEZVOUS_READY', 20000))[1];
  }
  ctx.host8 = spawn(exe, ['--dist', dist, '--library', LIB, '--room', 'rejoin-show', '--bind', '127.0.0.1',
    '--port', '0', '--http-port', '0', '--udp-port', '0', '--rendezvous', 'http://' + RV], { stdio: ['ignore', 'pipe', 'pipe'] });
  const h = out(ctx.host8, 'host8');
  const HTTP = (await waitFor(() => h.out.match(/HOST_READY http=(\S+)/), 'HOST_READY', 20000))[1];
  const link = (await waitFor(() => h.out.match(/CALL_LINK (\S+)/), 'CALL_LINK', 20000))[1];
  const [base, frag] = link.split('/#');
  const rooms = async () => {
    const r = await fetch(HTTP + '/rooms', { headers: { 'X-Castcorder-Native': '1' } });
    return (await r.json()).rooms.find((x) => x.name === 'rejoin-show');
  };
  const state = (p) => text(p, '#state');
  const connected = (p, who) => waitFor(async () => (await state(p)) === 'connected', who + ' connected', 40000);
  const card = (p) => p.locator('#my-rooms li[data-room="rejoin-show"]');
  const chip = async (p) => (await card(p).locator('[data-status]').getAttribute('data-status')) || '';
  const leave = async (p) => { await p.click('#leave'); await waitFor(async () => (await state(p)) === 'left', 'left', 15000); };

  // The producer at the host machine opens the room; two guests join by link.
  const P = await openGuest('RP', [], HTTP, undefined, 'Pat', '&room=rejoin-show');
  await P.click('#join');
  await connected(P, 'P');
  const first = await waitFor(async () => text(P, '#current-call'), 'P sees the call', 15000);
  const G = await openGuest('RG', [], base, undefined, 'Gus', '#' + frag);
  const K = await openGuest('RK', [], base, undefined, 'Kim', '#' + frag);
  check((await G.locator('#my-rooms').isHidden()), 'a browser that was in no call lists no rooms');
  for (const [p, n] of [[G, 'G'], [K, 'K']]) { await p.click('#join'); await connected(p, n); }

  // G leaves: its lobby lists the room, live, with who is in it.
  await leave(G);
  await waitFor(async () => (await card(G).count()) === 1, 'G lists the room', 15000);
  await waitFor(async () => (await chip(G)) === 'Live', 'G sees the room live', 20000);
  const who = await text(G, '#my-rooms li[data-room="rejoin-show"] .in-call');
  check(who.includes('Pat') && who.includes('Kim') && !who.includes('Gus'), 'the list says who is in the call: ' + who);
  check((await card(G).locator('.call-row[data-call="' + first + '"]').count()) === 1, 'the list holds the call G was in');
  await G.reload();
  await G.waitForSelector('#join');
  await waitFor(async () => (await card(G).count()) === 1 && (await chip(G)) === 'Live', 'the list survives a reload', 20000);
  check(true, 'the rooms list survives a reload and shows the room live');

  // Rejoin: the room's current call; then Rejoin this call on that call.
  await card(G).locator('.rejoin-room').click();
  await connected(G, 'G (rejoin)');
  check((await text(P, '#current-call')) === first, 'Rejoin lands in the room\x27s current call');
  await leave(G);
  const row = (p, c) => card(p).locator('.call-row[data-call="' + c + '"]');
  await waitFor(async () => (await row(G, first).locator('.rejoin-call').count()) === 1, 'the current call offers Rejoin this call', 20000);
  await row(G, first).locator('.rejoin-call').click();
  await connected(G, 'G (rejoin call)');
  check(h.err.includes('the room\x27s current call'), 'Rejoin this call names the current call and lands in it');
  await leave(G);

  // The producer moves everyone to a new call, then all leave: the first
  // call is past. G (no producer) naming it is refused; the next Join takes
  // the current call.
  await P.click('#new-call');
  const second = await waitFor(async () => { const c = await text(P, '#current-call'); return c && c !== first ? c : null; }, 'a new call', 20000);
  for (const p of [P, K]) await leave(p);
  await waitFor(async () => !(await rooms()).live, 'the room empties', 20000);
  await G.reload();
  await G.waitForSelector('#join');
  await waitFor(async () => (await row(G, first).locator('.needs-producer').count()) === 1, 'a past call says a producer can continue it', 20000);
  check((await row(G, first).locator('.continue-call').count()) === 0, 'a guest that is not a producer is not offered Continue');
  await G.goto(base + '/?name=Gus&call=' + encodeURIComponent(first) + '#' + frag);
  await G.waitForSelector('#join');
  check((await text(G, '#rejoin-target')).includes(first), 'a page opened to rejoin a call says so');
  await P.click('#join');
  await connected(P, 'P');
  await G.click('#join');
  await waitFor(async () => (await state(G)).includes('not the room\x27s current call'), 'G refused: not the current call', 40000);
  check((await text(P, '#current-call')) === second, 'the refused rejoin moved nobody');
  await G.click('#join');
  await connected(G, 'G (current call)');
  check((await text(P, '#current-call')) === second, 'after the refusal Join takes the current call');

  // Granted the producer role, G continues the past call.
  await waitFor(async () => (await P.locator('#guests li[data-name="Gus"] button.grant').count()) === 1, 'P can grant G', 15000);
  await P.click('#guests li[data-name="Gus"] button.grant');
  await sleep(1000);
  await leave(P);
  await leave(G);
  await waitFor(async () => !(await rooms()).live, 'the room empties again', 20000);
  await G.reload();
  await G.waitForSelector('#join');
  await waitFor(async () => (await row(G, first).locator('.continue-call').count()) === 1, 'a producer is offered Continue this call', 30000);
  await row(G, first).locator('.continue-call').click();
  await connected(G, 'G (continue)');
  const r = await rooms();
  check(r.live && r.current === first, 'a producer from that call continued it: live in ' + r.current);
  await P.click('#join');
  await connected(P, 'P');
  check((await text(P, '#current-call')) === first, 'whoever joins next lands in the continued call');

  // Kicked: the room says Removed, with no way back in.
  await waitFor(async () => (await P.locator('#guests li[data-name="Gus"] button.kick').count()) === 1, 'P can kick G', 15000);
  await P.click('#guests li[data-name="Gus"] button.kick');
  await waitFor(async () => (await state(G)) === 'removed from the call by a producer', 'G removed', 30000);
  await waitFor(async () => (await chip(G)) === 'Removed', 'G\x27s list says Removed', 20000);
  check(await card(G).locator('.rejoin-room').isHidden(), 'a kicked device is offered no Rejoin');
  check((await card(G).locator('.rejoin-call, .continue-call').count()) === 0, 'nor any call');

  // The link is regenerated (cast-lot): K (not trusted) is told plainly to ask
  // the host for the new link, not "Status unknown", and is offered no Rejoin.
  await P.click('#regen-link');
  // The host rotates the secret asynchronously: ask K only after it has.
  await waitFor(() => h.err.includes('call secret regenerated'), 'the host regenerated the link', 15000);
  await K.reload();
  await K.waitForSelector('#join');
  await waitFor(async () => (await card(K).count()) === 1 && (await chip(K)) === 'Link changed', 'K\x27s room says the link changed', 40000);
  check((await chip(K)) === 'Link changed', 'after the link is regenerated K\x27s room says the link changed, not unknown: ' + (await chip(K)));
  check((await card(K).locator('.link-changed-note').innerText()).includes('ask the host for the new link'), 'the room says to ask the host for the new link');
  check(await card(K).locator('.rejoin-room').isHidden(), 'no Rejoin is offered with a link that no longer works');

  // The host stops: K's page (served by the rendezvous) reloads and still
  // lists the room, offline.
  await leave(P);
  ctx.host8.kill();
  await waitFor(() => ctx.host8.exitCode !== null || ctx.host8.signalCode, 'the rejoin host stopped', 15000);
  await K.reload();
  await K.waitForSelector('#join');
  await waitFor(async () => (await card(K).count()) === 1 && (await chip(K)) === 'Host offline', 'K lists the room with the host offline', 30000);
  check((await card(K).locator('.call-row').count()) >= 1, 'the list shows the calls with the host offline');
  if (ctx.rv8) ctx.rv8.kill();
}
