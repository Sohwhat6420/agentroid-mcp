import assert from "node:assert/strict";
import { test } from "node:test";
import { BUDGET, estimateTokens, measure, serializedToolList } from "../src/budget.ts";
import { INSTRUCTIONS, MAX_TOOLS } from "../src/tools.ts";

test("the estimate is characters / 3.2 rounded up", () => {
  assert.equal(estimateTokens(""), 0);
  assert.equal(estimateTokens("a".repeat(32)), 10);
  assert.equal(estimateTokens("a".repeat(33)), 11);
  assert.equal(BUDGET.charsPerToken, 3.2);
});

test("the budgets hold: full <= 5000 tokens, minimal <= 1500, instructions < 4096 chars, <= 27 tools", () => {
  const r = measure();
  assert.deepEqual(r.problems, []);
  assert.ok(r.fullTokens <= 5000, `full ${r.fullTokens}`);
  assert.ok(r.minimalTokens <= 1500, `minimal ${r.minimalTokens}`);
  assert.ok(r.instructionsChars < 4096, `instructions ${r.instructionsChars}`);
  assert.ok(r.fullTools <= MAX_TOOLS);
  assert.ok(r.fullTools <= 27, "the tool limit by design");
  console.log(`budget: full ${r.fullTokens} tok (${r.fullTools} tools), minimal ${r.minimalTokens} tok (${r.minimalTools} tools), instructions ${r.instructionsChars} chars`);
});

test("the measured text is the list as sent, and minimal is smaller than full", () => {
  const full = serializedToolList("full");
  const min = serializedToolList("minimal");
  assert.ok(min.length < full.length);
  assert.ok(full.startsWith('[{"name":"list_devices"'));
  assert.equal(INSTRUCTIONS.length, measure().instructionsChars);
});
