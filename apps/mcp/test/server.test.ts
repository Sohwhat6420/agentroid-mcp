import assert from "node:assert/strict";
import { join } from "node:path";
import { test } from "node:test";
import { DaemonError } from "../src/daemon.ts";
import type { DaemonLike } from "../src/daemon.ts";
import { createServer } from "../src/server-core.ts";
import type { Envelope, ToolResult } from "../src/server-core.ts";
import { isValid, validate } from "../src/schema.ts";
import type { Schema } from "../src/schema.ts";
import { TOOLS, listTools, outputSchemaFor, toolByName } from "../src/tools.ts";
import { CODE, POCO, RO14, S24, S24_READS, STRANGER } from "./fake-daemon.ts";
import { readJson, rig, schemasDir } from "./util.ts";

const receiptSchema = readJson(join(schemasDir(), "receipt.json")) as Schema;
const env = (r: ToolResult): Envelope => r.structuredContent;

/** What every result must have, success or error: a header, a receipt that validates against schemas/receipt.json, and the output schema. */
function assertWellFormed(name: string, r: ToolResult): void {
  const e = env(r);
  assert.ok(e.header, `${name}: header`);
  assert.ok(e.receipt, `${name}: receipt`);
  assert.deepEqual(validate(e.receipt, receiptSchema), [], `${name}: receipt vs schemas/receipt.json`);
  const def = toolByName(name);
  if (def) assert.deepEqual(validate(e, outputSchemaFor(def, true)), [], `${name}: output schema`);
  assert.equal(e.ok, !r.isError, `${name}: ok matches isError`);
  assert.equal(r.isError === true, e.error !== undefined, `${name}: error member matches isError`);
  assert.ok(r.content[0]?.text.startsWith("["), `${name}: the text starts with the health line`);
}

const SAMPLE_ARGS: Record<string, Record<string, unknown>> = {
  list_devices: {}, lease: { action: "status" }, look: { max_width: 720 }, ui_tree: { interactive_only: true }, screenshot: {},
  tap: { index: 3 }, long_press: { index: 3, ms: 600 }, swipe: { x1: 1, y1: 2, x2: 3, y2: 4 }, scroll: { direction: "down" },
  type_text: { text: "hi" }, key: { name: "back" }, unlock: {}, open_app: { package: "com.android.settings" }, open_url: { uri: "https://example.com" },
  start_activity: { component: "com.android.settings/.Settings" }, list_apps: {}, wait: { seconds: 1 }, getprop: { key: "ro.product.model" },
  set_value: { index: 2, value: "+2" }, pixel_probe: { x: 5, y: 6 }, pull_new: { dir: "/sdcard/DCIM" }, push: { local: "a.txt", phone_path: "/sdcard/a.txt" }, doctor: { diagnose: "dumpsys battery" },
  // 2.11a: in TOOLS order job_start runs first, and the fake's first run id is fixed
  job_start: { flow: { name: "f", steps: [] } }, job_status: { run_id: "20261020-000000-fake0001" },
  job_stop: { run_id: "20261020-000000-fake0001", mode: "pause" }, run_script: { script: "1+1" },
};

test("every tool answers with a header, a receipt and a valid output, against the daemon as it is today", async () => {
  const r = await rig([POCO]);
  try {
    for (const t of TOOLS) {
      const res = await r.call(t.name, SAMPLE_ARGS[t.name]);
      assertWellFormed(t.name, res);
      if (t.daemon === "served") assert.equal(res.isError, undefined, `${t.name} is served by the daemon today: ${res.content[0]?.text}`);
      else {
        // not in the daemon's ToolCall yet: its own error comes back, kept, as a tool error
        assert.equal(res.isError, true, t.name);
        assert.match(env(res).error?.message ?? "", /planned/, t.name);
      }
    }
  } finally {
    await r.stop();
  }
});

