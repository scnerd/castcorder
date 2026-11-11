// Builds the wasm (scalar and simd128), serves this directory on localhost
// and runs the worker benchmark in the locally installed Chrome via
// playwright-core (no browser download). Usage: npm test (in browser/)
// Env: CHROME_PATH (default: standard Windows install), MEM_MB, OPFS_MB, CUTS.
import { execFileSync } from 'node:child_process';
import { createServer } from 'node:http';
import { readFileSync, statSync, existsSync } from 'node:fs';
import { gzipSync } from 'node:zlib';
import { join, extname, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

const here = dirname(fileURLToPath(import.meta.url));
const crate = join(here, '..');
const memMb = Number(process.env.MEM_MB || 64);
const opfsMb = Number(process.env.OPFS_MB || 100);
const cuts = Number(process.env.CUTS || 3);
const chrome = process.env.CHROME_PATH || 'C:/Program Files/Google/Chrome/Application/chrome.exe';

function sh(cmd, args, cwd, extraEnv = {}) {
  execFileSync(cmd, args, { cwd, stdio: 'inherit', env: { ...process.env, ...extraEnv } });
}

function build(variant, features, rustflags, manifestDir, name, outDir) {
  const targetDir = join(crate, 'target', 'wasm-' + variant);
  const args = ['build', '--release', '--target', 'wasm32-unknown-unknown', '--target-dir', targetDir];
  if (features) args.push('--features', features);
  sh('cargo', args, manifestDir, { RUSTFLAGS: rustflags });
  const wasm = join(targetDir, 'wasm32-unknown-unknown', 'release', name + '.wasm');
  sh('wasm-bindgen', ['--target', 'web', '--no-typescript', '--out-dir', outDir, wasm], manifestDir);
  const bg = join(outDir, name + '_bg.wasm');
  const bytes = readFileSync(bg);
  return { raw: statSync(bg).size, gzip: gzipSync(bytes, { level: 9 }).length };
}

const sizes = {};
sizes.scalar = build('scalar', '', '', crate, 'castcorder_transfer_spike', join(here, 'pkg-scalar'));
sizes.simd = build('simd', 'wasm-simd', '-C target-feature=+simd128', crate, 'castcorder_transfer_spike', join(here, 'pkg-simd'));
sizes.baseline_blake3_only = build('baseline', '', '', join(crate, 'size-baseline'), 'size_baseline', join(here, 'pkg-baseline'));

const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm' };
const server = createServer((req, res) => {
  const p = join(here, decodeURIComponent(new URL(req.url, 'http://x').pathname));
  if (!p.startsWith(here) || !existsSync(p) || statSync(p).isDirectory()) { res.writeHead(404); res.end(); return; }
  res.writeHead(200, { 'content-type': types[extname(p)] || 'application/octet-stream' });
  res.end(readFileSync(p));
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const port = server.address().port;

let failed = false;
const browser = await chromium.launch({ executablePath: chrome, headless: true });
const report = { sizes, runs: {} };
try {
  for (const pkg of ['pkg-scalar', 'pkg-simd']) {
    const page = await browser.newPage();
    page.on('console', (m) => console.log('[browser]', m.text()));
    await page.goto(`http://localhost:${port}/index.html?pkg=${pkg}&mem=${memMb}&opfs=${opfsMb}&cuts=${cuts}`);
    await page.waitForFunction(() => window.__result, null, { timeout: 600000, polling: 500 });
    const r = await page.evaluate(() => window.__result);
    report.runs[pkg] = r;
    if (r.error || !r.memory || !r.memory.decoded_equal || !r.opfs || !r.opfs.verified || r.opfs.rejected !== 2) failed = true;
    await page.close();
  }
} finally {
  await browser.close();
  server.close();
}
console.log(JSON.stringify(report, null, 2));
console.log(failed ? 'BROWSER TEST FAILED' : 'BROWSER TEST PASSED');
process.exit(failed ? 1 : 0);
