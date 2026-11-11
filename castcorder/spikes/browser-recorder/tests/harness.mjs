// Test harness: static server for dist/ and a Chromium we spawn ourselves
// (so the crash test can kill the whole process tree) and drive over CDP.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { spawn, execSync } from 'node:child_process';
import { chromium } from '@playwright/test';

const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1')), '..');
export const WORK = path.join(ROOT, 'work');
export const DIST = path.join(ROOT, 'dist');
export const INPUT_WAV = path.join(WORK, 'input-48k-s16-stereo.wav');

const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json' };

export function serve({ coi = true } = {}) {
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://x');
    let file = path.join(DIST, decodeURIComponent(url.pathname));
    if (url.pathname === '/' || !fs.existsSync(file)) file = path.join(DIST, 'index.html');
    const headers = { 'Content-Type': MIME[path.extname(file)] || 'application/octet-stream', 'Cache-Control': 'no-store' };
    if (coi) { headers['Cross-Origin-Opener-Policy'] = 'same-origin'; headers['Cross-Origin-Embedder-Policy'] = 'require-corp'; }
    res.writeHead(200, headers);
    fs.createReadStream(file).pipe(res);
  });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve({ server, url: `http://127.0.0.1:${server.address().port}` })));
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function launch(profileDir, { wav = INPUT_WAV, extraArgs = [] } = {}) {
  const profile = path.resolve(profileDir);
  fs.mkdirSync(profile, { recursive: true });
  const portFile = path.join(profile, 'DevToolsActivePort');
  fs.rmSync(portFile, { force: true });
  const args = [
    `--user-data-dir=${profile}`, '--remote-debugging-port=0', '--headless=new', '--no-sandbox',
    '--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream',
    `--use-file-for-fake-audio-capture=${wav}`, '--autoplay-policy=no-user-gesture-required',
    '--no-first-run', '--no-default-browser-check', '--disable-background-timer-throttling',
    ...extraArgs, 'about:blank',
  ];
  const proc = spawn(chromium.executablePath(), args, { stdio: 'ignore' });
  let port;
  for (let i = 0; i < 200 && !port; i++) {
    await sleep(100);
    if (fs.existsSync(portFile)) port = fs.readFileSync(portFile, 'utf8').split('\n')[0].trim();
  }
  if (!port) throw new Error('chromium did not report a DevTools port');
  const browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`);
  const context = browser.contexts()[0];
  const page = context.pages()[0] || (await context.newPage());
  const kill = async () => {
    try {
      if (process.platform === 'win32') execSync(`taskkill /F /T /PID ${proc.pid}`, { stdio: 'ignore' });
      else process.kill(proc.pid, 'SIGKILL');
    } catch { /* already gone */ }
    for (let i = 0; i < 50 && proc.exitCode === null && proc.signalCode === null; i++) await sleep(100);
    await sleep(500);
  };
  return { proc, browser, context, page, kill };
}

// Reads a file out of OPFS (main thread, via getFile) in base64 chunks.
export async function readOpfs(page, name) {
  const parts = [];
  let off = 0, size = Infinity;
  const CHUNK = 6 * 1024 * 1024;
  while (off < size) {
    const r = await page.evaluate(async ([name, off, len]) => {
      const d = await navigator.storage.getDirectory();
      const f = await (await d.getFileHandle(name)).getFile();
      const b = new Uint8Array(await f.slice(off, off + len).arrayBuffer());
      let s = '';
      for (let i = 0; i < b.length; i += 8192) s += String.fromCharCode.apply(null, b.subarray(i, i + 8192));
      return { size: f.size, b64: btoa(s) };
    }, [name, off, CHUNK]);
    size = r.size;
    const buf = Buffer.from(r.b64, 'base64');
    parts.push(buf);
    off += buf.length;
    if (buf.length === 0) break;
  }
  return Buffer.concat(parts);
}

export async function waitFor(fn, { timeout = 30000, interval = 100, what = 'condition' } = {}) {
  const t0 = Date.now();
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() - t0 > timeout) throw new Error(`timeout waiting for ${what}`);
    await sleep(interval);
  }
}

export async function text(page, sel) { return (await page.textContent(sel)) || ''; }
export async function json(page, sel) { const t = await text(page, sel); return t ? JSON.parse(t) : null; }

export async function errorsOf(page) { return text(page, '#errors'); }

export async function openApp(page, url) {
  await page.goto(url);
  await page.waitForSelector('#open');
  await waitFor(async () => (await json(page, '#debug-takes')) !== null, { what: 'worker ready' });
}

export async function openMics(page) {
  await page.click('#open');
  await waitFor(async () => (await text(page, '#live-settings')).length > 0 || (await errorsOf(page)).length > 0, { what: 'microphones' });
  const err = await errorsOf(page);
  if (err) throw new Error(`open failed: ${err}`);
  return { raw: await json(page, '#raw-settings'), live: await json(page, '#live-settings') };
}

export async function startTake(page) {
  await page.click('#start');
  await waitFor(async () => (await text(page, '#status')) === 'recording' || (await errorsOf(page)).length > 0, { what: 'recording' });
  const err = await errorsOf(page);
  if (err) throw new Error(`start failed: ${err}`);
}

export async function recordFor(page, seconds, onStats) {
  const rate = Number(await text(page, '#rate'));
  await waitFor(async () => {
    const s = await json(page, '#debug-stats');
    if (onStats && s && s.frames !== undefined) onStats(s);
    return s && s.frames >= seconds * rate;
  }, { timeout: (seconds * 2 + 30) * 1000, interval: 250, what: `${seconds}s of frames` });
  return rate;
}

export async function stopTake(page) {
  await page.click('#stop');
  await waitFor(async () => (await text(page, '#status')).startsWith('stopped'), { what: 'stopped', timeout: 30000 });
  const takes = await waitFor(async () => {
    const t = await json(page, '#debug-takes');
    return t && t.length && !t.some((x) => x.recording) ? t : null;
  }, { what: 'take list' });
  return { stopped: JSON.parse((await text(page, '#status')).replace(/^stopped: /, '')), takes };
}
