// The Phase 2 tool list. Order is the array order below and never changes at run time: a
// stable list keeps the model's prompt cache valid. 23 device tools, then the 4 job tools that
// The job tools named in the design (job_start, job_status, job_stop, run_script): 27, the limit (MAX_TOOLS).
//
// Design decision (2.11a): the 27-tool limit stands, so the daemon's eight job.* methods sit
// behind four tools (the daemon's methods are unchanged):
//   job_start   -> job.start; with dry_run: true -> job.dry_run; with resume_run_id -> job.resume
//   job_status  -> job.status, plus job.receipt once the job has ended
//   job_stop    -> job.stop; with mode: "pause" -> job.pause
//   run_script  -> job.run_script

import { HEADER_SCHEMA } from "./header.ts";
import { RECEIPT_SCHEMA } from "./receipt.ts";
import type { Schema } from "./schema.ts";

export const MAX_TOOLS = 27;
export const LEVELS = ["digest", "default", "full"] as const;
export type Level = (typeof LEVELS)[number];
export type Profile = "minimal" | "full";

export type Route = "devices" | "lease" | "device_call" | "job";

export interface ToolDef {
  name: string;
  description: string;
  /** JSON Schema (object, additionalProperties false). */
  input: Schema;
  /** JSON Schema of the `data` member of the result. */
  data: Schema;
  /** True for reads: they carry readOnlyHint and are allowed on read_only devices. */
  read: boolean;
  minimal: boolean;
  route: Route;
  /** Does the daemon serve this tool today? Every tool is served since the 2.4 daemon side (devcall.rs for 14, devtools.rs for doctor, push and pull_new); "planned" stays in the type for a tool added ahead of its daemon half. */
  daemon: "served" | "planned";
  /** Route "job": the daemon method without any mode argument (job.start, ...). */
  method?: string;
  /**
   * When set, a call whose arguments make this true counts as a read for token scopes, although the
   * tool is not a read in general (job_start with dry_run: true).
   */
  readWhen?: (args: Record<string, unknown>) => boolean;
  /** Route "job": the daemon call's own time limit (the whole MCP call is capped below 110 s anyway). */
  timeoutMs?: number;
  /** Input properties the minimal profile's list leaves out (the call still accepts them): the minimal list is tight on tokens. */
  minimalOmits?: string[];
}

// ---- small schema builders (short on purpose: the list is measured in tokens) ----
const S: Schema = { type: "string" };
const N: Schema = { type: "number" };
const I0: Schema = { type: "integer", minimum: 0 };
const B: Schema = { type: "boolean" };
const obj = (properties: Record<string, Schema>, required: string[] = []): Schema => ({
  type: "object",
  properties,
  ...(required.length > 0 ? { required } : {}),
  additionalProperties: false,
});
const DEVICE: Schema = { type: "string", description: "Device key; optional" };
const LEVEL: Schema = { enum: ["digest", "default", "full"] };
const SPACE: Schema = { enum: ["device", "image", "rel"] };
// A selector is {id|text|desc|near_text}; the daemon checks it (see instructions).
const SEL: Schema = { type: "object" };
const REQUIRE: Schema = { type: "array", items: SEL, maxItems: 8 };
const RETRY: Schema = { type: "integer", minimum: 0, maximum: 3 };
const VERIFY: Schema = SEL;
const REF: Schema = S;
const SINCE: Schema = S;
const PROP_KEY: Schema = { type: "string", pattern: "^[A-Za-z0-9._-]{1,64}$" };

const TEXT: Schema = { type: "object", required: ["text"], properties: { text: S } };

// job tools (2.11a): a flow is a flows/*.yaml object or its YAML text; run ids come from job_start
const OBJ: Schema = { type: "object" };
const FLOW: Schema = { type: ["object", "string"] };
const job = (name: string, method: string, description: string, input: Schema, read: boolean, timeoutMs: number): ToolDef => ({
  name, description, input, data: OBJ, read, minimal: false, route: "job", daemon: "served", method, timeoutMs,
});
/** A job_start call that only checks a flow on saved trees (job.dry_run) is a read. */
export const isDryRun = (args: Record<string, unknown>): boolean => args.dry_run === true;
/** Is this call a read for token scopes? */
export const callIsRead = (def: ToolDef, args: Record<string, unknown>): boolean => def.read || def.readWhen?.(args) === true;

