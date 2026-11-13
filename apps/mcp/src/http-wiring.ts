// The Streamable HTTP side of the MCP server (task 2.6): node:http plus the SDK's
// StreamableHTTPServerTransport, sharing server-core with the stdio side.
//
// This is the hub-mode listener exception. It binds ONE literal IPv4
// address: the laptop's tailnet address (100.64.0.0/10), or 127.0.0.1 for tests. Anything else
// is refused at startup. Every request needs a bearer token (tokens.ts), and each token is scoped
// to one (device, mode) pair.
//
// Order of checks per request:
//   1. bearer token (401, empty body);
//   2. per-token rate limit (429);
//   3. path is exactly /mcp, no query string (404), method POST (405);
//   4. declared body size (413), then the body cap while the body streams in (413);
//   5. inside the SDK transport: Host and Origin (DNS-rebinding protection, 403);
//   6. per tool call: the token's mode and device (a refused tool result), then the daemon's own gate.
// Every refusal is logged, without the token.
//
// Stateless: a fresh SDK Server and transport per POST. GET and DELETE get 405, which the spec
// allows and the SDK client accepts. The answer is plain JSON, except for a tools/call that carries
// a progressToken (2.11a): that one POST is answered as an SSE stream, so the progress
// notifications reach the client exactly as on stdio (in JSON mode the SDK would drop them). The
// handlers are the same module as stdio's (sdk-handlers.ts).

import http from "node:http";
import type { IncomingMessage, ServerResponse } from "node:http";
import { Server as SdkServer } from "@modelcontextprotocol/sdk/server/index.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import { DaemonError } from "./daemon.ts";
import type { DaemonLike } from "./daemon.ts";
import { installHandlers, wantsProgress } from "./sdk-handlers.ts";
import { createServer as createCore } from "./server-core.ts";
import type { Server as Core, ServerOptions } from "./server-core.ts";
import { Supervisor } from "./supervise.ts";
import { callIsRead, toolByName } from "./tools.ts";
import type { Profile } from "./tools.ts";
import { bearer, modeAllows, modeLimit, namesBoundDevice } from "./tokens.ts";
import type { Binding, DeviceRow, TokenTable } from "./tokens.ts";

export const MCP_PATH = "/mcp";
const DEFAULT_MAX_BODY = 1_048_576;
const REFUSED_CODE = -32003;

export interface RateLimit {
  /** Sustained requests per minute, per token. */
  perMinute: number;
  /** Requests allowed back to back before the rate applies. */
  burst: number;
}

export interface HttpConfig {
  /** One literal IPv4: the tailnet address (`tailscale ip -4`), or 127.0.0.1 for tests. */
  host: string;
  /** 1 to 65535; 0 picks a free port (tests). */
  port: number;
  /** Extra Host header values to accept, e.g. "laptop.tail1234.ts.net:8765" (MagicDNS). */
  allowedHosts?: string[];
  /** Extra Origin values to accept. Clients that send no Origin (Claude Code, Gemini) need none. */
  allowedOrigins?: string[];
  /** Largest POST body in bytes. Default 1 MiB. */
  maxBodyBytes?: number;
  rateLimit?: RateLimit;
  profile?: Profile;
}

export class BindRefused extends Error {
  constructor(message: string) {
    super(message);
    this.name = "BindRefused";
  }
}

const OCTET = "(25[0-5]|2[0-4]\\d|1\\d\\d|[1-9]?\\d)";
const IPV4_RE = new RegExp(`^${OCTET}\\.${OCTET}\\.${OCTET}\\.${OCTET}$`);

/**
 * The bind address, or BindRefused with the reason. Accepts 127.0.0.1 and literal IPv4 addresses
 * in 100.64.0.0/10 (Tailscale's CGNAT range), and nothing else.
 */
