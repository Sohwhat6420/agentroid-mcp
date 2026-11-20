// The health header: on 100% of results, errors included. Every field is {v, age_s}. When the
// daemon cannot give a field, v is the string "unknown" and age_s is the seconds since the field
// was last known (null when it never was). A value is never guessed and never carried over from an
// older reading as if it were current.

import type { Schema } from "./schema.ts";
import { cleanField } from "./untrusted.ts";

export type Unknown = "unknown";
export interface Field<T> {
  v: T | Unknown;
  age_s: number | null;
}
export interface Foreground {
  package?: string;
  title?: string;
}
export interface Header {
  alias: Field<string>;
  model: Field<string>;
  mode: Field<string>;
  battery: Field<number>;
  powered: Field<boolean>;
  temp_c: Field<number>;
  screen: Field<"on" | "off">;
  lock: Field<"locked" | "unlocked">;
  foreground: Field<Foreground>;
  focus_changed: Field<boolean> & { title: string | null };
  /** Age of the freshest health field, null when every health field is unknown. */
  data_age_s: number | null;
}

/** What one reading can carry; every part optional. `age_s` is how old the daemon says the reading is. */
export interface Observation {
  alias?: string;
  model?: string;
  mode?: string;
  battery?: number;
  powered?: boolean;
  temp_c?: number;
  screen?: "on" | "off";
  lock?: "locked" | "unlocked";
  foreground?: Foreground;
  age_s?: number;
}

const HEALTH_KEYS = ["battery", "powered", "temp_c", "screen", "lock", "foreground"] as const;
const KEYS = ["alias", "model", "mode", ...HEALTH_KEYS] as const;
type Key = (typeof KEYS)[number];

const isStr = (v: unknown): v is string => typeof v === "string" && v.length > 0;
/** Longest value kept for each header string, in characters. */
export const FIELD_MAX = { alias: 40, model: 40, mode: 16, package: 120, title: 80 } as const;
const isNum = (v: unknown, lo: number, hi: number): v is number => typeof v === "number" && Number.isFinite(v) && v >= lo && v <= hi;

/** Keep only the parts of a raw reading that are well-formed; anything else is dropped, so it shows as unknown. */
export function parseObservation(raw: unknown): Observation {
  const o: Observation = {};
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return o;
  const r = raw as Record<string, unknown>;
  // The strings below end up on the one-line header. A model name and a window title are whatever the
  // phone says they are, so each is cleaned first (no line breaks or control characters, bounded
  // length): a title cannot start a line of its own or pass as a server notice.
  const str = (v: unknown, max: number): string | undefined => {
    if (!isStr(v)) return undefined;
    const c = cleanField(v, max);
    return c.length > 0 ? c : undefined;
  };
  const alias = str(r.alias, FIELD_MAX.alias);
  if (alias !== undefined) o.alias = alias;
  const model = str(r.model, FIELD_MAX.model);
  if (model !== undefined) o.model = model;
  const mode = str(r.mode, FIELD_MAX.mode);
  if (mode !== undefined) o.mode = mode;
  if (isNum(r.battery, 0, 100)) o.battery = r.battery;
  if (typeof r.powered === "boolean") o.powered = r.powered;
  if (isNum(r.temp_c, -50, 150)) o.temp_c = r.temp_c;
  if (r.screen === "on" || r.screen === "off") o.screen = r.screen;
  if (r.lock === "locked" || r.lock === "unlocked") o.lock = r.lock;
  if (typeof r.foreground === "object" && r.foreground !== null && !Array.isArray(r.foreground)) {
    const f = r.foreground as Record<string, unknown>;
    const fg: Foreground = {};
    const pkg = str(f.package, FIELD_MAX.package);
    if (pkg !== undefined) fg.package = pkg;
    const title = str(f.title, FIELD_MAX.title);
    if (title !== undefined) fg.title = title;
    if (fg.package !== undefined || fg.title !== undefined) o.foreground = fg;
  }
  if (isNum(r.age_s, 0, 1e9)) o.age_s = r.age_s;
  return o;
}

export function mergeObservations(...list: Observation[]): Observation {
  const out: Observation = {};
  for (const o of list) {
    for (const k of KEYS) {
      if (o[k] !== undefined) (out as Record<string, unknown>)[k] = o[k];
    }
    if (o.age_s !== undefined) out.age_s = o.age_s;
  }
  return out;
}

const fgKey = (f: Foreground): string => `${f.package ?? ""}|${f.title ?? ""}`;

interface Seen {
  value: unknown;
  atMs: number;
}

/** Remembers when each field of each device was last known. One per server session. */
export class HealthTracker {
  private seen = new Map<string, Map<Key, Seen>>();
  private now: () => number;

  constructor(now: () => number = Date.now) {
    this.now = now;
  }

  /** The last foreground seen for a device (any age), used as the baseline when a call has no "before" reading. */
  lastForeground(device: string): Foreground | undefined {
    return this.seen.get(device)?.get("foreground")?.value as Foreground | undefined;
  }