const act = (extra: Record<string, Schema>, required: string[] = [], ui = true): Schema =>
  obj({ device: DEVICE, ...extra, ...(ui ? { require: REQUIRE } : {}), retry: RETRY, verify: VERIFY }, required);

export const TOOLS: readonly ToolDef[] = [
  {
    name: "list_devices",
    description: "List connected phones: key, model, mode, lease holder.",
    input: obj({ level: LEVEL }),
    data: { type: "object", required: ["devices"], properties: { devices: { type: "array", items: { type: "object", required: ["device", "model", "mode"] } } } },
    read: true, minimal: true, route: "devices", daemon: "served",
  },
  {
    name: "lease",
    description: "Take, release or check the lease on a phone. Acting calls take it for you.",
    input: obj({ action: { enum: ["acquire", "release", "status"] }, device: DEVICE }, ["action"]),
    data: { type: "object", properties: { lease: { type: "object" }, leases: { type: "array", items: { type: "object" } }, released: B } },
    read: false, minimal: false, route: "lease", daemon: "served",
  },
  {
    name: "look",
    description: "Screenshot plus UI tree. element and ocr: not implemented yet (error).",
    input: obj({ device: DEVICE, level: LEVEL, max_width: { type: "integer", minimum: 64, maximum: 4096 }, element: SEL, ocr: B, since: SINCE }),
    data: TEXT,
    read: true, minimal: true, route: "device_call", daemon: "served", minimalOmits: ["since"],
  },
  {
    name: "ui_tree",
    description: "UI tree as text.",
    input: obj({ device: DEVICE, level: LEVEL, interactive_only: B, require: REQUIRE, since: SINCE }),
    data: TEXT,
    read: true, minimal: true, route: "device_call", daemon: "served",
  },
  {
    name: "screenshot",
    description: "Take a screenshot.",
    input: obj({ device: DEVICE, max_width: { type: "integer", minimum: 64, maximum: 4096 } }),
    data: TEXT,
    read: true, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "tap",
    description: "Tap an index, ref or point.",
    input: act({ index: I0, ref: REF, x: N, y: N, space: SPACE }),
    data: TEXT,
    read: false, minimal: true, route: "device_call", daemon: "served",
  },
  {
    name: "long_press",
    description: "Press and hold an index, ref or point.",
    input: act({ index: I0, ref: REF, x: N, y: N, space: SPACE, ms: { type: "integer", minimum: 100, maximum: 10000 } }),
    data: TEXT,
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "swipe",
    description: "Swipe from (x1, y1) to (x2, y2).",
    input: act({ x1: N, y1: N, x2: N, y2: N, ms: { type: "integer", minimum: 50, maximum: 10000 }, space: SPACE }, ["x1", "y1", "x2", "y2"]),
    data: TEXT,
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "scroll",
    description: "Scroll the screen one page.",
    input: act({ direction: { enum: ["up", "down", "left", "right"] } }, ["direction"]),
    data: TEXT,
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "type_text",
    description: "Type text into the focused field or a tree index.",
    input: act({ text: S, index: I0, ref: REF, submit: B }, ["text"]),
    data: TEXT,
    read: false, minimal: true, route: "device_call", daemon: "served", minimalOmits: ["ref"],
  },
  {
    name: "key",
    description: "Press a key: back, home, enter, recents, ...",
    input: act({ name: S }, ["name"]),
    data: TEXT,
    read: false, minimal: true, route: "device_call", daemon: "served",
  },
  {
    name: "unlock",
    description: "Wake and unlock the phone.",
    input: act({}, [], false),
    data: TEXT,
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "open_app",
    description: "Launch an app by package name.",
    input: act({ package: S }, ["package"], false),
    data: TEXT,
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "open_url",
    description: "Open a URL, optionally in one package.",
    input: act({ uri: S, package: S }, ["uri"], false),
    data: TEXT,
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "start_activity",
    description: "Start an activity by component.",
    input: act({ component: S, action: S, data: S }, ["component"], false),
    data: TEXT,
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "list_apps",
    description: "List installed packages, optionally filtered.",
    input: obj({ device: DEVICE, level: LEVEL, filter: S }),
    data: TEXT,
    read: true, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "wait",
    description: "Wait up to 100 seconds for the screen to settle.",
    input: obj({ device: DEVICE, seconds: { type: "number", minimum: 0, maximum: 100 } }, ["seconds"]),
    data: TEXT,
    read: true, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "getprop",
    description: "Read one system property (adb shell getprop).",
    input: obj({ device: DEVICE, key: PROP_KEY }, ["key"]),
    data: TEXT,
    read: true, minimal: true, route: "device_call", daemon: "served",
  },
  {
    name: "set_value",
    description: "Set a text field to a value and read it back (text fields only).",
    input: act({ element: SEL, index: I0, value: { type: ["string", "number"] } }, ["value"]),
    data: { type: "object", required: ["text"], properties: { text: S, value: { type: ["string", "number"] } } },
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "pixel_probe",
    description: "Not implemented yet (always an error).",
    input: obj({ device: DEVICE, x: N, y: N, space: SPACE }, ["x", "y"]),
    data: { type: "object", required: ["text"], properties: { text: S, rgb: { type: "array", items: { type: "integer", minimum: 0, maximum: 255 }, minItems: 3, maxItems: 3 } } },
    read: true, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "pull_new",
    description: "Fetch a new file from the phone into the daemon's inbox once its size is stable across two polls.",
    input: obj({ device: DEVICE, dir: S, retry: RETRY }, ["dir"]),
    data: { type: "object", required: ["text", "path", "size", "sha256"], properties: { text: S, path: S, size: I0, sha256: { type: "string", pattern: "^[0-9a-f]{64}$" } } },
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "push",
    description: "Copy a file from the daemon's outbox folder to a phone path (full devices only).",
    input: obj({ device: DEVICE, local: S, phone_path: S, retry: RETRY }, ["local", "phone_path"]),
    data: TEXT,
    read: false, minimal: false, route: "device_call", daemon: "served",
  },
  {
    name: "doctor",
    description: "Read-only diagnostics: dumpsys <service>, getprop, settings get, pm list, ...",
    input: obj({ device: DEVICE, level: LEVEL, diagnose: S }),
    data: TEXT,
    read: true, minimal: true, route: "device_call", daemon: "served",
  },
  {
    ...job(
      "job_start",
      "job.start",
      "Start a flow, or steps in app; returns run_id. mode sync waits and returns a row per step. dry_run: check on saved trees. resume_run_id: resume a paused job. record: a done run saves a replayable flow; flow may be that file's name.",
      obj({ device: DEVICE, flow: FLOW, steps: { type: "array", items: OBJ }, app: S, mode: { enum: ["sync", "async"] }, vars: OBJ, oem: S, sdk: I0, auto_dismiss: B, dry_run: B, trees: { type: "array", items: OBJ }, resume_run_id: S, human_confirmed: B, record: B }),
      false,
      60_000,
    ),
    readWhen: isDryRun,
  },
  job("job_status", "job.status", "A job's state in one line; with its receipt once it has ended.", obj({ run_id: S }, ["run_id"]), true, 15_000),
  job("job_stop", "job.stop", "Stop a job, or pause it (mode pause).", obj({ run_id: S, mode: { enum: ["stop", "pause"] } }, ["run_id"]), false, 20_000),
  job("run_script", "job.run_script", "Run sandboxed JavaScript (full devices).", obj({ device: DEVICE, script: S }, ["script"]), false, 100_000),
];

