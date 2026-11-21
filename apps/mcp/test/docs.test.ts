// Docs that match the code. Each test reads a claim the docs make and checks it against the code,
// so a change to the tools, the error kinds, the environment variables or the version that forgets
// the docs fails `npm test` (and CI).

import assert from "node:assert/strict";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { test } from "node:test";
import { REFERENCE_MD, render } from "../scripts/docs-tables.ts";
import { ERROR_KINDS } from "../src/errors.ts";
import { TOOLS } from "../src/tools.ts";
import { VERSION } from "../src/version.ts";
import { mcpRoot } from "./util.ts";

const repo = join(mcpRoot, "..", "..");
const read = (rel: string): string => readFileSync(join(repo, rel), "utf8");
const README = read("README.md");
const REFERENCE = readFileSync(REFERENCE_MD, "utf8");
const pkg = JSON.parse(readFileSync(join(mcpRoot, "package.json"), "utf8")) as { version: string; engines?: { node?: string } };

const words = (n: number): string => String(n);

test("the generated tables in docs/REFERENCE.md are up to date (run npm run docs)", () => {
  assert.equal(render(REFERENCE), REFERENCE);
});

test("the tool counts the docs give are the code's", () => {
  const all = TOOLS.length;
  const jobs = TOOLS.filter((t) => t.route === "job").length;
  const device = all - jobs;
  const minimal = TOOLS.filter((t) => t.minimal).length;
  assert.match(REFERENCE, new RegExp(`\\*\\*${words(all)} MCP tools\\.\\*\\* ${words(device)} device tools \\(reads and actions\\) and ${words(jobs)} job tools`));
  assert.match(REFERENCE, new RegExp(`All ${words(all)} tools, in the order`));
  assert.match(REFERENCE, new RegExp(`lists only the ${words(minimal)} marked`));
  assert.match(README, new RegExp(`all ${words(all)} tools cost`));
  assert.match(README, new RegExp(`keeps ${words(minimal)} of them`));
  // every other "N MCP tools" / "N-tool" claim in the two files names the same number
  for (const [name, text] of [["README.md", README], ["docs/REFERENCE.md", REFERENCE]] as const) {
    for (const m of text.matchAll(/\b(\d+)(?: MCP tools|-tool MCP server| tools\b)/g)) {
      const n = Number(m[1]);
      assert.ok(n === all || n === minimal || n === device || n === jobs, `${name}: "${m[0]}" matches no tool count in the code`);
    }
  }
});

test("every tool is in the reference table, once", () => {
  for (const t of TOOLS) {
    const rows = REFERENCE.split("\n").filter((l) => l.startsWith(`| \`${t.name}\` |`));
    assert.equal(rows.length, 1, t.name);
  }
});

test("every error kind is documented, and the count is the code's", () => {
  for (const k of ERROR_KINDS) assert.ok(REFERENCE.includes(`| \`${k}\` |`), k);
  assert.match(REFERENCE, new RegExp(`\`kind\` is one of the ${ERROR_KINDS.length} below`));
});

/** Every source file under `dir` (recursively), skipping tests, build output and dependencies. */
function sources(dir: string, exts: string[]): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    if (["node_modules", "target", "tests", "test", "fixtures"].includes(name)) continue;
    const p = join(dir, name);
    if (statSync(p).isDirectory()) out.push(...sources(p, exts));
    else if (exts.some((e) => name.endsWith(e)) && !/(^|[._-])tests?([._-]|\.rs$)/.test(name)) out.push(p);
  }
  return out;
}

