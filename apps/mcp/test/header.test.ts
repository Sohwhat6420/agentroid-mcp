import assert from "node:assert/strict";
import { test } from "node:test";
import { HEADER_SCHEMA, HealthTracker, blankHeader, headerLine, parseObservation } from "../src/header.ts";
import { describeIssues, validate } from "../src/schema.ts";

const FULL = { alias: "poco", model: "POCO X6", mode: "full", battery: 81, powered: true, temp_c: 33.5, screen: "on", lock: "unlocked", foreground: { package: "com.android.settings", title: "Settings" } } as const;

test("a field never seen is unknown with a null age (never invented)", () => {
  const t = new HealthTracker(() => 1_000_000);
  const h = t.header("d", {});
  for (const k of ["alias", "model", "mode", "battery", "powered", "temp_c", "screen", "lock", "foreground"] as const) {
    assert.deepEqual(h[k], { v: "unknown", age_s: null }, k);
  }
  assert.equal(h.focus_changed.v, "unknown");
  assert.equal(h.data_age_s, null);
  assert.deepEqual(validate(h, HEADER_SCHEMA), []);
});

test("a field the daemon stops giving becomes unknown with its age, not its old value", () => {
  let now = 1_000_000;
  const t = new HealthTracker(() => now);
  const first = t.header("d", FULL);
  assert.equal(first.battery.v, 81);
  assert.equal(first.battery.age_s, 0);
  now += 12_340;
  const second = t.header("d", { model: "POCO X6" }); // battery etc. missing this time
  assert.deepEqual(second.battery, { v: "unknown", age_s: 12.3 });
  assert.deepEqual(second.temp_c, { v: "unknown", age_s: 12.3 });
  assert.deepEqual(second.model, { v: "POCO X6", age_s: 0 });
  assert.equal(second.data_age_s, null, "no health field is current");
  assert.deepEqual(validate(second, HEADER_SCHEMA), []);
});

test("the daemon's own age for a reading is kept", () => {
  const t = new HealthTracker(() => 5_000_000);
  const h = t.header("d", { battery: 50, age_s: 7.5 });
  assert.deepEqual(h.battery, { v: 50, age_s: 7.5 });
  assert.equal(h.data_age_s, 7.5);
});

test("headers are per device", () => {
  const t = new HealthTracker(() => 1);
  t.header("a", FULL);
  assert.equal(t.header("b", {}).battery.age_s, null);
});

test("focus_changed: true with the new window's title when the foreground differs", () => {
  const t = new HealthTracker();
  const before = { package: "com.android.settings", title: "Settings" };
  const h = t.header("d", { ...FULL, foreground: { package: "com.fixture.overlay", title: "Allow notifications?" } }, before);
  assert.equal(h.focus_changed.v, true);
  assert.equal(h.focus_changed.title, "Allow notifications?");
  const same = t.header("d", { ...FULL }, before);
  assert.equal(same.focus_changed.v, false);
  assert.equal(same.focus_changed.title, null);
});

test("focus_changed is unknown without a baseline or without a current foreground", () => {
  const t = new HealthTracker();
  assert.equal(t.header("d", { ...FULL }).focus_changed.v, "unknown");
  assert.equal(t.header("d", { model: "x" }, { package: "p" }).focus_changed.v, "unknown");
  assert.equal(t.lastForeground("d")?.package, "com.android.settings");
});

test("parseObservation drops every ill-formed part instead of guessing", () => {
  const o = parseObservation({ battery: 150, powered: "yes", temp_c: "hot", screen: "dim", lock: 1, foreground: {}, alias: "", model: 7, age_s: -3 });
  assert.deepEqual(o, {});
  assert.deepEqual(parseObservation(null), {});
  assert.deepEqual(parseObservation([1]), {});
  assert.deepEqual(parseObservation({ battery: 50, screen: "on", foreground: { title: "T" } }), { battery: 50, screen: "on", foreground: { title: "T" } });
});

test("the header schema accepts real and blank headers and rejects bad ones", () => {
  assert.deepEqual(validate(new HealthTracker().header("d", FULL), HEADER_SCHEMA), []);
  assert.deepEqual(validate(blankHeader(), HEADER_SCHEMA), []);
  const bad = blankHeader() as unknown as Record<string, unknown>;
  bad.battery = { v: 130, age_s: 0 };
  assert.ok(validate(bad, HEADER_SCHEMA).length > 0, describeIssues(validate(bad, HEADER_SCHEMA)));
  delete bad.battery;
  assert.ok(validate(bad, HEADER_SCHEMA).length > 0, "a missing field is a failure");
});

test("the one-line header says unknown, with the age, in plain text", () => {
  let now = 0;
  const t = new HealthTracker(() => now);
  t.header("d", FULL);
  now = 5000;
  const line = headerLine(t.header("d", { model: "POCO X6" }));
  assert.match(line, /battery unknown \(5s ago\)/);
  assert.match(line, /focus_changed unknown/);
  assert.match(headerLine(blankHeader()), /battery unknown,/);
  const fresh = headerLine(new HealthTracker().header("d", FULL, { package: "x" }));
  assert.match(fresh, /battery 81%/);
  assert.match(fresh, /focus_changed YES -> Settings/);
});
