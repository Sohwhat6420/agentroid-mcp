import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { validate } from "../src/schema.ts";
import { TOOLS, outputSchemaFor, toolByName } from "../src/tools.ts";
import { mcpRoot } from "./util.ts";

const dir = (k: "valid" | "invalid"): string => join(mcpRoot, "fixtures", "responses", k);
const files = (k: "valid" | "invalid"): string[] => readdirSync(dir(k)).filter((f) => f.endsWith(".json")).sort();
const load = (k: "valid" | "invalid", f: string): unknown => JSON.parse(readFileSync(join(dir(k), f), "utf8"));

/** A fixture is checked against its own tool's output schema; the error fixtures against the list_devices one (same envelope). */
const schemaFor = (file: string) => outputSchemaFor(toolByName(file.replace(/\.json$/, "")) ?? TOOLS[0]!, true);

test("there is one valid response fixture per tool, plus error fixtures", () => {
  const names = files("valid");
  for (const t of TOOLS) assert.ok(names.includes(`${t.name}.json`), `missing fixture for ${t.name}`);
  assert.ok(names.includes("error-refused.json") && names.includes("error-with-unknown-header.json"));
});

test("every valid response fixture validates against its output schema (header and receipt included)", () => {
  for (const f of files("valid")) assert.deepEqual(validate(load("valid", f), schemaFor(f)), [], f);
});

test("every invalid response fixture is rejected, for the reason its name gives", () => {
  const names = files("invalid");
  assert.ok(names.length >= 10);
  const reason: Record<string, RegExp> = {
    "missing-header.json": /missing required "header"/,
    "missing-receipt.json": /missing required "receipt"/,
    "ok-not-boolean.json": /^\/ok: expected boolean/,
    "battery-over-100.json": /battery\/v: matches none of anyOf/,
    "header-field-not-wrapped.json": /battery: expected object/,
    "header-missing-focus-changed.json": /missing required "focus_changed"/,
    "receipt-negative-usd.json": /usd: below minimum 0/,
    "receipt-extra-field.json": /receipt\/secret: unexpected property/,
    "error-kind-unknown.json": /kind: not one of/,
    "data-missing-text.json": /missing required "text"/,
    "receipt-peak-not-number-or-null.json": /phone_peak_c: expected number\|null/,
    "unknown-member.json": /extra: unexpected property/,
  };
  const look = outputSchemaFor(toolByName("look")!, true); // all the broken fixtures are look-shaped
  for (const f of names) {
    const issues = validate(load("invalid", f), look);
    assert.ok(issues.length > 0, `${f} should be rejected`);
    assert.ok(reason[f], `no expected reason for ${f}`);
    assert.ok(issues.some((i) => reason[f]!.test(`${i.path}: ${i.message}`)), `${f}: wrong reason: ${JSON.stringify(issues)}`);
  }
});

test("the pull_new fixture carries path, size and sha256", () => {
  const d = (load("valid", "pull_new.json") as { data: Record<string, unknown> }).data;
  assert.deepEqual(Object.keys(d).sort(), ["path", "sha256", "size", "text"]);
});
