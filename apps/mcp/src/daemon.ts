// JSON-RPC 2.0 client for bridged: one JSON document per line over the named pipe
// \\.\pipe\agentroid. Leases belong to the connection, so the client keeps ONE connection and
// reconnects only when it breaks (the lease is gone then, and `connectionEpoch` says so).
//
// * ERROR_PIPE_BUSY (231; libuv reports it as EBUSY): between accepts the daemon has no listening
//   instance, so a connect can hit 231. bridged's report says the client must retry; it does,
//   with backoff, inside the request's own deadline.
// * Every request has a timeout. No call may exceed MAX_CALL_MS (110 s): Claude Code moves an MCP
//   call to the background at 2 minutes, so the server answers first.

import net from "node:net";
import { StringDecoder } from "node:string_decoder";

export const PIPE_PATH = "\\\\.\\pipe\\agentroid";
export const MAX_CALL_MS = 110_000;
export const DEFAULT_TIMEOUT_MS = 30_000;
const MAX_LINE = 4 * 1024 * 1024;

export type DaemonErrorKind = "rpc" | "timeout" | "transport" | "protocol";

export class DaemonError extends Error {
  kind: DaemonErrorKind;
  /** JSON-RPC error code for kind "rpc", else null. */
  code: number | null;
  data: unknown;
  constructor(kind: DaemonErrorKind, message: string, code: number | null = null, data: unknown = undefined) {
    super(message);
    this.name = "DaemonError";
    this.kind = kind;
    this.code = code;
    this.data = data;
  }
}

/** What server-core needs from a daemon client (the tests also supply plain fakes of this). */
export interface DaemonLike {
  request(method: string, params?: Record<string, unknown>, opts?: { timeoutMs?: number }): Promise<unknown>;
  /** Pipe-busy retries so far (the receipt reports the difference per call). */
  readonly busyRetries: number;
  /** Counts completed connections; a change means the old connection, and its leases, are gone. */
  readonly connectionEpoch: number;
  close(): void;
}

export interface DaemonOptions {
  path?: string;
  /** Replace the socket factory (tests). */
  connect?: (path: string) => net.Socket;
  /** Label sent in `hello`; becomes the lease holder name. */
  session?: string;
  defaultTimeoutMs?: number;
  /** Waits between pipe-busy retries; the last value repeats until the deadline. */
  busyBackoffMs?: number[];
}

/** Every call's time limit: at least 1 ms, at most MAX_CALL_MS. */
export function clampTimeout(ms: number): number {
  return Number.isFinite(ms) ? Math.max(1, Math.min(Math.trunc(ms), MAX_CALL_MS)) : MAX_CALL_MS;
}

export function isPipeBusy(err: unknown): boolean {
  const e = err as { code?: unknown; errno?: unknown; message?: unknown } | null;
  if (e === null || typeof e !== "object") return false;
  return e.code === "EBUSY" || e.errno === 231 || e.errno === -231 || /PIPE_BUSY/i.test(String(e.message ?? ""));
}

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  timer: NodeJS.Timeout;
  method: string;
}

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

export class DaemonClient implements DaemonLike {
  busyRetries = 0;
  connectionEpoch = 0;
  /** The holder name the daemon gave this connection in `hello`, once connected. */
  holder: string | null = null;
  /** Lines that were not a usable reply (garbage, batches); a counter for the logs. */
  protocolFaults = 0;

  private path: string;
  private factory: (path: string) => net.Socket;
  private session: string | undefined;
  private defaultTimeoutMs: number;
  private backoff: number[];
  private sock: net.Socket | null = null;
  private connecting: Promise<void> | null = null;
  private pending = new Map<number, Pending>();
  private nextId = 1;
  private closed = false;

  constructor(opts: DaemonOptions = {}) {
    this.path = opts.path ?? PIPE_PATH;
    this.factory = opts.connect ?? ((p) => net.connect(p));
    this.session = opts.session;
    this.defaultTimeoutMs = Math.min(opts.defaultTimeoutMs ?? DEFAULT_TIMEOUT_MS, MAX_CALL_MS);
    this.backoff = opts.busyBackoffMs !== undefined && opts.busyBackoffMs.length > 0 ? opts.busyBackoffMs : [5, 10, 20, 40, 80, 160, 250];
  }

  async request(method: string, params: Record<string, unknown> = {}, opts: { timeoutMs?: number } = {}): Promise<unknown> {
    if (this.closed) throw new DaemonError("transport", "daemon client is closed");
    const timeoutMs = clampTimeout(opts.timeoutMs ?? this.defaultTimeoutMs);
    const deadline = Date.now() + timeoutMs;
    await this.ensureConnected(deadline, method, timeoutMs);
    const left = deadline - Date.now();
    if (left <= 0) throw new DaemonError("timeout", `${method}: timed out after ${timeoutMs} ms while connecting to bridged`);
    return this.send(method, params, left, timeoutMs);
  }

  close(): void {
    this.closed = true;
    this.dropConnection(new DaemonError("transport", "daemon client closed"));
  }

  // ---- connection ----

