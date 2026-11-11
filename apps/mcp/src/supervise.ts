// Supervision pushes (task 2.11a). By design: "Only when a session is open". The MCP
// server process exists only while a Claude Code session is open, so this is where pushes come
// from; there is no session-free path (no daemon-side poster, nothing scheduled outside this
// process).
//
// When the server sees a job become `stuck` or `done` it posts ONE ntfy push (the same topic file
// and request shape as drafts/connectivity/adb_watchdog.py `ntfy_post`: POST https://ntfy.sh/<topic>,
// the message as the body, a Title header) and queues a short "tell the user about the job"
// instruction for the session. The session posts the card; this code never does.
//
// Rules:
// - 0 pushes for routine progress: only `stuck` and `done` push (not running, paused, stopped, error);
// - at most one push per job per state, however often the state is seen;
// - the push text carries no screen content: it is built only from the run id, the flow's name and
//   the step counts, never from the status line, the last error, a step id or anything read off the
//   phone.
//
// The real poster (`ntfyPost`) is wired only by the entry points (sdk-wiring.ts, http-main.ts).
// `createServer` defaults to `noPoster`, so a test that forgets to inject a fake can never reach
// ntfy.sh.

import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

/** The watchdog's rule for a topic: 16 to 64 of A-Z a-z 0-9 _ -. */
export const TOPIC_RE = /^[A-Za-z0-9_-]{16,64}$/;

/** `%USERPROFILE%\.agentroid\ntfy-topic.txt`, shared with the adb watchdog. */
export function defaultTopicFile(): string {
  return join(process.env.USERPROFILE ?? homedir(), ".agentroid", "ntfy-topic.txt");
}

/** The topic in `file`, or null when the file is missing or does not hold a valid topic (pushes off). */
export function readTopic(file: string): string | null {
  try {
    const t = readFileSync(file, "utf8").trim();
    return TOPIC_RE.test(t) ? t : null;
  } catch {
    return null;
  }
}

/** Posts one push. Resolves to null on success, else the error text. Never rejects. */
export type Poster = (topic: string, title: string, message: string) => Promise<string | null>;

/** The real one: POST https://ntfy.sh/<topic>, body = message, header Title (adb_watchdog.py's shape). */
export const ntfyPost: Poster = async (topic, title, message) => {
  try {
    const r = await fetch(`https://ntfy.sh/${encodeURIComponent(topic)}`, { method: "POST", body: message, headers: { Title: title }, signal: AbortSignal.timeout(10_000) });
    return r.ok ? null : `HTTP ${r.status}`;
  } catch (e) {
    return e instanceof Error ? `${e.name}: ${e.message}` : String(e);
  }
};

/** The default: posts nothing. */
export const noPoster: Poster = async () => "pushes are off in this server";

export const PUSH_STATES = ["stuck", "done"] as const;
export type PushState = (typeof PUSH_STATES)[number];
const isPushState = (s: unknown): s is PushState => (PUSH_STATES as readonly unknown[]).includes(s);

/** What the server knows of a job (the daemon's job.status result, or part of it). */
export interface JobSnapshot {
  run_id: string;
  status: string;
  flow?: unknown;
  steps_done?: unknown;
  steps_planned?: unknown;
}

/** One state change worth telling the session about. */
export interface Notice {
  run_id: string;
  state: PushState;
  /** What happened to the phone push. */
  push: string;
  /** The instruction for the session (the card is the session's to post). */
  card: string;
}

/** Ids and names only: letters, digits, space and . _ : - survive, at most `max` characters. */
const clean = (s: unknown, max: number): string => String(s ?? "").replace(/[^A-Za-z0-9 ._:-]/g, "").slice(0, max) || "?";

function counts(j: JobSnapshot): string {
  return typeof j.steps_done === "number" && typeof j.steps_planned === "number" ? `${j.steps_done} of ${j.steps_planned} steps` : "step count unknown";
}

