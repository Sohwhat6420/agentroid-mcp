// An in-process stand-in for bridged, listening on a pipe with a random name (never the real
// \\.\pipe\agentroid). It copies the daemon's dispatch (crates/bridged/src/server.rs) and the
// device-mode gate (crates/core/src/gate.rs): same error codes, same refusal messages, so the
// tests can show those messages pass through the MCP server untouched.

import net from "node:net";
import { randomBytes } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

export const CODE = { REFUSED: -32003, NOT_HOLDER: -32002, LEASE_HELD: -32001, DEVICE_ERROR: -32004, UNKNOWN_DEVICE: -32005, UNKNOWN_JOB: -32008, INVALID_PARAMS: -32602, METHOD_NOT_FOUND: -32601 } as const;

/** The job methods (2.10b's daemon), all served by the fake (2.11a). */
export const JOB_METHODS = ["job.start", "job.status", "job.pause", "job.resume", "job.stop", "job.receipt", "job.dry_run", "job.run_script"] as const;

interface FakeJob {
  run_id: string;
  device: string;
  flow: string;
  status: string;
  /** Index into `jobScript` for the next job.status. */
  next: number;
  steps_planned: number;
  steps_done: number;
  /** Started with `record: true`: a done receipt carries a `recording`. */
  record?: boolean;
}

export function randomPipeName(): string {
  const id = `agentroid-test-${process.pid}-${randomBytes(6).toString("hex")}`;
  return process.platform === "win32" ? `\\\\.\\pipe\\${id}` : join(tmpdir(), `${id}.sock`);
}

export interface FakeDevice {
  device: string;
  serial: string;
  hw_serial: string;
  model: string;
  mode: "full" | "helper" | "read_only";
  allowed?: boolean;
}

export const POCO: FakeDevice = { device: "DEVICE0000EXAMPLE", serial: "192.0.2.50:37123", hw_serial: "DEVICE0000EXAMPLE", model: "POCO X6", mode: "full" };
export const RO14: FakeDevice = { device: "FAKE14T000001", serial: "192.0.2.51:5555", hw_serial: "FAKE14T000001", model: "14T", mode: "read_only" };
export const S24: FakeDevice = { device: "PROTECTED0000EXAMPLE", serial: "192.0.2.60:5555", hw_serial: "PROTECTED0000EXAMPLE", model: "S24", mode: "read_only" };
export const STRANGER: FakeDevice = { device: "STRANGER0001", serial: "192.0.2.99:5555", hw_serial: "STRANGER0001", model: "Unlisted", mode: "full", allowed: false };

// the tools the daemon serves since 2.4 (crates/bridged/src/devcall.rs, and devtools.rs for doctor, push and pull_new)
const SERVED = ["look", "screenshot", "ui_tree", "getprop", "tap", "long_press", "swipe", "scroll", "type_text", "key", "unlock", "open_app", "open_url", "start_activity", "list_apps", "wait", "set_value", "pixel_probe", "doctor", "push", "pull_new"];
const ACTING = ["tap", "long_press", "swipe", "scroll", "type_text", "key", "unlock", "open_app", "open_url", "start_activity"];
// The getprop keys among the protected phone's approved probes, read from the one list (probes/readonly-s24.txt,
// the approved set of 10), the same file guard.rs builds S24_READS from. The other approved
// lines (wm, pm, dumpsys) are probe-internal and have no MCP tool.
export const S24_READS: string[] = readFileSync(fileURLToPath(new URL("../../../probes/readonly-s24.txt", import.meta.url)), "utf8")
  .split(/\r?\n/).map((l) => l.trim()).filter((l) => l.startsWith("getprop ")).map((l) => l.slice("getprop ".length));

export type Misbehaviour = "none" | "garbage-line" | "not-an-object" | "no-result" | "bad-error" | "wrong-shape" | "no-id" | "hang" | "drop";

