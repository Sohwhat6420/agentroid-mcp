import assert from "node:assert/strict";
import { test } from "node:test";
import { RECEIPT_SCHEMA, buildReceipt, receiptProblems } from "../src/receipt.ts";

const base = { runId: "r1", device: "d", tool: "look", ok: true, wallTimeS: 1, laptopCpuS: 0, retries: 0, errors: [] as string[] };

test("a receipt with no header value says null for phone_peak_c and battery_change, never a made-up 0", () => {
  const r = buildReceipt(base);
  assert.equal(r.phone_peak_c, null);
  assert.equal(r.battery_change, null);
  assert.deepEqual(receiptProblems(r), []);
  const n = buildReceipt({ ...base, phonePeakC: null, batteryChange: Number.NaN });
  assert.equal(n.phone_peak_c, null);
  assert.equal(n.battery_change, null);
});

test("a measured value, including a real 0, is kept", () => {
  const r = buildReceipt({ ...base, phonePeakC: 0, batteryChange: -2 });
  assert.equal(r.phone_peak_c, 0);
  assert.equal(r.battery_change, -2);
  assert.deepEqual(receiptProblems(r), []);
});

test("the schema allows null only for those two fields", () => {
  const r = buildReceipt(base) as unknown as Record<string, unknown>;
  assert.deepEqual(receiptProblems({ ...r, wall_time_s: null }).length, 1);
  assert.deepEqual(receiptProblems({ ...r, phone_peak_c: "hot" }).length, 1);
  const props = (RECEIPT_SCHEMA as { properties: Record<string, { type: unknown }> }).properties;
  assert.deepEqual(props.phone_peak_c?.type, ["number", "null"]);
  assert.deepEqual(props.battery_change?.type, ["number", "null"]);
});
