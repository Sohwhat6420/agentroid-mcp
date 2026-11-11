// The receipt that rides on every tool call. Shape: schemas/receipt.json (a test keeps this copy
// identical to the file, so the server never reads the repo at run time).

import { describeIssues, validate } from "./schema.ts";
import type { Schema } from "./schema.ts";

export const RECEIPT_SCHEMA: Schema = {
  $schema: "https://json-schema.org/draft/2020-12/schema",
  $id: "https://agentroid.local/schemas/receipt.json",
  title: "Receipt card",
  type: "object",
  required: ["run_id", "device", "flow", "steps_planned", "steps_done", "model_calls", "tokens_in", "tokens_out", "pool", "wall_time_s", "phone_peak_c", "battery_change", "laptop_cpu_s", "pauses", "retries", "errors"],
  additionalProperties: false,
  properties: {
    run_id: { type: "string", minLength: 1 },
    device: { type: "string", minLength: 1 },
    flow: { type: "string" },
    steps_planned: { type: "integer", minimum: 0 },
    steps_done: { type: "integer", minimum: 0 },
    model_calls: { type: "integer", minimum: 0 },
    tokens_in: { type: "integer", minimum: 0 },
    tokens_out: { type: "integer", minimum: 0 },
    pool: { type: "object", required: ["name", "usd"], additionalProperties: false, properties: { name: { type: "string", minLength: 1 }, usd: { type: "number", minimum: 0 } } },
    wall_time_s: { type: "number", minimum: 0 },
    phone_peak_c: { type: ["number", "null"] },
    battery_change: { type: ["number", "null"] },
    laptop_cpu_s: { type: "number", minimum: 0 },
    pauses: { type: "object", required: ["heat", "human"], additionalProperties: false, properties: { heat: { type: "integer", minimum: 0 }, human: { type: "integer", minimum: 0 } } },
    retries: { type: "integer", minimum: 0 },
    errors: { type: "array", items: { type: "string" } },
  },
};

export interface Receipt {
  run_id: string;
  device: string;
  flow: string;
  steps_planned: number;
  steps_done: number;
  model_calls: number;
  tokens_in: number;
  tokens_out: number;
  pool: { name: string; usd: number };
  wall_time_s: number;
  phone_peak_c: number | null;
  battery_change: number | null;
  laptop_cpu_s: number;
  pauses: { heat: number; human: number };
  retries: number;
  errors: string[];
}

export interface ReceiptInput {
  runId: string;
  /** The device key, or "none" for a call that names no device. */
  device: string;
  /** The tool name: one call is one flow of one step. */
  tool: string;
  ok: boolean;
  wallTimeS: number;
  laptopCpuS: number;
  /** Pipe-busy reconnects plus anything else the daemon client retried for this call. */
  retries: number;
  /** Highest phone temperature the header showed for this call, when it showed one. */
  phonePeakC?: number | null;
  /** Battery percent after minus before, when both are known. */
  batteryChange?: number | null;
  errors: string[];
}

/**
 * One receipt per call. What the MCP server cannot know is reported as what it is: it makes no
 * model calls and spends nothing (0 tokens, pool "none", 0 USD). `phone_peak_c` and
 * `battery_change` are `null` when the header has no value (schema allows null since 2.4 daemon B); the header
 * carries the real unknown/age for those fields.
 */
export function buildReceipt(i: ReceiptInput): Receipt {
  const r: Receipt = {
    run_id: i.runId,
    device: i.device || "none",
    flow: i.tool,
    steps_planned: 1,
    steps_done: i.ok ? 1 : 0,
    model_calls: 0,
    tokens_in: 0,
    tokens_out: 0,
    pool: { name: "none", usd: 0 },
    wall_time_s: Math.max(0, i.wallTimeS),
    phone_peak_c: typeof i.phonePeakC === "number" && Number.isFinite(i.phonePeakC) ? i.phonePeakC : null,
    battery_change: typeof i.batteryChange === "number" && Number.isFinite(i.batteryChange) ? i.batteryChange : null,
    laptop_cpu_s: Math.max(0, i.laptopCpuS),
    pauses: { heat: 0, human: 0 },
    retries: Math.max(0, Math.trunc(i.retries)),
    errors: i.errors.map(String),
  };
  return r;
}

/** Returns the problems, empty when the receipt is valid. */
export function receiptProblems(r: unknown): string[] {
  const issues = validate(r, RECEIPT_SCHEMA);
  return issues.length === 0 ? [] : [describeIssues(issues, 5)];
}