export function checkBindAddress(host: unknown): string {
  const why = (s: string): BindRefused => new BindRefused(`refusing to listen on ${JSON.stringify(host)}: ${s}. The listener binds only the laptop's tailnet IPv4 address (100.64.0.0/10, see \`tailscale ip -4\`) or 127.0.0.1 for tests.`);
  if (typeof host !== "string" || host.length === 0) throw why("no address given");
  if (host === "127.0.0.1") return host;
  const m = IPV4_RE.exec(host);
  if (m === null) {
    if (host.includes(":")) throw why("IPv6 addresses (including :: and ::1) are not accepted");
    if (/^[0-9.]+$/.test(host)) throw why("not a well-formed dotted-quad IPv4 address");
    throw why("a hostname is not accepted, because a name can resolve to any interface; give the literal address");
  }
  const [a, b] = [Number(m[1]), Number(m[2])];
  if (host === "0.0.0.0") throw why("0.0.0.0 means every interface, the LAN and any public address included");
  if (a === 127) throw why("of the loopback range only 127.0.0.1 is accepted (for tests)");
  if (a === 10 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 169 && b === 254)) throw why("a LAN or link-local address would expose the listener to the local network");
  if (a === 100 && b >= 64 && b <= 127) {
    if (host === "100.100.100.100") throw why("100.100.100.100 is Tailscale's own DNS address, not this machine's");
    return host;
  }
  throw why("not a tailnet address");
}

export function checkPort(port: unknown): number {
  if (typeof port !== "number" || !Number.isInteger(port) || port < 0 || port > 65535) throw new BindRefused(`refusing port ${JSON.stringify(port)}: give an integer from 1 to 65535 in the config (0 picks a free port, for tests)`);
  return port;
}

export interface LogEntry {
  event: "refused" | "ready" | "closed" | "error";
  reason?: string;
  status?: number;
  remote?: string;
  method?: string;
  /** The path only. The query string is never logged. */
  path?: string;
  token_id?: string;
  tool?: string;
  detail?: string;
}
export type Logger = (e: LogEntry) => void;

/** One JSON line per entry on stderr. */
export const stderrLogger: Logger = (e) => {
  process.stderr.write(`${JSON.stringify({ ts: new Date().toISOString(), ...e })}\n`);
};

/** Token buckets keyed by the token's log id. */
export class RateLimiter {
  private readonly buckets = new Map<string, { tokens: number; at: number }>();
  private readonly perMs: number;
  private readonly burst: number;
  private readonly now: () => number;

  constructor(limit: RateLimit, now: () => number = Date.now) {
    if (!(limit.perMinute > 0) || !(limit.burst >= 1)) throw new Error("rateLimit needs perMinute > 0 and burst >= 1");
    this.perMs = limit.perMinute / 60_000;
    this.burst = limit.burst;
    this.now = now;
  }

  /** 0 if the request may go now, else the seconds to wait. */
  take(id: string): number {
    const t = this.now();
    const b = this.buckets.get(id) ?? { tokens: this.burst, at: t };
    b.tokens = Math.min(this.burst, b.tokens + (t - b.at) * this.perMs);
    b.at = t;
    this.buckets.set(id, b);
    if (b.tokens >= 1) {
      b.tokens -= 1;
      return 0;
    }
    return Math.max(1, Math.ceil((1 - b.tokens) / this.perMs / 1000));
  }
}

async function boundRow(inner: DaemonLike, b: Binding): Promise<DeviceRow | undefined> {
  const res = (await inner.request("devices.list", {})) as { devices?: unknown } | null;
  const rows = Array.isArray(res?.devices) ? (res.devices as DeviceRow[]) : [];
  return rows.find((r) => r !== null && typeof r === "object" && r.hw_serial === b.serial);
}

const scopeRefusal = (message: string): DaemonError => new DaemonError("rpc", message, REFUSED_CODE);

/**
 * The daemon's job methods a read_only token may reach: job.status and job.receipt (both behind
 * job_status) and job.dry_run (job_start with dry_run: true). job.start, job.resume, job.pause,
 * job.stop and job.run_script act, so they need a full token.
 */
const READ_JOB_METHODS = new Set(["job.status", "job.receipt", "job.dry_run"]);

/**
 * The daemon as one token may see it: devices.list shows only the token's device, and every
 * lease or device call must name that device and fit the token's mode. Defence in depth behind
 * the call gate; both use the same rules (modeAllows, namesBoundDevice).
 */
