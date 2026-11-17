// Post-deploy smoke check of the Castcorder Worker: node deploy/smoke.mjs <origin> <version>...
//
// For each client version, GET /v/<version>/ must answer 200 with the client's HTML. Then
// /healthz must say ok, and a WebSocket to /ws?mailbox=<a fixed host id> must open and
// receive the mailbox's hello first (so the Worker, the edge rate limit and the host key's
// Durable Object all answered). Needs Node 22 (global fetch and WebSocket). Prints only
// URLs and statuses: no secrets pass through here. Exits 1 on the first failure.

// A valid host id that belongs to no host: the Ed25519 base point, base64url. Its mailbox
// is the one Durable Object every smoke run reuses.
const SMOKE_HOST = 'WGZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmZmY';
const TIMEOUT_MS = 15000;

const [origin, ...versions] = process.argv.slice(2);
if (!origin || !/^https?:\/\/[^/]+$/.test(origin) || versions.length === 0) {
  console.log('usage: node smoke.mjs <origin, e.g. https://castcorder.example.workers.dev> <version>...');
  process.exit(2);
}

function fail(msg) {
  console.log('SMOKE FAIL ' + msg);
  process.exit(1);
}

async function get(path) {
  const url = origin + path;
  // A fresh deploy can take a few seconds to reach every location: retry briefly.
  for (let attempt = 1; ; attempt++) {
    try {
      const res = await fetch(url, { redirect: 'manual', signal: AbortSignal.timeout(TIMEOUT_MS) });
      const body = await res.text();
      console.log('GET ' + url + ' -> ' + res.status);
      if (res.status === 200 || attempt >= 5) return { status: res.status, body };
    } catch (e) {
      console.log('GET ' + url + ' -> ' + e.name);
      if (attempt >= 5) return { status: 0, body: '' };
    }
    await new Promise((r) => setTimeout(r, 3000 * attempt));
  }
}

for (const v of versions) {
  const { status, body } = await get('/v/' + v + '/');
  if (status !== 200) fail('/v/' + v + '/ answered ' + status);
  if (!/<html/i.test(body) || !/\.wasm/.test(body)) fail('/v/' + v + '/ is not the client');
}

const health = await get('/healthz');
if (health.status !== 200 || health.body.trim() !== 'ok') fail('/healthz answered ' + health.status);

const wsUrl = origin.replace(/^http/, 'ws') + '/ws?mailbox=' + SMOKE_HOST;
const first = await new Promise((resolve) => {
  const ws = new WebSocket(wsUrl);
  const timer = setTimeout(() => {
    ws.close();
    resolve({ error: 'no message within ' + TIMEOUT_MS + ' ms' });
  }, TIMEOUT_MS);
  ws.addEventListener('message', (ev) => {
    clearTimeout(timer);
    ws.close();
    resolve({ data: String(ev.data) });
  });
  ws.addEventListener('error', () => {
    clearTimeout(timer);
    resolve({ error: 'WebSocket error' });
  });
});
if (first.error) fail(wsUrl + ': ' + first.error);
let msg;
try {
  msg = JSON.parse(first.data);
} catch {
  fail(wsUrl + ': first frame is not JSON');
}
console.log('WS ' + wsUrl + ' -> first frame type=' + msg.type);
if (msg.type !== 'hello' || typeof msg.nonce !== 'string') fail('expected the mailbox hello, got type=' + msg.type);

console.log('SMOKE OK ' + origin + ' versions=' + versions.join(','));
