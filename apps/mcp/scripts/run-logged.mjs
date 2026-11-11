// Runs `node <args>` with a hidden window and prints the exit code, so a proof run records it:
//   node scripts/run-logged.mjs --experimental-strip-types --test "test/*.test.ts"
import { spawnSync } from "node:child_process";

const r = spawnSync(process.execPath, process.argv.slice(2), { stdio: "inherit", windowsHide: true });
console.log(`EXIT=${r.status}`);
process.exit(r.status ?? 1);
