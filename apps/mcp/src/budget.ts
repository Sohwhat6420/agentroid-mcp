// The size budget of the tool list and the server instructions.
// CI measures tokens with the free count_tokens endpoint (scripts/count-tokens-ci.ts). Offline we
// use a deliberately pessimistic estimate: characters / 3.2, rounded up. JSON with short keys
// usually runs 3.5 to 4 characters per token, so the estimate overstates and a pass here is safe.

import { INSTRUCTIONS, MAX_TOOLS, listTools } from "./tools.ts";
import type { Profile } from "./tools.ts";

export const BUDGET = {
  fullTokens: 5000,
  minimalTokens: 1500,
  instructionsChars: 4096,
  maxTools: MAX_TOOLS,
  charsPerToken: 3.2,
} as const;

export function estimateTokens(text: string): number {
  return Math.ceil(text.length / BUDGET.charsPerToken);
}

/** The serialized list exactly as the client receives it (compact JSON of the `tools` array). */
export function serializedToolList(profile: Profile): string {
  return JSON.stringify(listTools(profile));
}

export interface BudgetReport {
  fullTokens: number;
  minimalTokens: number;
  fullTools: number;
  minimalTools: number;
  instructionsChars: number;
  problems: string[];
}

export function measure(): BudgetReport {
  const full = serializedToolList("full");
  const minimal = serializedToolList("minimal");
  const r: BudgetReport = {
    fullTokens: estimateTokens(full),
    minimalTokens: estimateTokens(minimal),
    fullTools: listTools("full").length,
    minimalTools: listTools("minimal").length,
    instructionsChars: INSTRUCTIONS.length,
    problems: [],
  };
  if (r.fullTools > BUDGET.maxTools) r.problems.push(`${r.fullTools} tools, limit ${BUDGET.maxTools}`);
  if (r.fullTokens > BUDGET.fullTokens) r.problems.push(`full list ~${r.fullTokens} tokens, limit ${BUDGET.fullTokens}`);
  if (r.minimalTokens > BUDGET.minimalTokens) r.problems.push(`minimal list ~${r.minimalTokens} tokens, limit ${BUDGET.minimalTokens}`);
  if (r.instructionsChars >= BUDGET.instructionsChars) r.problems.push(`instructions ${r.instructionsChars} chars, must be under ${BUDGET.instructionsChars}`);
  return r;
}
