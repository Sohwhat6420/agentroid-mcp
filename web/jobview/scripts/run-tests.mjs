// Compiles the DOM-free modules (contract, model) with tsc, then runs tests/*.test.mjs with node --test.
// Children are spawned with windowsHide: true and no shell, so no console window can open.
import { spawnSync } from 'node:child_process';
import { readdirSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const run = (args) => spawnSync(process.execPath, args, { cwd: root, stdio: 'inherit', windowsHide: true, shell: false }).status ?? 1;

rmSync(new URL('../.test-build', import.meta.url), { recursive: true, force: true });
const tsc = run(['./node_modules/typescript/bin/tsc', '-p', 'tsconfig.test.json']);
if (tsc !== 0) { console.error(`tsc (test build) exited ${tsc}`); process.exit(tsc); }
const files = readdirSync(new URL('../tests', import.meta.url)).filter(f => f.endsWith('.test.mjs')).map(f => `tests/${f}`);
const code = run(['--test', '--test-reporter=spec', ...files]);
console.log(`node --test exited ${code}`);
process.exit(code);