test("every tool also validates when the daemon serves it (full result shapes)", async () => {
  const r = await rig([POCO]);
  r.fake.serveAll = true;
  try {
    for (const t of TOOLS) {
      if (t.route === "device_call") {
        if (t.name === "pull_new") r.fake.nextCallResult = { tool: "pull_new", text: "pulled", path: "C:/x/a.jpg", size: 10, sha256: "a".repeat(64) };
        if (t.name === "set_value") r.fake.nextCallResult = { tool: "set_value", text: "set", value: "+2" };
        if (t.name === "pixel_probe") r.fake.nextCallResult = { tool: "pixel_probe", text: "px", rgb: [1, 2, 3] };
      }
      const res = await r.call(t.name, SAMPLE_ARGS[t.name]);
      assertWellFormed(t.name, res);
      assert.equal(res.isError, undefined, `${t.name}: ${res.content[0]?.text}`);
    }
  } finally {
    await r.stop();
  }
});

test("the header and receipt are on error results too (every kind of error)", async () => {
  const r = await rig([POCO, RO14]);
  try {
    const cases: [string, string, unknown][] = [
      ["unknown_tool", "no_such_tool", {}],
      ["invalid_input", "tap", { retry: 9 }],
      ["invalid_input", "tap", { bogus: 1 }],
      ["invalid_input", "getprop", {}],
      ["invalid_input", "getprop", { key: "ro.serialno;reboot" }],
      ["invalid_input", "look", "not an object"],
      ["ambiguous_device", "look", {}],
      ["unknown_device", "look", { device: "nope" }],
      ["refused", "tap", { device: RO14.device, index: 1 }],
    ];
    for (const [kind, tool, args] of cases) {
      const res = await r.call(tool, args);
      assertWellFormed(tool, res);
      assert.equal(res.isError, true, `${kind}`);
      assert.equal(env(res).error?.kind, kind, `${tool} ${JSON.stringify(args)}: ${res.content[0]?.text}`);
      assert.equal(env(res).receipt.steps_done, 0);
      assert.ok(env(res).receipt.errors.length === 1);
    }
  } finally {
    await r.stop();
  }
});

test("a transport failure (daemon gone) is a tool error with header and receipt, not a crash", async () => {
  const r = await rig([POCO]);
  await r.fake.stop();
  const res = await r.call("look", {});
  assertWellFormed("look", res);
  assert.equal(env(res).error?.kind, "transport");
  r.client.close();
});

test("a timeout is a tool error that tells the model to look before repeating", async () => {
  const r = await rig([POCO]);
  try {
    await r.call("lease", { action: "acquire" });
    r.fake.misbehave = "hang";
    const server = (await import("../src/server-core.ts")).createServer({ daemon: wrapTimeout(r.client, 150), session: "t" });
    const res = await server.callTool("look", {});
    assertWellFormed("look", res);
    assert.equal(env(res).error?.kind, "timeout");
    assert.match(res.content[0]?.text ?? "", /look again before repeating/);
  } finally {
    await r.stop();
  }
});

/** Forces a short timeout on every request, to test timeouts quickly. */
function wrapTimeout(d: DaemonLike, ms: number): DaemonLike {
  return {
    request: (m, p, o) => d.request(m, p, { ...o, timeoutMs: ms }),
    get busyRetries() { return d.busyRetries; },
    get connectionEpoch() { return d.connectionEpoch; },
    close: () => d.close(),
  };
}

test("a gate refusal (read_only) is a tool error, and the daemon's message is kept byte for byte", async () => {
  const r = await rig([POCO, RO14]);
  try {
    const res = await r.call("tap", { device: RO14.device, index: 1 });
    assert.equal(res.isError, true);
    const e = env(res).error;
    assert.equal(e?.kind, "refused");
    assert.equal(e?.code, CODE.REFUSED);
    assert.equal(e?.message, "tap refused: this device is read_only, so it only answers reads");
    // the label is the server's; the daemon's message can quote the screen, so it sits inside the phone-data fence
    assert.match(res.content[0]?.text ?? "", /Refused by the device-mode gate:\n<phone-data (k[0-9a-f]{12})>\ntap refused: this device is read_only, so it only answers reads\n<\/phone-data \1>\nerror kind: refused$/);
    // reads on the same device are fine
    const ok = await r.call("ui_tree", { device: RO14.device });
    assert.equal(ok.isError, undefined);
  } finally {
    await r.stop();
  }
});

