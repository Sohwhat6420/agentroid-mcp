// The tool handlers, on plain objects (no SDK). `callTool` validates the input, talks to bridged,
// and returns {content, structuredContent} where structuredContent is {ok, data|error, header,
// receipt}. It never throws and never returns a result that fails its own output schema: a
// failure of any kind becomes an error result that still carries a header and a receipt.
//
// Where the health header comes from: bridged cannot be asked for it separately (that would be a
// second path to the phone, and the protected phone may only answer its approved getprop probes). It rides on the
// device.call result as `health`, or on a refusal's error `data.health`. Anything the daemon does
// not send shows as "unknown" with its age.
//
// Supervision (task 2.11a):
// - the 8 job tools forward to the daemon's job.* methods; long work is only job_start (returns at
//   once) plus job_status;
// - no call runs over CALL_DEADLINE_MS (109 s, under the 110 s cap; Claude Code backgrounds an MCP
//   call at 2 min): a call still waiting then becomes a `timeout` error;
// - while a call is in flight and the client gave a progressToken, a progress notification goes out
//   every PROGRESS_EVERY_MS (25 s, so never more than 30 s apart);
// - every job state the server sees (job tool results, and with `watch` a poll of the jobs it
//   started) goes to the Supervisor: `stuck` / `done` push once and queue a "tell the user about the job"
//   instruction, which the next tool result carries. Routine progress pushes nothing.

import { DaemonError, MAX_CALL_MS, clampTimeout } from "./daemon.ts";
import type { DaemonLike } from "./daemon.ts";
import { HealthTracker, blankHeader, headerLine, mergeObservations, parseObservation } from "./header.ts";
import type { Header, Observation } from "./header.ts";
import { buildReceipt } from "./receipt.ts";
import type { Receipt } from "./receipt.ts";
import { describeIssues, validate } from "./schema.ts";
import type { Schema } from "./schema.ts";
import { Supervisor } from "./supervise.ts";
import type { JobSnapshot, Notice } from "./supervise.ts";
import { INSTRUCTIONS, TOOLS, listTools, outputSchemaFor, toolByName } from "./tools.ts";
import type { ErrorKind, Level, Profile, ToolDef, WireTool } from "./tools.ts";
import { fence } from "./untrusted.ts";

// JSON-RPC application codes (crates/bridged/src/proto.rs).
const CODE = { LEASE_HELD: -32001, NOT_HOLDER: -32002, REFUSED: -32003, DEVICE_ERROR: -32004, UNKNOWN_DEVICE: -32005, INVALID_PARAMS: -32602 } as const;

/** Longest `data.text` per level; `full` is not cut. */
const TEXT_LIMIT: Record<Level, number> = { digest: 400, default: 4000, full: Number.POSITIVE_INFINITY };
/** Time for one device call when the tool says nothing else; the daemon client clamps to 110 s.
 *  Longer than the daemon's approval window (DEFAULT_EXPIRY_MS, 90 s, crates/core/src/approval.rs)
 *  so an `asked` call is never reported as a timeout while its Allow can still run. A phone that
 *  hangs still fails sooner: the daemon's own adb calls time out at 30 s. */
const CALL_TIMEOUT_MS = 100_000;
/** job_start with mode sync polls job.status this often (the daemon's own wait would hold its connection: one call at a time). */
const SYNC_POLL_MS = 250;
/** ...and stops waiting after this long, so the receipt read still fits inside the 109 s call deadline; the job goes on and the answer says running. */
const SYNC_BUDGET_MS = 90_000;
/** No MCP call runs longer than this (the design limit: no call over 110 s). */
export const CALL_DEADLINE_MS = MAX_CALL_MS - 1_000;
/** Progress notifications while a call is in flight (every 30 s or less). */
export const PROGRESS_EVERY_MS = 25_000;
export const MAX_PROGRESS_EVERY_MS = 30_000;
/** How often the server polls job.status for the jobs it is watching. */
export const JOB_POLL_MS = 15_000;
/** Job states after which a job is no longer watched. */
const TERMINAL = new Set(["done", "stuck", "stopped", "error"]);

/** The timer functions the server uses (a fake clock in tests). */
export interface Timers {
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
  setInterval(fn: () => void, ms: number): unknown;
  clearInterval(handle: unknown): void;
}

/** Node's timers, unref'd so a pending heartbeat or poll never keeps the process alive. */
export const realTimers: Timers = {
  setTimeout: (fn, ms) => setTimeout(fn, ms).unref(),
  clearTimeout: (h) => clearTimeout(h as NodeJS.Timeout),
  setInterval: (fn, ms) => setInterval(fn, ms).unref(),
  clearInterval: (h) => clearInterval(h as NodeJS.Timeout),
};

/** One MCP progress notification (the SDK adapter adds the client's progressToken). */
export interface Progress {
  progress: number;
  message: string;
}

/** Per call, from the SDK adapter. */
export interface CallExtra {
  /** Set when the client supplied a progressToken. */
  progress?: (p: Progress) => void;
}

