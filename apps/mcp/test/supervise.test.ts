// Supervision (task 2.11a): pushes only on stuck and done, once each, never with screen content;
// the "tell the user about job" instruction for the session; no MCP call over 110 s and progress
// every 30 s or less (fake clock); progress over stdio (in-memory SDK transport) and over HTTP
// (SSE); the read_only token scope on the job tools.
//
// Nothing here may reach ntfy.sh: every Supervisor gets a fake poster, and globalThis.fetch is
// replaced for the whole file by a stub that counts calls and fails; the last test asserts it was
// never called.

import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { Server as SdkServer } from "@modelcontextprotocol/sdk/server/index.js";
import { DaemonClient } from "../src/daemon.ts";
import type { DaemonLike } from "../src/daemon.ts";
import { scopedDaemon, startHttpServer } from "../src/http-wiring.ts";
import type { RunningServer } from "../src/http-wiring.ts";
import { installHandlers, wantsProgress } from "../src/sdk-handlers.ts";
import { CALL_DEADLINE_MS, MAX_PROGRESS_EVERY_MS, createServer } from "../src/server-core.ts";
import type { Progress, Server, Timers, ToolResult } from "../src/server-core.ts";
import { PUSH_STATES, Supervisor, cardInstruction, pushText, readTopic } from "../src/supervise.ts";
import type { Poster } from "../src/supervise.ts";
import { TokenTable, newToken } from "../src/tokens.ts";
import { TOOLS } from "../src/tools.ts";
import { FakeDaemon, POCO } from "./fake-daemon.ts";
import { MemoryStore } from "./fake-store.ts";

const TOPIC = "agentroid-test-topic-0001";
const FLOW = { name: "daily-checkin", steps: [{ id: "s1", tap: { text: "Check in" } }] };

// ---------------------------------------------------------------- no real push, ever

/** fetch calls to anywhere but this machine's loopback (the HTTP tests' own client uses fetch). */
let fetchCalls = 0;
const realFetch = globalThis.fetch;
before(() => {
  globalThis.fetch = (async (input: string | URL | Request, init?: RequestInit) => {
    const url = new URL(input instanceof Request ? input.url : String(input));
    if (url.hostname === "127.0.0.1") return realFetch(input, init);
    fetchCalls++;
    throw new Error(`the network is off in these tests (${url.hostname})`);
  }) as typeof fetch;
});
after(() => {
  globalThis.fetch = realFetch;
});

interface Posted {
  topic: string;
  title: string;
  message: string;
}

function fakePoster(): { poster: Poster; posted: Posted[] } {
  const posted: Posted[] = [];
  return {
    posted,
    poster: async (topic, title, message) => {
      posted.push({ topic, title, message });
      return null;
    },
  };
}

type Structured = { ok: boolean; error?: { kind: string; message: string }; data?: Record<string, unknown> };
const sc = (r: unknown): Structured => (r as { structuredContent: Structured }).structuredContent;
const text = (r: ToolResult): string => r.content.map((c) => c.text).join("\n");

// ---------------------------------------------------------------- the Supervisor alone

test("routine progress pushes nothing: running, paused, stopped and error never push", async () => {
  const { poster, posted } = fakePoster();
  const sup = new Supervisor({ poster, topic: () => TOPIC });
  for (let i = 0; i < 50; i++) assert.equal(sup.observe({ run_id: "r1", status: "running", steps_done: i, steps_planned: 50 }), null);
  for (const status of ["paused", "running", "stopped", "error", "unknown"]) assert.equal(sup.observe({ run_id: "r1", status }), null);
  await sup.idle();
  assert.equal(posted.length, 0);
  assert.equal(sup.pushes.length, 0);
  assert.deepEqual(sup.drain(), []);
  assert.deepEqual([...PUSH_STATES], ["stuck", "done"]);
});

test("exactly one push on stuck and one on done per job, however often the state is seen", async () => {
  const { poster, posted } = fakePoster();
  const sup = new Supervisor({ poster, topic: () => TOPIC });
  for (let i = 0; i < 5; i++) sup.observe({ run_id: "r1", status: "stuck", flow: "daily-checkin", steps_done: 3, steps_planned: 7 });
  for (let i = 0; i < 5; i++) sup.observe({ run_id: "r1", status: "done", flow: "daily-checkin", steps_done: 7, steps_planned: 7 });
  sup.observe({ run_id: "r2", status: "done", flow: "other", steps_done: 1, steps_planned: 1 });
  await sup.idle();
  assert.deepEqual(posted.map((p) => [p.topic, p.title]), [
    [TOPIC, "Agentroid: job stuck"],
    [TOPIC, "Agentroid: job done"],
    [TOPIC, "Agentroid: job done"],
  ]);
  const notices = sup.drain();
  assert.deepEqual(notices.map((n) => [n.run_id, n.state]), [["r1", "stuck"], ["r1", "done"], ["r2", "done"]]);
  assert.deepEqual(sup.drain(), [], "each notice is handed to the session once");
});

