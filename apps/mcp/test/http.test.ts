// The Streamable HTTP listener (task 2.6), on 127.0.0.1 only, against the in-process fake bridged
// and an in-memory token store. Every server started here is closed in `after`.

import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { DaemonClient } from "../src/daemon.ts";
import { BindRefused, checkBindAddress, checkPort, parseConfig, startHttpServer } from "../src/http-wiring.ts";
import type { HttpConfig, LogEntry, RunningServer } from "../src/http-wiring.ts";
import { TokenTable, newToken } from "../src/tokens.ts";
import type { Mode } from "../src/tokens.ts";
import { FakeDaemon, POCO, RO14, S24 } from "./fake-daemon.ts";
import type { FakeDevice } from "./fake-daemon.ts";
import { MemoryStore } from "./fake-store.ts";

/** Every log entry from every server in this file. */
const LOGS: LogEntry[] = [];
/** Everything written to stderr or the console while this file runs. */
const CAPTURED: string[] = [];
/** Every token string used in this file, valid or not. */
const SECRETS: string[] = [];
const tok = (): string => {
  const t = newToken();
  SECRETS.push(t);
  return t;
};

interface Rig {
  fake: FakeDaemon;
  srv: RunningServer;
  t: { pocoFull: string; pocoRead: string; ro14Read: string; s24Full: string };
  stop(): Promise<void>;
}

const rigs: Rig[] = [];

async function makeRig(config: Partial<HttpConfig> = {}, devices: FakeDevice[] = [POCO, RO14, S24]): Promise<Rig> {
  const fake = new FakeDaemon(devices);
  await fake.start();
  const store = new MemoryStore();
  const t = { pocoFull: tok(), pocoRead: tok(), ro14Read: tok(), s24Full: tok() };
  const put = (d: FakeDevice, m: Mode, token: string): Promise<void> => store.put(d.hw_serial, m, token);
  await put(POCO, "full", t.pocoFull);
  await put(POCO, "read_only", t.pocoRead);
  await put(RO14, "read_only", t.ro14Read);
  await put(S24, "full", t.s24Full); // a token cannot widen the daemon's S24 rule; tested below
  const daemon = new DaemonClient({ path: fake.path, session: "http-test", defaultTimeoutMs: 5000 });
  const srv = await startHttpServer({ config: { host: "127.0.0.1", port: 0, ...config }, tokens: new TokenTable(await store.list()), daemon, log: (e) => LOGS.push(e) });
  const r: Rig = {
    fake,
    srv,
    t,
    stop: async () => {
      await srv.close();
      daemon.close();
      await fake.stop();
    },
  };
  rigs.push(r);
  return r;
}

let R: Rig;
const restore: (() => void)[] = [];

before(async () => {
  const origWrite = process.stderr.write.bind(process.stderr);
  process.stderr.write = ((chunk: unknown, ...rest: unknown[]) => {
    CAPTURED.push(String(chunk));
    return (origWrite as (...a: unknown[]) => boolean)(chunk, ...rest);
  }) as typeof process.stderr.write;
  restore.push(() => {
    process.stderr.write = origWrite;
  });
  for (const k of ["log", "error", "warn", "info", "debug"] as const) {
    const orig = console[k];
    console[k] = (...a: unknown[]) => {
      CAPTURED.push(a.map(String).join(" "));
      orig(...a);
    };
    restore.push(() => {
      console[k] = orig;
    });
  }
  R = await makeRig({ rateLimit: { perMinute: 60_000, burst: 10_000 } }); // the rate limit has its own test and rig
});

after(async () => {
  for (const r of rigs) await r.stop();
  for (const f of restore) f();
});

const INIT = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "raw", version: "0" } } });

interface Reply {
  status: number;
  headers: http.IncomingHttpHeaders;
  body: string;
}

