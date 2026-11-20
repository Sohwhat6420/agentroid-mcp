// Per-device, per-mode bearer tokens for the Streamable HTTP listener (task 2.6).
//
// A token maps to exactly one (hardware serial, mode) pair, and a request carrying it may act only
// within that pair: a read_only token gets read tools only, and a token for device A cannot name
// device B. This layer only narrows. The daemon's own gate (the per-token modes, the protected-device rule)
// still applies to everything that gets past it.
//
// Secrets: a token is never printed, logged or put in a URL. Logs carry `Binding.id`, a short
// fingerprint that does not reveal the token. Tokens live in Windows Credential Manager, reached
// through the credstore helper (crates/credstore), which is spawned hidden. The secret travels over
// the helper's stdin and stdout pipes only, never on its command line.

import { spawn } from "node:child_process";
import { createHash, randomBytes, timingSafeEqual } from "node:crypto";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/** The credstore helper: AGENTROID_CREDSTORE, else the repo's target/release, then target/debug. */
export function credstorePath(): string {
  if (process.env.AGENTROID_CREDSTORE) return process.env.AGENTROID_CREDSTORE;
  const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
  const exe = process.platform === "win32" ? "credstore.exe" : "credstore";
  for (const p of [join(root, "target", "release", exe), join(root, "target", "debug", exe)]) if (existsSync(p)) return p;
  throw new Error("credstore helper not found: build it (cargo build --release -p agentroid-credstore) or set AGENTROID_CREDSTORE");
}

export const MODES = ["full", "helper", "read_only"] as const;
export type Mode = (typeof MODES)[number];

/** What a valid token grants. `id` is safe to log; the token itself never is. */
export interface Binding {
  serial: string;
  mode: Mode;
  id: string;
}

export interface StoredToken {
  serial: string;
  mode: Mode;
  token: string;
}

export interface TokenStore {
  list(): Promise<StoredToken[]>;
  put(serial: string, mode: Mode, token: string): Promise<void>;
  remove(serial: string, mode: Mode): Promise<boolean>;
}

/** Credential Manager target names: `agentroid:mcp-token:<hardware serial>:<mode>`. */
export const TARGET_PREFIX = "agentroid:mcp-token:";
const SERIAL_RE = /^[A-Za-z0-9._-]{1,64}$/;
const MIN_TOKEN_CHARS = 32;
const MAX_TOKEN_CHARS = 256;

export const isMode = (x: unknown): x is Mode => typeof x === "string" && (MODES as readonly string[]).includes(x);
export const validSerial = (s: unknown): s is string => typeof s === "string" && SERIAL_RE.test(s);

export function targetFor(serial: string, mode: Mode, prefix: string = TARGET_PREFIX): string {
  if (!validSerial(serial)) throw new Error(`bad device serial ${JSON.stringify(serial)}: use the hardware serial (ro.serialno), letters, digits, . _ - only`);
  if (!isMode(mode)) throw new Error(`bad mode ${JSON.stringify(mode)}: use one of ${MODES.join(", ")}`);
  return `${prefix}${serial}:${mode}`;
}

export function parseTarget(target: string, prefix: string = TARGET_PREFIX): { serial: string; mode: Mode } | null {
  if (!target.startsWith(prefix)) return null;
  const rest = target.slice(prefix.length);
  const i = rest.lastIndexOf(":");
  if (i < 0) return null;
  const serial = rest.slice(0, i);
  const mode = rest.slice(i + 1);
  return validSerial(serial) && isMode(mode) ? { serial, mode } : null;
}

/** 256 random bits, URL-safe. */
export const newToken = (): string => `agr_${randomBytes(32).toString("base64url")}`;

/** A short, one-way label for logs. It does not reveal the token. */
export const fingerprint = (token: string): string => createHash("sha256").update("agentroid-token-id\0").update(token, "utf8").digest("hex").slice(0, 10);

const digest = (s: string): Buffer => createHash("sha256").update(s, "utf8").digest();

export class TokenTable {
  private readonly entries: { hash: Buffer; binding: Binding }[] = [];

