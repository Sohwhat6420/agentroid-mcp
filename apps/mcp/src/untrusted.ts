// Phone content is untrusted input to the model (OWASP LLM01:2025, prompt injection).
//
// Everything a tool result says that was read from a phone, or that describes one, can be chosen by
// whoever controls what the phone shows: a web page, a chat message, a notification, an app's window
// title, a file name, a system property. This module keeps that text apart from the server's own:
//
// - `fence` wraps it in <phone-data ID> ... </phone-data ID>. ID is random per result and is made
//   after the text was read, so text on the phone cannot contain the closing line that ends its own
//   block. The server instructions (tools.ts) tell the model that the block is data, never
//   instructions, and that the server's own notices count only outside it.
// - `neutralise` (used by `fence`) defuses the two things phone text could imitate inside the block:
//   the fence tag itself and a line starting with the server's notice marker.
// - `cleanField` makes one short value safe for the one-line health header: no line breaks or control
//   characters, bounded length.
//
// This marks and separates untrusted text; it does not make a model immune to it. The daemon's
// screen guard and device modes remain the control on what a misled model can do.

import { randomBytes } from "node:crypto";

/** The marker that starts a server notice line (supervise.ts `cardInstruction`). */
export const NOTICE_MARKER = "SESSION ACTION";
/** The fence tag name. */
export const FENCE_TAG = "phone-data";
/** What a forged notice line inside phone data is prefixed with. */
export const FORGED_PREFIX = "[phone text] ";

// C0 and C1 controls, DEL, line and paragraph separators, and the bidirectional controls that can
// reorder what a person or a model reads (U+202A to U+202E, U+2066 to U+2069).
const CONTROL = /[\u0000-\u001f\u007f-\u009f\u2028\u2029\u202a-\u202e\u2066-\u2069]/g;
const FORGED_TAG = /<(\/?)\s*phone-data/gi;
const FORGED_NOTICE = /^(\s*)(?=SESSION[ \t_-]*ACTION)/gim;

/** A fresh fence id: 12 hex characters (48 random bits) after a letter. */
export function fenceId(): string {
  return `k${randomBytes(6).toString("hex")}`;
}

/** Defuse a forged fence tag and a forged notice line. The text stays readable; nothing is dropped. */
export function neutralise(text: string): string {
  return text.replace(FORGED_TAG, "<$1phone_data").replace(FORGED_NOTICE, `$1${FORGED_PREFIX}`);
}

/** Wrap text read from, or describing, a phone. Empty text gives an empty string (no empty block). */
export function fence(text: string, id: string = fenceId()): string {
  if (text.length === 0) return "";
  return `<${FENCE_TAG} ${id}>\n${neutralise(text)}\n</${FENCE_TAG} ${id}>`;
}

/**
 * One value for the health header line: control characters and line breaks become spaces, runs of
 * white space collapse, and the result is cut to `max` characters. An empty result means "drop it".
 */
export function cleanField(value: string, max = 80): string {
  const s = value.replace(CONTROL, " ").replace(/\s+/g, " ").trim();
  const cut = [...s].slice(0, max).join("");
  return neutralise(cut);
}