interface Conn {
  id: number;
  session: string | undefined;
  socket: net.Socket;
}

export class FakeDaemon {
  readonly path = randomPipeName();
  devices: FakeDevice[];
  /** Health the fake attaches to device.call results and refusals, per device key. Mutate between calls. */
  health: Record<string, unknown> = {};
  /** What the next device.call does wrong (reset after one use). */
  misbehave: Misbehaviour = "none";
  /** Replace the device.call result entirely (reset after one use). */
  nextCallResult: unknown = undefined;
  /** Everything received, in order. */
  requests: { conn: number; method: string; params: unknown }[] = [];
  /** Serve every tool name, not only the four the real daemon passes through today. */
  serveAll = false;
  /** Delay before every reply, ms. */
  delayMs = 0;
  /**
   * The statuses successive job.status calls report for each job (the last one repeats). A step
   * is "done" per status seen, so routine progress shows as `running` with a rising count.
   */
  jobScript: string[] = ["running", "running", "done"];
  /** The job methods that never answer (tests of the 110 s cap). */
  hangJobs = new Set<string>();
  /** When set, a done recorded job's receipt says the recording was refused for this reason. */
  recordError: string | null = null;
  /** The jobs started, by run id. */
  readonly jobs = new Map<string, FakeJob>();
  private nextJob = 1;

  private server = net.createServer((s) => this.onConnection(s));
  private conns = new Set<Conn>();
  private leases = new Map<string, { conn: number; holder: string; epoch: number }>();
  private epoch = 0;
  private nextConn = 1;

  constructor(devices: FakeDevice[] = [POCO]) {
    this.devices = devices;
  }

  start(): Promise<void> {
    return new Promise((resolve, reject) => {
      this.server.once("error", reject);
      this.server.listen(this.path, () => resolve());
    });
  }

  async stop(): Promise<void> {
    for (const c of this.conns) c.socket.destroy();
    await new Promise<void>((r) => this.server.close(() => r()));
  }

  callsTo(method: string): number {
    return this.requests.filter((r) => r.method === method).length;
  }