  constructor(tokens: StoredToken[]) {
    const seen = new Set<string>();
    for (const t of tokens) {
      const label = `${String(t.serial)}/${String(t.mode)}`;
      if (!validSerial(t.serial) || !isMode(t.mode)) throw new Error(`token entry ${label} has a bad serial or mode`);
      if (typeof t.token !== "string" || t.token.length < MIN_TOKEN_CHARS || t.token.length > MAX_TOKEN_CHARS) throw new Error(`token entry ${label} has a token of the wrong length (want ${MIN_TOKEN_CHARS} to ${MAX_TOKEN_CHARS} characters)`);
      const hash = digest(t.token);
      const key = hash.toString("hex");
      if (seen.has(key)) throw new Error(`token entry ${label} reuses another entry's token`);
      seen.add(key);
      this.entries.push({ hash, binding: { serial: t.serial, mode: t.mode, id: `${t.serial}/${t.mode}#${fingerprint(t.token)}` } });
    }
  }

  get size(): number {
    return this.entries.length;
  }

  /**
   * The binding for a presented token, or null. Constant time in the secret: both sides are
   * hashed to 32 bytes, every entry is compared with timingSafeEqual, and the loop never exits
   * early.
   */
  authenticate(presented: string | undefined): Binding | null {
    if (typeof presented !== "string" || presented.length === 0 || presented.length > MAX_TOKEN_CHARS) return null;
    const h = digest(presented);
    let found: Binding | null = null;
    for (const e of this.entries) {
      const same = timingSafeEqual(h, e.hash);
      if (same && found === null) found = e.binding;
    }
    return found;
  }
}

/** The token from an `Authorization: Bearer <token>` header, or undefined. */
export function bearer(header: string | undefined): string | undefined {
  if (typeof header !== "string") return undefined;
  const m = /^Bearer +([\x21-\x7e]+) *$/i.exec(header);
  return m?.[1];
}

/**
 * What a `helper` token may do besides reading: the calls the daemon's gate gives a helper route
 * (gestures, keys and typing; `Call::has_helper_route` in crates/core/src/gate.rs, and `set_value`,
 * which the daemon gates as a tap), plus `lease`, which acting needs. Everything that needs adb
 * shell is absent: unlock, open_app, open_url, start_activity, push, pull_new, run_script and the
 * job methods that act.
 */
export const HELPER_ACTING: ReadonlySet<string> = new Set(["tap", "long_press", "swipe", "scroll", "type_text", "key", "set_value", "lease"]);
/** Reads a `helper` token is refused, as the daemon refuses them on a helper device: `doctor` needs adb shell. */
export const HELPER_REFUSED_READS: ReadonlySet<string> = new Set(["doctor"]);

/**
 * The mode rule, used by both enforcement points (the call gate and the scoped daemon). `name` is
 * the tool, or the daemon method for a job call.
 * - full: any tool; the daemon's own gate still applies.
 * - read_only: read tools only.
 * - helper: read tools except `doctor`, and the acting tools of HELPER_ACTING. The token's mode is
 *   decided here and not left to the daemon, whose gate knows the device's mode, not the token's.
 */
export function modeAllows(mode: Mode, readTool: boolean, name: string): boolean {
  if (mode === "full") return true;
  if (mode === "read_only") return readTool;
  return readTool ? !HELPER_REFUSED_READS.has(name) : HELPER_ACTING.has(name);
}

/** The end of a mode refusal: what this mode is limited to. */
export function modeLimit(mode: Mode): string {
  return mode === "helper" ? "so it may only call read tools (not doctor), gestures, keys and typing" : "so it may only call read tools";
}

export interface DeviceRow {
  device: string;
  serial: string;
  hw_serial: string;
  alias?: string;
}

/**
 * Does `name` (a `device` argument) pick the token's device? `row` is the connected device whose
 * hardware serial is the token's serial, or undefined if it is not connected.
 */
