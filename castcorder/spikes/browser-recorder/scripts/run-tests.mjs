// The one command: `npm test`. Builds everything and runs the browser tests.
// Keeps Playwright's Chromium inside node_modules (PLAYWRIGHT_BROWSERS_PATH=0).
import { spawnSync } from 'node:child_process';
const env = { ...process.env, PLAYWRIGHT_BROWSERS_PATH: '0', RUSTC_WRAPPER: '' };
function run(cmd, args) {
  console.log(`> ${cmd} ${args.join(' ')}`);
  const r = spawnSync(cmd, args, { stdio: 'inherit', env, shell: process.platform === 'win32' });
  if (r.status !== 0) { console.log(`FAILED: ${cmd} ${args.join(' ')}`); process.exit(r.status ?? 1); }
}
run('npx', ['playwright', 'install', 'chromium']);
run('cargo', ['test', '--lib']);
run('trunk', ['build', '--release']);
run('npx', ['playwright', 'test', ...process.argv.slice(2)]);
