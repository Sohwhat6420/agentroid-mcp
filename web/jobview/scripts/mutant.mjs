// Mutation check: plant one mutant in the validator (the `required` keyword is silently skipped),
// show the unit tests catch it, then restore the original byte for byte.
//   exit 0 = the mutant was caught and the file is restored identically
// The original is copied to LANE/tools/schema.ts.orig first, so `cmp` can prove the restore afterwards.
import { spawnSync } from 'node:child_process';
import { copyFileSync, readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const file = fileURLToPath(new URL('../src/contract/schema.ts', import.meta.url));
const orig = fileURLToPath(new URL('../../../../tools/schema.ts.orig', import.meta.url));
const FROM = 'if (!(k in obj)) out.push({ path: `${path}/${k}`, msg: \'is required and missing\' });';
const TO = 'if (false && !(k in obj)) out.push({ path: `${path}/${k}`, msg: \'is required and missing\' }); // MUTANT';

copyFileSync(file, orig);
const src = readFileSync(file, 'utf8');
if (src.split(FROM).length !== 2) { console.error('mutation site not found exactly once; nothing changed'); process.exit(2); }
let code;
try {
  writeFileSync(file, src.replace(FROM, TO));
  console.log('MUTANT planted: src/contract/schema.ts, the `required` check never fires');
  const r = spawnSync(process.execPath, ['scripts/run-tests.mjs'], { cwd: root, encoding: 'utf8', windowsHide: true, shell: false });
  code = r.status;
  const failed = (r.stdout + r.stderr).split('\n').filter(l => /^✖ /.test(l.trim()) && !/failing tests/.test(l)).map(l => l.trim());
  console.log(`tests with the mutant: exit ${code}; ${failed.length} failing:`);
  for (const l of [...new Set(failed)].slice(0, 10)) console.log('  ' + l);
} finally {
  copyFileSync(orig, file);
  console.log('restored from tools/schema.ts.orig');
}
const same = Buffer.compare(readFileSync(file), readFileSync(orig)) === 0;
console.log(`restore byte-identical: ${same}`);
const caught = code !== 0;
console.log(caught ? 'RESULT: mutant CAUGHT' : 'RESULT: mutant SURVIVED');
process.exit(caught && same ? 0 : 1);
