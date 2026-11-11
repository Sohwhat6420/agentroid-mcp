// The real Windows Credential Manager, through the built credstore helper, on throwaway targets
// that are deleted afterwards. Build the helper first: cargo build --offline -p agentroid-credstore.

import { after, test } from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { randomBytes } from "node:crypto";
import { join } from "node:path";
import { CredStore, TokenTable, newToken } from "../src/tokens.ts";
import { mcpRoot } from "./util.ts";

const exe = join(mcpRoot, "..", "..", "target", "debug", process.platform === "win32" ? "credstore.exe" : "credstore");
const have = process.platform === "win32" && existsSync(exe);
const prefix = `agentroid:test:mcp-${process.pid}-${randomBytes(4).toString("hex")}:`;
const store = have ? new CredStore(exe, prefix) : undefined;

after(async () => {
  // belt and braces: nothing under the throwaway prefix may survive the run
  if (store === undefined) return;
  for (const i of await store.listRaw()) await store.delete(i.target);
  assert.deepEqual(await store.listRaw(), []);
});

test("credstore: write, read, list, delete on a throwaway target (real Credential Manager)", { skip: have ? false : `credstore helper not built at ${exe}` }, async () => {
  const s = store as CredStore;
  const target = `${prefix}raw`;
  const secret = newToken();
  try {
    assert.equal(await s.read(target), null);
    await s.write(target, secret, "test");
    assert.equal(await s.read(target), secret);
    assert.deepEqual(await s.listRaw(), [{ target, secret }]);
    assert.equal(await s.delete(target), true);
    assert.equal(await s.read(target), null);
    assert.equal(await s.delete(target), false);
  } finally {
    await s.delete(target);
  }
});

test("credstore: tokens put through the TokenStore interface load into a working TokenTable, then are removed", { skip: have ? false : `credstore helper not built at ${exe}` }, async () => {
  const s = store as CredStore;
  const t1 = newToken();
  const t2 = newToken();
  try {
    await s.put("TESTSERIAL01", "full", t1);
    await s.put("TESTSERIAL01", "read_only", t2);
    const table = new TokenTable(await s.list());
    assert.equal(table.size, 2);
    assert.equal(table.authenticate(t1)?.mode, "full");
    assert.equal(table.authenticate(t2)?.mode, "read_only");
    assert.equal(table.authenticate(newToken()), null);
  } finally {
    assert.equal(await s.remove("TESTSERIAL01", "full"), true);
    assert.equal(await s.remove("TESTSERIAL01", "read_only"), true);
  }
  assert.deepEqual(await s.list(), []);
});

test("credstore: the helper refuses targets outside agentroid: and its errors carry no secret", { skip: have ? false : `credstore helper not built at ${exe}` }, async () => {
  const s = store as CredStore;
  const secret = newToken();
  await assert.rejects(s.run({ op: "write", target: "windows:not-ours", secret }), (e: Error) => /must start with agentroid:/.test(e.message) && !e.message.includes(secret));
  await assert.rejects(s.run({ op: "list", prefix: "" }), /must start with agentroid:/);
  assert.throws(() => new CredStore(exe, "other:"), /must start with agentroid:/);
});