/** A raw request, so the test controls Host, Origin and every other header. */
function raw(srv: RunningServer, o: { token?: string; auth?: string; path?: string; method?: string; headers?: Record<string, string>; body?: string; chunks?: string[] } = {}): Promise<Reply> {
  return new Promise((resolve, reject) => {
    const headers: Record<string, string> = { Accept: "application/json, text/event-stream", "Content-Type": "application/json", ...o.headers };
    if (o.token !== undefined) headers.Authorization = `Bearer ${o.token}`;
    if (o.auth !== undefined) headers.Authorization = o.auth;
    const body = o.chunks === undefined ? (o.body ?? INIT) : undefined;
    if (body !== undefined) headers["Content-Length"] = String(Buffer.byteLength(body));
    const req = http.request({ host: "127.0.0.1", port: srv.port, path: o.path ?? "/mcp", method: o.method ?? "POST", headers }, (res) => {
      let data = "";
      res.setEncoding("utf8");
      res.on("data", (c: string) => (data += c));
      res.on("end", () => resolve({ status: res.statusCode ?? 0, headers: res.headers, body: data }));
    });
    req.on("error", reject);
    if (o.chunks !== undefined) for (const c of o.chunks) req.write(c);
    else if (o.method !== "GET" && o.method !== "DELETE") req.write(body);
    req.end();
  });
}

async function connect(srv: RunningServer, token: string): Promise<Client> {
  const client = new Client({ name: "http-test", version: "0.0.0" });
  await client.connect(new StreamableHTTPClientTransport(new URL(srv.url), { requestInit: { headers: { Authorization: `Bearer ${token}` } } }));
  return client;
}

type Structured = { ok: boolean; error?: { kind: string; message: string }; data?: Record<string, unknown> };
const sc = (r: unknown): Structured => (r as { structuredContent: Structured }).structuredContent;
const deviceCalls = (fake: FakeDaemon): { device: string; tool: string }[] =>
  fake.requests.filter((q) => q.method === "device.call").map((q) => {
    const p = q.params as { device: string; call: { tool: string } };
    return { device: p.device, tool: p.call.tool };
  });

// ---------------------------------------------------------------- bind

