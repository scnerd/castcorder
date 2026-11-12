// Install Playwright's Chromium and Firefox into this directory's node_modules (not a
// user-wide cache). Idempotent: skips the download when already present.
import { spawnSync } from 'node:child_process';
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const r = spawnSync('npx', ['playwright', 'install', 'chromium', 'firefox'], { stdio: 'inherit', shell: true, env: process.env });
process.exit(r.status ?? 1);
