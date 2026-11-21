// The tables in docs/REFERENCE.md that are made from the code: the tool list and the error kinds.
//
//   node --experimental-strip-types scripts/docs-tables.ts           rewrite them in place
//   node --experimental-strip-types scripts/docs-tables.ts --check   exit 1 if they are out of date
//
// Each table sits between a pair of markers (<!-- gen:tools --> ... <!-- /gen:tools -->), so the
// prose around it stays hand-written. test/docs.test.ts runs the same check in `npm test`.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { ERROR_KINDS, ERROR_KIND_INFO } from "../src/errors.ts";
import type { Retry } from "../src/errors.ts";
import { TOOLS } from "../src/tools.ts";

export const REFERENCE_MD = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "docs", "REFERENCE.md");

const cell = (s: string): string => s.replace(/\|/g, "\\|").replace(/\s+/g, " ").trim();

export function toolsTable(): string {
  const rows = TOOLS.map((t) => `| \`${t.name}\` | ${t.route === "job" ? "job" : "device"} | ${t.read ? "yes" : "no"} | ${t.minimal ? "yes" : "no"} | ${cell(t.description)} |`);
  return ["| Tool | Kind | Read only | In `minimal` | What it does |", "| --- | --- | --- | --- | --- |", ...rows].join("\n");
}

const RETRY_WORDS: Record<Retry, string> = {
  no: "No: the same call fails the same way.",
  after_fix: "Only after fixing the input, the device or the lease.",
  after_look: "Look at the screen first: the action may have happened.",
  yes: "Yes, once bridged is reachable again.",
};

export function errorsTable(): string {
  const rows = ERROR_KINDS.map((k) => `| \`${k}\` | ${cell(ERROR_KIND_INFO[k].meaning)} | ${RETRY_WORDS[ERROR_KIND_INFO[k].retry]} |`);
  return ["| Kind | What it means | Repeat the call? |", "| --- | --- | --- |", ...rows].join("\n");
}

export const TABLES: Record<string, () => string> = { tools: toolsTable, errors: errorsTable };

/** `md` with every generated table replaced by its current text. Throws if a marker pair is missing. */
export function render(md: string): string {
  let out = md;
  for (const [name, make] of Object.entries(TABLES)) {
    const open = `<!-- gen:${name} -->`;
    const close = `<!-- /gen:${name} -->`;
    const a = out.indexOf(open);
    const b = out.indexOf(close);
    if (a < 0 || b < a) throw new Error(`docs/REFERENCE.md has no ${open} ... ${close} pair`);
    out = `${out.slice(0, a + open.length)}\n${make()}\n${out.slice(b)}`;
  }
  return out;
}

if (process.argv[1] !== undefined && fileURLToPath(import.meta.url) === process.argv[1]) {
  const md = readFileSync(REFERENCE_MD, "utf8");
  const next = render(md);
  if (process.argv.includes("--check")) {
    if (next !== md) {
      process.stderr.write("docs/REFERENCE.md tables are out of date: run node --experimental-strip-types scripts/docs-tables.ts\n");
      process.exit(1);
    }
  } else if (next !== md) writeFileSync(REFERENCE_MD, next);
}