export interface ToolError {
  kind: ErrorKind;
  code: number | null;
  /** The daemon's own message, byte for byte, for errors that came from the daemon. */
  message: string;
  data?: unknown;
}

export interface Envelope {
  ok: boolean;
  data?: Record<string, unknown>;
  error?: ToolError;
  header: Header;
  receipt: Receipt;
}

export interface ToolResult {
  content: { type: "text"; text: string }[];
  structuredContent: Envelope;
  isError?: boolean;
}

export interface ServerOptions {
  daemon: DaemonLike;
  profile?: Profile;
  /** Wall clock in ms. */
  now?: () => number;
  /** Process CPU seconds so far. */
  cpu?: () => number;
  /** Used in run ids. */
  session?: string;
  /**
   * An extra refusal check that runs after input validation and before any daemon work (the HTTP
   * listener's token scope, task 2.6). It returns a refusal message, or null to let the call go on.
   * It can only refuse; it never widens what the daemon allows. stdio leaves it unset.
   */
  gate?: (tool: ToolDef, args: Record<string, unknown>) => Promise<string | null> | string | null;
  /** Timer functions (default: Node's, unref'd). */
  timers?: Timers;
  /** Progress cadence while a call is in flight; clamped to 30 s at most. */
  progressEveryMs?: number;
  /** Whole-call limit; clamped to CALL_DEADLINE_MS at most. */
  callDeadlineMs?: number;
  /** Pushes and card instructions (default: a Supervisor that posts nothing). */
  supervisor?: Supervisor;
  /**
   * Poll job.status for the jobs this server started or saw running, so `stuck` / `done` is seen
   * even when the session is not asking. On for stdio (the process lives exactly as long as the
   * session); off for the stateless HTTP listener, which outlives sessions: there a state is only
   * seen through a call the session makes (by design: pushes only while a session is open).
   */
  watch?: boolean;
  /** Poll interval for `watch`. */
  pollMs?: number;
}

export interface Server {
  instructions: string;
  listTools(): WireTool[];
  callTool(name: string, args: unknown, extra?: CallExtra): Promise<ToolResult>;
  /** Stops the job watcher. */
  close(): void;
}

class ToolFailure extends Error {
  err: ToolError;
  constructor(err: ToolError) {
    super(err.message);
    this.err = err;
  }
}
const fail = (kind: ErrorKind, message: string, code: number | null = null, data?: unknown): ToolFailure => new ToolFailure({ kind, code, message, ...(data === undefined ? {} : { data }) });

interface Row {
  device: string;
  serial: string;
  hw_serial: string;
  model: string;
  mode: string;
  holder?: string | null;
  alias?: string;
  /** false: the phone is connected but not on the daemon's allowlist, so every call and lease on it is refused (absent from an older daemon: allowed). */
  allowed?: boolean;
}

const S: Schema = { type: "string" };
const ROW_SCHEMA: Schema = {
  type: "object",
  required: ["device", "serial", "hw_serial", "model", "mode"],
  properties: { device: { type: "string", minLength: 1 }, serial: S, hw_serial: S, model: S, mode: S, holder: { type: ["string", "null"] }, alias: S, allowed: { type: "boolean" } },
};
const DEVICES_SCHEMA: Schema = { type: "object", required: ["devices"], properties: { devices: { type: "array", items: ROW_SCHEMA } } };
const LEASE_SCHEMA: Schema = { type: "object", required: ["device", "holder", "epoch"], properties: { device: S, holder: S, epoch: { type: "integer", minimum: 0 } } };
const LEASES_SCHEMA: Schema = { type: "object", required: ["leases"], properties: { leases: { type: "array", items: LEASE_SCHEMA } } };
const RELEASED_SCHEMA: Schema = { type: "object", required: ["released"], properties: { released: { type: "boolean" } } };
const CALL_RESULT_SCHEMA: Schema = {
  type: "object",
  required: ["tool", "text"],
  properties: { tool: S, text: S, image_bytes: { type: ["integer", "null"], minimum: 0 } },
};

/**
 * Errors whose message is this server's own wording about the call itself (the arguments, the tool
 * name, a time limit, the pipe). Every other error message was written by the daemon about a phone
 * and can quote what the phone shows (a refusal quotes the on-screen label it matched, a device
 * error carries adb's output), so it goes inside the phone-data fence like any other phone text.
 */
const SERVER_WORDED: ReadonlySet<ErrorKind> = new Set<ErrorKind>(["invalid_input", "unknown_tool", "timeout", "transport", "bad_reply", "bad_output"]);

/** The text of one result, in the order it is printed after the health line. */
interface ResultText {
  /** The server's own line before the body (an error's label); may be empty. */
  lead: string;
  /** The answer itself. */
  body: string;
  /** True when `body` was read from, or describes, a phone: it is printed inside the phone-data fence. */
  fenced: boolean;
  /** The server's own notices (supervision cards), printed after the body and never fenced. */
  notes: string[];
}

const GENERIC_DEF: ToolDef = { name: "?", description: "", input: true, data: { type: "object" }, read: false, minimal: false, route: "device_call", daemon: "planned" };