test("the push text carries no screen content: only the run id, the flow name and the counts", async () => {
  const { poster, posted } = fakePoster();
  const sup = new Supervisor({ poster, topic: () => TOPIC });
  const secret = "Hi Alex, your code is 482913";
  sup.observe({
    run_id: "20261020-101500-ab12",
    status: "stuck",
    flow: "daily-checkin<script>\n",
    steps_done: 3,
    steps_planned: 7,
    // everything below is read off the phone or derived from it, and must never be pushed
    line: secret,
    last_error: `text "${secret}" not found`,
    step_id: "tap-the-otp-482913",
    pause_reason: "unknown_overlay",
  } as never);
  await sup.idle();
  assert.equal(posted.length, 1);
  const all = `${posted[0]?.title} ${posted[0]?.message}`;
  for (const bad of ["Alex", "482913", "<", ">", "\n", "unknown_overlay", "tap-the"]) assert.equal(all.includes(bad), false, `push text holds ${JSON.stringify(bad)}: ${all}`);
  assert.equal(posted[0]?.message, "Job 20261020-101500-ab12 (daily-checkinscript) is stuck after 3 of 7 steps. Open the Claude Code session to look.");
  assert.equal(pushText({ run_id: "x", status: "done" }, "done").message, "Job x (?) is done: step count unknown.");
});