test("an allowlist refusal is a tool error with the daemon's message", async () => {
  const r = await rig([POCO, STRANGER]);
  try {
    const res = await r.call("look", { device: STRANGER.device });
    assert.equal(env(res).error?.kind, "refused");
    assert.equal(env(res).error?.message, "192.0.2.99:5555 is not on the allowlist. Start bridged with --allow-serial <hardware serial> (or list it in allow.txt) to drive it.");
    assertWellFormed("look", res);
  } finally {
    await r.stop();
  }
});

test("the fake's S24 getprop keys are exactly the five approved getprop lines", () => {
  assert.deepEqual(S24_READS, ["ro.serialno", "ro.build.version.sdk", "ro.build.version.release", "ro.product.model", "ro.build.version.oneui"]);
});

test("S24: every call except an approved getprop is refused by the daemon, and the refusal passes through untouched", async () => {
  const r = await rig([POCO, S24]);
  try {
    const refusals: [string, Record<string, unknown>][] = [
      ["look", {}], ["ui_tree", {}], ["tap", { index: 1 }], ["tap", { x: 1, y: 2 }],
      ["getprop", { key: "ro.product.brand" }], ["getprop", { key: "ro.serialno.extra" }], ["getprop", { key: "ro.build.fingerprint" }],
    ];
    for (const [tool, args] of refusals) {
      const before = r.fake.callsTo("device.call");
      const res = await r.call(tool, { device: S24.device, ...args });
      assertWellFormed(tool, res);
      assert.equal(res.isError, true, `${tool} ${JSON.stringify(args)}`);
      assert.equal(env(res).error?.kind, "refused");
      assert.equal(env(res).error?.message, `${tool} refused: the protected phone is read-only and never a target`, "the daemon's text, unchanged");
      assert.equal(r.fake.callsTo("device.call"), before + 1, "the refusal came from the daemon: the client sent the call and did not pre-empt it");
    }
    const ok = await r.call("getprop", { device: S24.device, key: "ro.serialno" });
    assert.equal(ok.isError, undefined);
    assert.equal(env(ok).data?.text, S24.hw_serial);
    assertWellFormed("getprop", ok);
    for (const key of S24_READS.slice(1)) {
      const res = await r.call("getprop", { device: S24.device, key });
      assert.equal(res.isError, undefined, `approved probe ${key}`);
      assert.equal(env(res).data?.text, `value-of-${key}`);
    }
    // the protected phone is never picked for you: with two devices and no name the call errors
    const none = await r.call("getprop", { key: "ro.serialno" });
    assert.equal(env(none).error?.kind, "ambiguous_device");
  } finally {
    await r.stop();
  }
});

test("a malformed daemon reply never crashes the server (every kind), and the next call works", async () => {
  const r = await rig([POCO]);
  try {
    for (const mode of ["garbage-line", "not-an-object", "no-id", "no-result", "bad-error", "wrong-shape", "drop"] as const) {
      await r.call("ui_tree", {}); // make sure we hold the lease on a live connection
      r.fake.misbehave = mode;
      const res = await r.call("look", {});
      assertWellFormed("look", res);
      assert.equal(res.isError, true, mode);
      assert.ok(["bad_reply", "transport"].includes(env(res).error?.kind ?? ""), `${mode}: ${env(res).error?.kind}`);
      const next = await r.call("ui_tree", {});
      assert.equal(next.isError, undefined, `after ${mode}: ${next.content[0]?.text}`);
    }
  } finally {
    await r.stop();
  }
});