  /**
   * Build the header for one result from the readings of this call. `baseline` is the foreground
   * before the call, when known; `focus_changed` compares it with the foreground after.
   */
  header(device: string, current: Observation, baseline?: Foreground): Header {
    const t = this.now();
    let m = this.seen.get(device);
    if (m === undefined) {
      m = new Map();
      this.seen.set(device, m);
    }
    const ageMs = Math.round((current.age_s ?? 0) * 1000);
    const field = <T>(k: Key): Field<T> => {
      const cur = current[k];
      if (cur !== undefined) {
        m.set(k, { value: cur, atMs: t - ageMs });
        return { v: cur as T, age_s: round1(ageMs / 1000) };
      }
      const prev = m.get(k);
      return { v: "unknown", age_s: prev === undefined ? null : round1(Math.max(0, t - prev.atMs) / 1000) };
    };
    const h: Header = {
      alias: field<string>("alias"),
      model: field<string>("model"),
      mode: field<string>("mode"),
      battery: field<number>("battery"),
      powered: field<boolean>("powered"),
      temp_c: field<number>("temp_c"),
      screen: field<"on" | "off">("screen"),
      lock: field<"locked" | "unlocked">("lock"),
      foreground: field<Foreground>("foreground"),
      focus_changed: { v: "unknown", age_s: null, title: null },
      data_age_s: null,
    };
    const fg = h.foreground;
    if (fg.v !== "unknown" && baseline !== undefined) {
      const changed = fgKey(fg.v) !== fgKey(baseline);
      h.focus_changed = { v: changed, age_s: fg.age_s, title: changed ? (fg.v.title ?? fg.v.package ?? null) : null };
    } else {
      // no current foreground, or nothing to compare with: the honest answer is unknown, with the foreground's age
      h.focus_changed = { v: "unknown", age_s: fg.age_s, title: null };
    }
    const ages = HEALTH_KEYS.map((k) => h[k]).filter((f) => f.v !== "unknown" && f.age_s !== null).map((f) => f.age_s as number);
    h.data_age_s = ages.length === 0 ? null : Math.min(...ages);
    return h;
  }
}

/** A header with every field unknown and never seen: used when nothing at all is known. */
export function blankHeader(): Header {
  const u = { v: "unknown" as const, age_s: null };
  return {
    alias: { ...u }, model: { ...u }, mode: { ...u }, battery: { ...u }, powered: { ...u }, temp_c: { ...u },
    screen: { ...u }, lock: { ...u }, foreground: { ...u }, focus_changed: { ...u, title: null }, data_age_s: null,
  };
}

function round1(n: number): number {
  return Math.round(n * 10) / 10;
}

function fieldSchema(valueSchema: Schema): Schema {
  return {
    type: "object",
    required: ["v", "age_s"],
    properties: { v: { anyOf: [valueSchema, { const: "unknown" }] }, age_s: { type: ["number", "null"], minimum: 0 } },
  };
}

export const HEADER_SCHEMA: Schema = {
  type: "object",
  required: ["alias", "model", "mode", "battery", "powered", "temp_c", "screen", "lock", "foreground", "focus_changed", "data_age_s"],
  properties: {
    alias: fieldSchema({ type: "string", minLength: 1 }),
    model: fieldSchema({ type: "string", minLength: 1 }),
    mode: fieldSchema({ type: "string", minLength: 1 }),
    battery: fieldSchema({ type: "number", minimum: 0, maximum: 100 }),
    powered: fieldSchema({ type: "boolean" }),
    temp_c: fieldSchema({ type: "number" }),
    screen: fieldSchema({ enum: ["on", "off"] }),
    lock: fieldSchema({ enum: ["locked", "unlocked"] }),
    foreground: fieldSchema({ type: "object", properties: { package: { type: "string" }, title: { type: "string" } } }),
    focus_changed: {
      type: "object",
      required: ["v", "age_s", "title"],
      properties: { v: { anyOf: [{ type: "boolean" }, { const: "unknown" }] }, age_s: { type: ["number", "null"], minimum: 0 }, title: { type: ["string", "null"] } },
    },
    data_age_s: { type: ["number", "null"], minimum: 0 },
  },
};

/** The header as one line of text, put first in every result so the model sees it without parsing JSON. */
export function headerLine(h: Header): string {
  const val = <T>(f: Field<T>, show: (x: T) => string): string => (f.v === "unknown" ? `unknown${f.age_s === null ? "" : ` (${f.age_s}s ago)`}` : show(f.v));
  const fg = val(h.foreground, (x) => x.title ?? x.package ?? "?");
  const focus = h.focus_changed.v === "unknown" ? "unknown" : h.focus_changed.v ? `YES -> ${h.focus_changed.title ?? "?"}` : "no";
  const who = [val(h.alias, (x) => x), val(h.model, (x) => x), val(h.mode, (x) => x)].join(" / ");
  return (
    `[${who}] battery ${val(h.battery, (x) => `${x}%`)}, powered ${val(h.powered, (x) => (x ? "yes" : "no"))}, ` +
    `temp ${val(h.temp_c, (x) => `${x}C`)}, screen ${val(h.screen, (x) => x)}, lock ${val(h.lock, (x) => x)}, ` +
    `foreground ${fg}, focus_changed ${focus}, data age ${h.data_age_s === null ? "unknown" : `${h.data_age_s}s`}`
  );
}