test("bind: refuses 0.0.0.0, ::, hostnames, LAN and every address outside 100.64.0.0/10, with a clear message", () => {
  const refused: [unknown, RegExp][] = [
    ["0.0.0.0", /every interface/],
    ["::", /IPv6/],
    ["::1", /IPv6/],
    ["[::]", /IPv6/],
    ["::ffff:100.64.0.1", /IPv6/],
    ["localhost", /hostname/],
    ["laptop.tail1234.ts.net", /hostname/],
    ["192.168.1.10", /LAN/],
    ["10.0.0.5", /LAN/],
    ["172.16.0.1", /LAN/],
    ["172.31.255.255", /LAN/],
    ["169.254.10.10", /link-local/],
    ["127.0.0.2", /only 127\.0\.0\.1/],
    ["100.63.255.255", /not a tailnet address/],
    ["100.128.0.1", /not a tailnet address/],
    ["100.100.100.100", /Tailscale's own DNS/],
    ["8.8.8.8", /not a tailnet address/],
    ["100.064.0.1", /dotted-quad/],
    ["100.64.0", /dotted-quad/],
    ["100.64.0.256", /dotted-quad/],
    [" 100.64.0.1", /hostname/],
    ["", /no address/],
    [undefined, /no address/],
    [1684275201, /no address/],
  ];
  for (const [host, why] of refused) {
    assert.throws(() => checkBindAddress(host), (e: Error) => {
      assert.ok(e instanceof BindRefused, `${String(host)}: ${e.name}`);
      assert.match(e.message, why, String(host));
      assert.match(e.message, /tailscale ip -4/);
      return true;
    });
  }
});

test("bind: accepts 127.0.0.1 and literal tailnet addresses", () => {
  for (const h of ["127.0.0.1", "100.64.0.0", "100.64.0.1", "100.101.102.103", "100.127.255.254"]) assert.equal(checkBindAddress(h), h);
});

test("bind: startHttpServer refuses 0.0.0.0, :: and a LAN address before listening", async () => {
  const daemon = new DaemonClient({ path: "\\\\.\\pipe\\agentroid-test-never-used", defaultTimeoutMs: 100 });
  const tokens = new TokenTable([{ serial: "X1", mode: "full", token: tok() }]);
  for (const host of ["0.0.0.0", "::", "192.168.1.10", "localhost"]) {
    // safety: if the check were broken, stop here, before startHttpServer could bind a real interface
    assert.throws(() => checkBindAddress(host), BindRefused, `${host} must be refused before anything listens`);
    await assert.rejects(startHttpServer({ config: { host, port: 0 }, tokens, daemon, log: (e) => LOGS.push(e) }), BindRefused);
  }
  daemon.close();
  assert.equal(LOGS.some((e) => e.event === "ready" && /0\.0\.0\.0|::|192\.168|localhost/.test(e.detail ?? "")), false);
});

test("config: the port comes from config; bad ports and port 0 are refused there", () => {
  assert.deepEqual(parseConfig({ host: "100.64.1.2", port: 8765 }), { host: "100.64.1.2", port: 8765 });
  for (const p of [-1, 65536, 1.5, "8765", undefined, null]) assert.throws(() => checkPort(p), BindRefused);
  assert.throws(() => parseConfig({ host: "100.64.1.2", port: 0 }), /tests only/);
  assert.throws(() => parseConfig({ host: "0.0.0.0", port: 8765 }), BindRefused);
  assert.throws(() => parseConfig({ host: "100.64.1.2" }), BindRefused);
  assert.throws(() => parseConfig([]), /JSON object/);
});

// ---------------------------------------------------------------- authentication

test("401 with no token, a wrong token, a token in the URL, or another scheme; empty body, no detail", async () => {
  const wrong = tok();
  const cases: { o: NonNullable<Parameters<typeof raw>[1]>; reason: string }[] = [
    { o: {}, reason: "no_token" },
    { o: { token: wrong }, reason: "bad_token" },
    { o: { token: R.t.pocoFull.slice(0, -2) }, reason: "bad_token" },
    { o: { auth: `Basic ${Buffer.from(`x:${R.t.pocoFull}`).toString("base64")}` }, reason: "bad_token" },
    { o: { auth: R.t.pocoFull }, reason: "bad_token" },
    { o: { path: `/mcp?token=${R.t.pocoFull}` }, reason: "no_token" },
    { o: { path: `/mcp?access_token=${R.t.pocoFull}`, method: "GET" }, reason: "no_token" },
    { o: { path: "/", method: "GET" }, reason: "no_token" },
    { o: { path: "/.well-known/oauth-protected-resource", method: "GET" }, reason: "no_token" },
  ];
  for (const { o, reason } of cases) {
    const before = LOGS.length;
    const r = await raw(R.srv, o);
    assert.equal(r.status, 401, JSON.stringify(o.path ?? ""));
    assert.equal(r.body, "");
    assert.equal(r.headers["www-authenticate"], "Bearer");
    const entry = LOGS.slice(before).find((e) => e.status === 401);
    assert.ok(entry, "the refusal was logged");
    assert.equal(entry.reason, reason);
    assert.equal(entry.path?.includes("?"), false, "the query string is never logged");
  }
  assert.equal(deviceCalls(R.fake).length, 0);
});

test("a valid token in the URL instead of the header is still refused, and the query string is never logged", async () => {
  const r = await raw(R.srv, { token: R.t.pocoFull, path: `/mcp?token=${R.t.pocoFull}` });
  assert.equal(r.status, 404);
  assert.equal(r.body, "");
  assert.equal(LOGS.at(-1)?.reason, "query_string");
  assert.equal(LOGS.at(-1)?.path, "/mcp");
});

// ---------------------------------------------------------------- MCP over HTTP

test("initialize plus tools/list over HTTP on 127.0.0.1 with a valid token, then a call", async () => {
  assert.match(R.srv.url, /^http:\/\/127\.0\.0\.1:\d+\/mcp$/);
  const client = await connect(R.srv, R.t.pocoFull);
  try {
    assert.equal(client.getServerVersion()?.name, "agentroid");
    assert.ok((client.getInstructions() ?? "").length > 100);
    const { tools } = await client.listTools();
    assert.equal(tools.length, 27);
    assert.equal(tools[0]?.name, "list_devices");
    const look = await client.callTool({ name: "look", arguments: {} });
    assert.equal(sc(look).ok, true, JSON.stringify(sc(look).error));
    assert.match(String(sc(look).data?.text), /POCO X6/);
  } finally {
    await client.close();
  }
});

test("a raw initialize gets a JSON answer (stateless, no SSE stream to hold open); GET and DELETE get 405", async () => {
  const r = await raw(R.srv, { token: R.t.pocoFull });
  assert.equal(r.status, 200);
  assert.match(String(r.headers["content-type"]), /application\/json/);
  assert.equal((JSON.parse(r.body) as { result: { serverInfo: { name: string } } }).result.serverInfo.name, "agentroid");
  assert.equal(r.headers["mcp-session-id"], undefined);
  for (const method of ["GET", "DELETE"]) {
    const g = await raw(R.srv, { token: R.t.pocoFull, method });
    assert.equal(g.status, 405);
    assert.equal(g.headers.allow, "POST");
  }
  assert.equal((await raw(R.srv, { token: R.t.pocoFull, path: "/other" })).status, 404);
});

// ---------------------------------------------------------------- token scope

test("a token bound to read_only is refused an acting tool, and the daemon never sees the call", async () => {
  const client = await connect(R.srv, R.t.pocoRead);
  try {
    const before = deviceCalls(R.fake).length;
    for (const [name, args] of [["tap", { x: 10, y: 20 }], ["type_text", { text: "hi" }], ["lease", { action: "acquire" }]] as const) {
      const r = await client.callTool({ name, arguments: args });
      assert.equal(r.isError, true, name);
      assert.equal(sc(r).error?.kind, "refused", name);
      assert.match(sc(r).error?.message ?? "", /this token is read_only/);
    }
    assert.equal(deviceCalls(R.fake).length, before, "no device.call reached the daemon");
    // reads still work with the same token
    const look = await client.callTool({ name: "look", arguments: {} });
    assert.equal(sc(look).ok, true, JSON.stringify(sc(look).error));
    const refusal = LOGS.findLast((e) => e.reason === "token_mode");
    assert.equal(refusal?.tool, "lease");
    assert.match(refusal?.token_id ?? "", new RegExp(`^${POCO.hw_serial}/read_only#[0-9a-f]{10}$`));
  } finally {
    await client.close();
  }
});

test("the same device with a full token may act (the refusal above came from the token's mode)", async () => {
  const client = await connect(R.srv, R.t.pocoFull);
  try {
    const r = await client.callTool({ name: "tap", arguments: { x: 10, y: 20 } });
    assert.equal(sc(r).ok, true, JSON.stringify(sc(r).error));
    assert.deepEqual(deviceCalls(R.fake).at(-1), { device: POCO.device, tool: "tap" });
  } finally {
    await client.close();
  }
});

test("a token for device A naming device B is refused, by any of B's names; the daemon never sees it", async () => {
  const client = await connect(R.srv, R.t.pocoFull);
  try {
    const before = deviceCalls(R.fake).length;
    for (const name of [RO14.device, RO14.serial, S24.hw_serial, "NOSUCHDEVICE"]) {
      for (const [tool, args] of [["look", {}], ["tap", { x: 1, y: 1 }], ["lease", { action: "acquire" }], ["lease", { action: "status" }]] as const) {
        const r = await client.callTool({ name: tool, arguments: { ...args, device: name } });
        assert.equal(r.isError, true, `${tool} ${name}`);
        assert.equal(sc(r).error?.kind, "refused", `${tool} ${name}`);
        assert.match(sc(r).error?.message ?? "", /bound to one device/);
      }
    }
    assert.equal(deviceCalls(R.fake).length, before);
    assert.equal(R.fake.requests.some((q) => q.method.startsWith("lease.") && [RO14.device, S24.device].includes(String((q.params as { device?: unknown }).device))), false);
    // its own device by its adb name works
    const own = await client.callTool({ name: "look", arguments: { device: POCO.serial } });
    assert.equal(sc(own).ok, true, JSON.stringify(sc(own).error));
    assert.equal(LOGS.findLast((e) => e.reason === "token_device")?.token_id?.startsWith(`${POCO.hw_serial}/full#`), true);
  } finally {
    await client.close();
  }
});

test("a scoped token sees only its device: list_devices shows one row, and no device named means its device", async () => {
  const client = await connect(R.srv, R.t.ro14Read);
  try {
    const list = await client.callTool({ name: "list_devices", arguments: { level: "full" } });
    assert.deepEqual((sc(list).data?.devices as { device: string }[]).map((d) => d.device), [RO14.device]);
    const look = await client.callTool({ name: "look", arguments: {} }); // three devices connected, none named
    assert.equal(sc(look).ok, true, JSON.stringify(sc(look).error));
    assert.deepEqual(deviceCalls(R.fake).at(-1), { device: RO14.device, tool: "look" });
  } finally {
    await client.close();
  }
});

test("the token scope never widens the daemon: an S24 token claiming full is still refused by the daemon's S24 rule", async () => {
  const client = await connect(R.srv, R.t.s24Full);
  try {
    const r = await client.callTool({ name: "tap", arguments: { x: 1, y: 1 } });
    assert.equal(sc(r).error?.kind, "refused");
    assert.equal(sc(r).error?.message, "tap refused: the protected phone is read-only and never a target");
    assert.deepEqual(deviceCalls(R.fake).at(-1), { device: S24.device, tool: "tap" }); // the daemon decided
    const look = await client.callTool({ name: "look", arguments: {} });
    assert.equal(sc(look).error?.kind, "refused");
    const serial = await client.callTool({ name: "getprop", arguments: { key: "ro.serialno" } });
    assert.equal(sc(serial).data?.text, S24.hw_serial);
  } finally {
    await client.close();
  }
});

// ---------------------------------------------------------------- hardening

test("Origin check: a foreign Origin is refused 403 and logged; the listener's own origin and no Origin pass", async () => {
  for (const origin of ["http://evil.example", "http://127.0.0.1.evil.example", "null", `http://localhost:${R.srv.port}`]) {
    const before = LOGS.length;
    const r = await raw(R.srv, { token: R.t.pocoFull, headers: { Origin: origin } });
    assert.equal(r.status, 403, origin);
    const entry = LOGS.slice(before).find((e) => e.reason === "origin");
    assert.ok(entry, `refusal logged for ${origin}`);
  }
  assert.equal((await raw(R.srv, { token: R.t.pocoFull, headers: { Origin: `http://127.0.0.1:${R.srv.port}` } })).status, 200);
  assert.equal((await raw(R.srv, { token: R.t.pocoFull })).status, 200);
});

test("Host check: a foreign Host header (DNS rebinding) is refused 403 and logged", async () => {
  for (const host of ["evil.example", `evil.example:${R.srv.port}`, `localhost:${R.srv.port}`]) {
    const before = LOGS.length;
    const r = await raw(R.srv, { token: R.t.pocoFull, headers: { Host: host } });
    assert.equal(r.status, 403, host);
    assert.ok(LOGS.slice(before).some((e) => e.reason === "host"));
  }
  assert.equal((await raw(R.srv, { token: R.t.pocoFull, headers: { Host: `127.0.0.1:${R.srv.port}` } })).status, 200);
});

test("body cap: a declared or streamed body over maxBodyBytes gets 413, and is logged", async () => {
  const small = await makeRig({ maxBodyBytes: 2048 });
  const big = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: { pad: "x".repeat(4096) } });
  const declared = await raw(small.srv, { token: small.t.pocoFull, body: big });
  assert.equal(declared.status, 413);
  assert.equal(LOGS.at(-1)?.reason, "body_too_large");
  const streamed = await raw(small.srv, { token: small.t.pocoFull, chunks: [big.slice(0, 1500), big.slice(1500)] });
  assert.equal(streamed.status, 413);
  assert.equal(LOGS.at(-1)?.reason, "body_too_large");
  assert.equal((await raw(small.srv, { token: small.t.pocoFull })).status, 200);
});