test("a malformed or hostile client object never throws out of callTool", async () => {
  const bad: unknown[] = [null, undefined, 5, "x", [], [1], { device: 5 }, { level: "loud" }, { index: -1 }, { x: "1" }];
  const mk = (reply: unknown): DaemonLike => ({
    request: async () => reply,
    busyRetries: 0,
    connectionEpoch: 1,
    close: () => {},
  });
  for (const reply of [null, undefined, 7, "s", [], {}, { devices: "x" }, { devices: [{}] }, { devices: [null] }, { devices: [{ device: "d", serial: 1, hw_serial: "h", model: "m", mode: "full" }] }]) {
    const s = createServer({ daemon: mk(reply) });
    for (const tool of ["list_devices", "look", "lease", "nope"]) {
      const res = await s.callTool(tool, tool === "lease" ? { action: "acquire" } : {});
      assertWellFormed(tool, res);
      assert.equal(res.isError, true);
    }
  }
  const r = createServer({ daemon: mk({ devices: [] }) });
  for (const a of bad) assertWellFormed("tap", await r.callTool("tap", a));
  for (const a of bad) assertWellFormed("x", await r.callTool(a as string, a));
  // a daemon double that throws things that are not even Errors
  for (const thrown of [new Error("boom"), "string", 42, null, undefined, new DaemonError("rpc", "m", -32603)]) {
    const s = createServer({ daemon: { request: async () => { throw thrown; }, busyRetries: 0, connectionEpoch: 1, close: () => {} } });
    const res = await s.callTool("list_devices", {});
    assertWellFormed("list_devices", res);
    assert.equal(res.isError, true);
  }
});

test("a daemon result that breaks its output schema is a bad_output error, not a crash", async () => {
  const r = await rig([POCO]);
  r.fake.serveAll = true;
  try {
    r.fake.nextCallResult = { tool: "pull_new", text: "pulled", path: "C:/x/a.jpg", size: 10 }; // no sha256
    const res = await r.call("pull_new", { dir: "/sdcard" });
    assert.equal(res.isError, true);
    assert.equal(env(res).error?.kind, "bad_output");
    assert.match(env(res).error?.message ?? "", /sha256/);
    assert.equal(env(res).data, undefined, "no unvalidated data leaks out");
    assertWellFormed("pull_new", res);
  } finally {
    await r.stop();
  }
});

test("require, retry and verify pass through to the daemon unchanged; retry above 3 never leaves the server", async () => {
  const r = await rig([POCO]);
  try {
    const args = { index: 1, require: [{ text: "OK" }], retry: 3, verify: { text: "Done" } };
    const res = await r.call("tap", args);
    assert.equal(res.isError, undefined);
    const sent = r.fake.requests.filter((q) => q.method === "device.call").at(-1)?.params as { device: string; call: Record<string, unknown> };
    assert.deepEqual(sent.call, { tool: "tap", index: 1, require: [{ text: "OK" }], retry: 3, verify: { text: "Done" } });
    assert.equal(sent.device, POCO.device);
    const before = r.fake.callsTo("device.call");
    const over = await r.call("tap", { index: 1, retry: 4 });
    assert.equal(env(over).error?.kind, "invalid_input");
    assert.equal(r.fake.callsTo("device.call"), before);
    // level is for the server only: it is not forwarded
    await r.call("look", { level: "full", max_width: 400 });
    const look = r.fake.requests.filter((q) => q.method === "device.call").at(-1)?.params as { call: Record<string, unknown> };
    assert.deepEqual(look.call, { tool: "look", max_width: 400 });
  } finally {
    await r.stop();
  }
});

test("no call is asked for more than 110 s, whatever retry and wait say", async () => {
  const seen: (number | undefined)[] = [];
  const d: DaemonLike = {
    request: async (m, _p, o) => {
      seen.push(o?.timeoutMs);
      if (m === "devices.list") return { devices: [{ device: "d", serial: "s", hw_serial: "d", model: "m", mode: "full" }] };
      if (m === "lease.acquire") return { device: "d", holder: "h", epoch: 1 };
      return { tool: "wait", text: "ok" };
    },
    busyRetries: 0,
    connectionEpoch: 1,
    close: () => {},
  };
  const server = createServer({ daemon: d });
  const waited = await server.callTool("wait", { seconds: 100 }); // 30 s + 100 s
  assert.equal(waited.isError, undefined, waited.content[0]?.text ?? "");
  assert.equal(seen.at(-1), 110_000);
  const tapped = await server.callTool("tap", { index: 1, retry: 3 }); // 30 s x 4
  assert.equal(tapped.isError, undefined, tapped.content[0]?.text ?? "");
  assert.equal(seen.at(-1), 110_000);
  assert.ok(seen.filter((x) => x !== undefined).every((x) => (x as number) <= 110_000), JSON.stringify(seen));
});

