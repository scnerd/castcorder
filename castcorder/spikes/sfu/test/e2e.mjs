// End-to-end spike test: real host binary + 3 headless Chromium guests with
// fake media devices. Run from the spike directory with:  npm test
//
// Asserts (via RTCPeerConnection.getStats in each page):
//   1. every page receives audio AND video RTP from each of the other two
//      (packets and bytes increasing over a 2 s window, frames decoded > 0
//      and increasing for video);
//   2. data channel echo works and a 50 MB guest->host blob hash matches;
//   3. a guest that leaves gracefully and rejoins recovers (both directions);
//   4. a guest whose page is killed without saying bye is detected by the host
//      and recovers after rejoining.
// Everything printed is ASCII.

import { spawn, spawnSync } from 'node:child_process';
import { createWriteStream, mkdirSync, writeFileSync, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
// Browsers live in node_modules (installed by `npm run setup`), not in a
// user-wide cache.
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const { chromium, firefox } = await import('playwright');
// Browser engine per guest A,B,C, e.g. CC_MIX=chromium,firefox,chromium
const MIX = (process.env.CC_MIX || 'chromium,chromium,chromium').split(',');

const OUT = join(ROOT, 'test-results');
mkdirSync(OUT, { recursive: true });
const BLOB_MB = Number(process.env.CC_BLOB_MB || 50);
const results = { checks: [], numbers: {} };
let failures = 0;

function check(name, ok, detail) {
  results.checks.push({ name, ok, detail });
  console.log((ok ? 'PASS ' : 'FAIL ') + name + (detail ? '  ' + detail : ''));
  if (!ok) failures++;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---------------------------------------------------------------- host
if (!process.env.CC_SKIP_BUILD) {
  const features = process.env.CC_FEATURES;
  const args = ['build', '--release'];
  if (features) args.push('--no-default-features', '--features', features);
  console.log('building host: cargo ' + args.join(' '));
  const b = spawnSync('cargo', args, { cwd: ROOT, stdio: 'inherit', shell: false });
  if (b.status !== 0) { console.log('FAIL cargo build'); process.exit(1); }
}
const exe = process.env.CC_HOST_EXE
  || join(ROOT, 'target', 'release', process.platform === 'win32' ? 'host.exe' : 'host');
if (!existsSync(exe)) { console.log('FAIL host binary missing: ' + exe); process.exit(1); }

const hostLog = createWriteStream(join(OUT, 'host.log'));
const host = spawn(exe, [], {
  cwd: ROOT,
  env: { ...process.env, RUST_LOG: process.env.RUST_LOG || 'host=info,str0m=warn' },
  stdio: ['ignore', 'pipe', 'pipe'],
});
host.stderr.pipe(hostLog);
const HTTP = await new Promise((resolve, reject) => {
  let buf = '';
  const t = setTimeout(() => reject(new Error('host did not print HOST_READY')), 20000);
  host.stdout.on('data', (d) => {
    hostLog.write(d);
    buf += d.toString();
    const m = buf.match(/HOST_READY http=(\S+) udp=(\S+)/);
    if (m) { clearTimeout(t); resolve(m[1]); }
  });
  host.on('exit', (c) => reject(new Error('host exited early with ' + c)));
});
console.log('host up at ' + HTTP);

const browsers = {};
async function cleanup() {
  for (const b of Object.values(browsers)) { try { await b.close(); } catch (e) {} }
  try { host.kill(); } catch (e) {}
}

async function hostStats() {
  const r = await fetch(HTTP + '/stats');
  return r.json();
}

// ---------------------------------------------------------------- pages
if (MIX.includes('chromium')) {
  browsers.chromium = await chromium.launch({
    headless: true,
    args: [
      '--use-fake-device-for-media-stream',
      '--use-fake-ui-for-media-stream',
      '--allow-loopback-in-peer-connection',
      '--autoplay-policy=no-user-gesture-required',
    ],
  });
}
if (MIX.includes('firefox')) {
  browsers.firefox = await firefox.launch({
    headless: true,
    firefoxUserPrefs: {
      'media.navigator.streams.fake': true,
      'media.navigator.permission.disabled': true,
      'media.peerconnection.ice.loopback': true,
      'media.autoplay.default': 0,
    },
  });
}
for (const [k, b] of Object.entries(browsers)) {
  console.log(k + ' ' + b.version());
  results.numbers[k] = b.version();
}
const engineOf = (name) => MIX[['A', 'B', 'C'].indexOf(name)] || 'chromium';

const pages = {};
async function openGuest(name) {
  const eng = engineOf(name);
  const ctx = await browsers[eng].newContext(eng === 'chromium' ? { permissions: ['camera', 'microphone'] } : {});
  const page = await ctx.newPage();
  page.on('console', (m) => { if (process.env.CC_VERBOSE) console.log('  [page ' + name + '] ' + m.text()); });
  page.on('pageerror', (e) => console.log('  [page ' + name + ' error] ' + e.message));
  await page.goto(HTTP + '/?name=' + name);
  await page.evaluate(() => window.cc.join());
  pages[name] = { ctx, page };
  return page;
}

// Pick, for page `p`, the inbound entries coming from guest `other` (latest
// host-side client id wins, so a stale pre-rejoin stream is never counted).
function fromOrigin(stats, other) {
  const mine = stats.inbound.filter((i) => i.origin.startsWith(other + '-'));
  if (mine.length === 0) return {};
  const maxId = Math.max(...mine.map((i) => Number(i.origin.split('-')[1])));
  const out = {};
  for (const i of mine) if (Number(i.origin.split('-')[1]) === maxId) out[i.kind] = i;
  return out;
}

// One evaluation: every listed page receives from every other listed page.
async function flowSnapshot(names) {
  const s1 = {}, s2 = {};
  for (const n of names) s1[n] = await pages[n].page.evaluate(() => window.cc.stats());
  await sleep(2000);
  for (const n of names) s2[n] = await pages[n].page.evaluate(() => window.cc.stats());
  const rows = [];
  let ok = true;
  for (const rx of names) {
    for (const tx of names) {
      if (rx === tx) continue;
      const a = fromOrigin(s1[rx], tx), b = fromOrigin(s2[rx], tx);
      for (const kind of ['audio', 'video']) {
        const x = a[kind], y = b[kind];
        let good = !!(x && y && x.mid === y.mid
          && y.packetsReceived > x.packetsReceived && y.bytesReceived > x.bytesReceived);
        if (good && kind === 'video') good = y.framesDecoded > 0 && y.framesDecoded > x.framesDecoded;
        if (!good) ok = false;
        rows.push({
          rx, tx, kind, good,
          pkts_per_s: y && x ? Math.round((y.packetsReceived - x.packetsReceived) / 2) : 0,
          kbit_s: y && x ? Math.round((y.bytesReceived - x.bytesReceived) * 8 / 2 / 1000) : 0,
          frames: y ? y.framesDecoded : 0,
          fps: y && x ? Math.round((y.framesDecoded - x.framesDecoded) / 2) : 0,
          size: y && y.frameWidth ? y.frameWidth + 'x' + y.frameHeight : '',
          codec: y ? y.codec : '',
          lost: y ? y.packetsLost : 0,
          pli: y ? y.pliCount : 0,
          firstFrameMs: s2[rx].firstFrameMs ? s2[rx].firstFrameMs[(y || {}).origin] : undefined,
        });
      }
    }
  }
  return { ok, rows };
}

async function waitForFlow(names, label, timeoutMs = 30000) {
  const t0 = Date.now();
  let last;
  while (Date.now() - t0 < timeoutMs) {
    last = await flowSnapshot(names);
    if (last.ok) break;
  }
  const ms = Date.now() - t0;
  for (const r of last.rows) {
    console.log('    ' + r.tx + ' -> ' + r.rx + ' ' + r.kind.padEnd(5) + ' ' + (r.good ? 'ok  ' : 'BAD ')
      + r.pkts_per_s + ' pkt/s ' + r.kbit_s + ' kbit/s'
      + (r.kind === 'video' ? ' frames=' + r.frames + ' fps=' + r.fps + ' ' + r.size + ' pli=' + r.pli
        + ' firstFrameMs=' + r.firstFrameMs : '')
      + ' lost=' + r.lost + ' ' + r.codec);
  }
  check(label, last.ok, 'settled in ' + ms + ' ms (includes 2 s sampling windows)');
  return { ms, rows: last.rows };
}

async function waitGone(observers, gone, label, timeoutMs = 20000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    let still = false;
    for (const n of observers) {
      const s = await pages[n].page.evaluate(() => window.cc.stats());
      if (s.inbound.some((i) => i.origin.startsWith(gone + '-'))) still = true;
    }
    const hs = await hostStats();
    if (!still && !hs.clients.some((c) => c.name === gone)) {
      const ms = Date.now() - t0;
      check(label, true, 'host and peers dropped ' + gone + ' in ' + ms + ' ms');
      return ms;
    }
    await sleep(100);
  }
  check(label, false, 'still present after ' + timeoutMs + ' ms');
  return null;
}

try {
  // 1. three guests, full mesh of forwarded audio+video through the host.
  for (const n of ['A', 'B', 'C']) await openGuest(n);
  const initial = await waitForFlow(['A', 'B', 'C'], 'initial: 3 guests each receive audio+video from the other 2');
  results.numbers.initial = initial;

  // 2. data channel: echo, then the blob while media keeps flowing.
  const echo = await pages.A.page.evaluate(() => window.cc.echo(200));
  check('data channel echo x200', echo.ok, 'median ' + echo.median_ms.toFixed(2) + ' ms, max ' + echo.max_ms.toFixed(2) + ' ms');
  results.numbers.echo = echo;

  const blob = await pages.A.page.evaluate((mb) => window.cc.blob(mb * 1024 * 1024), BLOB_MB);
  check(BLOB_MB + ' MB blob guest->host sha256 match', blob.match,
    'guest ' + blob.guest_ms + ' ms = ' + blob.guest_mbit_s.toFixed(1) + ' Mbit/s; host saw '
    + blob.host_ms + ' ms = ' + blob.host_mbit_s.toFixed(1) + ' Mbit/s; sha ' + blob.sha_host.slice(0, 16) + '...');
  results.numbers.blob = blob;
  const during = await waitForFlow(['A', 'B', 'C'], 'media still flowing after blob', 15000);
  results.numbers.afterBlob = during;

  // 3. graceful leave + rejoin.
  await pages.B.page.evaluate(() => window.cc.leave());
  results.numbers.gracefulLeaveMs = await waitGone(['A', 'C'], 'B', 'graceful leave: B removed from host and from A, C');
  const t3 = Date.now();
  await pages.B.page.evaluate(() => window.cc.join());
  const rejoin = await waitForFlow(['A', 'B', 'C'], 'graceful rejoin: B recovers, both directions');
  results.numbers.gracefulRejoin = { totalMs: Date.now() - t3, rows: rejoin.rows };

  // 4. ungraceful leave: kill C's page (no bye), then a fresh C joins.
  await pages.C.ctx.close();
  results.numbers.crashLeaveMs = await waitGone(['A', 'B'], 'C', 'ungraceful leave: C (page killed) detected by host', 30000);
  const t4 = Date.now();
  await openGuest('C');
  const crashRejoin = await waitForFlow(['A', 'B', 'C'], 'ungraceful rejoin: new C recovers, both directions');
  results.numbers.crashRejoin = { totalMs: Date.now() - t4, rows: crashRejoin.rows };

  // Host-side counters (keyframe requests, renegotiations, bwe).
  const hs = await hostStats();
  results.numbers.host = hs;
  for (const c of hs.clients) {
    console.log('    host view ' + c.name + '#' + c.id + ': frames_in=' + c.frames_in + ' frames_out=' + c.frames_out
      + ' kf_req_sent=' + c.keyframe_req_sent + ' kf_req_recv=' + c.keyframe_req_recv
      + ' renegotiations=' + c.renegotiations + ' bwe_kbps=' + c.bwe_kbps + ' codecs=' + c.codecs_in.join(','));
  }
  check('host forwarded frames without errors', hs.clients.length === 3 && hs.clients.every((c) => c.frames_out > 0),
    'joined=' + hs.total_joined + ' left=' + hs.total_left);
} catch (e) {
  check('test ran to completion', false, String(e && e.stack || e));
} finally {
  writeFileSync(join(OUT, 'summary.json'), JSON.stringify(results, null, 2));
  await cleanup();
}

console.log(failures === 0 ? 'ALL CHECKS PASSED' : failures + ' CHECK(S) FAILED');
console.log('details: test-results/summary.json, host log: test-results/host.log');
process.exit(failures === 0 ? 0 : 1);