export function scopedDaemon(inner: DaemonLike, b: Binding): DaemonLike {
  const checkDevice = async (device: unknown): Promise<void> => {
    if (typeof device !== "string" || !namesBoundDevice(b, device, await boundRow(inner, b))) throw scopeRefusal("refused by the token scope: this token is bound to one device, and the call names another");
  };
  return {
    get busyRetries() {
      return inner.busyRetries;
    },
    get connectionEpoch() {
      return inner.connectionEpoch;
    },
    close() {
      // shared connection: the HTTP server's owner closes it
    },
    async request(method, params = {}, opts) {
      switch (method) {
        case "devices.list": {
          const res = await inner.request(method, params, opts);
          const r = res as { devices?: unknown } | null;
          if (r === null || typeof r !== "object" || !Array.isArray(r.devices)) return res; // core reports the malformed reply
          return { ...r, devices: (r.devices as DeviceRow[]).filter((d) => d !== null && typeof d === "object" && d.hw_serial === b.serial) };
        }
        case "lease.status": {
          if (params.device !== undefined && params.device !== null) {
            await checkDevice(params.device);
            return inner.request(method, params, opts);
          }
          const row = await boundRow(inner, b);
          const res = await inner.request(method, params, opts);
          const r = res as { leases?: unknown } | null;
          if (r === null || typeof r !== "object" || !Array.isArray(r.leases)) return res;
          return { ...r, leases: (r.leases as { device?: unknown }[]).filter((l) => typeof l?.device === "string" && namesBoundDevice(b, l.device, row)) };
        }
        case "lease.acquire":
        case "lease.release":
          await checkDevice(params.device);
          return inner.request(method, params, opts);
        case "device.call": {
          await checkDevice(params.device);
          const tool = (params.call as { tool?: unknown } | undefined)?.tool;
          const def = typeof tool === "string" ? toolByName(tool) : undefined;
          if (!modeAllows(b.mode, def?.read === true, String(tool))) throw scopeRefusal(`${String(tool)} refused by the token scope: this token is ${b.mode}, ${modeLimit(b.mode)}`);
          return inner.request(method, params, opts);
        }
        // 2.11a: the job methods. read_only tokens get READ_JOB_METHODS only (job_status, and
        // job_start with dry_run: true); the device is the one named, or, for a run id, the device
        // the daemon says the job runs on.
        case "job.start":
        case "job.dry_run":
        case "job.run_script":
        case "job.status":
        case "job.pause":
        case "job.resume":
        case "job.stop":
        case "job.receipt": {
          if (!modeAllows(b.mode, READ_JOB_METHODS.has(method), method)) throw scopeRefusal(`${method} refused by the token scope: this token is ${b.mode}, ${modeLimit(b.mode)}`);
          if (method === "job.start" || method === "job.dry_run" || method === "job.run_script") {
            await checkDevice(params.device);
            return inner.request(method, params, opts);
          }
          const status = (await inner.request("job.status", { run_id: params.run_id }, opts)) as { device?: unknown } | null;
          await checkDevice(status?.device);
          return method === "job.status" ? status : inner.request(method, params, opts);
        }
        default:
          throw scopeRefusal(`refused by the token scope: ${method} is not available over HTTP`);
      }
    },
  };
}

export interface RunningServer {
  host: string;
  port: number;
  /** http://host:port/mcp (never carries a token). */
  url: string;
  close(): Promise<void>;
}

export interface StartOptions {
  config: HttpConfig;
  tokens: TokenTable;
  daemon: DaemonLike;
  log?: Logger;
  now?: () => number;
  /** Pushes and card instructions, one for all tokens (default: posts nothing). */
  supervisor?: Supervisor;
  /** Extra server-core options (tests: a faster progress cadence). */
  core?: Pick<ServerOptions, "timers" | "progressEveryMs" | "callDeadlineMs">;
}

/** The request body, or null once it passes `max` bytes. */
function readBody(req: IncomingMessage, max: number): Promise<string | null> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    let done = false;
    req.on("data", (c: Buffer) => {
      if (done) return;
      size += c.length;
      if (size > max) {
        done = true;
        resolve(null);
        return;
      }
      chunks.push(c);
    });
    req.on("end", () => {
      if (!done) {
        done = true;
        resolve(Buffer.concat(chunks).toString("utf8"));
      }
    });
    req.on("error", (e) => {
      if (!done) {
        done = true;
        reject(e);
      }
    });
  });
}