/** The push: run id, flow name and counts only. */
export function pushText(j: JobSnapshot, state: PushState): { title: string; message: string } {
  const id = clean(j.run_id, 64);
  const flow = clean(j.flow, 60);
  return state === "stuck"
    ? { title: "Agentroid: job stuck", message: `Job ${id} (${flow}) is stuck after ${counts(j)}. Open the Claude Code session to look.` }
    : { title: "Agentroid: job done", message: `Job ${id} (${flow}) is done: ${counts(j)}.` };
}

/** The instruction for the session. */
export function cardInstruction(j: JobSnapshot, state: PushState, push: string): string {
  const id = clean(j.run_id, 64);
  const where = state === "stuck" ? `; the flight recorder (last 10 trees, 3 frames) is in runs/${id}/flight/` : "";
  return `SESSION ACTION: tell the user about job ${id} (${clean(j.flow, 60)}): ${state}, ${counts(j)}. job_status carries the receipt${where}. Phone push: ${push}. The server never posts cards itself.`;
}

export interface SupervisorOptions {
  /** Default `noPoster`. */
  poster?: Poster;
  /** The ntfy topic, read at push time (default: none, so no push). */
  topic?: () => string | null;
  /** Diagnostics (stderr in the entry points). Never gets the topic. */
  log?: (line: string) => void;
}

export class Supervisor {
  /** Every push handed to the poster, in order (title and message; the topic is not kept). */
  readonly pushes: { run_id: string; state: PushState; title: string; message: string }[] = [];
  private readonly poster: Poster;
  private readonly topic: () => string | null;
  private readonly log: (line: string) => void;
  private readonly seen = new Set<string>();
  private pending: Notice[] = [];
  private readonly inflight = new Set<Promise<void>>();

  constructor(opts: SupervisorOptions = {}) {
    this.poster = opts.poster ?? noPoster;
    this.topic = opts.topic ?? ((): null => null);
    this.log = opts.log ?? ((): void => {});
  }

  /** Mark a job state as already seen, with no push and no notice: a run whose end the caller was handed in the same call. */
  acknowledge(runId: string, state: PushState): void {
    this.seen.add(`${runId}\u0000${state}`);
  }

  /**
   * Look at one job state. `stuck` and `done` push once per job per state and queue a notice for
   * the session; every other state (all routine progress) does nothing.
   */
  observe(j: JobSnapshot): Notice | null {
    if (!isPushState(j.status) || typeof j.run_id !== "string") return null;
    const state = j.status;
    const key = `${j.run_id}\u0000${state}`;
    if (this.seen.has(key)) return null;
    this.seen.add(key);
    let push: string;
    const topic = this.topic();
    if (topic === null || !TOPIC_RE.test(topic)) {
      push = "not sent (no ntfy topic file)";
    } else {
      const { title, message } = pushText(j, state);
      this.pushes.push({ run_id: j.run_id, state, title, message });
      push = "sent to the phone";
      const p: Promise<void> = this.poster(topic, title, message)
        .then((err) => {
          // the topic is the only secret here: an error text that echoes the URL must not leak it
          if (err !== null) this.log(`push for ${clean(j.run_id, 64)} (${state}) failed: ${err.split(topic).join("<topic>")}`);
        })
        .catch(() => {})
        .finally(() => {
          this.inflight.delete(p);
        });
      this.inflight.add(p);
    }
    const n: Notice = { run_id: j.run_id, state, push, card: cardInstruction(j, state, push) };
    this.pending.push(n);
    return n;
  }

  /** The notices not yet shown to the session; each is returned once. */
  drain(): Notice[] {
    const out = this.pending;
    this.pending = [];
    return out;
  }

  /** Resolves when every push handed to the poster has finished (tests, shutdown). */
  async idle(): Promise<void> {
    await Promise.all([...this.inflight]);
  }
}