test("the server takes the lease once, and a second session is refused with the holder's name", async () => {
  const r = await rig([POCO]);
  const other = await (async () => {
    const { DaemonClient } = await import("../src/daemon.ts");
    const c = new DaemonClient({ path: r.fake.path, session: "other-session" });
    return { c, s: createServer({ daemon: c, session: "other-session" }) };
  })();
  try {
    await r.call("look", {});
    await r.call("look", {});
    assert.equal(r.fake.callsTo("lease.acquire"), 1, "leased once, not per call");
    const res = await other.s.callTool("tap", { index: 1 });
    assert.equal(env(res).error?.kind, "lease_held");
    assert.match(env(res).error?.message ?? "", /is held by test-session/);
    assertWellFormed("tap", res);
    const rel = await r.call("lease", { action: "release" });
    assert.equal(env(rel).data?.released, true);
    const now = await other.s.callTool("tap", { index: 1 });
    assert.equal(now.isError, undefined, "free after release");
  } finally {
    other.c.close();
    await r.stop();
  }
});

test("after a reconnect the lease is taken again (the old one died with the connection)", async () => {
  const r = await rig([POCO]);
  try {
    await r.call("look", {});
    r.fake.misbehave = "drop";
    const dropped = await r.call("look", {});
    assert.equal(dropped.isError, true);
    const again = await r.call("look", {});
    assert.equal(again.isError, undefined, again.content[0]?.text ?? "");
    assert.equal(r.fake.callsTo("lease.acquire"), 2);
  } finally {
    await r.stop();
  }
});

test("with one device the name may be left out; with two it may not; the server never picks devices[0]", async () => {
  const one = await rig([POCO]);
  try {
    assert.equal((await one.call("look", {})).isError, undefined);
  } finally {
    await one.stop();
  }
  const two = await rig([POCO, RO14]);
  try {
    const res = await two.call("look", {});
    assert.equal(env(res).error?.kind, "ambiguous_device");
    assert.equal(two.fake.callsTo("device.call"), 0);
    assert.equal((await two.call("look", { device: POCO.serial })).isError, undefined, "adb serial works too");
  } finally {
    await two.stop();
  }
  const none = await rig([]);
  try {
    assert.equal(env(await none.call("look", {})).error?.kind, "unknown_device");
  } finally {
    await none.stop();
  }
});

test("header: identity from devices.list, unknown health with null age today, no invention", async () => {
  const r = await rig([POCO]);
  try {
    const h = env(await r.call("look", {})).header;
    assert.deepEqual(h.model, { v: "POCO X6", age_s: 0 });
    assert.deepEqual(h.mode, { v: "full", age_s: 0 });
    assert.deepEqual(h.alias, { v: "unknown", age_s: null }, "the daemon gives no alias, so none is shown");
    assert.deepEqual(h.battery, { v: "unknown", age_s: null });
    assert.deepEqual(h.temp_c, { v: "unknown", age_s: null });
    assert.deepEqual(h.screen, { v: "unknown", age_s: null });
    assert.equal(h.focus_changed.v, "unknown");
  } finally {
    await r.stop();
  }
});