export async function startHttpServer(opts: StartOptions): Promise<RunningServer> {
  const { config, tokens, daemon } = opts;
  const log = opts.log ?? stderrLogger;
  const host = checkBindAddress(config.host);
  const port = checkPort(config.port);
  const maxBody = config.maxBodyBytes ?? DEFAULT_MAX_BODY;
  if (!Number.isInteger(maxBody) || maxBody < 1 || maxBody > 16 * 1_048_576) throw new Error(`maxBodyBytes must be an integer from 1 to ${16 * 1_048_576}`);
  if (tokens.size === 0) throw new Error("no tokens are stored, so nobody could use the listener; issue one first (src/token-admin.ts issue)");
  for (const o of config.allowedOrigins ?? []) if (typeof o !== "string" || !/^https?:\/\/[^\s/*]+$/.test(o)) throw new Error(`allowedOrigins entry ${JSON.stringify(o)} must be a plain origin like http://host:port, no wildcards`);
  for (const h of config.allowedHosts ?? []) if (typeof h !== "string" || !/^[A-Za-z0-9.-]+(:\d{1,5})?$/.test(h)) throw new Error(`allowedHosts entry ${JSON.stringify(h)} must be host or host:port, no wildcards`);
  const limiter = new RateLimiter(config.rateLimit ?? { perMinute: 120, burst: 30 }, opts.now);
  const profile: Profile = config.profile ?? "full";
  const supervisor = opts.supervisor ?? new Supervisor();
  const cores = new Map<string, Core>();
  let allowedHosts: string[] = [];
  let allowedOrigins: string[] = [];

  function coreFor(b: Binding): Core {
    let core = cores.get(b.id);
    if (core === undefined) {
      core = createCore({
        daemon: scopedDaemon(daemon, b),
        profile,
        session: `http-${b.serial}-${b.mode}`,
        ...opts.core,
        supervisor,
        // no background job watch: this listener outlives sessions (see ServerOptions.watch)
        watch: false,
        gate: async (def, args) => {
          let why: string | null = null;
          let reason = "";
          // a call's own arguments may make it a read (job_start with dry_run: true)
          if (!modeAllows(b.mode, callIsRead(def, args), def.name)) {
            why = `${def.name} refused: this token is ${b.mode}, ${modeLimit(b.mode)}`;
            reason = "token_mode";
          } else if (typeof args.device === "string" && !namesBoundDevice(b, args.device, await boundRow(daemon, b))) {
            why = `device ${JSON.stringify(args.device)} refused: this token is bound to one device, and that is not it`;
            reason = "token_device";
          }
          if (why !== null) log({ event: "refused", reason, token_id: b.id, tool: def.name });
          return why;
        },
      });
      cores.set(b.id, core);
    }
    return core;
  }

  async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const remote = req.socket.remoteAddress ?? "?";
    const method = req.method ?? "?";
    const rawUrl = req.url ?? "";
    const path = rawUrl.split("?")[0] ?? "";
    const refuse = (status: number, reason: string, tokenId?: string, headers: Record<string, string> = {}): void => {
      log({ event: "refused", reason, status, remote, method, path, ...(tokenId === undefined ? {} : { token_id: tokenId }) });
      res.writeHead(status, { ...headers, "Content-Length": "0", "Cache-Control": "no-store", Connection: "close" });
      res.end();
    };

    const presented = bearer(req.headers.authorization);
    const b = tokens.authenticate(presented);
    if (b === null) return refuse(401, req.headers.authorization === undefined ? "no_token" : "bad_token", undefined, { "WWW-Authenticate": "Bearer" });
    const wait = limiter.take(b.id);
    if (wait > 0) return refuse(429, "rate_limited", b.id, { "Retry-After": String(wait) });
    if (rawUrl !== MCP_PATH) return refuse(404, path === MCP_PATH ? "query_string" : "not_found", b.id);
    if (method !== "POST") return refuse(405, "method", b.id, { Allow: "POST" });
    const declared = Number(req.headers["content-length"]);
    if (Number.isFinite(declared) && declared > maxBody) return refuse(413, "body_too_large", b.id);

    // the body is read here (capped) so a tools/call with a progressToken can get an SSE answer
    const text = await readBody(req, maxBody);
    if (text === null) return refuse(413, "body_too_large", b.id);
    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch {
      parsed = { jsonrpc: "not json" }; // the SDK answers 400 (after its Host and Origin checks)
    }

    const core = coreFor(b);
    const sdk = new SdkServer({ name: "agentroid", version: "0.1.0" }, { capabilities: { tools: {} }, instructions: core.instructions });
    installHandlers(sdk, core);
    const transport = new StreamableHTTPServerTransport({
      sessionIdGenerator: undefined,
      enableJsonResponse: !wantsProgress(parsed),
      enableDnsRebindingProtection: true,
      allowedHosts,
      allowedOrigins,
      maxRequestBodySize: maxBody,
    });
    // set before connect: the SDK chains to whatever handler is already there
    transport.onerror = (e: Error): void => {
      let detail = e.message;
      if (presented !== undefined) detail = detail.split(presented).join("[token]");
      const reason = /^Invalid Origin/.test(detail) ? "origin" : /^Invalid Host/.test(detail) ? "host" : /too large|exceeds/i.test(detail) ? "body_too_large" : "transport";
      log({ event: "refused", reason, remote, method, path, token_id: b.id, detail: detail.slice(0, 200) });
    };
    res.on("close", () => {
      void transport.close().catch(() => {});
      void sdk.close().catch(() => {});
    });
    await sdk.connect(transport);
    await transport.handleRequest(req, res, parsed);
  }

  const server = http.createServer({ maxHeaderSize: 16_384, requestTimeout: 120_000, headersTimeout: 10_000 }, (req, res) => {
    handle(req, res).catch((e: unknown) => {
      log({ event: "error", detail: e instanceof Error ? e.message.slice(0, 200) : "unknown", method: req.method ?? "?", path: (req.url ?? "").split("?")[0] ?? "" });
      if (!res.headersSent) res.writeHead(500, { "Content-Length": "0", Connection: "close" });
      res.end();
    });
  });

  await new Promise<void>((resolve, reject) => {
    server.once("error", (e: NodeJS.ErrnoException) => {
      if (e.code === "EADDRNOTAVAIL") reject(new BindRefused(`cannot listen on ${host}: that address is not on this machine. Is Tailscale up? Check \`tailscale ip -4\`.`));
      else if (e.code === "EADDRINUSE") reject(new BindRefused(`cannot listen on ${host}:${port}: the port is in use. Pick another port in the config.`));
      else reject(e);
    });
    server.listen({ host, port, exclusive: true }, () => resolve());
  });
  const addr = server.address();
  const actualPort = addr !== null && typeof addr === "object" ? addr.port : port;
  allowedHosts = [`${host}:${actualPort}`, host, ...(config.allowedHosts ?? [])];
  allowedOrigins = [`http://${host}:${actualPort}`, ...(config.allowedOrigins ?? [])];
  const url = `http://${host}:${actualPort}${MCP_PATH}`;
  log({ event: "ready", detail: `${url} profile=${profile} tokens=${tokens.size}` });

  return {
    host,
    port: actualPort,
    url,
    close: async () => {
      server.closeAllConnections();
      await new Promise<void>((r) => server.close(() => r()));
      log({ event: "closed", detail: url });
    },
  };
}

/** Reads and checks a config object (from JSON). */
export function parseConfig(raw: unknown): HttpConfig {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) throw new Error("config must be a JSON object");
  const o = raw as Record<string, unknown>;
  const cfg: HttpConfig = { host: checkBindAddress(o.host), port: checkPort(o.port) };
  if (o.port === 0) throw new BindRefused("port 0 is for tests only; give a fixed port in the config so clients can find the server");
  const strings = (k: string): string[] | undefined => {
    const v = o[k];
    if (v === undefined) return undefined;
    if (!Array.isArray(v) || !v.every((x) => typeof x === "string")) throw new Error(`${k} must be a list of strings`);
    return v as string[];
  };
  const ah = strings("allowedHosts");
  if (ah) cfg.allowedHosts = ah;
  const ao = strings("allowedOrigins");
  if (ao) cfg.allowedOrigins = ao;
  if (o.maxBodyBytes !== undefined) cfg.maxBodyBytes = o.maxBodyBytes as number;
  if (o.rateLimit !== undefined) cfg.rateLimit = o.rateLimit as RateLimit;
  if (o.profile !== undefined) {
    if (o.profile !== "full" && o.profile !== "minimal") throw new Error('profile must be "full" or "minimal"');
    cfg.profile = o.profile;
  }
  return cfg;
}
