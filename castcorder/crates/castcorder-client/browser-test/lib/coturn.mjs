// A real coturn (coturn/coturn:4.6 in Docker) for the relay tests: relay.mjs
// (through castcorder-rendezvous) and system.mjs against the Worker
// (RELAY=coturn). use-auth-secret with a random per-run secret; only its
// listening port is published (UDP, a port Docker picks), reached on
// 127.0.0.1 or the WSL2 VM address. With every relayed side relaying through
// it, media moves between two allocations inside the container, so the relay
// port range needs no publishing. DOCKER_HOST is honoured by the docker CLI.
// Everything printed is ASCII.

import { execFileSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import dgram from 'node:dgram';

export const COTURN_IMAGE = 'coturn/coturn:4.6';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export const docker = (args) => execFileSync('docker', args, {
  encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, MSYS_NO_PATHCONV: '1' },
}).trim();

// Whether the docker daemon answers; the error text when it does not.
export function dockerProblem() {
  try {
    docker(['version', '--format', '{{.Server.Version}}']);
    return null;
  } catch (e) {
    return 'docker is not reachable (set DOCKER_HOST?): ' + String(e.stderr || e.message).trim();
  }
}

// One STUN Binding request; true if anything answers within `ms`.
export function stunAnswers(ip, port, ms = 700) {
  return new Promise((resolve) => {
    const s = dgram.createSocket('udp4');
    const req = Buffer.alloc(20);
    req.writeUInt16BE(1, 0);
    req.writeUInt32BE(0x2112A442, 4);
    randomBytes(12).copy(req, 8);
    const done = (v) => { clearTimeout(t); try { s.close(); } catch { /* closed */ } resolve(v); };
    const t = setTimeout(() => done(false), ms);
    s.on('message', () => done(true));
    s.on('error', () => done(false));
    s.send(req, port, ip);
  });
}

// The local address this machine sends from toward ip:port (connecting a UDP
// socket sends nothing).
export function localAddressToward(ip, port) {
  return new Promise((resolve, reject) => {
    const s = dgram.createSocket('udp4');
    s.connect(port, ip, (e) => {
      if (e) { s.close(); reject(e); return; }
      const a = s.address().address;
      s.close();
      resolve(a);
    });
  });
}

// Where this machine reaches the container's published UDP port. Docker Desktop
// forwards UDP on 127.0.0.1; a Docker Engine inside WSL2 only forwards TCP to
// Windows' localhost, so there the WSL VM's own address is used. Override with
// CASTCORDER_TURN_HOST.
async function reachableDockerAddress(container, port) {
  let candidates = process.env.CASTCORDER_TURN_HOST ? [process.env.CASTCORDER_TURN_HOST] : ['127.0.0.1'];
  if (!process.env.CASTCORDER_TURN_HOST) {
    const probe = container + '-ip';
    try {
      docker(['run', '-d', '--name', probe, '--network', 'host', '--entrypoint', '/bin/cat', COTURN_IMAGE, '/proc/net/fib_trie']);
      await sleep(1500);
      const trie = docker(['logs', probe]).split('\n');
      for (let i = 1; i < trie.length; i++) {
        const m = trie[i - 1].match(/\|-- (\d+\.\d+\.\d+\.\d+)/);
        if (m && trie[i].includes('/32 host LOCAL') && !m[1].startsWith('127.')) candidates.push(m[1]);
      }
    } catch { /* not a Linux daemon we can inspect */ }
    try { docker(['rm', '-f', probe]); } catch { /* gone */ }
    candidates = [...new Set(candidates)];
  }
  for (let round = 0; round < 15; round++) {
    for (const ip of candidates) if (await stunAnswers(ip, port)) return ip;
  }
  throw new Error('coturn does not answer on port ' + port + ' at any of ' + candidates.join(', '));
}

// Start coturn as `container` with `secret`; `extra` adds coturn flags and
// `publish` more container ports; `transport: 'tcp'` also publishes TCP and
// makes the URL a TCP one. Resolves to { ip, port, url } (the turn: URL) once it answers STUN. Stop it with stopCoturn(container).
export async function startCoturn(container, secret, { extra = [], publish = [], transport = 'udp' } = {}) {
  const problem = dockerProblem();
  if (problem) throw new Error(problem);
  docker(['run', '-d', '--rm', '--name', container, '-p', '3478/udp', ...(transport === 'tcp' ? ['-p', '3478/tcp'] : []), ...publish.flatMap((p) => ['-p', p]), COTURN_IMAGE,
    '-n', '--listening-port=3478', '--realm=castcorder.test', '--fingerprint',
    '--use-auth-secret', '--static-auth-secret=' + secret, '--min-port=49160', '--max-port=49200',
    ...extra,
    '--no-tls', '--no-dtls', '--no-tcp-relay', '--no-cli', '--log-file=stdout', '--simple-log', '--verbose']);
  const mapped = docker(['port', container, '3478/udp']).split('\n')[0];
  const port = Number(mapped.split(':').pop());
  const ip = await reachableDockerAddress(container, port);
  // transport 'tcp': the URL names coturn's TCP listener (a separately mapped
  // port), so a client that follows it reaches TURN over TCP only; `port`
  // stays the UDP one the host's address probe and STUN checks use.
  if (transport === 'tcp') {
    const tcpPort = Number(docker(['port', container, '3478/tcp']).split('\n')[0].split(':').pop());
    return { ip, port, tcpPort, url: 'turn:' + ip + ':' + tcpPort + '?transport=tcp' };
  }
  return { ip, port, url: 'turn:' + ip + ':' + port + '?transport=udp' };
}

// coturn's log ('' if it never started), then the container removed.
export function stopCoturn(container) {
  let log = '';
  try { log = docker(['logs', container]); } catch { /* not started */ }
  try { docker(['rm', '-f', container]); } catch { /* not started */ }
  return log;
}