test("header: health from the daemon is shown, and focus_changed is true with the new title when an overlay takes focus", async () => {
  const r = await rig([POCO]);
  try {
    r.fake.health[POCO.device] = { battery: 81, powered: false, temp_c: 33.5, screen: "on", lock: "unlocked", foreground: { package: "com.android.settings", title: "Settings" } };
    const first = env(await r.call("look", {})).header;
    assert.equal(first.battery.v, 81);
    assert.equal(first.powered.v, false);
    assert.equal(first.temp_c.v, 33.5);
    assert.equal(first.screen.v, "on");
    assert.equal(first.lock.v, "unlocked");
    assert.deepEqual(first.foreground.v, { package: "com.android.settings", title: "Settings" });
    assert.equal(first.focus_changed.v, "unknown", "no earlier reading to compare with");
    const same = env(await r.call("look", {})).header;
    assert.equal(same.focus_changed.v, false);
    // the fixture overlay takes focus
    r.fake.health[POCO.device] = { battery: 80, powered: false, temp_c: 34, screen: "on", lock: "unlocked", foreground: { package: "com.fixture.overlay", title: "Allow notifications?" } };
    const res = await r.call("tap", { index: 4 });
    const h = env(res).header;
    assert.equal(h.focus_changed.v, true);
    assert.equal(h.focus_changed.title, "Allow notifications?");
    assert.match(res.content[0]?.text ?? "", /focus_changed YES -> Allow notifications\?/);
    assert.equal(env(res).receipt.battery_change, -1, "receipt battery change from the header");
    assert.equal(env(res).receipt.phone_peak_c, 34);
    // the daemon stops giving health: unknown with an age, never the old value
    delete r.fake.health[POCO.device];
    const gone = env(await r.call("look", {})).header;
    assert.equal(gone.battery.v, "unknown");
    assert.equal(typeof gone.battery.age_s, "number");
    assert.equal(gone.focus_changed.v, "unknown");
  } finally {
    await r.stop();
  }
});

test("header: a refusal that carries health shows it, and a garbage health field shows unknown", async () => {
  const r = await rig([POCO, RO14]);
  try {
    r.fake.health[RO14.device] = { battery: 55, screen: "dim", temp_c: "warm", foreground: { title: "Lock screen" } };
    const res = await r.call("tap", { device: RO14.device, index: 1 });
    const h = env(res).header;
    assert.equal(h.battery.v, 55);
    assert.equal(h.screen.v, "unknown", "'dim' is not a screen state");
    assert.equal(h.temp_c.v, "unknown");
    assert.deepEqual(h.foreground.v, { title: "Lock screen" });
    assertWellFormed("tap", res);
  } finally {
    await r.stop();
  }
});

test("receipt: one per call, sequential run ids, honest numbers, errors recorded", async () => {
  const r = await rig([POCO]);
  try {
    const a = env(await r.call("look", {})).receipt;
    const b = env(await r.call("nope", {})).receipt;
    assert.equal(a.run_id, "test-session-1");
    assert.equal(b.run_id, "test-session-2");
    assert.equal(a.flow, "look");
    assert.equal(a.device, POCO.device);
    assert.equal(b.device, "none");
    assert.equal(a.steps_planned, 1);
    assert.equal(a.steps_done, 1);
    assert.equal(b.steps_done, 0);
    assert.deepEqual([a.model_calls, a.tokens_in, a.tokens_out, a.pool.usd], [0, 0, 0, 0], "the server makes no model calls");
    assert.deepEqual(b.errors, ['no tool named "nope"']);
    assert.ok(a.wall_time_s >= 0 && a.laptop_cpu_s >= 0);
  } finally {
    await r.stop();
  }
});