test("the card instruction is for the session to post, and names the receipt and the flight recorder", () => {
  const c = cardInstruction({ run_id: "r1", status: "stuck", flow: "f", steps_done: 1, steps_planned: 2 }, "stuck", "sent to the phone");
  assert.match(c, /^SESSION ACTION: tell the user about job r1 \(f\): stuck, 1 of 2 steps\./);
  assert.match(c, /job_status carries the receipt/);
  assert.match(c, /runs\/r1\/flight\//);
  assert.match(c, /never posts cards itself/);
  assert.doesNotMatch(cardInstruction({ run_id: "r1", status: "done" }, "done", "x"), /flight/);
});

test("no topic file means no push, and the notice says so (the card is still asked for)", async () => {
  const { poster, posted } = fakePoster();
  const sup = new Supervisor({ poster, topic: () => readTopic("Z:\\no\\such\\dir\\ntfy-topic.txt") });
  const n = sup.observe({ run_id: "r1", status: "done" });
  await sup.idle();
  assert.equal(posted.length, 0);
  assert.equal(n?.push, "not sent (no ntfy topic file)");
  assert.match(n?.card ?? "", /SESSION ACTION/);
  // a topic that breaks the watchdog's rule is no topic
  const bad = new Supervisor({ poster, topic: () => "short" });
  bad.observe({ run_id: "r1", status: "done" });
  await bad.idle();
  assert.equal(posted.length, 0);
});

test("a failed push is logged without the topic, even when the error text echoes the URL", async () => {
  const logged: string[] = [];
  const sup = new Supervisor({ poster: async (topic) => `TypeError: bad URL https://ntfy.sh/${topic}`, topic: () => TOPIC, log: (l) => logged.push(l) });
  sup.observe({ run_id: "r1", status: "stuck" });
  await sup.idle();
  assert.equal(logged.length, 1);
  assert.equal(logged[0]?.includes(TOPIC), false, logged[0] ?? "");
  assert.match(logged[0] ?? "", /push for r1 \(stuck\) failed: TypeError: bad URL https:\/\/ntfy\.sh\/<topic>/);
});

test("a supervisor with no poster given posts nothing (createServer's default cannot reach ntfy.sh)", async () => {
  const sup = new Supervisor({ topic: () => TOPIC });
  sup.observe({ run_id: "r1", status: "done" });
  await sup.idle();
  assert.equal(fetchCalls, 0);
});

// ---------------------------------------------------------------- through the MCP server

interface JobRig {
  fake: FakeDaemon;
  server: Server;
  sup: Supervisor;
  posted: Posted[];
  call(name: string, args?: unknown): Promise<ToolResult>;
  stop(): Promise<void>;
}

async function jobRig(script: string[], o: { watch?: boolean; pollMs?: number; now?: () => number; timers?: Timers } = {}): Promise<JobRig> {
  const fake = new FakeDaemon([POCO]);
  fake.jobScript = script;
  await fake.start();
  const client = new DaemonClient({ path: fake.path, session: "sup-test", defaultTimeoutMs: 5000 });
  const { poster, posted } = fakePoster();
  const sup = new Supervisor({ poster, topic: () => TOPIC });
  const server = createServer({ daemon: client, session: "sup-test", supervisor: sup, watch: o.watch ?? false, ...(o.pollMs === undefined ? {} : { pollMs: o.pollMs }), ...(o.now === undefined ? {} : { now: o.now }), ...(o.timers === undefined ? {} : { timers: o.timers }) });
  return {
    fake,
    server,
    sup,
    posted,
    call: (name, args) => server.callTool(name, args),
    stop: async () => {
      server.close();
      client.close();
      await fake.stop();
    },
  };
}

test("a routine run through the MCP tools: start, many status polls, pause (job_stop mode pause), resume (job_start resume_run_id): 0 pushes, no card", async () => {
  const r = await jobRig(["running"]);
  try {
    const start = await r.call("job_start", { flow: FLOW });
    assert.equal(sc(start).ok, true, JSON.stringify(sc(start).error));
    const runId = String(sc(start).data?.run_id);
    assert.match(text(start), /runs in the background: poll job_status/);
    for (let i = 0; i < 20; i++) {
      const s = await r.call("job_status", { run_id: runId });
      assert.equal(sc(s).ok, true, JSON.stringify(sc(s).error));
      assert.equal(sc(s).data?.status, "running");
      assert.doesNotMatch(text(s), /SESSION ACTION/);
      assert.equal(sc(s).data?.supervision, undefined);
    }
    assert.equal(sc(await r.call("job_stop", { run_id: runId, mode: "pause" })).data?.status, "paused");
    assert.equal(sc(await r.call("job_start", { resume_run_id: runId })).data?.status, "running");
    await r.sup.idle();
    assert.equal(r.posted.length, 0);
    assert.equal(r.fake.callsTo("job.status"), 20);
  } finally {
    await r.stop();
  }
});

const STEPS = [{ kind: "tap", select: { text: "Wi-Fi" } }, { kind: "assert", select: { text: "On" } }];

test("run_steps = job_start {steps, app, mode: sync}: the server polls, rows come back, a done run sends no push now or on a later job_status", async () => {
  const r = await jobRig(["running", "done"]);
  try {
    const ok = await r.call("job_start", { steps: STEPS, app: "com.android.settings", mode: "sync" });
    assert.equal(sc(ok).ok, true, JSON.stringify(sc(ok).error));
    const sent = r.fake.requests.filter((q) => q.method === "job.start").at(-1)?.params as Record<string, unknown>;
    assert.deepEqual({ steps: sent.steps, app: sent.app, mode: sent.mode, flow: sent.flow }, { steps: STEPS, app: "com.android.settings", mode: undefined, flow: undefined }, "mode is the server's own; the daemon never sees it");
    assert.equal(sc(ok).data?.status, "done");
    assert.deepEqual((sc(ok).data?.steps as { status: string }[]).map((s) => s.status), ["done"]);
    assert.match(text(ok), /\n {2}s1 done/);
    assert.match(text(ok), /receipt: outcome done, 1 steps done/);
    assert.ok(r.fake.callsTo("job.status") >= 2, "polled until the job left running");
    assert.deepEqual(r.fake.requests.map((q) => q.method).filter((m) => m.startsWith("job.")), ["job.start", ...Array(r.fake.callsTo("job.status")).fill("job.status"), "job.receipt"], "no other daemon call holds the connection");
    const runId = String(sc(ok).data?.run_id);
    // asking about it afterwards must not send the push the sync reply made unnecessary
    const later = await r.call("job_status", { run_id: runId });
    assert.equal(sc(later).data?.status, "done");
    await r.sup.idle();
    assert.equal(r.posted.length, 0, "the caller already had the answer: no done push");
    assert.doesNotMatch(text(ok) + text(later), /SESSION ACTION/);
  } finally {
    await r.stop();
  }
});

test("run_steps that ends stuck still pushes and asks for the card; one that ends in error pushes nothing (as an async job); both show the failed step", async () => {
  const stuck = await jobRig(["running", "running", "stuck"]);
  try {
    const bad = await stuck.call("job_start", { steps: STEPS, app: "com.android.settings", mode: "sync" });
    assert.equal(sc(bad).data?.status, "stuck");
    assert.deepEqual((sc(bad).data?.steps as { status: string }[]).map((s) => s.status), ["done", "failed"]);
    assert.match(text(bad), /s2 failed: no match/);
    await stuck.sup.idle();
    assert.equal(stuck.posted.length, 1, "a stuck run still goes through the supervisor");
    assert.match(text(bad), /SESSION ACTION/);
  } finally {
    await stuck.stop();
  }
  const errored = await jobRig(["running", "error"]);
  try {
    const bad = await errored.call("job_start", { steps: STEPS, app: "com.android.settings", mode: "sync" });
    assert.equal(sc(bad).data?.status, "error");
    assert.equal((sc(bad).data?.steps as { status: string }[])[0]?.status, "failed");
    await errored.sup.idle();
    assert.equal(errored.posted.length, 0);
  } finally {
    await errored.stop();
  }
});

test("run_steps that outlasts the wait comes back running with its run id (and the job keeps being watched), the budget spent by the clock", async () => {
  let t = 0;
  const r = await jobRig(["running"], { now: () => (t += 20_000), timers: { setTimeout: (fn, ms) => { if (ms <= 1_000) queueMicrotask(fn); return 0; }, clearTimeout: () => {}, setInterval: () => 0, clearInterval: () => {} } });
  try {
    const out = await r.call("job_start", { steps: STEPS, app: "com.android.settings", mode: "sync" });
    assert.equal(sc(out).ok, true, JSON.stringify(sc(out).error));
    assert.equal(sc(out).data?.status, "running");
    assert.match(String(sc(out).data?.run_id), /fake0001/);
    assert.match(text(out), /still running: poll job_status/);
    assert.equal((sc(out).data?.steps as { status: string }[])[0]?.status, "running");
    // the run can be asked about by its id
    assert.equal(sc(await r.call("job_status", { run_id: String(sc(out).data?.run_id) })).data?.status, "running");
  } finally {
    await r.stop();
  }
});

test("record: true goes to the daemon, and where the recording was saved comes back in the sync reply and in job_status", async () => {
  const r = await jobRig(["running", "done"]);
  try {
    const ok = await r.call("job_start", { steps: STEPS, app: "com.android.settings", mode: "sync", record: true });
    assert.equal(sc(ok).ok, true, JSON.stringify(sc(ok).error));
    const sent = r.fake.requests.filter((q) => q.method === "job.start").at(-1)?.params as Record<string, unknown>;
    assert.equal(sent.record, true);
    const rec = sc(ok).data?.recording as { flow: string; maestro: string; steps: number };
    assert.match(rec.flow, /fake0001\.flow\.yaml$/);
    assert.match(text(ok), /recorded: 1 steps in 20261020-000000-fake0001\.flow\.yaml \(replay: job_start flow "20261020-000000-fake0001\.flow\.yaml"\); Maestro export 20261020-000000-fake0001\.maestro\.yaml, 1 step\(s\) left as comments/);
    const later = await r.call("job_status", { run_id: String(sc(ok).data?.run_id) });
    assert.match(String((sc(later).data?.recording as { flow: string })?.flow), /\.flow\.yaml$/);
    assert.match(text(later), /\nrecorded: /);
  } finally {
    await r.stop();
  }
  // asked, refused: the caller is told why in the reply
  const refused = await jobRig(["running", "done"]);
  try {
    refused.fake.recordError = "not recorded: /steps/0/value holds the value of secret variable pw";
    const ok = await refused.call("job_start", { steps: STEPS, app: "com.android.settings", mode: "sync", record: true });
    assert.equal(sc(ok).ok, true);
    assert.match(text(ok), /\nrecording not saved: not recorded: \/steps\/0\/value holds the value of secret variable pw/);
    assert.doesNotMatch(text(ok), /\nrecorded: /);
  } finally {
    await refused.stop();
  }
  // not asked: nothing about a recording anywhere
  const plain = await jobRig(["running", "done"]);
  try {
    const ok = await plain.call("job_start", { steps: STEPS, app: "com.android.settings", mode: "sync" });
    assert.equal(sc(ok).data?.recording, undefined);
    assert.doesNotMatch(text(ok), /recorded:/);
  } finally {
    await plain.stop();
  }
});

test("record is refused with dry_run and with resume_run_id before the daemon is asked", async () => {
  const r = await jobRig(["running"]);
  try {
    const before = r.fake.requests.length;
    for (const bad of [{ flow: FLOW, trees: [], dry_run: true, record: true }, { resume_run_id: "r1", record: false }]) {
      const out = await r.call("job_start", bad);
      assert.equal(sc(out).ok, false, JSON.stringify(bad));
      assert.match(JSON.stringify(sc(out).error), /record/);
    }
    assert.equal(r.fake.requests.filter((q) => q.method.startsWith("job.")).length, r.fake.requests.slice(0, before).filter((q) => q.method.startsWith("job.")).length, "no job call was made");
  } finally {
    await r.stop();
  }
});

test("job_start arguments that do not fit together never reach the daemon", async () => {
  const r = await jobRig(["running"]);
  try {
    const before = r.fake.requests.length;
    for (const bad of [
      { steps: STEPS },
      { steps: STEPS, app: " " },
      { steps: STEPS, app: "p", flow: FLOW },
      { flow: FLOW, app: "p" },
      { steps: STEPS, app: "p", resume_run_id: "r1" },
      { steps: STEPS, app: "p", dry_run: true, flow: FLOW, trees: [] },
      { flow: FLOW, mode: "later" },
      { resume_run_id: "r1", mode: "sync" },
    ]) {
      assert.equal(sc(await r.call("job_start", bad)).error?.kind, "invalid_input", JSON.stringify(bad));
    }
    assert.equal(r.fake.requests.length, before);
  } finally {
    await r.stop();
  }
});

test("stuck then done through job_status: one push each, and the card instruction rides on the result that saw it", async () => {
  const r = await jobRig(["running", "stuck", "stuck", "stuck"]);
  try {
    const runId = String(sc(await r.call("job_start", { flow: FLOW })).data?.run_id);
    const seen: ToolResult[] = [];
    for (let i = 0; i < 4; i++) seen.push(await r.call("job_status", { run_id: runId }));
    await r.sup.idle();
    assert.deepEqual(r.posted.map((p) => p.title), ["Agentroid: job stuck"]);
    assert.doesNotMatch(text(seen[0] as ToolResult), /SESSION ACTION/);
    assert.match(text(seen[1] as ToolResult), /SESSION ACTION: tell the user about job \S+ \(daily-checkin\): stuck/);
    const sup = sc(seen[1]).data?.supervision as { state: string; push: string }[];
    assert.deepEqual(sup.map((n) => [n.state, n.push]), [["stuck", "sent to the phone"]]);
    for (const later of seen.slice(2)) assert.doesNotMatch(text(later), /SESSION ACTION/, "the card is asked for once");
    // the same run then finishes (the fake lets a stuck job be resumed by the script)
    r.fake.jobScript = ["done"];
    (r.fake.jobs.get(runId) as { next: number }).next = 0;
    const d1 = await r.call("job_status", { run_id: runId });
    const d2 = await r.call("job_status", { run_id: runId });
    await r.sup.idle();
    assert.deepEqual(r.posted.map((p) => p.title), ["Agentroid: job stuck", "Agentroid: job done"]);
    assert.match(text(d1), /SESSION ACTION: tell the user about job \S+ \(daily-checkin\): done, 1 of 1 steps|SESSION ACTION: tell the user about job \S+ \(daily-checkin\): done/);
    assert.doesNotMatch(text(d2), /SESSION ACTION/);
    for (const p of r.posted) assert.equal(p.topic, TOPIC);
  } finally {
    await r.stop();
  }
});

test("with watch on (stdio), the server's own job.status poll sees done: one push, and the next call carries the card", async () => {
  const r = await jobRig(["running", "running", "done"], { watch: true, pollMs: 15 });
  try {
    const runId = String(sc(await r.call("job_start", { flow: FLOW })).data?.run_id);
    const t0 = Date.now();
    while (r.sup.pushes.length === 0 && Date.now() - t0 < 5000) await new Promise((res) => setTimeout(res, 10));
    await r.sup.idle();
    assert.deepEqual(r.posted.map((p) => p.title), ["Agentroid: job done"]);
    assert.match(r.posted[0]?.message ?? "", new RegExp(`^Job ${runId} \\(daily-checkin\\) is done`));
    // the poll stops once the job ended
    const polls = r.fake.callsTo("job.status");
    await new Promise((res) => setTimeout(res, 80));
    assert.equal(r.fake.callsTo("job.status"), polls, "a done job is no longer polled");
    const next = await r.call("list_devices", {});
    assert.match(text(next), /SESSION ACTION: tell the user about job/);
    assert.doesNotMatch(text(await r.call("list_devices", {})), /SESSION ACTION/);
    assert.equal(r.posted.length, 1);
  } finally {
    await r.stop();
  }
});

test("the four job tools reach all eight daemon job.* methods (old tool -> new call), and job_status adds the receipt once the job has ended", async () => {
  const r = await jobRig(["running"]);
  try {
    const runId = String(sc(await r.call("job_start", { flow: FLOW })).data?.run_id);
    const jobMethods = (): string[] => r.fake.requests.map((q) => q.method).filter((m) => m.startsWith("job."));
    const calls: [string, Record<string, unknown>, string[]][] = [
      ["job_status", { run_id: runId }, ["job.status"]], // running: no receipt
      ["job_stop", { run_id: runId, mode: "pause" }, ["job.pause"]], // was job_pause
      ["job_start", { resume_run_id: runId, human_confirmed: true }, ["job.resume"]], // was job_resume
      ["job_start", { flow: FLOW, trees: [{ name: "home", xml: "<hierarchy/>" }], dry_run: true }, ["job.dry_run"]], // was job_dry_run
      ["run_script", { script: "1+1" }, ["job.run_script"]], // was job_run_script
      ["job_stop", { run_id: runId }, ["job.stop"]], // mode defaults to stop
      ["job_status", { run_id: runId }, ["job.status", "job.receipt"]], // stopped: with the receipt (was job_receipt)
    ];
    const results: ToolResult[] = [];
    for (const [name, args, want] of calls) {
      const before = jobMethods().length;
      const res = await r.call(name, args);
      results.push(res);
      assert.equal(sc(res).ok, true, `${name} ${JSON.stringify(args)}: ${JSON.stringify(sc(res).error)}`);
      assert.deepEqual(jobMethods().slice(before), want, `${name} ${JSON.stringify(args)}`);
    }
    const human = r.fake.requests.find((q) => q.method === "job.resume");
    assert.deepEqual(human?.params, { run_id: runId, human_confirmed: true });
    assert.equal(sc(results[0]).data?.receipt, undefined, "no receipt while the job runs");
    const last = sc(results.at(-1));
    assert.equal(last.data?.status, "stopped");
    assert.deepEqual(last.data?.detail, { outcome: "stopped" });
    assert.match(text(results.at(-1) as ToolResult), /receipt: outcome stopped/);
    assert.deepEqual(new Set(jobMethods()), new Set(["job.start", "job.status", "job.pause", "job.resume", "job.dry_run", "job.run_script", "job.stop", "job.receipt"]));
    // arguments that do not fit together never reach the daemon
    const before = jobMethods().length;
    for (const bad of [{}, { flow: FLOW, dry_run: true }, { flow: FLOW, resume_run_id: runId }, { flow: FLOW, trees: [] }, { trees: [], dry_run: true, flow: FLOW, resume_run_id: runId }, { flow: FLOW, human_confirmed: true }]) {
      assert.equal(sc(await r.call("job_start", bad)).error?.kind, "invalid_input", JSON.stringify(bad));
    }
    assert.equal(sc(await r.call("job_stop", { run_id: runId, mode: "halt" })).error?.kind, "invalid_input");
    assert.equal(jobMethods().length, before);
    assert.equal(sc(await r.call("job_status", { run_id: "nope" })).error?.kind, "daemon_error");
  } finally {
    await r.stop();
  }
});

// ---------------------------------------------------------------- 110 s cap and progress cadence, fake clock

class FakeClock implements Timers {
  t = 0;
  private id = 0;
  private due = new Map<number, { at: number; fn: () => void; every?: number }>();
  setTimeout(fn: () => void, ms: number): unknown {
    this.due.set(++this.id, { at: this.t + ms, fn });
    return this.id;
  }
  setInterval(fn: () => void, ms: number): unknown {
    this.due.set(++this.id, { at: this.t + ms, fn, every: ms });
    return this.id;
  }
  clearTimeout(h: unknown): void {
    this.due.delete(h as number);
  }
  clearInterval(h: unknown): void {
    this.due.delete(h as number);
  }
  /** Moves the clock to `to`, firing what falls due in order. */
  advanceTo(to: number): void {
    for (;;) {
      let next: [number, { at: number; fn: () => void; every?: number }] | undefined;
      for (const e of this.due) if (e[1].at <= to && (next === undefined || e[1].at < next[1].at)) next = e;
      if (next === undefined) break;
      const [id, e] = next;
      this.t = e.at;
      if (e.every !== undefined) e.at += e.every;
      else this.due.delete(id);
      e.fn();
    }
    this.t = to;
  }
}

/** A daemon whose job methods never answer (the device side answers at once). */
function hangingDaemon(): DaemonLike {
  return {
    busyRetries: 0,
    connectionEpoch: 1,
    close() {},
    async request(method) {
      if (method === "devices.list") return { devices: [{ device: POCO.device, serial: POCO.serial, hw_serial: POCO.hw_serial, model: POCO.model, mode: "full", holder: null }] };
      if (method === "lease.acquire") return { device: POCO.device, holder: "sup-test", epoch: 1 };
      return new Promise(() => {}); // job.*: never
    },
  };
}

const flush = (): Promise<void> => new Promise((r) => setImmediate(r));

/** Every shape of job call: each of the daemon's job.* methods behind the four tools. */
const JOB_CALLS: [string, Record<string, unknown>][] = [
  ["job_start", { flow: FLOW }],
  ["job_start", { flow: FLOW, trees: [], dry_run: true }],
  ["job_start", { resume_run_id: "r1" }],
  ["job_status", { run_id: "r1" }],
  ["job_stop", { run_id: "r1" }],
  ["job_stop", { run_id: "r1", mode: "pause" }],
  ["run_script", { script: "while(true){}" }],
];

/** Runs one call to its end on a fake clock; returns the result, when it ended, and the progress times. */
async function onFakeClock(name: string, args: Record<string, unknown>, withToken: boolean, progressEveryMs?: number): Promise<{ result: ToolResult; endedAt: number; beats: number[] }> {
  const clock = new FakeClock();
  const server = createServer({ daemon: hangingDaemon(), session: "clock", timers: clock, now: () => clock.t, ...(progressEveryMs === undefined ? {} : { progressEveryMs }) });
  const beats: number[] = [];
  let result: ToolResult | undefined;
  let endedAt = -1;
  void server.callTool(name, args, withToken ? { progress: (_p: Progress) => beats.push(clock.t) } : {}).then((r) => {
    result = r;
    endedAt = clock.t;
  });
  for (let t = 0; result === undefined && t <= 200_000; t += 500) {
    await flush();
    clock.advanceTo(t);
    await flush();
  }
  server.close();
  assert.ok(result !== undefined, `${name} never ended`);
  return { result, endedAt, beats };
}

test("no MCP call runs over 110 s: every job tool, in every mode, against a daemon that never answers ends as a timeout by 110 s (fake clock)", async () => {
  assert.ok(CALL_DEADLINE_MS <= 110_000);
  const jobTools = TOOLS.filter((t) => t.route === "job").map((t) => t.name);
  assert.deepEqual(jobTools, ["job_start", "job_status", "job_stop", "run_script"]);
  assert.deepEqual([...new Set(JOB_CALLS.map(([n]) => n))].sort(), [...jobTools].sort(), "every job tool is timed");
  for (const [name, args] of JOB_CALLS) {
    const { result, endedAt, beats } = await onFakeClock(name, args, true);
    assert.ok(endedAt > 0 && endedAt <= 110_000, `${name} ended at ${endedAt} ms`);
    assert.equal(sc(result).error?.kind, "timeout", `${name} ${JSON.stringify(args)}`);
    assert.match(sc(result).error?.message ?? "", /no call may run over 110 s/);
    assert.match(sc(result).error?.message ?? "", /poll job_status/);
    // progress every 30 s or less, from the start to the end of the call
    const marks = [0, ...beats, endedAt];
    for (let i = 1; i < marks.length; i++) assert.ok((marks[i] as number) - (marks[i - 1] as number) <= MAX_PROGRESS_EVERY_MS, `${name}: a ${(marks[i] as number) - (marks[i - 1] as number)} ms gap with no progress`);
    assert.ok(beats.length >= 3, `${name}: ${beats.length} progress notifications in ${endedAt} ms`);
  }
});

test("progress is sent only when the client gave a progressToken, and the cadence is clamped to 30 s", async () => {
  const none = await onFakeClock("run_script", { script: "1" }, false);
  assert.equal(none.beats.length, 0);
  assert.ok(none.endedAt <= 110_000);
  const slow = await onFakeClock("run_script", { script: "1" }, true, 90_000); // asks for 90 s; gets 30 s
  const marks = [0, ...slow.beats, slow.endedAt];
  for (let i = 1; i < marks.length; i++) assert.ok((marks[i] as number) - (marks[i - 1] as number) <= MAX_PROGRESS_EVERY_MS);
  assert.deepEqual(slow.beats.slice(0, 3), [30_000, 60_000, 90_000]);
});

// ---------------------------------------------------------------- progress over the wire: stdio and HTTP

/** A daemon that answers job.dry_run after `ms` (real time). */
function slowDaemon(ms: number): DaemonLike {
  return {
    busyRetries: 0,
    connectionEpoch: 1,
    close() {},
    async request(method) {
      if (method === "devices.list") return { devices: [{ device: POCO.device, serial: POCO.serial, hw_serial: POCO.hw_serial, model: POCO.model, mode: "full", holder: null }] };
      if (method === "lease.acquire") return { device: POCO.device, holder: "sup-test", epoch: 1 };
      await new Promise((r) => setTimeout(r, ms));
      return { flow: "daily-checkin", matched: 1, rows: [], overlays: [], text: "1 of 1 selectors matched" };
    },
  };
}

test("stdio side: a call with a progressToken gets notifications/progress over the SDK transport", async () => {
  const core = createServer({ daemon: slowDaemon(150), session: "stdio", progressEveryMs: 20 });
  const sdk = new SdkServer({ name: "agentroid", version: "0.1.0" }, { capabilities: { tools: {} } });
  installHandlers(sdk, core);
  const [a, b] = InMemoryTransport.createLinkedPair();
  await sdk.connect(a);
  const client = new Client({ name: "t", version: "0" });
  await client.connect(b);
  try {
    const got: number[] = [];
    const r = await client.callTool({ name: "job_start", arguments: { flow: FLOW, trees: [], dry_run: true } }, undefined, { onprogress: (p) => got.push(p.progress) });
    assert.equal(sc(r).ok, true, JSON.stringify(sc(r).error));
    assert.ok(got.length >= 2, `only ${got.length} progress notifications`);
    assert.deepEqual(got, got.map((_, i) => i + 1), "progress counts up");
    // without a token, none
    const quiet: number[] = [];
    const sent: string[] = [];
    client.fallbackNotificationHandler = async (n) => {
      sent.push(n.method);
    };
    await client.callTool({ name: "job_start", arguments: { flow: FLOW, trees: [], dry_run: true } });
    assert.deepEqual(quiet, []);
    assert.equal(sent.includes("notifications/progress"), false);
  } finally {
    await client.close();
    core.close();
  }
});

interface HttpRig {
  fake: FakeDaemon;
  srv: RunningServer;
  full: string;
  read: string;
  sup: Supervisor;
  posted: Posted[];
  stop(): Promise<void>;
}

async function httpRig(): Promise<HttpRig> {
  const fake = new FakeDaemon([POCO]);
  fake.jobScript = ["running", "done"];
  await fake.start();
  const store = new MemoryStore();
  const full = newToken();
  const read = newToken();
  await store.put(POCO.hw_serial, "full", full);
  await store.put(POCO.hw_serial, "read_only", read);
  const daemon = new DaemonClient({ path: fake.path, session: "http-sup", defaultTimeoutMs: 5000 });
  const { poster, posted } = fakePoster();
  const sup = new Supervisor({ poster, topic: () => TOPIC });
  const srv = await startHttpServer({
    config: { host: "127.0.0.1", port: 0, rateLimit: { perMinute: 60_000, burst: 10_000 } },
    tokens: new TokenTable(await store.list()),
    daemon,
    log: () => {},
    supervisor: sup,
    core: { progressEveryMs: 20 },
  });
  return {
    fake,
    srv,
    full,
    read,
    sup,
    posted,
    stop: async () => {
      await srv.close();
      daemon.close();
      await fake.stop();
    },
  };
}

async function connect(srv: RunningServer, token: string): Promise<Client> {
  const client = new Client({ name: "sup-test", version: "0.0.0" });
  await client.connect(new StreamableHTTPClientTransport(new URL(srv.url), { requestInit: { headers: { Authorization: `Bearer ${token}` } } }));
  return client;
}

test("HTTP side: the same call with a progressToken gets its progress (over SSE: JSON mode would drop it); calls without one still answer", async () => {
  assert.equal(wantsProgress({ method: "tools/call", params: { name: "x", _meta: { progressToken: 1 } } }), true);
  assert.equal(wantsProgress([{ method: "tools/list" }, { method: "tools/call", params: { _meta: { progressToken: "a" } } }]), true);
  assert.equal(wantsProgress({ method: "tools/call", params: { name: "x" } }), false);
  assert.equal(wantsProgress("garbage"), false);
  const R = await httpRig();
  let client: Client | undefined;
  try {
    client = await connect(R.srv, R.full);
    R.fake.delayMs = 120;
    const got: number[] = [];
    const r = await client.callTool({ name: "job_start", arguments: { flow: FLOW, trees: [], dry_run: true } }, undefined, { onprogress: (p) => got.push(p.progress) });
    assert.equal(sc(r).ok, true, JSON.stringify(sc(r).error));
    assert.ok(got.length >= 2, `only ${got.length} progress notifications over HTTP`);
    R.fake.delayMs = 0;
    const plain = await client.callTool({ name: "job_status", arguments: { run_id: "nope" } });
    assert.equal(sc(plain).error?.kind, "daemon_error");
  } finally {
    await client?.close();
    await R.stop();
  }
});

test("HTTP: a read_only token is refused job_start without dry_run and refused job_stop (the daemon never sees them), but may call job_status and job_start with dry_run: true", async () => {
  const R = await httpRig();
  const clients: Client[] = [];
  try {
    const ro = await connect(R.srv, R.read);
    clients.push(ro);
    const rw = await connect(R.srv, R.full);
    clients.push(rw);
    for (const args of [{ flow: FLOW }, { flow: FLOW, dry_run: false }, { resume_run_id: "r" }]) {
      const refused = await ro.callTool({ name: "job_start", arguments: args });
      assert.equal(refused.isError, true, JSON.stringify(args));
      assert.equal(sc(refused).error?.kind, "refused", JSON.stringify(args));
      assert.match(sc(refused).error?.message ?? "", /job_start refused: this token is read_only/);
    }
    for (const args of [{ run_id: "r" }, { run_id: "r", mode: "stop" }, { run_id: "r", mode: "pause" }]) {
      const refused = await ro.callTool({ name: "job_stop", arguments: args });
      assert.equal(sc(refused).error?.kind, "refused", JSON.stringify(args));
      assert.match(sc(refused).error?.message ?? "", /job_stop refused: this token is read_only/);
    }
    assert.equal(sc(await ro.callTool({ name: "run_script", arguments: { script: "1" } })).error?.kind, "refused", "run_script");
    for (const m of ["job.start", "job.resume", "job.pause", "job.stop", "job.run_script"]) assert.equal(R.fake.callsTo(m), 0, `${m} never reached the daemon`);

    // a full token starts the job; the read_only token may then read it and dry-run a flow
    const started = await rw.callTool({ name: "job_start", arguments: { flow: FLOW } });
    assert.equal(sc(started).ok, true, JSON.stringify(sc(started).error));
    const runId = String(sc(started).data?.run_id);
    const st = await ro.callTool({ name: "job_status", arguments: { run_id: runId } });
    assert.equal(sc(st).ok, true, JSON.stringify(sc(st).error));
    assert.equal(sc(st).data?.receipt, undefined, "no receipt while it runs");
    const dr = await ro.callTool({ name: "job_start", arguments: { flow: FLOW, trees: [], dry_run: true } });
    assert.equal(sc(dr).ok, true, JSON.stringify(sc(dr).error));
    assert.equal(R.fake.callsTo("job.dry_run"), 1);
    assert.equal(R.posted.length, 0, "running is routine: no push yet");
    // the HTTP side pushes too, once, when a session's call shows the job done (no background watch there)
    const done1 = await ro.callTool({ name: "job_status", arguments: { run_id: runId } });
    const done2 = await rw.callTool({ name: "job_status", arguments: { run_id: runId } });
    assert.equal(sc(done1).data?.status, "done");
    assert.deepEqual(sc(done1).data?.detail, { outcome: "done" }, "the read_only token gets the receipt with the ended status");
    assert.ok(R.fake.callsTo("job.receipt") >= 1);
    assert.match(JSON.stringify(done1.content), /SESSION ACTION: tell the user about job/);
    assert.doesNotMatch(JSON.stringify(done2.content), /SESSION ACTION/, "one card, shared across tokens");
    await R.sup.idle();
    assert.deepEqual(R.posted.map((p) => p.title), ["Agentroid: job done"]);
  } finally {
    for (const c of clients) await c.close();
    await R.stop();
  }
});

test("HTTP defence in depth: behind the gate, a read_only scoped daemon passes only job.status, job.receipt and job.dry_run", async () => {
  const seen: string[] = [];
  const inner: DaemonLike = {
    busyRetries: 0,
    connectionEpoch: 1,
    close() {},
    async request(method) {
      seen.push(method);
      if (method === "devices.list") return { devices: [{ device: POCO.device, serial: POCO.serial, hw_serial: POCO.hw_serial, model: POCO.model, mode: "full", holder: null }] };
      if (method === "job.status") return { run_id: "r", device: POCO.device, status: "running" };
      return { run_id: "r" };
    },
  };
  const ro = scopedDaemon(inner, { serial: POCO.hw_serial, mode: "read_only", id: "t-ro" });
  for (const m of ["job.start", "job.resume", "job.pause", "job.stop", "job.run_script"]) {
    await assert.rejects(ro.request(m, { device: POCO.device, run_id: "r" }), /refused by the token scope: this token is read_only/, m);
  }
  assert.equal(seen.filter((m) => m.startsWith("job.")).length, 0, "nothing acting reached the daemon");
  for (const m of ["job.status", "job.receipt", "job.dry_run"]) await ro.request(m, { device: POCO.device, run_id: "r" });
  assert.ok(seen.includes("job.receipt") && seen.includes("job.dry_run"));
  const full = scopedDaemon(inner, { serial: POCO.hw_serial, mode: "full", id: "t-full" });
  await full.request("job.start", { device: POCO.device, flow: {} });
  assert.ok(seen.includes("job.start"));
});

test("nothing in this file contacted ntfy.sh (fetch was never called)", () => {
  assert.equal(fetchCalls, 0);
});
