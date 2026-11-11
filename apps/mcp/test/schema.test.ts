import assert from "node:assert/strict";
import { test } from "node:test";
import { join } from "node:path";
import { RECEIPT_SCHEMA } from "../src/receipt.ts";
import { isValid, validate } from "../src/schema.ts";
import { fixtureFiles, readJson, schemasDir } from "./util.ts";

test("checker: types, required, additionalProperties, enum, const, pattern, bounds", () => {
  const s = { type: "object", required: ["a"], additionalProperties: false, properties: { a: { type: "integer", minimum: 0, maximum: 5 }, b: { enum: ["x", "y"] }, c: { const: 1 }, d: { type: "string", pattern: "^z+$", minLength: 2 } } };
  assert.ok(isValid({ a: 1, b: "x", c: 1, d: "zz" }, s));
  assert.ok(!isValid({}, s), "missing required");
  assert.ok(!isValid({ a: 1.5 }, s), "not an integer");
  assert.ok(!isValid({ a: 6 }, s), "above maximum");
  assert.ok(!isValid({ a: 1, b: "q" }, s), "not in enum");
  assert.ok(!isValid({ a: 1, c: 2 }, s), "const");
  assert.ok(!isValid({ a: 1, d: "z" }, s), "minLength and pattern");
  assert.ok(!isValid({ a: 1, extra: true }, s), "additionalProperties false");
  assert.ok(isValid(null, { type: ["null", "string"] }));
  assert.ok(!isValid(NaN, { type: "number" }));
});

test("checker: anyOf, oneOf, items, tuple items, $ref and boolean schemas", () => {
  assert.ok(isValid(3, { anyOf: [{ type: "string" }, { type: "integer" }] }));
  assert.ok(!isValid(true, { anyOf: [{ type: "string" }, { type: "integer" }] }));
  assert.ok(!isValid(3, { oneOf: [{ type: "integer" }, { type: "number" }] }), "matches two of oneOf");
  assert.ok(isValid([1, 2], { type: "array", items: { type: "integer" }, minItems: 2, maxItems: 2 }));
  assert.ok(!isValid([1, "x"], { type: "array", items: { type: "integer" } }));
  assert.ok(isValid(["a", 1], { type: "array", items: [{ type: "string" }, { type: "integer" }] }));
  assert.ok(isValid({ k: 1 }, { $defs: { n: { type: "integer" } }, type: "object", properties: { k: { $ref: "#/$defs/n" } } }));
  assert.ok(!isValid({ k: "s" }, { $defs: { n: { type: "integer" } }, type: "object", properties: { k: { $ref: "#/$defs/n" } } }));
  assert.ok(!isValid(1, { $ref: "#/nope" }), "unresolved ref is an error, not a pass");
  assert.ok(isValid(1, true));
  assert.ok(!isValid(1, false));
});

test("the receipt schema copy in receipt.ts is identical to schemas/receipt.json", () => {
  assert.deepEqual(RECEIPT_SCHEMA, readJson(join(schemasDir(), "receipt.json")));
});

for (const kind of ["receipt", "progress", "job-view"]) {
  const schema = readJson(join(schemasDir(), `${kind}.json`)) as Record<string, unknown>;
  const files = fixtureFiles(kind);
  test(`checker agrees with every ${kind} fixture (${files.length} files)`, () => {
    assert.ok(files.length > 0);
    for (const f of files) {
      const issues = validate(f.value, schema as { [k: string]: unknown });
      if (f.name.startsWith("valid-")) assert.deepEqual(issues, [], `${kind}/${f.name} should validate`);
      else assert.ok(issues.length > 0, `${kind}/${f.name} should be rejected`);
    }
  });
}
