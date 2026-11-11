// CI STUB: measures the tool list with Anthropic's free count_tokens endpoint.
// Not run offline (needs network and a key). Written so CI can run it as
//   ANTHROPIC_API_KEY=... node --experimental-strip-types scripts/count-tokens-ci.ts
//   node --experimental-strip-types scripts/count-tokens-ci.ts --emit-bodies <dir>
// writes the five request bodies (baseline.json, full-plain.json, minimal-plain.json, full-worst.json,
// minimal-worst.json) and exits 0 without a key or the network: a dispatcher that holds the key
// POSTs each file to /v1/messages/count_tokens, and the figures are the same as this script's
// (full-plain minus baseline, and so on). No key ever enters this repo.
// Exit 0 when full <= 5000 and minimal <= 1500 tokens, 1 when over, 2 when the call itself failed.
//
// Two things to settle the first time it runs (not verified offline):
// 1. COUNT_TOKENS_MODEL: the model the CI should measure against (the count differs by tokenizer).
// 2. The API adds its own fixed system prompt when `tools` is present. This script subtracts a
//    baseline call with one tiny tool, so the figure is the list's own size (the number to compare
//    with the budget). If CI prefers the raw figure, set COUNT_TOKENS_RAW=1.

import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { BUDGET } from "../src/budget.ts";
import { INSTRUCTIONS, listTools } from "../src/tools.ts";
import type { Profile } from "../src/tools.ts";

const URL = "https://api.anthropic.com/v1/messages/count_tokens";
const MODEL = process.env.COUNT_TOKENS_MODEL ?? "claude-sonnet-4-5";
const KEY = process.env.ANTHROPIC_API_KEY;

interface ApiTool {
  name: string;
  description: string;
  input_schema: unknown;
}

/** The request body for one count_tokens call. */
export function requestBody(tools: ApiTool[], model: string = MODEL): { model: string; messages: { role: string; content: string }[]; tools: ApiTool[] } {
  return { model, messages: [{ role: "user", content: "x" }], tools };
}

const BASELINE_TOOLS: ApiTool[] = [{ name: "t", description: "t", input_schema: { type: "object" } }];

async function count(tools: ApiTool[]): Promise<number> {
  const res = await fetch(URL, {
    method: "POST",
    headers: { "content-type": "application/json", "x-api-key": KEY ?? "", "anthropic-version": "2023-06-01" },
    body: JSON.stringify(requestBody(tools)),
  });
  if (!res.ok) throw new Error(`count_tokens ${res.status}: ${await res.text()}`);
  const body = (await res.json()) as { input_tokens?: unknown };
  if (typeof body.input_tokens !== "number") throw new Error("count_tokens reply has no input_tokens");
  return body.input_tokens;
}

// The API accepts only name, description and input_schema for a tool. Whether a client forwards an MCP
// outputSchema to the model is not settled, so the worst case is measured: the output schema appended to
// the description as text. The budget is checked against that worst case; the plain figure is printed too.
const toApi = (profile: Profile, withOutput: boolean): ApiTool[] =>
  listTools(profile).map((t) => ({ name: t.name, description: withOutput ? `${t.description}
Output: ${JSON.stringify(t.outputSchema)}` : t.description, input_schema: t.inputSchema }));

/** Write the five request bodies to `dir`; no network. Returns the file names written. */
export function emitBodies(dir: string): string[] {
  const out = resolve(dir);
  mkdirSync(out, { recursive: true });
  const bodies: Record<string, ApiTool[]> = {
    "baseline.json": BASELINE_TOOLS,
    "full-plain.json": toApi("full", false),
    "minimal-plain.json": toApi("minimal", false),
    "full-worst.json": toApi("full", true),
    "minimal-worst.json": toApi("minimal", true),
  };
  for (const [name, tools] of Object.entries(bodies)) writeFileSync(join(out, name), JSON.stringify(requestBody(tools)));
  return Object.keys(bodies);
}

async function main(): Promise<number> {
  const at = process.argv.indexOf("--emit-bodies");
  if (at >= 0) {
    const dir = process.argv[at + 1];
    if (!dir || dir.startsWith("--")) {
      console.error("--emit-bodies needs a directory");
      return 2;
    }
    console.log(JSON.stringify({ model: MODEL, dir: resolve(dir), files: emitBodies(dir) }));
    return 0;
  }
  if (!KEY) {
    console.error("ANTHROPIC_API_KEY is not set");
    return 2;
  }
  const raw = process.env.COUNT_TOKENS_RAW === "1";
  const baseline = raw ? 0 : await count(BASELINE_TOOLS);
  const fullPlain = (await count(toApi("full", false))) - baseline;
  const minimalPlain = (await count(toApi("minimal", false))) - baseline;
  const full = (await count(toApi("full", true))) - baseline;
  const minimal = (await count(toApi("minimal", true))) - baseline;
  console.log(JSON.stringify({ model: MODEL, baseline, fullPlain, minimalPlain, full, minimal, limits: { full: BUDGET.fullTokens, minimal: BUDGET.minimalTokens }, instructionsChars: INSTRUCTIONS.length, tools: listTools("full").length }));
  const problems: string[] = [];
  if (full > BUDGET.fullTokens) problems.push(`full list ${full} tokens > ${BUDGET.fullTokens}`);
  if (minimal > BUDGET.minimalTokens) problems.push(`minimal list ${minimal} tokens > ${BUDGET.minimalTokens}`);
  if (INSTRUCTIONS.length >= BUDGET.instructionsChars) problems.push(`instructions ${INSTRUCTIONS.length} chars >= ${BUDGET.instructionsChars}`);
  if (listTools("full").length > BUDGET.maxTools) problems.push(`more than ${BUDGET.maxTools} tools`);
  for (const p of problems) console.error(`BUDGET: ${p}`);
  return problems.length === 0 ? 0 : 1;
}

main().then(
  (code) => process.exit(code),
  (e: unknown) => {
    console.error(String(e));
    process.exit(2);
  },
);