  private async ensureConnected(deadline: number, method: string, timeoutMs: number): Promise<void> {
    if (this.connecting === null && this.sock !== null && !this.sock.destroyed) return;
    if (this.connecting === null) {
      this.connecting = this.connectLoop(deadline).finally(() => {
        this.connecting = null;
      });
    }
    // The shared attempt may have been started by a caller with a later deadline: this caller still honours its own.
    let timer: NodeJS.Timeout | undefined;
    const busy0 = this.busyRetries;
    const limit = new Promise<never>((_, rej) => {
      timer = setTimeout(() => {
        const busy = this.busyRetries > busy0 ? ` (the pipe was busy, ERROR_PIPE_BUSY, ${this.busyRetries - busy0} retries)` : "";
        rej(new DaemonError("timeout", `${method}: timed out after ${timeoutMs} ms while connecting to bridged${busy}`));
      }, Math.max(1, deadline - Date.now()));
    });
    try {
      await Promise.race([this.connecting, limit]);
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  }

  private async connectLoop(deadline: number): Promise<void> {
    let attempt = 0;
    for (;;) {
      try {
        const s = await this.connectOnce();
        this.attach(s);
        break;
      } catch (e) {
        if (!isPipeBusy(e)) {
          const err = e as NodeJS.ErrnoException;
          const hint = err.code === "ENOENT" ? " (is bridged running?)" : "";
          throw new DaemonError("transport", `cannot connect to bridged at ${this.path}: ${err.message}${hint}`);
        }
        const wait = this.backoff[Math.min(attempt, this.backoff.length - 1)] as number;
        attempt++;
        this.busyRetries++;
        if (Date.now() + wait >= deadline) throw new DaemonError("timeout", `bridged pipe stayed busy (ERROR_PIPE_BUSY) through ${attempt} retries until the call's deadline`);
        await sleep(wait);
      }
    }
    try {
      const hello = (await this.send("hello", this.session === undefined ? {} : { session: this.session }, 10_000, 10_000)) as { holder?: unknown } | null;
      this.holder = hello !== null && typeof hello === "object" && typeof hello.holder === "string" ? hello.holder : null;
      this.connectionEpoch++;
    } catch (e) {
      this.dropConnection(e as Error);
      throw e;
    }
  }

  private connectOnce(): Promise<net.Socket> {
    return new Promise((resolve, reject) => {
      let s: net.Socket;
      try {
        s = this.factory(this.path);
      } catch (e) {
        reject(e);
        return;
      }
      const onError = (e: Error): void => {
        s.destroy();
        reject(e);
      };
      s.once("error", onError);
      s.once("connect", () => {
        s.off("error", onError);
        resolve(s);
      });
    });
  }

  private attach(s: net.Socket): void {
    this.sock = s;
    const decoder = new StringDecoder("utf8");
    let buf = "";
    s.on("data", (chunk: Buffer) => {
      buf += decoder.write(chunk);
      for (;;) {
        const nl = buf.indexOf("\n");
        if (nl < 0) break;
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (line.length > 0) this.onLine(line);
      }
      if (buf.length > MAX_LINE) this.protocolFailure("reply line longer than 4 MiB");
    });
    const gone = (e?: Error): void => {
      if (this.sock === s) this.dropConnection(new DaemonError("transport", `bridged connection lost${e ? `: ${e.message}` : ""}`));
    };
    s.on("error", gone);
    s.on("close", () => gone());
  }

  private dropConnection(reason: Error): void {
    const s = this.sock;
    this.sock = null;
    this.holder = null;
    if (s !== null && !s.destroyed) s.destroy();
    for (const [id, p] of this.pending) {
      clearTimeout(p.timer);
      this.pending.delete(id);
      p.reject(reason);
    }
  }

  private protocolFailure(why: string): void {
    this.protocolFaults++;
    this.dropConnection(new DaemonError("protocol", `bad reply from bridged: ${why}`));
  }

  // ---- requests ----

  private send(method: string, params: Record<string, unknown>, timeoutMs: number, reportedMs: number): Promise<unknown> {
    const s = this.sock;
    if (s === null || s.destroyed) return Promise.reject(new DaemonError("transport", "not connected to bridged"));
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new DaemonError("timeout", `${method}: no answer from bridged within ${reportedMs} ms`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer, method });
      s.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n", (err) => {
        if (err) {
          const p = this.pending.get(id);
          if (p !== undefined) {
            clearTimeout(p.timer);
            this.pending.delete(id);
            p.reject(new DaemonError("transport", `write to bridged failed: ${err.message}`));
          }
        }
      });
    });
  }

  private onLine(line: string): void {
    let msg: unknown;
    try {
      msg = JSON.parse(line);
    } catch {
      this.protocolFailure("not JSON");
      return;
    }
    if (typeof msg !== "object" || msg === null || Array.isArray(msg)) {
      this.protocolFailure("not a JSON-RPC response object");
      return;
    }
    const r = msg as { id?: unknown; result?: unknown; error?: unknown };
    if (r.id === null || r.id === undefined) {
      // an id-less reply is the daemon's answer to something it could not parse: nothing we send is unparseable
      this.protocolFailure("reply without an id");
      return;
    }
    if (typeof r.id !== "number") return; // not ours
    const p = this.pending.get(r.id);
    if (p === undefined) return; // late reply for a call that already timed out
    clearTimeout(p.timer);
    this.pending.delete(r.id);
    const e = r.error as { code?: unknown; message?: unknown; data?: unknown } | undefined | null;
    if (e !== undefined && e !== null) {
      if (typeof e === "object" && typeof e.code === "number" && typeof e.message === "string") p.reject(new DaemonError("rpc", e.message, e.code, e.data));
      else {
        this.protocolFaults++;
        p.reject(new DaemonError("protocol", "bad reply from bridged: malformed error object"));
      }
      return;
    }
    if (!("result" in r)) {
      this.protocolFaults++;
      p.reject(new DaemonError("protocol", "bad reply from bridged: neither result nor error"));
      return;
    }
    p.resolve(r.result);
  }
}
