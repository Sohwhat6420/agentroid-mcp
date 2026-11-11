import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { listTools } from "../src/tools.ts";
import { mcpRoot } from "./util.ts";

const script = join(mcpRoot, "scripts", "count-tokens-ci.ts");
const node = (args: string[], env: NodeJS.ProcessEnv) => spawnSync(process.execPath, ["--experimental-strip-types", "--no-warnings", script, ...args], { encoding: "utf8", windowsHide: true, env });
// no key, no proxy: if the script tried the network it would fail, not hang
const env = { ...process.env, ANTHROPIC_API_KEY: "", HTTPS_PROXY: "http://127.0.0.1:9", HTTP_PROXY: "http://127.0.0.1:9" };

test("--emit-bodies writes the five request bodies with no key and no network", () => {
  const dir = mkdtempSync(join(tmpdir(), "agentroid-bodies-"));
  try {
    const r = node(["--emit-bodies", dir], env);
    assert.equal(r.status, 0, r.stderr);
    assert.deepEqual(readdirSync(dir).sort(), ["baseline.json", "full-plain.json", "full-worst.json", "minimal-plain.json", "minimal-worst.json"]);
    const body = (n: string) => JSON.parse(readFileSync(join(dir, n), "utf8")) as { model: string; messages: unknown[]; tools: { name: string; description: string; input_schema: unknown }[] };
    assert.equal(body("baseline.json").tools.length, 1);
    assert.deepEqual(body("full-plain.json").tools.map((t) => t.name), listTools("full").map((t) => t.name));
    assert.deepEqual(body("minimal-plain.json").tools.map((t) => t.name), listTools("minimal").map((t) => t.name));
    for (const n of ["full-plain.json", "minimal-plain.json", "full-worst.json", "minimal-worst.json"]) for (const t of body(n).tools) assert.deepEqual(Object.keys(t).sort(), ["description", "input_schema", "name"], n);
    // worst case: the output schema is appended to the description as text
    const plain = body("full-plain.json").tools[0]!;
    const worst = body("full-worst.json").tools[0]!;
    assert.ok(worst.description.startsWith(plain.description) && worst.description.includes("\nOutput: {"));
    assert.equal(plain.description.includes("Output: {"), false);
    assert.ok(typeof body("baseline.json").model === "string" && body("baseline.json").messages.length === 1);
    assert.ok(readFileSync(join(dir, "full-worst.json"), "utf8").length > readFileSync(join(dir, "full-plain.json"), "utf8").length);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("without --emit-bodies and without a key the CI script still stops with 2, before any network call", () => {
  const r = node([], env);
  assert.equal(r.status, 2);
  assert.match(r.stderr, /ANTHROPIC_API_KEY is not set/);
});

test("--emit-bodies without a directory is an error", () => {
  assert.equal(node(["--emit-bodies"], env).status, 2);
});