  private onConnection(socket: net.Socket): void {
    const conn: Conn = { id: this.nextConn++, session: undefined, socket };
    this.conns.add(conn);
    let buf = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => {
      buf += chunk;
      for (;;) {
        const nl = buf.indexOf("\n");
        if (nl < 0) break;
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        if (line.trim().length > 0) void this.handle(conn, line);
      }
    });
    socket.on("error", () => {});
    socket.on("close", () => {
      this.conns.delete(conn);
      for (const [k, l] of this.leases) if (l.conn === conn.id) this.leases.delete(k);
    });
  }

  private reply(conn: Conn, id: unknown, body: { result?: unknown; error?: unknown }): void {
    const send = (): void => {
      if (!conn.socket.destroyed) conn.socket.write(JSON.stringify({ jsonrpc: "2.0", id, ...body }) + "\n");
    };
    if (this.delayMs > 0) setTimeout(send, this.delayMs);
    else send();
  }

  private rawWrite(conn: Conn, text: string): void {
    if (!conn.socket.destroyed) conn.socket.write(text);
  }

  private async handle(conn: Conn, line: string): Promise<void> {
    const req = JSON.parse(line) as { id: number; method: string; params?: Record<string, unknown> };
    this.requests.push({ conn: conn.id, method: req.method, params: req.params });
    const err = (code: number, message: string, data?: unknown): void => this.reply(conn, req.id, { error: { code, message, ...(data === undefined ? {} : { data }) } });
    const p = req.params ?? {};
    const holder = conn.session ?? `conn-${conn.id}`;
    const find = (name: unknown): FakeDevice | undefined => this.devices.find((d) => d.serial === name || d.hw_serial === name);

    switch (req.method) {
      case "hello":
        conn.session = typeof p.session === "string" ? p.session : undefined;
        this.reply(conn, req.id, { result: { name: "bridged", version: "0.1.0", protocol: "2.0", capabilities: ["hello", "devices.list", "lease.acquire", "lease.release", "lease.status", "device.call", ...JOB_METHODS], holder: conn.session ?? `conn-${conn.id}` } });
        return;
      case "devices.list":
        this.reply(conn, req.id, {
          result: { devices: this.devices.map((d) => ({ device: d.device, serial: d.serial, hw_serial: d.hw_serial, model: d.model, mode: d.mode, holder: this.leases.get(d.device)?.holder ?? null })) },
        });
        return;
      case "lease.acquire": {
        const d = find(p.device);
        if (!d) return err(CODE.UNKNOWN_DEVICE, `no connected device matches ${JSON.stringify(p.device)}`);
        const held = this.leases.get(d.device);
        if (held && held.conn !== conn.id) return err(CODE.LEASE_HELD, `device ${d.device} is held by ${held.holder} (epoch ${held.epoch})`, { device: d.device, holder: held.holder, epoch: held.epoch });
        if (!held) this.leases.set(d.device, { conn: conn.id, holder, epoch: ++this.epoch });
        const l = this.leases.get(d.device)!;
        return this.reply(conn, req.id, { result: { device: d.device, holder: l.holder, epoch: l.epoch } });
      }
      case "lease.release": {
        const key = find(p.device)?.device ?? String(p.device);
        const held = this.leases.get(key);
        if (!held) return this.reply(conn, req.id, { result: { released: false } });
        if (held.conn !== conn.id) return err(CODE.NOT_HOLDER, `device ${key} is held by ${held.holder}, not by you`, { device: key, holder: held.holder, epoch: held.epoch });
        this.leases.delete(key);
        return this.reply(conn, req.id, { result: { released: true } });
      }
      case "lease.status": {
        const key = p.device === undefined || p.device === null ? undefined : (find(p.device)?.device ?? String(p.device));
        const leases = [...this.leases.entries()].filter(([k]) => key === undefined || k === key).map(([device, l]) => ({ device, holder: l.holder, epoch: l.epoch }));
        return this.reply(conn, req.id, { result: { leases } });
      }
      case "device.call":
        return this.deviceCall(conn, req.id, p, err);
      case "job.start":
      case "job.status":
      case "job.pause":
      case "job.resume":
      case "job.stop":
      case "job.receipt":
      case "job.dry_run":
      case "job.run_script":
        return this.jobCall(conn, req.id, req.method, p, err);
      default:
        return err(CODE.METHOD_NOT_FOUND, `method not found: ${req.method}`);
    }
  }

  /** The job.* methods, shaped like proto.rs; every one needs the caller to hold the device's lease. */
  private jobCall(conn: Conn, id: number, method: string, p: Record<string, unknown>, err: (c: number, m: string, d?: unknown) => void): void {
    if (this.hangJobs.has(method)) return;
    let job: FakeJob | undefined;
    let key: string;
    if (method === "job.start" || method === "job.dry_run" || method === "job.run_script") {
      const d = this.devices.find((x) => x.serial === p.device || x.hw_serial === p.device || x.device === p.device);
      if (!d) return err(CODE.UNKNOWN_DEVICE, `no connected device matches ${JSON.stringify(p.device)}`);
      key = d.device;
    } else {
      job = this.jobs.get(String(p.run_id));
      if (!job) return err(CODE.UNKNOWN_JOB, `no job ${JSON.stringify(p.run_id)}`);
      key = job.device;
    }
    const held = this.leases.get(key);
    if (!held) return err(CODE.NOT_HOLDER, `you do not hold a lease on ${key}; call lease.acquire first`);
    if (held.conn !== conn.id) return err(CODE.NOT_HOLDER, `device ${key} is held by ${held.holder}, not by you`, { device: key, holder: held.holder, epoch: held.epoch });
    const flowName = (f: unknown): string => (f !== null && typeof f === "object" && typeof (f as { name?: unknown }).name === "string" ? (f as { name: string }).name : "flow");
    const statusOf = (j: FakeJob): Record<string, unknown> => ({
      run_id: j.run_id,
      device: j.device,
      flow: j.flow,
      status: j.status,
      step_id: j.status === "done" ? null : `s${j.steps_done + 1}`,
      steps_planned: j.steps_planned,
      steps_done: j.steps_done,
      retries: 0,
      line: `${j.flow}: ${j.status}, ${j.steps_done}/${j.steps_planned} steps`,
      last_error: null,
      pause_reason: null,
      journal_seq: j.steps_done * 2 + 1,
    });
    switch (method) {
      case "job.start": {
        const run_id = `20261020-000000-fake${String(this.nextJob++).padStart(4, "0")}`;
        const j: FakeJob = { run_id, device: key, flow: flowName(p.flow), status: "running", next: 0, steps_planned: Math.max(1, this.jobScript.length - 1), steps_done: 0, record: p.record === true };
        this.jobs.set(run_id, j);
        return this.reply(conn, id, { result: { run_id, status: "running", journal: `runs/${run_id}/journal.ndjson`, progress: `runs/${run_id}/progress.ndjson` } });
      }
      case "job.status": {
        const j = job as FakeJob;
        if (j.status === "running" || j.status === "stuck" || j.status === "done") {
          const s = this.jobScript[Math.min(j.next, this.jobScript.length - 1)] ?? "running";
          if (j.next < this.jobScript.length) j.next++;
          j.status = s;
          // a job that ends stuck or in error stopped at the step after the last one it finished
          j.steps_done = s === "done" ? j.steps_planned : s === "stuck" || s === "error" ? Math.max(0, Math.min(j.steps_planned, j.next - 1) - 1) : Math.min(j.steps_planned, j.next - 1);
        }
        return this.reply(conn, id, { result: statusOf(j) });
      }
      case "job.pause":
      case "job.resume": {
        const j = job as FakeJob;
        if (j.status === "running" || j.status === "paused") j.status = method === "job.pause" ? "paused" : "running";
        return this.reply(conn, id, { result: statusOf(j) });
      }
      case "job.stop": {
        const j = job as FakeJob;
        if (j.status !== "done") j.status = "stopped";
        return this.reply(conn, id, { result: { run_id: j.run_id, status: j.status, journal_seq: j.steps_done * 2 + 1 } });
      }
      case "job.receipt": {
        const j = job as FakeJob;
        // rows like the daemon's: the first step the job did not finish is failed (stuck/error) or interrupted
        const stoppedAt = j.steps_done;
        const rows = Array.from({ length: j.steps_planned }, (_, i) => ({ step_id: `s${i + 1}`, status: i < stoppedAt ? "done" : i > stoppedAt ? "not_run" : j.status === "stuck" || j.status === "error" ? "failed" : j.status === "done" ? "not_run" : j.status === "running" ? "running" : "interrupted", note: i === stoppedAt && (j.status === "stuck" || j.status === "error") ? "no match" : "" }));
        return this.reply(conn, id, { result: { run_id: j.run_id, receipt: { steps_done: j.steps_done }, detail: { outcome: j.status }, steps: rows, ...(j.record === true && j.status === "done" && this.recordError !== null ? { recording: { error: this.recordError } } : j.record === true && j.status === "done" ? { recording: { flow: `C:\\runs\\${j.run_id}.flow.yaml`, maestro: `C:\\runs\\${j.run_id}.maestro.yaml`, steps: j.steps_planned, maestro_skipped: ["s2: x"] } } : {}) } });
      }
      case "job.dry_run":
        return this.reply(conn, id, { result: { flow: flowName(p.flow), matched: 1, rows: [], overlays: [], text: "1 of 1 selectors matched" } });
      default: {
        // job.run_script
        const run_id = `20261020-000000-script${String(this.nextJob++).padStart(4, "0")}`;
        return this.reply(conn, id, {
          result: { run_id, outcome: "ok", value: 2, error: null, stdout: [], stdout_truncated: false, refusals: [], calls: 0, wall_ms: 5, cpu_ms: 1, journal: `runs/${run_id}/journal.ndjson`, journalled_refusals: 0 },
        });
      }
    }
  }

  private deviceCall(conn: Conn, id: number, p: Record<string, unknown>, err: (c: number, m: string, d?: unknown) => void): void {
    const d = this.devices.find((x) => x.serial === p.device || x.hw_serial === p.device);
    if (!d) return err(CODE.UNKNOWN_DEVICE, `no connected device matches ${JSON.stringify(p.device)}`);
    const held = this.leases.get(d.device);
    if (!held) return err(CODE.NOT_HOLDER, `you do not hold a lease on ${d.device}; call lease.acquire first`);
    if (held.conn !== conn.id) return err(CODE.NOT_HOLDER, `device ${d.device} is held by ${held.holder}, not by you`, { device: d.device, holder: held.holder, epoch: held.epoch });
    const call = (p.call ?? {}) as Record<string, unknown>;
    const tool = String(call.tool);
    if (!this.serveAll && !SERVED.includes(tool)) return err(CODE.METHOD_NOT_FOUND, `tool ${tool} is planned: not implemented in this bridged yet`);

    // the gate: S24, allowlist, then the mode (gate.rs `check`)
    const health = this.health[d.device];
    const refuse = (message: string): void => err(CODE.REFUSED, message, { tool, ...(health === undefined ? {} : { health }) });
    if (d.device === S24.device) {
      if (!(tool === "getprop" && S24_READS.includes(String(call.key)))) return refuse(`${tool} refused: the protected phone is read-only and never a target`);
    } else if (d.allowed === false) {
      return refuse(`${d.serial} is not on the allowlist. Start bridged with --allow-serial <hardware serial> (or list it in allow.txt) to drive it.`);
    } else if (ACTING.includes(tool) && d.mode === "read_only") {
      return refuse(`${tool} refused: this device is read_only, so it only answers reads`);
    }

    const mis = this.misbehave;
    this.misbehave = "none";
    if (mis === "garbage-line") return this.rawWrite(conn, "this is not json at all\n");
    if (mis === "not-an-object") return this.rawWrite(conn, "[1,2,3]\n");
    if (mis === "no-id") return this.rawWrite(conn, JSON.stringify({ jsonrpc: "2.0", result: {} }) + "\n");
    if (mis === "no-result") return this.rawWrite(conn, JSON.stringify({ jsonrpc: "2.0", id }) + "\n");
    if (mis === "bad-error") return this.rawWrite(conn, JSON.stringify({ jsonrpc: "2.0", id, error: "boom" }) + "\n");
    if (mis === "wrong-shape") return this.reply(conn, id, { result: { tool: 7, text: ["not", "a", "string"] } });
    if (mis === "hang") return;
    if (mis === "drop") {
      conn.socket.destroy();
      return;
    }
    if (this.nextCallResult !== undefined) {
      const r = this.nextCallResult;
      this.nextCallResult = undefined;
      return this.reply(conn, id, { result: r });
    }
    const text = tool === "getprop" ? (call.key === "ro.serialno" ? d.hw_serial : `value-of-${String(call.key)}`) : `${tool} ok on ${d.model}`;
    // pull_new's data schema needs path, size and sha256 (devtools.rs answers them); push answers the same three
    const files = tool === "pull_new" || tool === "push" ? { path: "/sdcard/Download/fake.bin", size: 4, sha256: "9f64a747e1b97f131fabb6b447296c9b6f0201e79fb3c5356e6c77e89b6a806a" } : {};
    this.reply(conn, id, { result: { tool, text, image_bytes: tool === "look" ? 1234 : null, ...files, ...(health === undefined ? {} : { health }) } });
  }
}