export const INSTRUCTIONS = `Agentroid drives Android phones through a local daemon (bridged). Use it to look at a phone, tap, type, read diagnostics and move files.

Every result starts with a health line and carries a structured header and a receipt:
- The header gives battery, powered, temperature, screen and lock state, the foreground window, focus_changed (true, with the new window's title, when something took focus during the call), the data age, and the device alias, model and mode.
- A field that says "unknown" could not be read; the number in brackets is how long ago it was last known. Do not assume a value for it.
- The receipt counts the call: steps, wall time, retries, errors. The server makes no model calls, so tokens and cost are 0.

Workflow:
1. list_devices. With one phone connected, device may be left out; with several you must name one. The server never picks for you.
2. look (screenshot plus UI tree) or ui_tree to see the screen. Prefer ui_tree and indexes over pixel coordinates.
3. Act with tap, type_text, key, swipe, scroll, open_app and the rest. The server takes the lease for you; only one session may hold a phone, and a second one is refused with the holder's name.
4. After an action, read focus_changed in the header. If a dialog or overlay took focus, deal with it before going on.

Options on acting tools: require (elements that must be on screen first), retry (0 to 3; repeats only reads, set_value and open_app, never a tap or key) and verify (an element expected afterwards; after a tap or long_press the screen must also have changed, and after type_text the element must still be there 150 and 600 ms later) are passed to the daemon, which enforces them. If one fails you get an error; do not assume the action worked.

Refs: each clickable line of a tree ends in a ref (r3fa91c) that stays the same for the same element after a scroll or re-layout, unlike its index. Prefer tap(ref=...) after anything moved. The tree header names a snapshot (s4): pass since="s4" to ui_tree to get only the added (+), changed (~) and removed (-) lines. Selectors also take {ref}.

Result size: level digest (default) gives a short text, default gives more, full gives everything. Ask for more only when the digest is not enough. pixel_probe and look's element/ocr are not implemented yet; set_value sets text fields only.

Device modes: full allows everything; helper allows only gestures, keys and typing; read_only answers reads only (look, ui_tree, list_apps, getprop, doctor, wait). A refusal is an error that quotes the daemon's reason. Do not retry it or look for a way round it. A device on the protected list answers only a fixed set of reads (a few getprop keys), whatever the mode.

doctor takes read-only commands from an allowlist (dumpsys <service> with no arguments for a few services, getprop, settings get, pm list and similar). Anything that changes state is refused.

Jobs: job_start runs a flow in the background and returns a run_id at once; then poll job_status, which adds the receipt once the job has ended. For a short fixed sequence pass steps (flow steps, no flow file) with app and mode sync: it stops at the first failure and returns a row per step. When a job ends stuck or done the server sends one phone push, and a result carries a line starting "SESSION ACTION": tell the user about that job yourself; the server never does.

No call runs longer than 110 seconds. If a call times out, look again before repeating the action: it may have happened.`;

