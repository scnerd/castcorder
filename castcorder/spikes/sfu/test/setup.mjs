// One-time: install Playwright's Chromium into node_modules (not a user-wide cache).
import { spawnSync } from 'node:child_process';
process.env.PLAYWRIGHT_BROWSERS_PATH = '0';
const r = spawnSync('npx', ['playwright', 'install', 'chromium'], { stdio: 'inherit', shell: true, env: process.env });
process.exit(r.status ?? 1);
