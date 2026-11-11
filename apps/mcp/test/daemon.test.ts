import assert from "node:assert/strict";
import net from "node:net";
import { test } from "node:test";
import { DaemonClient, DaemonError, MAX_CALL_MS, PIPE_PATH, clampTimeout, isPipeBusy } from "../src/daemon.ts";
import { CODE, FakeDaemon, POCO, randomPipeName } from "./fake-daemon.ts";

const busyError = (p: string): Error => Object.assign(new Error(`connect EBUSY ${p}`), { code: "EBUSY", errno: -4082 });

/** A socket factory that fails with ERROR_PIPE_BUSY `times` times, then connects for real. */
function busyThenReal(times: number): { connect: (p: string) => net.Socket; attempts: () => number } {
  let n = 0;
  return {
    attempts: () => n,
    connect: (p) => {
      n++;
      if (n <= times) {
        const s = new net.Socket();
        setImmediate(() => s.emit("error", busyError(p)));
        return s;
      }
      return net.connect(p);
    },
  };
}

async function withFake<T>(fn: (fake: FakeDaemon) => Promise<T>): Promise<T> {
  const fake = new FakeDaemon([POCO]);
  await fake.start();
  try {
    return await fn(fake);
  } finally {
    await fake.stop();
  }
}

const asDaemonError = (e: unknown): DaemonError => {
  assert.ok(e instanceof DaemonError, `expected DaemonError, got ${String(e)}`);
  return e;
};

test("the default path is the agentroid pipe, and this suite never uses it", () => {
  assert.equal(PIPE_PATH, "\\\\.\\pipe\\agentroid");
  assert.notEqual(randomPipeName(), PIPE_PATH);
  assert.match(randomPipeName(), /agentroid-test-/);
});

test("round trip over a real pipe: hello is sent first and names the holder", async () => {
  await withFake(async (fake) => {
    const c = new DaemonClient({ path: fake.path, session: "sess-1" });
    const res = (await c.request("devices.list")) as { devices: { device: string }[] };
    assert.equal(res.devices[0]?.device, POCO.device);
    assert.equal(fake.requests[0]?.method, "hello");
    assert.deepEqual(fake.requests[0]?.params, { session: "sess-1" });
    assert.equal(c.holder, "sess-1");
    assert.equal(c.connectionEpoch, 1);
    c.close();
  });
});

test("retries ERROR_PIPE_BUSY with backoff and then succeeds", async () => {
  await withFake(async (fake) => {
    const b = busyThenReal(3);
    const c = new DaemonClient({ path: fake.path, connect: b.connect, busyBackoffMs: [5, 10, 20] });
    const res = await c.request("devices.list");
    assert.ok(res);
    assert.equal(b.attempts(), 4, "three busy attempts and one that connected");
    assert.equal(c.busyRetries, 3);
    c.close();
  });
});

test("a pipe that stays busy ends in a timeout error inside the call's own deadline", async () => {
  const c = new DaemonClient({ path: randomPipeName(), connect: (p) => {
    const s = new net.Socket();
    setImmediate(() => s.emit("error", busyError(p)));
    return s;
  }, busyBackoffMs: [5] });
  const t0 = Date.now();
  const e = await c.request("devices.list", {}, { timeoutMs: 250 }).then(() => null, (x: unknown) => x);
  assert.equal(asDaemonError(e).kind, "timeout");
  assert.match((e as Error).message, /busy/i);
  assert.ok(c.busyRetries >= 3, `retried ${c.busyRetries} times`);
  assert.ok(Date.now() - t0 < 1500, "gave up near the deadline");
  c.close();
});

test("an error that is not pipe-busy is not retried", async () => {
  const c = new DaemonClient({ path: randomPipeName() });
  const e = await c.request("devices.list", {}, { timeoutMs: 2000 }).then(() => null, (x: unknown) => x);
  assert.equal(asDaemonError(e).kind, "transport");
  assert.match((e as Error).message, /cannot connect to bridged/);
  assert.equal(c.busyRetries, 0);
  c.close();
});

test("isPipeBusy recognises EBUSY, errno 231 and the Win32 name", () => {
  assert.ok(isPipeBusy({ code: "EBUSY" }));
  assert.ok(isPipeBusy({ errno: 231 }));
  assert.ok(isPipeBusy(new Error("ERROR_PIPE_BUSY")));
  assert.ok(!isPipeBusy({ code: "ENOENT" }));
  assert.ok(!isPipeBusy(null));
});