test("list_devices says which connected phones are not on the allowlist, and stays quiet for allowed rows and older daemons", async () => {
  const row = (device: string, extra: Record<string, unknown>) => ({ device, serial: device, hw_serial: device, model: "X", mode: "full", holder: null, ...extra });
  const d: DaemonLike = {
    request: async (m) => (m === "devices.list" ? { devices: [row("ALLOWED000001", { allowed: true }), row("REFUSED000002", { allowed: false }), row("OLDDAEMON0003", {})] } : {}),
    busyRetries: 0,
    connectionEpoch: 1,
    close: () => {},
  };
  const res = await createServer({ daemon: d }).callTool("list_devices", {});
  assertWellFormed("list_devices", res);
  const lines = String(res.content[0]?.text).split("\n").filter((l) => l.includes("mode="));
  assert.equal(lines.length, 3);
  assert.ok(!lines[0]?.includes("NOT ON THE ALLOWLIST"), lines[0] ?? "");
  assert.match(lines[1] ?? "", /REFUSED000002 .*NOT ON THE ALLOWLIST \(every call is refused/);
  assert.ok(!lines[2]?.includes("NOT ON THE ALLOWLIST"), "a daemon that sends no allowed field is treated as allowed");
  const devices = (env(res).data as { devices: { device: string; allowed?: boolean }[] }).devices;
  assert.deepEqual(devices.map((x) => x.allowed), [true, false, undefined]);
});

test("receipt: pipe-busy retries during the call are counted", async () => {
  let busy = 0;
  const d: DaemonLike = {
    request: async (m) => {
      if (m === "devices.list") { busy += 2; return { devices: [] }; }
      return {};
    },
    get busyRetries() { return busy; },
    connectionEpoch: 1,
    close: () => {},
  };
  const res = await createServer({ daemon: d }).callTool("list_devices", {});
  assert.equal(env(res).receipt.retries, 2);
});

test("response levels: digest cuts long text, default gives more, full gives all", async () => {
  const r = await rig([POCO]);
  r.fake.serveAll = true;
  try {
    const long = "x".repeat(5000);
    const get = async (level: string): Promise<Envelope> => {
      r.fake.nextCallResult = { tool: "ui_tree", text: long };
      return env(await r.call("ui_tree", { level }));
    };
    const d = await get("digest");
    const m = await get("default");
    const f = await get("full");
    assert.ok(String(d.data?.text).length < 600 && d.data?.truncated === true);
    assert.match(String(d.data?.text), /ask for level "default"/);
    assert.ok(String(m.data?.text).length > 3900 && String(m.data?.text).length < 4300 && m.data?.truncated === true);
    assert.equal(f.data?.text, long);
    assert.equal(f.data?.truncated, undefined);
    const dflt = env(await (async () => { r.fake.nextCallResult = { tool: "ui_tree", text: long }; return r.call("ui_tree", {}); })());
    assert.equal(dflt.data?.truncated, true, "digest is the default");
    // list_devices follows the same levels
    const dig = env(await r.call("list_devices", {})).data?.devices as Record<string, unknown>[];
    const ful = env(await r.call("list_devices", { level: "full" })).data?.devices as Record<string, unknown>[];
    assert.ok(!("serial" in (dig[0] ?? {})) && "serial" in (ful[0] ?? {}));
  } finally {
    await r.stop();
  }
});

test("the minimal profile hides the other tools: calling one is an unknown_tool error", async () => {
  const r = await rig([POCO], { profile: "minimal" });
  try {
    assert.deepEqual(r.server.listTools().map((t) => t.name), listTools("minimal").map((t) => t.name));
    const res = await r.call("swipe", { x1: 1, y1: 1, x2: 2, y2: 2 });
    assert.equal(env(res).error?.kind, "unknown_tool");
    assertWellFormed("swipe", res);
    assert.equal((await r.call("getprop", { key: "ro.product.model" })).isError, undefined);
  } finally {
    await r.stop();
  }
});

test("concurrent calls do not mix up their answers", async () => {
  const r = await rig([POCO]);
  try {
    const keys = ["a.b", "c.d", "e.f", "g.h", "i.j"];
    const out = await Promise.all(keys.map((key) => r.call("getprop", { key })));
    out.forEach((res, i) => assert.equal(env(res).data?.text, `value-of-${keys[i]}`));
  } finally {
    await r.stop();
  }
});

test("lease tool: acquire, status, release", async () => {
  const r = await rig([POCO]);
  try {
    const a = await r.call("lease", { action: "acquire" });
    assert.equal((env(a).data?.lease as { holder: string }).holder, "test-session");
    const s = await r.call("lease", { action: "status" });
    assert.equal((env(s).data?.leases as unknown[]).length, 1);
    const all = await r.call("lease", { action: "status", device: POCO.device });
    assert.equal((env(all).data?.leases as unknown[]).length, 1);
    const rel = await r.call("lease", { action: "release" });
    assert.equal(env(rel).data?.released, true);
    assert.equal(isValid(env(rel), outputSchemaFor(toolByName("lease")!, true)), true);
  } finally {
    await r.stop();
  }
});

test("instructions and the tool list come from the same source the budget measures", async () => {
  const r = await rig([POCO]);
  try {
    assert.ok(r.server.instructions.length < 4096);
    assert.deepEqual(r.server.listTools(), listTools("full"));
  } finally {
    await r.stop();
  }
});