test("every AGENTROID_ environment variable the code reads is in the reference table", () => {
  const files = [...sources(join(mcpRoot, "src"), [".ts"]), ...sources(join(repo, "crates"), [".rs"])];
  const seen = new Map<string, string>();
  for (const f of files) {
    const text = readFileSync(f, "utf8");
    // reads only: process.env.X, env::var("X"), env::var_os("X")
    for (const m of text.matchAll(/process\.env\.(AGENTROID_[A-Z0-9_]+)|env::var(?:_os)?\("(AGENTROID_[A-Z0-9_]+)"\)/g)) seen.set((m[1] ?? m[2]) as string, relative(repo, f));
  }
  assert.ok(seen.size >= 8, `found only ${seen.size} variables: the scan is broken`);
  const table = REFERENCE.slice(REFERENCE.indexOf("## Environment variables"), REFERENCE.indexOf("## Safety model"));
  for (const [v, where] of seen) assert.ok(table.includes(`| \`${v}\` |`), `${v} (read in ${where}) is not in docs/REFERENCE.md#environment-variables`);
  // and the table lists nothing the code no longer reads
  for (const m of table.matchAll(/^\| `(AGENTROID_[A-Z0-9_]+)` \|/gm)) assert.ok(seen.has(m[1] as string), `${m[1]} is documented but no code reads it`);
});

test("one version everywhere: package.json, the server, the Cargo workspace and the docs", () => {
  assert.equal(pkg.version, VERSION);
  const cargo = read("Cargo.toml");
  const ws = /\[workspace\.package\][^[]*?\nversion = "([^"]+)"/.exec(cargo);
  assert.equal(ws?.[1], VERSION, "Cargo.toml [workspace.package] version");
  const jobview = JSON.parse(read("web/jobview/package.json")) as { version: string };
  assert.equal(jobview.version, VERSION, "web/jobview/package.json");
  // every version-looking string in the user-facing docs is this one
  for (const [name, text] of [["README.md", README], ["docs/REFERENCE.md", REFERENCE], ["SECURITY.md", read("SECURITY.md")]] as const) {
    for (const m of text.matchAll(/\b(?:version|preview|bridged|Agentroid is a preview \()\s*v?(\d+\.\d+\.\d+)\b/gi)) assert.equal(m[1], VERSION, `${name}: "${m[0]}"`);
  }
  assert.ok(README.includes(VERSION), "README names the version");
  assert.match(read("CHANGELOG.md"), new RegExp(`^## \\[?${VERSION.replace(/\./g, "\\.")}\\]?`, "m"), "CHANGELOG.md has a section for this version");
});

test("the Node version the docs ask for is the one package.json declares", () => {
  const want = /^>=(\d+)\.(\d+)\.0$/.exec(pkg.engines?.node ?? "");
  assert.ok(want, `engines.node is ${String(pkg.engines?.node)}`);
  const v = `${want[1]}.${want[2]}`;
  assert.match(README, new RegExp(`Node\\.js\\]\\(https://nodejs\\.org\\) ${v.replace(".", "\\.")} or later`));
  assert.match(REFERENCE, new RegExp(`Node\\.js ${v.replace(".", "\\.")} or later`));
  const [maj, min] = process.versions.node.split(".").map(Number) as [number, number];
  assert.ok(maj > Number(want[1]) || (maj === Number(want[1]) && min >= Number(want[2])), `this Node (${process.versions.node}) is older than the declared floor`);
});

test("every relative link in README.md and docs/*.md points at a file that exists", () => {
  let checked = 0;
  const docs = ["README.md", ...readdirSync(join(repo, "docs")).filter((f) => f.endsWith(".md")).map((f) => `docs/${f}`)];
  for (const d of docs) {
    const text = read(d);
    const base = join(repo, d, "..");
    for (const m of text.matchAll(/\]\(([^)\s#]+)(?:#[^)]*)?\)|(?:src|srcset)="([^"#:]+)"/g)) {
      const target = (m[1] ?? m[2]) as string;
      if (/^[a-z]+:/i.test(target)) continue; // https:, mailto:
      let ok = true;
      try {
        statSync(join(base, decodeURIComponent(target)));
      } catch {
        ok = false;
      }
      assert.ok(ok, `${d}: link to ${target} is broken`);
      checked++;
    }
  }
  assert.ok(checked >= 20, `only ${checked} links checked: the scan is broken`);
});
