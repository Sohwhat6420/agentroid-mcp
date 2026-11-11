import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { TARGET_PREFIX, TokenTable, bearer, fingerprint, modeAllows, namesBoundDevice, newToken, parseTarget, targetFor } from "../src/tokens.ts";
import { MemoryStore } from "./fake-store.ts";
import { mcpRoot } from "./util.ts";

test("tokens: a stored token authenticates to exactly its (serial, mode) pair; anything else is null", async () => {
  const store = new MemoryStore();
  const a = newToken();
  const b = newToken();
  await store.put("SERIALA", "full", a);
  await store.put("SERIALA", "read_only", b);
  const table = new TokenTable(await store.list());
  assert.equal(table.size, 2);
  assert.deepEqual({ ...table.authenticate(a), id: undefined }, { serial: "SERIALA", mode: "full", id: undefined });
  assert.equal(table.authenticate(b)?.mode, "read_only");
  for (const bad of [undefined, "", newToken(), a.slice(0, -1), `${a}x`, a.toUpperCase(), " ".repeat(40), "x".repeat(10_000)]) assert.equal(table.authenticate(bad), null);
});

test("tokens: the log id is a fingerprint, never the token or a piece of it", () => {
  const t = newToken();
  const table = new TokenTable([{ serial: "SERIALA", mode: "full", token: t }]);
  const id = table.authenticate(t)?.id ?? "";
  assert.match(id, /^SERIALA\/full#[0-9a-f]{10}$/);
  assert.equal(id.endsWith(fingerprint(t)), true);
  for (let i = 0; i + 8 <= t.length; i++) assert.equal(id.includes(t.slice(i, i + 8)), false);
});

test("tokens: new tokens are 256-bit and URL-safe; short, long, reused and badly bound entries are refused", () => {
  const t = newToken();
  assert.match(t, /^agr_[A-Za-z0-9_-]{43}$/);
  assert.notEqual(newToken(), t);
  assert.throws(() => new TokenTable([{ serial: "S1", mode: "full", token: "short" }]), /wrong length/);
  assert.throws(() => new TokenTable([{ serial: "S1", mode: "full", token: t }, { serial: "S2", mode: "full", token: t }]), /reuses/);
  assert.throws(() => new TokenTable([{ serial: "bad serial", mode: "full", token: t }]), /bad serial or mode/);
  assert.throws(() => new TokenTable([{ serial: "S1", mode: "denied" as "full", token: t }]), /bad serial or mode/);
  // the error text never carries the token
  try {
    new TokenTable([{ serial: "S1", mode: "full", token: t }, { serial: "S2", mode: "full", token: t }]);
  } catch (e) {
    assert.equal(String(e).includes(t), false);
  }
});

test("tokens: comparison is constant-time code (hash both sides, timingSafeEqual, no early exit)", () => {
  const src = readFileSync(join(mcpRoot, "src", "tokens.ts"), "utf8");
  const body = src.slice(src.indexOf("authenticate(presented"), src.indexOf("/** The token from an"));
  assert.match(body, /timingSafeEqual\(h, e\.hash\)/);
  assert.doesNotMatch(body, /return e\.binding|break;/);
  assert.doesNotMatch(body, /===\s*presented|presented\s*===/);
});

test("tokens: bearer() takes only `Bearer <token>`", () => {
  assert.equal(bearer("Bearer abc"), "abc");
  assert.equal(bearer("bearer abc"), "abc");
  for (const h of [undefined, "", "Basic abc", "Bearer", "Bearer ", "Bearer a b", "abc", "Token abc"]) assert.equal(bearer(h), undefined);
});

test("tokens: the mode rule: read_only gets read tools only; full and helper get every tool", () => {
  assert.equal(modeAllows("read_only", true), true);
  assert.equal(modeAllows("read_only", false), false);
  assert.equal(modeAllows("full", false), true);
  assert.equal(modeAllows("helper", false), true);
});

test("tokens: the device rule matches only the bound hardware serial's row", () => {
  const b = { serial: "HWA", mode: "full" as const, id: "x" };
  const row = { device: "HWA", serial: "192.0.2.5:5555", hw_serial: "HWA", alias: "poco" };
  for (const n of ["HWA", "192.0.2.5:5555", "poco"]) assert.equal(namesBoundDevice(b, n, row), true);
  assert.equal(namesBoundDevice(b, "HWB", row), false);
  assert.equal(namesBoundDevice(b, "HWA", { device: "HWB", serial: "s", hw_serial: "HWB" }), false);
  assert.equal(namesBoundDevice(b, "HWA", undefined), true);
  assert.equal(namesBoundDevice(b, "HWB", undefined), false);
});

test("tokens: Credential Manager target names round-trip and refuse bad input", () => {
  const t = targetFor("DEVICE0000EXAMPLE", "read_only");
  assert.equal(t, `${TARGET_PREFIX}DEVICE0000EXAMPLE:read_only`);
  assert.deepEqual(parseTarget(t), { serial: "DEVICE0000EXAMPLE", mode: "read_only" });
  assert.equal(parseTarget("agentroid:mcp-token:X:denied"), null);
  assert.equal(parseTarget("other:X:full"), null);
  assert.throws(() => targetFor("a:b", "full"), /bad device serial/);
  assert.throws(() => targetFor("A", "root" as "full"), /bad mode/);
});

test("tokens: the credstore helper is spawned hidden, with the secret on stdin, never argv", () => {
  const src = readFileSync(join(mcpRoot, "src", "tokens.ts"), "utf8");
  assert.match(src, /spawn\(this\.exe, \[\], \{ windowsHide: true/);
  assert.match(src, /child\.stdin\.end\(JSON\.stringify\(req\)\)/);
});
