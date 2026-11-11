// Writes fixtures/responses/{valid,invalid}/*.json: one valid result per tool plus hand-broken ones.
// Run once (node --experimental-strip-types scripts/make-fixtures.ts) and commit the output; the test
// fixtures.test.ts then checks the files, so a change to a schema that breaks a fixture is caught.

import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { HealthTracker, blankHeader } from "../src/header.ts";
import { buildReceipt } from "../src/receipt.ts";
import { TOOLS } from "../src/tools.ts";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "fixtures", "responses");
rmSync(root, { recursive: true, force: true });
mkdirSync(join(root, "valid"), { recursive: true });
mkdirSync(join(root, "invalid"), { recursive: true });

const header = new HealthTracker(() => 1_000).header(
  "DEVICE0000EXAMPLE",
  { alias: "poco-x6", model: "POCO X6", mode: "full", battery: 81, powered: false, temp_c: 33.5, screen: "on", lock: "unlocked", foreground: { package: "com.fixture.overlay", title: "Allow notifications?" } },
  { package: "com.android.settings", title: "Settings" },
);
const receipt = (tool: string, ok: boolean, errors: string[] = []) =>
  buildReceipt({ runId: `fixture-${tool}`, device: "DEVICE0000EXAMPLE", tool, ok, wallTimeS: 0.42, laptopCpuS: 0.01, retries: 0, phonePeakC: 33.5, batteryChange: 0, errors });

const data: Record<string, unknown> = {
  list_devices: { devices: [{ device: "DEVICE0000EXAMPLE", model: "POCO X6", mode: "full", holder: null }] },
  lease: { lease: { device: "DEVICE0000EXAMPLE", holder: "claude-1", epoch: 4 } },
  set_value: { text: "slider set to +2", value: "+2" },
  pixel_probe: { text: "rgb(12, 200, 7)", rgb: [12, 200, 7] },
  pull_new: { text: "pulled", path: "C:/fixtures/IMG_0001.jpg", size: 2048, sha256: "0123456789abcdef".repeat(4) },
};
const write = (dir: string, name: string, v: unknown): void => writeFileSync(join(root, dir, `${name}.json`), JSON.stringify(v, null, 1) + "\n");

for (const t of TOOLS) write("valid", t.name, { ok: true, data: data[t.name] ?? { text: `${t.name} ok`, image_bytes: null }, header, receipt: receipt(t.name, true) });

write("valid", "error-refused", {
  ok: false,
  error: { kind: "refused", code: -32003, message: "tap refused: this device is read_only, so it only answers reads", data: { tool: "tap" } },
  header,
  receipt: receipt("tap", false, ["tap refused: this device is read_only, so it only answers reads"]),
});
write("valid", "error-with-unknown-header", {
  ok: false,
  error: { kind: "transport", code: null, message: "cannot connect to bridged at \\\\.\\pipe\\agentroid: connect ENOENT (is bridged running?)" },
  header: blankHeader(),
  receipt: buildReceipt({ runId: "fixture-x", device: "none", tool: "look", ok: false, wallTimeS: 0.01, laptopCpuS: 0, retries: 0, errors: ["no bridged"] }),
});

const good = { ok: true, data: { text: "ok" }, header, receipt: receipt("look", true) };
write("invalid", "missing-header", { ok: true, data: good.data, receipt: good.receipt });
write("invalid", "missing-receipt", { ok: true, data: good.data, header });
write("invalid", "ok-not-boolean", { ...good, ok: "yes" });
write("invalid", "battery-over-100", { ...good, header: { ...header, battery: { v: 130, age_s: 0 } } });
write("invalid", "header-field-not-wrapped", { ...good, header: { ...header, battery: 81 } });
write("invalid", "header-missing-focus-changed", { ...good, header: Object.fromEntries(Object.entries(header).filter(([k]) => k !== "focus_changed")) });
write("invalid", "receipt-negative-usd", { ...good, receipt: { ...good.receipt, pool: { name: "none", usd: -1 } } });
write("invalid", "receipt-extra-field", { ...good, receipt: { ...good.receipt, secret: 1 } });
write("invalid", "error-kind-unknown", { ok: false, error: { kind: "mystery", code: null, message: "x" }, header, receipt: receipt("look", false, ["x"]) });
write("invalid", "data-missing-text", { ...good, data: {} });
write("invalid", "receipt-peak-not-number-or-null", { ...good, receipt: { ...good.receipt, phone_peak_c: "hot" } });
write("invalid", "unknown-member", { ...good, extra: 1 });
console.log(`wrote ${TOOLS.length + 2} valid and 12 invalid fixtures to ${root}`);