test("rate limit per token: a burst passes, then 429 with Retry-After; another token is unaffected", async () => {
  const rl = await makeRig({ rateLimit: { perMinute: 1, burst: 3 } });
  for (let i = 0; i < 3; i++) assert.equal((await raw(rl.srv, { token: rl.t.pocoFull })).status, 200);
  const r = await raw(rl.srv, { token: rl.t.pocoFull });
  assert.equal(r.status, 429);
  assert.equal(r.body, "");
  assert.ok(Number(r.headers["retry-after"]) >= 1);
  assert.equal(LOGS.at(-1)?.reason, "rate_limited");
  assert.equal((await raw(rl.srv, { token: rl.t.pocoRead })).status, 200);
});

// ---------------------------------------------------------------- secrets

test("no token string appears in any log line or any stderr/console output", () => {
  assert.ok(LOGS.length > 30, `expected many log lines, got ${LOGS.length}`);
  assert.ok(LOGS.some((e) => e.reason === "bad_token") && LOGS.some((e) => e.reason === "origin") && LOGS.some((e) => e.reason === "token_mode"));
  const text = [...LOGS.map((e) => JSON.stringify(e)), ...CAPTURED].join("\n");
  assert.ok(SECRETS.length >= 10);
  for (const s of SECRETS) {
    assert.equal(text.includes(s), false, "a whole token was logged");
    const body = s.slice(4); // after "agr_"
    for (let i = 0; i + 12 <= body.length; i += 4) assert.equal(text.includes(body.slice(i, i + 12)), false, "a piece of a token was logged");
  }
  for (const e of LOGS) assert.equal(JSON.stringify(e).toLowerCase().includes("authorization"), false);
});
