// Stage a client build as the Worker's static assets: node stage-assets.mjs <dist> <version>
// copies <dist> to worker/assets/v/<version>/ (replacing what was there).
import { cpSync, existsSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const [dist, version] = process.argv.slice(2);
if (!dist || !version || !/^[0-9A-Za-z.+-]+$/.test(version)) {
  console.log('usage: node stage-assets.mjs <client dist dir> <version>');
  process.exit(2);
}
if (!existsSync(join(dist, 'index.html'))) {
  console.log('no client build at ' + resolve(dist) + ' (run trunk build first)');
  process.exit(1);
}
const to = join(dirname(fileURLToPath(import.meta.url)), 'assets', 'v', version);
rmSync(to, { recursive: true, force: true });
cpSync(dist, to, { recursive: true });
console.log('staged ' + resolve(dist) + ' at ' + to);
