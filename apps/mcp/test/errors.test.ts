// The typed errors: the closed list of tool-result kinds, and the thrown start-up errors with their
// stable codes. Nothing here needs bridged or Windows.

import assert from "node:assert/strict";
import { test } from "node:test";
import { AgentroidError, BindRefused, ConfigError, CredstoreError, ERROR_CODES, ERROR_KINDS, ERROR_KIND_INFO, TokenError, isErrorKind } from "../src/errors.ts";
import { checkBindAddress, checkPort, parseConfig, RateLimiter } from "../src/http-wiring.ts";
import { ERROR_SCHEMA } from "../src/tools.ts";
import { CredStore, TokenTable, targetFor } from "../src/tokens.ts";
import { POCO, RO14 } from "./fake-daemon.ts";
import { rig } from "./util.ts";

/** Runs `f`, which must throw, and returns what it threw. */
function thrown(f: () => unknown): unknown {
  try {
    f();
  } catch (e) {
    return e;
  }
  assert.fail("expected a throw");
}

test("every error kind has a meaning and a retry rule, and the output schema allows exactly those kinds", () => {
  assert.deepEqual(Object.keys(ERROR_KIND_INFO).sort(), [...ERROR_KINDS].sort());
  for (const k of ERROR_KINDS) {
    const info = ERROR_KIND_INFO[k];
    assert.ok(info.meaning.length > 10 && info.meaning.endsWith("."), `${k}: a full sentence`);
    assert.ok(["no", "after_fix", "after_look", "yes"].includes(info.retry), `${k}: retry`);
  }
  const kinds = ((ERROR_SCHEMA as { properties: { kind: { enum: string[] } } }).properties.kind.enum);
  assert.deepEqual(kinds, [...ERROR_KINDS]);
  assert.equal(new Set(ERROR_KINDS).size, ERROR_KINDS.length, "no kind listed twice");
});

test("isErrorKind accepts the listed kinds only", () => {
  for (const k of ERROR_KINDS) assert.ok(isErrorKind(k));
  for (const v of ["", "Refused", "refused ", "error", null, undefined, 3, {}]) assert.equal(isErrorKind(v), false, String(v));
});

test("config errors are ConfigError with code config, bind refusals are a kind of ConfigError", () => {
  const cases: [() => unknown, string][] = [
    [() => parseConfig([]), "config"],
    [() => parseConfig({ host: "100.64.1.2", port: 8765, profile: "huge" }), "config"],
    [() => parseConfig({ host: "100.64.1.2", port: 8765, allowedHosts: "x" }), "config"],
    [() => new RateLimiter({ perMinute: 0, burst: 1 }), "config"],
    [() => parseConfig({ host: "100.64.1.2", port: 0 }), "bind_refused"],
    [() => checkBindAddress("0.0.0.0"), "bind_refused"],
    [() => checkPort(70000), "bind_refused"],
  ];
  for (const [f, code] of cases) {
    const e = thrown(f);
    assert.ok(e instanceof ConfigError, String(e));
    assert.ok(e instanceof AgentroidError);
    assert.equal(e.code, code, e.message);
    if (code === "bind_refused") {
      assert.ok(e instanceof BindRefused);
      assert.equal(e.name, "BindRefused");
    } else assert.equal(e.name, "ConfigError");
  }
});

test("token errors are TokenError with code token, and never quote the token", () => {
  const secret = "s".repeat(40);
  const cases: (() => unknown)[] = [
    () => new TokenTable([{ serial: "S1", mode: "full", token: "short" }]),
    () => new TokenTable([{ serial: "S1", mode: "full", token: secret }, { serial: "S2", mode: "full", token: secret }]),
    () => new TokenTable([{ serial: "bad serial", mode: "full", token: secret }]),
    () => targetFor("a:b", "full"),
    () => targetFor("A", "root" as "full"),
  ];
  for (const f of cases) {
    const e = thrown(f);
    assert.ok(e instanceof TokenError, String(e));
    assert.equal(e.code, "token");
    assert.ok(!e.message.includes(secret), "no secret in the message");
  }
});

test("credstore errors are CredstoreError with code credstore", async () => {
  const e = thrown(() => new CredStore("credstore", "other:"));
  assert.ok(e instanceof CredstoreError);
  assert.equal(e.code, "credstore");
  // a helper that cannot be started is a CredstoreError too, not a bare Error
  const s = new CredStore("/no/such/credstore-helper-for-this-test");
  await assert.rejects(s.list(), (err: unknown) => err instanceof CredstoreError && err.code === "credstore" && /cannot start/.test(err.message));
});

test("the thrown codes are a closed list", () => {
  assert.deepEqual([...ERROR_CODES], ["config", "bind_refused", "token", "credstore"]);
});

test("an error result names its kind and its run id in the text, matching the receipt (#32)", async () => {
  const r = await rig([POCO, RO14]);
  try {
    const refused = await r.call("tap", { device: RO14.device, index: 1 });
    const unknown = await r.call("no_such_tool", {});
    const bad = await r.call("tap", { device: POCO.device, index: "one" });
    for (const [res, kind] of [[refused, "refused"], [unknown, "unknown_tool"], [bad, "invalid_input"]] as const) {
      assert.equal(res.isError, true);
      const env = res.structuredContent;
      assert.equal(env.error?.kind, kind);
      const runId = env.receipt.run_id;
      assert.match(runId, /^test-session-\d+$/);
      const text = res.content[0]?.text ?? "";
      assert.ok(text.split("\n").includes(`error kind: ${kind}, run ${runId}`), text);
      assert.equal(env.receipt.steps_done, 0);
      assert.deepEqual(env.receipt.errors, [env.error?.message ?? ""]);
    }
    // each call gets its own run id
    const ids = [refused, unknown, bad].map((x) => x.structuredContent.receipt.run_id);
    assert.equal(new Set(ids).size, 3);
  } finally {
    await r.stop();
  }
});
