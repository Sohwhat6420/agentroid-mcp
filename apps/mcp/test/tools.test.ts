import assert from "node:assert/strict";
import { test } from "node:test";
import { INSTRUCTIONS, LEVELS, MAX_TOOLS, TOOLS, callIsRead, listTools, outputSchemaFor, toolByName } from "../src/tools.ts";

// The order is part of the contract (it keeps the model's prompt cache valid). Adding a tool means
// adding it here on purpose, at the end, never in the middle.
const EXPECTED_ORDER = [
  "list_devices", "lease", "look", "ui_tree", "screenshot", "tap", "long_press", "swipe", "scroll", "type_text", "key", "unlock",
  "open_app", "open_url", "start_activity", "list_apps", "wait", "getprop", "set_value", "pixel_probe", "pull_new", "push", "doctor",
  // 2.11a, appended: the four job tools
  "job_start", "job_status", "job_stop", "run_script",
];

test("tool order is stable and matches the pinned list", () => {
  assert.deepEqual(listTools("full").map((t) => t.name), EXPECTED_ORDER);
  assert.equal(JSON.stringify(listTools("full")), JSON.stringify(listTools("full")), "two calls serialize identically");
});

test("the minimal profile is an ordered subset of the full one and keeps doctor and getprop", () => {
  const full = listTools("full").map((t) => t.name);
  const min = listTools("minimal").map((t) => t.name);
  assert.ok(min.length < full.length);
  let at = -1;
  for (const n of min) {
    const i = full.indexOf(n);
    assert.ok(i > at, `${n} is out of order in minimal`);
    at = i;
  }
  assert.ok(min.includes("doctor"), "doctor stays in minimal (plan 2.4, U5)");
  assert.ok(min.includes("getprop"));
});

test("at most MAX_TOOLS (27, the design limit) tools, unique names", () => {
  assert.equal(MAX_TOOLS, 27);
  assert.ok(TOOLS.length <= MAX_TOOLS);
  assert.equal(TOOLS.length, 27);
  assert.equal(new Set(TOOLS.map((t) => t.name)).size, TOOLS.length);
});

test("the four job tools forward to job.*, never wait over 110 s; job_status is a read, and job_start only with dry_run: true", () => {
  const jobs = TOOLS.filter((t) => t.route === "job");
  assert.deepEqual(jobs.map((t) => t.name), EXPECTED_ORDER.slice(-4));
  assert.deepEqual(jobs.map((t) => t.method), ["job.start", "job.status", "job.stop", "job.run_script"]);
  for (const t of jobs) {
    assert.ok((t.timeoutMs ?? 0) > 0 && (t.timeoutMs ?? 0) <= 110_000, t.name);
    assert.equal(t.minimal, false, `${t.name} stays out of the minimal profile`);
  }
  assert.deepEqual(jobs.filter((t) => t.read).map((t) => t.name), ["job_status"]);
  const start = toolByName("job_start")!;
  assert.equal(callIsRead(start, { flow: {}, trees: [], dry_run: true }), true);
  for (const args of [{ flow: {} }, { flow: {}, dry_run: false }, { resume_run_id: "r" }]) assert.equal(callIsRead(start, args), false, JSON.stringify(args));
  for (const n of ["job_stop", "run_script"]) assert.equal(callIsRead(toolByName(n)!, { run_id: "r", mode: "pause", dry_run: true }), false, n);
  const stop = toolByName("job_stop")!.input as { properties: { mode: { enum: string[] } } };
  assert.deepEqual(stop.properties.mode.enum, ["stop", "pause"]);
});

