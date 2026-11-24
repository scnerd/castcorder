// The workspace version (castcorder/Cargo.toml, [workspace.package]), which the host
// pins in its call links (/v/<version>/, ADR 0002). Tests serve the client there.
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const manifest = join(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..', 'Cargo.toml');
const text = readFileSync(manifest, 'utf8').replace(/\r\n/g, '\n');
const section = text.split('[workspace.package]\n')[1] ?? '';
const match = section.match(/^version = "([^"]+)"$/m);
if (!match) throw new Error('no [workspace.package] version in ' + manifest);

export const VERSION = match[1];