export function namesBoundDevice(b: Binding, name: string, row: DeviceRow | undefined): boolean {
  if (row === undefined) return name === b.serial;
  if (row.hw_serial !== b.serial) return false;
  return name === row.device || name === row.serial || name === row.hw_serial || (typeof row.alias === "string" && name === row.alias);
}

/** Tokens in Windows Credential Manager, through the credstore helper binary. */
export class CredStore implements TokenStore {
  private readonly exe: string;
  readonly prefix: string;
  private readonly timeoutMs: number;

  constructor(exe: string, prefix: string = TARGET_PREFIX, timeoutMs = 10_000) {
    if (!prefix.startsWith("agentroid:")) throw new Error("credstore targets must start with agentroid:");
    this.exe = exe;
    this.prefix = prefix;
    this.timeoutMs = timeoutMs;
  }

  /** One request to the helper: JSON on stdin, JSON on stdout. Errors never carry the secret. */
  run(req: Record<string, unknown>): Promise<Record<string, unknown>> {
    return new Promise((resolve, reject) => {
      const child = spawn(this.exe, [], { windowsHide: true, stdio: ["pipe", "pipe", "pipe"] });
      const out: Buffer[] = [];
      let size = 0;
      let settled = false;
      const done = (f: () => void): void => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        f();
      };
      const timer = setTimeout(() => {
        child.kill();
        done(() => reject(new Error(`credstore: no answer in ${this.timeoutMs} ms`)));
      }, this.timeoutMs);
      child.stdout.on("data", (b: Buffer) => {
        size += b.length;
        if (size <= 1_048_576) out.push(b);
      });
      child.stderr.resume(); // the helper writes nothing secret there; drain it
      child.on("error", (e) => done(() => reject(new Error(`credstore: cannot start ${this.exe}: ${e.message}`))));
      child.on("close", () =>
        done(() => {
          let r: unknown;
          try {
            r = JSON.parse(Buffer.concat(out).toString("utf8"));
          } catch {
            return reject(new Error("credstore: the helper's answer was not JSON"));
          }
          if (r === null || typeof r !== "object") return reject(new Error("credstore: the helper's answer was not an object"));
          const o = r as Record<string, unknown>;
          if (o.ok !== true) return reject(new Error(`credstore: ${typeof o.error === "string" ? o.error : "failed"}`));
          resolve(o);
        }),
      );
      child.stdin.on("error", () => {}); // a helper that dies early surfaces through "close"
      child.stdin.end(JSON.stringify(req));
    });
  }

  async read(target: string): Promise<string | null> {
    try {
      const r = await this.run({ op: "read", target });
      return typeof r.secret === "string" ? r.secret : null;
    } catch (e) {
      if (e instanceof Error && /not found/.test(e.message)) return null;
      throw e;
    }
  }

  async write(target: string, secret: string, user = "agentroid"): Promise<void> {
    await this.run({ op: "write", target, secret, user });
  }

  async delete(target: string): Promise<boolean> {
    const r = await this.run({ op: "delete", target });
    return r.deleted === true;
  }

  async listRaw(): Promise<{ target: string; secret: string }[]> {
    const r = await this.run({ op: "list", prefix: this.prefix });
    const items: unknown[] = Array.isArray(r.items) ? r.items : [];
    const out: { target: string; secret: string }[] = [];
    for (const i of items) {
      const o = i as { target?: unknown; secret?: unknown } | null;
      if (o !== null && typeof o === "object" && typeof o.target === "string" && typeof o.secret === "string") out.push({ target: o.target, secret: o.secret });
    }
    return out;
  }

  async list(): Promise<StoredToken[]> {
    const out: StoredToken[] = [];
    for (const i of await this.listRaw()) {
      const p = parseTarget(i.target, this.prefix);
      if (p !== null) out.push({ ...p, token: i.secret });
    }
    return out;
  }

  put(serial: string, mode: Mode, token: string): Promise<void> {
    return this.write(targetFor(serial, mode, this.prefix), token, `${serial}/${mode}`);
  }

  remove(serial: string, mode: Mode): Promise<boolean> {
    return this.delete(targetFor(serial, mode, this.prefix));
  }
}