test("every tool has an input and an output schema; inputs are closed objects", () => {
  for (const t of TOOLS) {
    const i = t.input as Record<string, unknown>;
    assert.equal(i.type, "object", t.name);
    assert.equal(i.additionalProperties, false, `${t.name} input must reject unknown arguments`);
    const o = outputSchemaFor(t, false) as { required: string[]; properties: Record<string, unknown> };
    assert.deepEqual(o.required, ["ok", "header", "receipt"], t.name);
    // the short form leaves out a data schema that only says "an object" (the job tools)
    assert.ok(o.properties.data !== undefined || JSON.stringify(t.data) === '{"type":"object"}', t.name);
    const d = outputSchemaFor(t, true) as { required: string[]; properties: Record<string, unknown> };
    assert.ok(d.properties.header && d.properties.receipt && d.properties.error, `${t.name} detailed form`);
  }
});

test("getprop is a read, in the minimal profile, with a key pattern", () => {
  const g = toolByName("getprop");
  assert.ok(g);
  assert.equal(g.read, true);
  assert.equal(g.minimal, true);
  const w = listTools("full").find((t) => t.name === "getprop");
  assert.deepEqual(w?.annotations, { readOnlyHint: true });
});

test("reads carry readOnlyHint and acting tools do not", () => {
  for (const t of TOOLS) assert.equal(listTools("full").find((w) => w.name === t.name)?.annotations?.readOnlyHint === true, t.read, t.name);
  assert.equal(toolByName("tap")?.read, false);
  assert.equal(toolByName("doctor")?.read, true);
});

test("require, retry (at most 3) and verify are declared on the acting tools", () => {
  const props = (n: string): Record<string, { maximum?: number }> => (toolByName(n)?.input as { properties: Record<string, { maximum?: number }> }).properties;
  for (const n of ["tap", "swipe", "type_text", "key"]) {
    assert.ok(props(n).require && props(n).verify && props(n).retry, n);
    assert.equal(props(n).retry?.maximum, 3, n);
  }
});

test("three response levels, digest first", () => {
  assert.deepEqual([...LEVELS], ["digest", "default", "full"]);
  const p = (toolByName("look")?.input as { properties: { level: { enum: string[] } } }).properties.level;
  assert.deepEqual(p.enum, ["digest", "default", "full"]);
});

test("instructions mention the header, the protected-list rule and the 110 s limit", () => {
  assert.match(INSTRUCTIONS, /focus_changed/);
  assert.match(INSTRUCTIONS, /device on the protected list acts only under its owner's approval lease/);
  assert.match(INSTRUCTIONS, /lease:pending refusal means the owner has been asked; wait for his Allow, then retry the same call/);
  assert.doesNotMatch(INSTRUCTIONS, /fixed set of reads/);
  assert.match(INSTRUCTIONS, /110 seconds/);
});

test("2.4: every tool is served by the daemon (A's 14 in devcall.rs, B's doctor, pull_new and push in devtools.rs)", () => {
  const planned = TOOLS.filter((t) => t.daemon === "planned").map((t) => t.name).sort();
  assert.deepEqual(planned, []);
});

test("stable refs: tap and long_press take a ref, ui_tree takes since, and the instructions explain both", () => {
  const props = (name: string) => Object.keys((toolByName(name)?.input as { properties: Record<string, unknown> }).properties);
  assert.ok(props("tap").includes("ref") && props("long_press").includes("ref"));
  assert.ok(props("ui_tree").includes("since"));
  assert.match(INSTRUCTIONS, /ref/);
  assert.match(INSTRUCTIONS, /since=/);
  // look.since and type_text.ref are in the full list only (the minimal list is tight on tokens), and the call accepts them in both
  const wireProps = (profile: "full" | "minimal", name: string) => Object.keys((listTools(profile).find((t) => t.name === name)?.inputSchema as { properties: Record<string, unknown> }).properties);
  assert.ok(wireProps("full", "look").includes("since") && wireProps("full", "type_text").includes("ref"));
  assert.ok(!wireProps("minimal", "look").includes("since") && !wireProps("minimal", "type_text").includes("ref"));
  assert.ok(props("look").includes("since") && props("type_text").includes("ref"));
});
