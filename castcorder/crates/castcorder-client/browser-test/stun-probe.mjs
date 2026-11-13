// Send one STUN Binding request to each ip:port given and require an answer
// (the CI check that the docker-compose dev stack's rendezvous STUN port and
// coturn both answer). Usage: node stun-probe.mjs 127.0.0.1:3479 127.0.0.1:3478
// Everything printed is ASCII.

import { randomBytes } from 'node:crypto';
import dgram from 'node:dgram';

function probe(ip, port, ms = 1000) {
  return new Promise((resolve) => {
    const s = dgram.createSocket('udp4');
    const req = Buffer.alloc(20);
    req.writeUInt16BE(1, 0);
    req.writeUInt32BE(0x2112A442, 4);
    randomBytes(12).copy(req, 8);
    const done = (v) => { clearTimeout(t); try { s.close(); } catch { /* closed */ } resolve(v); };
    const t = setTimeout(() => done(false), ms);
    // A Binding success response (0x0101) with our transaction id.
    s.on('message', (m) => done(m.length >= 20 && m.readUInt16BE(0) === 0x0101 && m.subarray(8, 20).equals(req.subarray(8, 20))));
    s.on('error', () => done(false));
    s.send(req, port, ip);
  });
}

let ok = true;
for (const target of process.argv.slice(2)) {
  const [ip, port] = target.split(':');
  let answered = false;
  for (let i = 0; i < 20 && !answered; i++) answered = await probe(ip, Number(port));
  console.log((answered ? 'ok   STUN answers at ' : 'BAD  no STUN answer at ') + target);
  ok &&= answered;
}
process.exit(ok ? 0 : 1);