export const toolNames = (): string[] => TOOLS.map((t) => t.name);
export const toolByName = (name: string): ToolDef | undefined => TOOLS.find((t) => t.name === name);

export const ERROR_KINDS = ["invalid_input", "unknown_tool", "refused", "lease_held", "not_holder", "unknown_device", "ambiguous_device", "device_error", "daemon_error", "timeout", "transport", "bad_reply", "bad_output"] as const;
export type ErrorKind = (typeof ERROR_KINDS)[number];

export const ERROR_SCHEMA: Schema = {
  type: "object",
  required: ["kind", "message"],
  properties: { kind: { enum: [...ERROR_KINDS] }, code: { type: ["integer", "null"] }, message: { type: "string" }, data: true },
};

/**
 * The output schema of a tool: {ok, data?, error?, header, receipt}. `detailed` swaps the one-word
 * placeholders for header, receipt and error for their full schemas; the server validates every
 * result against the detailed form, the list that goes to the model carries the short form.
 */
export function outputSchemaFor(def: ToolDef, detailed: boolean): Schema {
  if (!detailed) {
    // the short form sent to the model: the members that are always there, and this tool's data
    // (left out when it says no more than "an object", as for the job tools, to save tokens)
    const trivial = JSON.stringify(def.data) === '{"type":"object"}';
    return { type: "object", required: ["ok", "header", "receipt"], properties: trivial ? { ok: { type: "boolean" } } : { ok: { type: "boolean" }, data: def.data } };
  }
  return {
    type: "object",
    required: ["ok", "header", "receipt"],
    properties: { ok: { type: "boolean" }, data: def.data, error: ERROR_SCHEMA, header: HEADER_SCHEMA, receipt: RECEIPT_SCHEMA },
    additionalProperties: false,
  };
}

export interface WireTool {
  name: string;
  description: string;
  inputSchema: Schema;
  outputSchema: Schema;
  annotations?: { readOnlyHint: boolean };
}

export function wireTool(def: ToolDef, profile: Profile = "full"): WireTool {
  let input = def.input;
  if (profile === "minimal" && def.minimalOmits?.length) {
    const obj = input as { properties: Record<string, Schema> };
    const props = { ...obj.properties };
    for (const k of def.minimalOmits) delete props[k];
    input = { ...obj, properties: props };
  }
  const w: WireTool = { name: def.name, description: def.description, inputSchema: input, outputSchema: outputSchemaFor(def, false) };
  if (def.read) w.annotations = { readOnlyHint: true };
  return w;
}

/** The tool list for a profile, in the fixed order. */
export function listTools(profile: Profile = "full"): WireTool[] {
  return TOOLS.filter((t) => profile === "full" || t.minimal).map((t) => wireTool(t, profile));
}