export function createServer(opts: ServerOptions): Server {
  const daemon = opts.daemon;
  const profile: Profile = opts.profile ?? "full";
  const now = opts.now ?? Date.now;
  const cpu = opts.cpu ?? ((): number => {
    const u = process.cpuUsage();
    return (u.user + u.system) / 1e6;
  });
  const session = opts.session ?? `mcp-${process.pid}`;
  const tracker = new HealthTracker(now);
  const lastBattery = new Map<string, number>();
  const held = new Map<string, number>(); // device key -> connection epoch the lease was taken on
  let seq = 0;
  const timers = opts.timers ?? realTimers;
  const progressEvery = Math.max(1, Math.min(opts.progressEveryMs ?? PROGRESS_EVERY_MS, MAX_PROGRESS_EVERY_MS));
  const deadline = Math.max(1, Math.min(opts.callDeadlineMs ?? CALL_DEADLINE_MS, CALL_DEADLINE_MS));
  const supervisor = opts.supervisor ?? new Supervisor();
  const runDevice = new Map<string, string>(); // run id -> device key, for jobs this server saw start

  // ---- the job watcher (opts.watch): polls job.status for jobs not yet ended ----
  const watched = new Map<string, number>(); // run id -> consecutive poll failures
  let poll: unknown;
  let polling = false;
  function watchJob(runId: string, status: string): void {
    if (TERMINAL.has(status)) {
      watched.delete(runId);
    } else if (opts.watch === true) {
      if (!watched.has(runId)) watched.set(runId, 0);
    }
    if (watched.size === 0 && poll !== undefined) {
      timers.clearInterval(poll);
      poll = undefined;
    } else if (watched.size > 0 && poll === undefined) {
      poll = timers.setInterval(() => void pollJobs(), opts.pollMs ?? JOB_POLL_MS);
    }
  }
  async function pollJobs(): Promise<void> {
    if (polling) return;
    polling = true;
    try {
      for (const runId of [...watched.keys()]) {
        try {
          const r = (await daemon.request("job.status", { run_id: runId }, { timeoutMs: 10_000 })) as Record<string, unknown> | null;
          if (r === null || typeof r !== "object" || typeof r.status !== "string") throw new Error("malformed job.status reply");
          watched.set(runId, 0);
          seeJob(runId, r);
        } catch {
          const n = (watched.get(runId) ?? 0) + 1;
          if (n >= 3) watchJob(runId, "error"); // gone, or no longer ours: stop asking
          else watched.set(runId, n);
        }
      }
    } finally {
      polling = false;
    }
  }
  /** Every job state the server sees passes here: the supervisor decides about pushes. */
  function seeJob(runId: string, r: Record<string, unknown>): void {
    if (typeof r.status !== "string") return;
    supervisor.observe({ ...r, run_id: runId } as JobSnapshot);
    watchJob(runId, r.status);
  }

  const visible = (): ToolDef[] => TOOLS.filter((t) => profile === "full" || t.minimal);

  async function rpc(method: string, params: Record<string, unknown>, timeoutMs?: number): Promise<unknown> {
    try {
      return await daemon.request(method, params, timeoutMs === undefined ? {} : { timeoutMs });
    } catch (e) {
      throw mapDaemonError(e);
    }
  }

  async function readDevices(): Promise<Row[]> {
    const res = await rpc("devices.list", {});
    const issues = validate(res, DEVICES_SCHEMA);
    if (issues.length > 0) throw fail("bad_reply", `bridged sent a malformed devices.list reply: ${describeIssues(issues)}`);
    return (res as { devices: Row[] }).devices;
  }

  function pickDevice(rows: Row[], wanted: unknown): Row {
    if (typeof wanted === "string") {
      const row = rows.find((r) => r.device === wanted || r.serial === wanted || r.hw_serial === wanted || r.alias === wanted);
      if (row === undefined) throw fail("unknown_device", `no connected device matches "${wanted}". Connected: ${rows.map((r) => r.device).join(", ") || "none"}`);
      return row;
    }
    if (rows.length === 1) return rows[0] as Row;
    if (rows.length === 0) throw fail("unknown_device", "no devices are connected");
    throw fail("ambiguous_device", `${rows.length} devices are connected and none was named; pass device. Connected: ${rows.map((r) => r.device).join(", ")}`);
  }

  async function ensureLease(key: string): Promise<void> {
    if (held.get(key) === daemon.connectionEpoch) return;
    const res = await rpc("lease.acquire", { device: key });
    const issues = validate(res, LEASE_SCHEMA);
    if (issues.length > 0) throw fail("bad_reply", `bridged sent a malformed lease.acquire reply: ${describeIssues(issues)}`);
    held.set(key, daemon.connectionEpoch);
  }

  function shapeText(text: string, level: Level): { text: string; truncated?: boolean } {
    const limit = TEXT_LIMIT[level];
    if (text.length <= limit) return { text };
    const cut = [...text].slice(0, limit).join("");
    return { text: `${cut}\n[cut: ${text.length - cut.length} more characters; ask for level "${level === "digest" ? "default" : "full"}"]`, truncated: true };
  }

  interface Outcome {
    data: Record<string, unknown>;
    observation: Observation;
    /** Text for the model, after the header line. */
    body: string;
  }

  async function run(def: ToolDef, args: Record<string, unknown>, ctx: { device: string; observation: Observation }): Promise<Outcome> {
    const level = (typeof args.level === "string" ? args.level : "digest") as Level;
    if (def.route === "devices") {
      const rows = await readDevices();
      const keys: Record<Level, string[]> = { digest: ["device", "model", "mode", "holder", "allowed"], default: ["device", "model", "mode", "holder", "allowed", "serial", "hw_serial"], full: [] };
      const shown = rows.map((r) => (level === "full" ? r : Object.fromEntries(keys[level].filter((k) => k in r).map((k) => [k, (r as unknown as Record<string, unknown>)[k]]))));
      const body = rows.length === 0 ? "no devices connected" : rows.map((r) => `${r.device}  ${r.model}  mode=${r.mode}  holder=${r.holder ?? "none"}${r.allowed === false ? "  NOT ON THE ALLOWLIST (every call is refused; start bridged with --allow-serial or list it in allow.txt)" : ""}`).join("\n");
      return { data: { devices: shown }, observation: {}, body };
    }
    if (def.route === "lease") {
      const action = args.action as string;
      if (action === "status" && args.device === undefined) {
        const res = await rpc("lease.status", {});
        const issues = validate(res, LEASES_SCHEMA);
        if (issues.length > 0) throw fail("bad_reply", `bridged sent a malformed lease.status reply: ${describeIssues(issues)}`);
        const leases = (res as { leases: { device: string; holder: string; epoch: number }[] }).leases;
        return { data: { leases }, observation: {}, body: leases.length === 0 ? "no leases" : leases.map((l) => `${l.device} held by ${l.holder} (epoch ${l.epoch})`).join("\n") };
      }
      const row = pickDevice(await readDevices(), args.device);
      ctx.device = row.device;
      ctx.observation = identity(row);
      if (action === "acquire") {
        const res = await rpc("lease.acquire", { device: row.device });
        const issues = validate(res, LEASE_SCHEMA);
        if (issues.length > 0) throw fail("bad_reply", `bridged sent a malformed lease.acquire reply: ${describeIssues(issues)}`);
        held.set(row.device, daemon.connectionEpoch);
        const l = res as { holder: string; epoch: number };
        return { data: { lease: res as Record<string, unknown> }, observation: ctx.observation, body: `lease on ${row.device} taken by ${l.holder} (epoch ${l.epoch})` };
      }
      if (action === "release") {
        const res = await rpc("lease.release", { device: row.device });
        const issues = validate(res, RELEASED_SCHEMA);
        if (issues.length > 0) throw fail("bad_reply", `bridged sent a malformed lease.release reply: ${describeIssues(issues)}`);
        held.delete(row.device);
        const released = (res as { released: boolean }).released;
        return { data: { released }, observation: ctx.observation, body: released ? `lease on ${row.device} released` : `${row.device} was not leased` };
      }
      const res = await rpc("lease.status", { device: row.device });
      const issues = validate(res, LEASES_SCHEMA);
      if (issues.length > 0) throw fail("bad_reply", `bridged sent a malformed lease.status reply: ${describeIssues(issues)}`);
      const leases = (res as { leases: { device: string; holder: string; epoch: number }[] }).leases;
      return { data: { leases }, observation: ctx.observation, body: leases.length === 0 ? `${row.device} is not leased` : leases.map((l) => `${l.device} held by ${l.holder} (epoch ${l.epoch})`).join("\n") };
    }
    if (def.route === "job") return runJob(def, args, ctx);
    // device_call
    const row = pickDevice(await readDevices(), args.device);
    ctx.device = row.device;
    ctx.observation = identity(row);
    await ensureLease(row.device);
    const { device: _device, level: _level, ...rest } = args;
    // require, retry and verify ride along inside the call, for the daemon to enforce
    const retry = typeof args.retry === "number" ? args.retry : 0;
    const base = def.name === "wait" && typeof args.seconds === "number" ? CALL_TIMEOUT_MS + args.seconds * 1000 : CALL_TIMEOUT_MS;
    const res = await rpc("device.call", { device: row.device, call: { tool: def.name, ...rest } }, clampTimeout(base * (1 + retry)));
    const issues = validate(res, CALL_RESULT_SCHEMA);
    if (issues.length > 0) throw fail("bad_reply", `bridged sent a malformed device.call reply: ${describeIssues(issues)}`);
    const { health, ...payload } = res as Record<string, unknown>;
    ctx.observation = mergeObservations(ctx.observation, parseObservation(health));
    const shaped = shapeText(String(payload.text), level);
    const data: Record<string, unknown> = { ...payload, text: shaped.text, ...(shaped.truncated ? { truncated: true } : {}) };
    return { data, observation: ctx.observation, body: shaped.text };
  }

  /**
   * The daemon method and its params for one job tool call (the orchestrator's 27-tool ruling:
   * four MCP tools over the daemon's eight job.* methods, which are unchanged).
   */
  function jobRequest(def: ToolDef, args: Record<string, unknown>): { method: string; params: Record<string, unknown>; runRef?: string; sync?: boolean } {
    const pick = (...keys: string[]): Record<string, unknown> => Object.fromEntries(keys.filter((k) => args[k] !== undefined).map((k) => [k, args[k]]));
    switch (def.name) {
      case "job_start": {
        const resume = typeof args.resume_run_id === "string" ? args.resume_run_id : undefined;
        if (args.dry_run === true) {
          if (resume !== undefined) throw fail("invalid_input", "job_start: dry_run and resume_run_id cannot be combined");
          if (args.flow === undefined || !Array.isArray(args.trees)) throw fail("invalid_input", "job_start: dry_run needs flow and trees");
          if (args.steps !== undefined || args.app !== undefined || args.mode !== undefined || args.record !== undefined) throw fail("invalid_input", "job_start: dry_run checks a flow; steps, app, mode and record are for a real run");
          return { method: "job.dry_run", params: pick("flow", "trees") };
        }
        if (args.trees !== undefined) throw fail("invalid_input", "job_start: trees are only for dry_run: true");
        if (resume !== undefined) {
          if (args.flow !== undefined || args.steps !== undefined || args.app !== undefined) throw fail("invalid_input", "job_start: resume_run_id resumes the job's own flow; leave flow, steps and app out");
          if (args.mode !== undefined || args.record !== undefined) throw fail("invalid_input", "job_start: mode and record are not for resume_run_id (a resumed job keeps the record setting it started with); poll job_status");
          return { method: "job.resume", params: { run_id: resume, ...pick("human_confirmed") }, runRef: resume };
        }
        if (args.flow === undefined && args.steps === undefined) throw fail("invalid_input", "job_start: flow or steps is required (unless resume_run_id is given)");
        if (args.flow !== undefined && args.steps !== undefined) throw fail("invalid_input", "job_start: give flow or steps, not both");
        if (args.steps !== undefined && (typeof args.app !== "string" || args.app.trim() === "")) throw fail("invalid_input", "job_start: steps need app, the package they run in");
        if (args.steps === undefined && args.app !== undefined) throw fail("invalid_input", "job_start: app goes with steps; a flow names its own app");
        if (args.human_confirmed !== undefined) throw fail("invalid_input", "job_start: human_confirmed is only for resume_run_id");
        // sync is this server's own loop (start, poll job.status, read the receipt): the daemon never holds a call
        return { method: "job.start", params: pick("flow", "steps", "app", "vars", "oem", "sdk", "auto_dismiss", "record"), ...(args.mode === "sync" ? { sync: true } : {}) };
      }
      case "job_stop":
        return { method: args.mode === "pause" ? "job.pause" : "job.stop", params: pick("run_id"), runRef: args.run_id as string };
      case "job_status":
        return { method: "job.status", params: pick("run_id"), runRef: args.run_id as string };
      default: // run_script
        return { method: def.method ?? "job.run_script", params: pick("script") };
    }
  }

  /** Says where a recorded run's flow and Maestro export were saved (empty when it was not recorded). */
  function recordedLine(rec: unknown): string {
    if (rec === null || typeof rec !== "object") return "";
    const o = rec as Record<string, unknown>;
    if (typeof o.error === "string") return `\nrecording not saved: ${o.error}`;
    const base = (p: unknown): string => String(p ?? "").split(/[\\/]/).pop() ?? "";
    const skipped = Array.isArray(o.maestro_skipped) ? o.maestro_skipped.length : 0;
    return `\nrecorded: ${String(o.steps ?? "?")} steps in ${base(o.flow)} (replay: job_start flow "${base(o.flow)}"); Maestro export ${base(o.maestro)}${skipped > 0 ? `, ${skipped} step(s) left as comments` : ""}`;
  }

  /**
   * The job tools. Tools that name a device pick it and take the lease like a device call; tools
   * that name a run id use the device the job started on (when this server saw it start) to re-take
   * the lease after a reconnect. The daemon checks the lease on every job method.
   */
  async function runJob(def: ToolDef, args: Record<string, unknown>, ctx: { device: string; observation: Observation }): Promise<Outcome> {
    const { method, params, runRef, sync } = jobRequest(def, args);
    let device: string | undefined;
    if (runRef !== undefined) {
      device = runDevice.get(runRef);
    } else {
      const row = pickDevice(await readDevices(), args.device);
      device = row.device;
      ctx.observation = identity(row);
      params.device = row.device;
    }
    if (device !== undefined) {
      ctx.device = device;
      await ensureLease(device);
    }
    const res = await rpc(method, params, clampTimeout(def.timeoutMs ?? CALL_TIMEOUT_MS));
    if (res === null || typeof res !== "object" || Array.isArray(res)) throw fail("bad_reply", `bridged sent a malformed ${method} reply: not an object`);
    let r = res as Record<string, unknown>;
    const str = (k: string): string => (typeof r[k] === "string" ? (r[k] as string) : "");
    const runId = str("run_id") || (runRef ?? "");
    if (["job.start", "job.status", "job.pause", "job.resume", "job.stop"].includes(method) && runId === "") throw fail("bad_reply", `bridged sent a malformed ${method} reply: no run_id`);
    if (method === "job.start" && device !== undefined) runDevice.set(runId, device);
    if (typeof r.device === "string" && runId !== "" && !runDevice.has(runId)) runDevice.set(runId, r.device);
    if (ctx.device === "none" && typeof r.device === "string") ctx.device = r.device;
    let synced = false;
    if (sync === true && method === "job.start") {
      // mode sync: poll until the job leaves running (or the budget is spent), then read its receipt, which carries a row per step
      seeJob(runId, r);
      const t0 = now();
      let last = r;
      for (;;) {
        const left = SYNC_BUDGET_MS - (now() - t0);
        if (left <= 0) break;
        const s = await rpc("job.status", { run_id: runId }, clampTimeout(Math.min(10_000, Math.max(1_000, left))));
        if (s === null || typeof s !== "object" || Array.isArray(s) || typeof (s as Record<string, unknown>).status !== "string") throw fail("bad_reply", "bridged sent a malformed job.status reply");
        last = s as Record<string, unknown>;
        if (last.status !== "running") break;
        await new Promise<void>((resolve) => timers.setTimeout(resolve, Math.min(SYNC_POLL_MS, left)));
      }
      const rc = await rpc("job.receipt", { run_id: runId }, clampTimeout(8_000));
      if (rc === null || typeof rc !== "object" || Array.isArray(rc)) throw fail("bad_reply", "bridged sent a malformed job.receipt reply: not an object");
      const rr = rc as Record<string, unknown>;
      r = { ...last, journal: r.journal, progress: r.progress, receipt: rr.receipt ?? null, detail: rr.detail ?? null, steps: Array.isArray(rr.steps) ? rr.steps : [], ...(rr.recording !== undefined ? { recording: rr.recording } : {}) };
      synced = true;
    }
    if (typeof r.status === "string" && runId !== "") {
      // a sync run that came back done was answered in the call itself: no "done" push for the person (a
      // benchmark would send one per task), and a later job_status will not send one either. stuck, running,
      // paused, error and stopped go through the supervisor as ever.
      if (synced && r.status === "done") {
        supervisor.acknowledge(runId, "done");
        watchJob(runId, "done");
      } else seeJob(runId, r);
    }
    // job_status: once the job has ended, its receipt rides along (replaces job_receipt)
    let ended: { receipt: unknown; detail: unknown; recording?: unknown } | undefined;
    if (method === "job.status" && typeof r.status === "string" && TERMINAL.has(r.status)) {
      const rc = await rpc("job.receipt", { run_id: runId }, clampTimeout(30_000));
      if (rc === null || typeof rc !== "object" || Array.isArray(rc)) throw fail("bad_reply", "bridged sent a malformed job.receipt reply: not an object");
      const rr = rc as Record<string, unknown>;
      ended = { receipt: rr.receipt ?? null, detail: rr.detail ?? null, ...(rr.recording !== undefined ? { recording: rr.recording } : {}) };
    }
    let body: string;
    switch (method) {
      case "job.start":
        if (Array.isArray(r.steps)) {
          // mode sync: the job's end state and one row per step (a run that outlasts the wait says running)
          const rows = (r.steps as Record<string, unknown>[]).map((s) => `  ${String(s.step_id)} ${String(s.status)}${typeof s.note === "string" && s.note !== "" ? `: ${s.note}` : ""}`);
          body = `job ${runId} on ${ctx.device}: ${str("line") || str("status") || "?"}${str("status") === "running" ? ". It is still running: poll job_status." : ""}\n${rows.join("\n")}`;
          if (r.receipt !== null && typeof r.receipt === "object" && str("status") !== "running") {
            const detail = (r.detail ?? {}) as Record<string, unknown>;
            body += `\nreceipt: outcome ${String(detail.outcome ?? "?")}, ${String((r.receipt as Record<string, unknown>).steps_done ?? "?")} steps done`;
            body += recordedLine(r.recording);
          }
          body = shapeText(body, "default").text;
          break;
        }
        body = `job ${runId} started on ${ctx.device}: ${str("status") || "?"}. It runs in the background: poll job_status (no call waits on a job). Progress: ${str("progress") || "?"}`;
        break;
      case "job.stop":
        body = `job ${runId} ${str("status") || "?"} at journal position ${String(r.journal_seq ?? "?")}`;
        break;
      case "job.dry_run":
        body = shapeText(str("text") || `${String(r.matched ?? "?")} selectors matched`, "default").text;
        break;
      case "job.run_script":
        body = shapeText(`script ${str("outcome") || "?"}: ${JSON.stringify(r.value ?? null)}${typeof r.error === "string" ? `; error: ${r.error}` : ""}; ${Array.isArray(r.refusals) ? r.refusals.length : 0} refused`, "default").text;
        break;
      default: // status, pause, resume: the daemon's one line
        body = str("line") || `job ${runId}: ${str("status") || "?"}`;
    }
    if (ended !== undefined) {
      const detail = (ended.detail ?? {}) as Record<string, unknown>;
      const receipt = (ended.receipt ?? {}) as Record<string, unknown>;
      body += `\nreceipt: outcome ${String(detail.outcome ?? "?")}, ${String(receipt.steps_done ?? "?")} steps done`;
      body += recordedLine(ended.recording);
      return { data: { ...r, receipt: ended.receipt, detail: ended.detail, ...(ended.recording !== undefined ? { recording: ended.recording } : {}) }, observation: ctx.observation, body };
    }
    return { data: { ...r }, observation: ctx.observation, body };
  }

  const identity = (r: Row): Observation => parseObservation({ alias: r.alias, model: r.model, mode: r.mode });

  function mapDaemonError(e: unknown): ToolFailure {
    if (e instanceof ToolFailure) return e;
    if (e instanceof DaemonError) {
      if (e.kind === "timeout") return fail("timeout", `${e.message}. The action may still have happened: look again before repeating it.`);
      if (e.kind === "transport") return fail("transport", e.message);
      if (e.kind === "protocol") return fail("bad_reply", e.message);
      const code = e.code;
      const kind: ErrorKind =
        code === CODE.REFUSED ? "refused"
        : code === CODE.LEASE_HELD ? "lease_held"
        : code === CODE.NOT_HOLDER ? "not_holder"
        : code === CODE.UNKNOWN_DEVICE ? "unknown_device"
        : code === CODE.DEVICE_ERROR ? "device_error"
        : code === CODE.INVALID_PARAMS ? "invalid_input"
        : "daemon_error";
      // the daemon's message is kept exactly
      return fail(kind, e.message, code, e.data);
    }
    return fail("daemon_error", `unexpected failure: ${e instanceof Error ? e.message : String(e)}`);
  }

  const LABEL: Partial<Record<ErrorKind, string>> = {
    refused: "Refused by the device-mode gate",
    lease_held: "Device is leased by another session",
    not_holder: "Lease problem",
    unknown_device: "Unknown device",
    ambiguous_device: "Which device?",
    device_error: "Device error",
    timeout: "Timed out",
    transport: "Cannot reach bridged",
    bad_reply: "Bad reply from bridged",
    bad_output: "Result rejected by its schema",
    invalid_input: "Invalid input",
    unknown_tool: "Unknown tool",
    daemon_error: "bridged error",
  };

  /** `p`, or a `timeout` failure once the call has been in flight for `deadline` ms. */
  function withDeadline<T>(p: Promise<T>, name: string, isJob: boolean): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const t = timers.setTimeout(() => {
        const after = isJob ? "Any job keeps running in the background: poll job_status." : "The action may still have happened: look again before repeating it.";
        reject(fail("timeout", `${name}: no answer within ${Math.round(deadline / 1000)} s, and no call may run over 110 s. ${after}`));
      }, deadline);
      p.then(
        (v) => {
          timers.clearTimeout(t);
          resolve(v);
        },
        (e: unknown) => {
          timers.clearTimeout(t);
          reject(e);
        },
      );
    });
  }

  async function callTool(name: string, args: unknown, extra: CallExtra = {}): Promise<ToolResult> {
    name = typeof name === "string" ? name : String(name); // a hostile client may send anything
    const t0 = now();
    const cpu0 = cpu();
    const busy0 = daemon.busyRetries;
    const runId = `${session}-${++seq}`;
    const ctx = { device: "none", observation: {} as Observation };
    const def = toolByName(name);
    let data: Record<string, unknown> | undefined;
    let error: ToolError | undefined;
    // every successful answer comes from the daemon and describes a phone: fenced unless shown otherwise
    const text: ResultText = { lead: "", body: "", fenced: true, notes: [] };
    // MCP progress while the call is in flight, only when the client gave a progressToken
    let beat: unknown;
    const progress = extra.progress;
    if (progress !== undefined) {
      let n = 0;
      beat = timers.setInterval(() => {
        n++;
        try {
          progress({ progress: n, message: `${name}: still working after ${Math.round((now() - t0) / 1000)} s` });
        } catch {
          // a notification that cannot be sent never fails the call
        }
      }, progressEvery);
    }
    try {
      const work = async (): Promise<Outcome> => {
        if (def === undefined || !visible().includes(def)) throw fail("unknown_tool", `no tool named "${name}"`);
        const input = args === undefined || args === null ? {} : args;
        if (typeof input !== "object" || Array.isArray(input)) throw fail("invalid_input", "arguments must be an object");
        const issues = validate(input, def.input);
        if (issues.length > 0) throw fail("invalid_input", `${name}: ${describeIssues(issues)}`);
        if (opts.gate !== undefined) {
          const why = await opts.gate(def, input as Record<string, unknown>);
          if (why !== null) throw fail("refused", why);
        }
        return run(def, input as Record<string, unknown>, ctx);
      };
      const out = await withDeadline(work(), name, def?.route === "job");
      data = out.data;
      text.body = out.body;
    } catch (e) {
      const f = e instanceof ToolFailure ? e : mapDaemonError(e);
      error = f.err;
      const label = LABEL[f.err.kind] ?? "Error";
      // a daemon-coded invalid_input echoes the daemon's view of the call, which may name phone content
      if (SERVER_WORDED.has(f.err.kind) && f.err.code === null) {
        text.body = `${label}: ${f.err.message}`;
        text.fenced = false;
      } else {
        text.lead = `${label}:`;
        text.body = f.err.message;
      }
      // a refusal may carry the health the daemon could give for the device (never asked for separately)
      const h = (f.err.data as { health?: unknown } | undefined)?.health;
      if (h !== undefined) ctx.observation = mergeObservations(ctx.observation, parseObservation(h));
      if (f.err.kind === "not_holder" || f.err.kind === "lease_held") held.delete(ctx.device);
    } finally {
      if (beat !== undefined) timers.clearInterval(beat);
    }
    // stuck / done notices (from this call or from the watcher) go out with the next result, once
    const notices: Notice[] = supervisor.drain();
    if (notices.length > 0) {
      text.notes = notices.map((n) => n.card);
      if (data !== undefined && def?.route === "job") data = { ...data, supervision: notices };
    }
    return finish(def ?? GENERIC_DEF, name, runId, ctx, data, error, text, { t0, cpu0, busy0 });
  }

  function finish(def: ToolDef, name: string, runId: string, ctx: { device: string; observation: Observation }, data: Record<string, unknown> | undefined, error: ToolError | undefined, text: ResultText, m: { t0: number; cpu0: number; busy0: number }): ToolResult {
    const baseline = tracker.lastForeground(ctx.device);
    let header: Header;
    try {
      header = tracker.header(ctx.device, ctx.observation, baseline);
    } catch {
      header = blankHeader();
    }
    const battery = header.battery.v;
    const before = lastBattery.get(ctx.device);
    const change = typeof battery === "number" && before !== undefined ? battery - before : null;
    if (typeof battery === "number") lastBattery.set(ctx.device, battery);
    const mkReceipt = (ok: boolean, errors: string[]): Receipt =>
      buildReceipt({
        runId,
        device: ctx.device,
        tool: name,
        ok,
        wallTimeS: (now() - m.t0) / 1000,
        laptopCpuS: Math.max(0, cpu() - m.cpu0),
        retries: daemon.busyRetries - m.busy0,
        phonePeakC: typeof header.temp_c.v === "number" ? header.temp_c.v : null,
        batteryChange: change,
        errors,
      });

    const schema = outputSchemaFor(def, true);
    let env: Envelope = { ok: error === undefined, header, receipt: mkReceipt(error === undefined, error ? [error.message] : []) };
    if (data !== undefined) env.data = data;
    if (error !== undefined) env.error = error;
    const issues = validate(env, schema);
    if (issues.length > 0) {
      // the result broke its own schema: say so, and keep the header and receipt
      const msg = `the ${name} result failed its output schema: ${describeIssues(issues)}`;
      const e: ToolError = { kind: "bad_output", code: null, message: msg };
      env = { ok: false, error: e, header, receipt: mkReceipt(false, [msg]) };
      // the server's own words about a schema: the rejected answer itself is not repeated
      text = { lead: "", body: `${LABEL.bad_output}: ${msg}`, fenced: false, notes: text.notes };
      if (validate(env, schema).length > 0) {
        env = { ok: false, error: e, header: blankHeader(), receipt: buildReceipt({ runId, device: "none", tool: name, ok: false, wallTimeS: 0, laptopCpuS: 0, retries: 0, errors: [msg] }) };
      }
    }
    // Order: the health line, the server's own lead, the answer (fenced when it is phone text), the
    // server's notices, the error kind. Only the fenced part can hold text the phone chose.
    const lines = [headerLine(env.header), text.lead, text.fenced ? fence(text.body) : text.body, ...text.notes];
    if (!env.ok) lines.push(`error kind: ${env.error?.kind ?? "?"}`);
    const result: ToolResult = { content: [{ type: "text", text: lines.filter((l) => l.length > 0).join("\n") }], structuredContent: env };
    if (!env.ok) result.isError = true;
    return result;
  }

  return {
    instructions: INSTRUCTIONS,
    listTools: () => listTools(profile),
    close: () => {
      watched.clear();
      if (poll !== undefined) timers.clearInterval(poll);
      poll = undefined;
    },
    callTool: async (name, args, extra) => {
      try {
        return await callTool(name, args, extra);
      } catch (e) {
        // last resort: callTool is written not to throw, but a bug must still not take the server down
        const msg = `internal error: ${e instanceof Error ? e.message : String(e)}`;
        const receipt = buildReceipt({ runId: `${session}-x`, device: "none", tool: String(name), ok: false, wallTimeS: 0, laptopCpuS: 0, retries: 0, errors: [msg] });
        const header = blankHeader();
        return { content: [{ type: "text", text: `${headerLine(header)}\n${msg}` }], structuredContent: { ok: false, error: { kind: "daemon_error", code: null, message: msg }, header, receipt }, isError: true };
      }
    },
  };
}