test("a request times out when bridged does not answer, and the connection stays usable", async () => {
  await withFake(async (fake) => {
    const c = new DaemonClient({ path: fake.path });
    await c.request("lease.acquire", { device: POCO.device });
    fake.misbehave = "hang";
    const t0 = Date.now();
    const e = await c.request("device.call", { device: POCO.device, call: { tool: "look" } }, { timeoutMs: 150 }).then(() => null, (x: unknown) => x);
    assert.equal(asDaemonError(e).kind, "timeout");
    assert.match((e as Error).message, /150 ms/);
    assert.ok(Date.now() - t0 < 1000);
    assert.equal(c.connectionEpoch, 1, "no reconnect after a timeout");
    assert.ok(await c.request("devices.list"), "the next call works");
    c.close();
  });
});

test("a late reply to a call that timed out is ignored", async () => {
  await withFake(async (fake) => {
    const c = new DaemonClient({ path: fake.path });
    await c.request("devices.list");
    fake.delayMs = 300;
    const e = await c.request("devices.list", {}, { timeoutMs: 80 }).then(() => null, (x: unknown) => x);
    assert.equal(asDaemonError(e).kind, "timeout");
    fake.delayMs = 0;
    await new Promise((r) => setTimeout(r, 400)); // the stale reply arrives now
    const res = (await c.request("lease.status")) as { leases: unknown[] };
    assert.deepEqual(res.leases, [], "the answer is lease.status's, not the stale devices.list");
    c.close();
  });
});

test("no call may be longer than 110 s", () => {
  assert.equal(MAX_CALL_MS, 110_000);
  assert.equal(clampTimeout(600_000), 110_000);
  assert.equal(clampTimeout(Number.POSITIVE_INFINITY), 110_000);
  assert.equal(clampTimeout(Number.NaN), 110_000);
  assert.equal(clampTimeout(30_000), 30_000);
  assert.equal(clampTimeout(0), 1);
});

test("an RPC error keeps its code, message and data", async () => {
  await withFake(async (fake) => {
    const a = new DaemonClient({ path: fake.path, session: "A" });
    const b = new DaemonClient({ path: fake.path, session: "B" });
    await a.request("lease.acquire", { device: POCO.device });
    const e = await b.request("lease.acquire", { device: POCO.device }).then(() => null, (x: unknown) => x);
    const de = asDaemonError(e);
    assert.equal(de.kind, "rpc");
    assert.equal(de.code, CODE.LEASE_HELD);
    assert.match(de.message, /is held by A/);
    assert.equal((de.data as { holder: string }).holder, "A");
    a.close();
    b.close();
  });
});

for (const [mode, kind] of [["garbage-line", "protocol"], ["not-an-object", "protocol"], ["no-id", "protocol"], ["no-result", "protocol"], ["bad-error", "protocol"]] as const) {
  test(`malformed reply (${mode}) is a protocol error, not a crash, and the client recovers`, async () => {
    await withFake(async (fake) => {
      const c = new DaemonClient({ path: fake.path });
      await c.request("lease.acquire", { device: POCO.device });
      fake.misbehave = mode;
      const e = await c.request("device.call", { device: POCO.device, call: { tool: "look" } }, { timeoutMs: 1000 }).then(() => null, (x: unknown) => x);
      assert.equal(asDaemonError(e).kind, kind);
      assert.ok(c.protocolFaults >= 1);
      assert.ok(await c.request("devices.list", {}, { timeoutMs: 2000 }), "a later call works (reconnecting if needed)");
      c.close();
    });
  });
}

test("a dropped connection is a transport error; the next call reconnects and the epoch moves", async () => {
  await withFake(async (fake) => {
    const c = new DaemonClient({ path: fake.path });
    await c.request("lease.acquire", { device: POCO.device });
    assert.equal(c.connectionEpoch, 1);
    fake.misbehave = "drop";
    const e = await c.request("device.call", { device: POCO.device, call: { tool: "look" } }, { timeoutMs: 1000 }).then(() => null, (x: unknown) => x);
    assert.equal(asDaemonError(e).kind, "transport");
    assert.ok(await c.request("devices.list", {}, { timeoutMs: 2000 }));
    assert.equal(c.connectionEpoch, 2, "a new connection: the old lease is gone");
    c.close();
  });
});

test("a closed client refuses new requests", async () => {
  const c = new DaemonClient({ path: randomPipeName() });
  c.close();
  const e = await c.request("devices.list").then(() => null, (x: unknown) => x);
  assert.equal(asDaemonError(e).kind, "transport");
});
