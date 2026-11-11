// Real-browser test for castcorder-transport (ADR 0003's real boundary), ported
// from the SFU spike: the browser_host example on the real UDP socket port plus 3
// headless Chromium guests with fake camera and microphone. Passes when every
// guest receives audio AND video from each of the other two (packets and bytes
// rising over a 2 s window, video frames decoded and rising).
//
// Run from the repo root with:  task test:browser
// (which installs Playwright and Chromium into this directory's node_modules,
// builds the example, and runs this script). Everything printed is ASCII.

import { spawn } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const WORKSPACE = join(HERE, '..', '..', '..');
// Browsers live in node_modules (installed by setup.mjs), not a user-wide cache.
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const { chromium } = await import('playwright');

const exe = join(WORKSPACE, 'target', 'release', 'examples',
  process.platform === 'win32' ? 'browser_host.exe' : 'browser_host');
if (!existsSync(exe)) { console.log('FAIL host binary missing: ' + exe); process.exit(1); }

const NAMES = ['A', 'B', 'C'];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const host = spawn(exe, [], { stdio: ['ignore', 'pipe', 'pipe'] });
let hostLog = '';
host.stderr.on('data', (d) => { hostLog += d.toString(); });
const HTTP = await new Promise((resolve, reject) => {
  let buf = '';
  const t = setTimeout(() => reject(new Error('host did not print HOST_READY')), 20000);
  host.stdout.on('data', (d) => {
    buf += d.toString();
    const m = buf.match(/HOST_READY http=(\S+)/);
    if (m) { clearTimeout(t); resolve(m[1]); }
  });
  host.on('exit', (c) => reject(new Error('host exited early with ' + c)));
});
console.log('host up at ' + HTTP);

const browser = await chromium.launch({
  headless: true,
  args: [
    '--use-fake-device-for-media-stream',
    '--use-fake-ui-for-media-stream',
    '--allow-loopback-in-peer-connection',
    '--autoplay-policy=no-user-gesture-required',
  ],
});
console.log('chromium ' + browser.version());

// The inbound entries page `rx` has from guest `tx` (stream id "<name>-<id>").
function fromOrigin(stats, tx) {
  const out = {};
  for (const i of stats.inbound) if (i.origin.startsWith(tx + '-')) out[i.kind] = i;
  return out;
}

async function flowSnapshot(pages) {
  const s1 = {}, s2 = {};
  for (const n of NAMES) s1[n] = await pages[n].evaluate(() => window.cc.stats());
  await sleep(2000);
  for (const n of NAMES) s2[n] = await pages[n].evaluate(() => window.cc.stats());
  const rows = [];
  let ok = true;
  for (const rx of NAMES) {
    for (const tx of NAMES) {
      if (rx === tx) continue;
      const a = fromOrigin(s1[rx], tx), b = fromOrigin(s2[rx], tx);
      for (const kind of ['audio', 'video']) {
        const x = a[kind], y = b[kind];
        let good = !!(x && y && y.packetsReceived > x.packetsReceived && y.bytesReceived > x.bytesReceived);
        if (good && kind === 'video') good = y.framesDecoded > x.framesDecoded;
        if (!good) ok = false;
        const pkts = x && y ? Math.round((y.packetsReceived - x.packetsReceived) / 2) : 0;
        rows.push(tx + ' -> ' + rx + ' ' + kind.padEnd(5) + ' ' + (good ? 'ok  ' : 'BAD ') + pkts + ' pkt/s'
          + (kind === 'video' && y ? ' frames=' + y.framesDecoded : ''));
      }
    }
  }
  return { ok, rows };
}

let passed = false;
try {
  const pages = {};
  for (const n of NAMES) {
    const ctx = await browser.newContext({ permissions: ['camera', 'microphone'] });
    const page = await ctx.newPage();
    page.on('pageerror', (e) => console.log('  [page ' + n + ' error] ' + e.message));
    await page.goto(HTTP + '/?name=' + n);
    await page.evaluate(() => window.cc.join());
    pages[n] = page;
  }
  const t0 = Date.now();
  let last;
  while (Date.now() - t0 < 30000) {
    last = await flowSnapshot(pages);
    if (last.ok) break;
  }
  for (const r of last.rows) console.log('    ' + r);
  passed = last.ok;
  console.log((passed ? 'PASS' : 'FAIL') + ' 3 guests each receive audio+video from the other 2 ('
    + (Date.now() - t0) + ' ms)');
} catch (e) {
  console.log('FAIL test did not complete: ' + String(e && e.stack || e));
} finally {
  await browser.close().catch(() => {});
  host.kill();
}
if (!passed) {
  console.log('host log tail:');
  console.log(hostLog.split('\n').slice(-40).join('\n'));
}
mkdirSync(join(HERE, 'test-results'), { recursive: true });
writeFileSync(join(HERE, 'test-results', 'host.log'), hostLog);
console.log('host log: browser-test/test-results/host.log');
process.exit(passed ? 0 : 1);
