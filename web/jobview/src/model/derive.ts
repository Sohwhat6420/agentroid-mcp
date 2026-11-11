// Everything the page shows is computed here from validated data and a clock. Pure and DOM-free,
// so the unit tests can drive it with the repo fixtures.

import type { ChangeInForce, FrameMeta, Job, ProgressEvent } from '../contract/types.js';
import type { ViewState } from './reducer.js';

/** One set of thresholds everywhere. */
export const LIMITS = {
  pauseAt: 42, resumeAt: 37, staleS: 60, frameEveryS: 30, frameMaxBytes: 61440, batteryFloor: 12, chargeCap: 80,
  /** hysteresis for the "near the limit" sentence (DESIGN §4): on at 40.5 °C, off at 40.0 °C */
  nearOn: 40.5, nearOff: 40.0,
  /** rising / falling words: on past 0.08 °C/min, off under 0.03 */
  trendOn: 0.08, trendOff: 0.03,
} as const;

export type StateName =
  | 'connecting' | 'unavailable' | 'queued' | 'running' | 'cooling' | 'waiting' | 'stuck'
  | 'paused' | 'held' | 'taken' | 'stopped' | 'failed' | 'done';
export const TERMINAL: readonly StateName[] = ['stopped', 'failed', 'done'];
export const HELD: readonly StateName[] = ['waiting', 'stuck', 'paused', 'held', 'taken'];

export type PauseKind = 'heat' | 'needs' | 'user' | 'taken' | 'stuck' | 'other';

export interface Sample { t: number; v: number }
export interface Pause {
  start: number; end: number | null; kind: PauseKind; msg: string; stepId?: string;
  fromTemp: number | null; afterStep: number;
}
export interface StepRow {
  n: number; stepId: string; msg: string; t: number; dur: number | null; tries: number; notes: string[];
}
export interface HeatState { dir: 'rising' | 'falling' | 'steady'; near: boolean }
export const HEAT0: HeatState = { dir: 'steady', near: false };

export type Finish =
  | { kind: 'estimate'; t: number; basis: 'measured' | 'engine'; pauses: number; pauseEachS: number | null; includesOpenPause: boolean }
  | { kind: 'hold'; leftS: number | null }
  | { kind: 'ended'; t: number }
  | { kind: 'unknown' };

export interface Reading { v: number; t: number; age: number }

export interface Derived {
  state: StateName;
  now: number;
  job: Job | null;
  planned: number | null;
  done: number;
  retried: number;
  start: number | null;
  end: number | null;
  lastEventAt: number | null;
  temp: Reading | null;
  batt: Reading | null;
  samples: Sample[];
  batts: Sample[];
  /** °C per minute over the last ~90 s; null when there are too few readings */
  trend: number | null;
  heat: HeatState;
  readingsStale: boolean;
  pauses: Pause[];
  openPause: Pause | null;
  cooling: { etaS: number | null; ratePerMin: number | null; measured: boolean } | null;
  finish: Finish;
  steps: StepRow[];
  /** the step in flight (1-based number) and its id when known */
  live: { n: number; stepId: string | null; label: string | null } | null;
  lastError: { msg: string; t: number; stepId?: string } | null;
  totals: { work: number; cool: number; wait: number; total: number };
  terminalEvent: ProgressEvent | null;
  stuckEvent: ProgressEvent | null;
  frame: { meta: FrameMeta; age: number; stale: boolean } | null;
  changes: ChangeInForce[];
  problem: string | null;
  linkDown: boolean;
}

const tms = (ts: string) => Date.parse(ts);

export function classifyPause(msg = ''): PauseKind {
  const m = msg.trim().toLowerCase();
  if (/^heat\b/.test(m)) return 'heat';
  if (/^(user|paused)\b/.test(m) || m.includes('paused_by_user')) return 'user';
  if (/^(needs_attention|awaiting_human|needs_you)\b/.test(m)) return 'needs';
  return 'other';
}

/** Linear-regression slope in °C per minute over samples in [from, to]; needs 3 readings across 45 s. */
export function slopePerMin(samples: Sample[], from: number, to: number): number | null {
  const xs = samples.filter(s => s.t >= from && s.t <= to);
  if (xs.length < 3 || xs[xs.length - 1]!.t - xs[0]!.t < 45_000) return null;
  const n = xs.length, mx = xs.reduce((a, s) => a + s.t, 0) / n, my = xs.reduce((a, s) => a + s.v, 0) / n;
  let num = 0, den = 0;
  for (const s of xs) { num += (s.t - mx) * (s.v - my); den += (s.t - mx) ** 2; }
  return den === 0 ? null : (num / den) * 60_000;
}

/** Hysteresis (shapeshift): a word turns on at one threshold and off at another, so noise cannot make it flicker. */
export function nextHeat(prev: HeatState, trend: number | null, v: number | null): HeatState {
  let dir = prev.dir;
  const tr = trend ?? 0;
  if (dir === 'rising' ? tr < LIMITS.trendOff : dir === 'falling' ? tr > -LIMITS.trendOff : false) dir = 'steady';
  if (dir === 'steady') { if (tr > LIMITS.trendOn) dir = 'rising'; else if (tr < -LIMITS.trendOn) dir = 'falling'; }
  let near = prev.near;
  if (v != null) { if (!near && v >= LIMITS.nearOn) near = true; else if (near && v <= LIMITS.nearOff) near = false; }
  return dir === prev.dir && near === prev.near ? prev : { dir, near };
}

const median = (xs: number[]) => { if (!xs.length) return null; const s = [...xs].sort((a, b) => a - b); return s[Math.floor(s.length / 2)]!; };

function overlap(a0: number, a1: number, b0: number, b1: number) { return Math.max(0, Math.min(a1, b1) - Math.max(a0, b0)); }

export function derive(s: ViewState, clientNow: number, prevHeat: HeatState = HEAT0): Derived {
  const now = clientNow - s.skew;
  const job = s.job?.value ?? null;
  const ev = s.events;
  const samples: Sample[] = [], batts: Sample[] = [];
  const pauses: Pause[] = [];
  const steps: StepRow[] = [];
  const byId = new Map<string, StepRow>();
  const pendingNotes = new Map<string, { notes: string[]; tries: number }>();
  let open: Pause | null = null, lastError: Derived['lastError'] = null, terminal: ProgressEvent | null = null, stuck: ProgressEvent | null = null;
  let boundary: number | null = null; // where the next step's time starts counting
  let lastTemp: number | null = null;
  const start = ev.length ? tms(ev[0]!.ts) : null;
  const closeOpen = (t: number) => { if (open) { open.end = t; open = null; } };

  for (const e of ev) {
    const t = tms(e.ts);
    if (typeof e.temp_c === 'number') { samples.push({ t, v: e.temp_c }); lastTemp = e.temp_c; }
    if (typeof e.batt === 'number') batts.push({ t, v: e.batt });
    if (boundary == null) boundary = t;
    switch (e.kind) {
      case 'pause': {
        closeOpen(t);
        open = { start: t, end: null, kind: classifyPause(e.msg), msg: e.msg ?? '', fromTemp: e.temp_c ?? lastTemp, afterStep: steps.length, ...(e.step_id ? { stepId: e.step_id } : {}) };
        pauses.push(open); break;
      }
      case 'taken_over':
        closeOpen(t);
        open = { start: t, end: null, kind: 'taken', msg: e.msg ?? '', fromTemp: lastTemp, afterStep: steps.length, ...(e.step_id ? { stepId: e.step_id } : {}) };
        pauses.push(open); break;
      case 'stuck':
        closeOpen(t); stuck = e;
        open = { start: t, end: null, kind: 'stuck', msg: e.msg ?? '', fromTemp: lastTemp, afterStep: steps.length, ...(e.step_id ? { stepId: e.step_id } : {}) };
        pauses.push(open); break;
      case 'resume': closeOpen(t); stuck = null; break;
      case 'error': lastError = { msg: e.msg ?? 'error (no message)', t, ...(e.step_id ? { stepId: e.step_id } : {}) }; if (job?.status === 'error') { closeOpen(t); terminal = e; } break;
      case 'done': closeOpen(t); terminal = e; break;
      case 'progress': {
        if (e.step_id && e.msg) {
          const p = pendingNotes.get(e.step_id) ?? { notes: [], tries: 1 };
          if (/^attempt \d+ of \d+ failed/i.test(e.msg)) p.tries++;
          p.notes.push(e.msg); pendingNotes.set(e.step_id, p);
        }
        break;
      }
      case 'step': {
        closeOpen(t); // a step after any hold means it was resumed, even if the resume line was lost
        stuck = null;
        const id = e.step_id ?? `#${steps.length + 1}`;
        const from = boundary ?? t;
        const paused = pauses.reduce((a, p) => a + overlap(from, t, p.start, p.end ?? t), 0);
        const notes = pendingNotes.get(id); pendingNotes.delete(id);
        const prev = byId.get(id);
        if (prev) { prev.t = t; prev.msg = e.msg ?? prev.msg; prev.dur = (t - from - paused) / 1000; prev.tries += (notes?.tries ?? 1) - 1; prev.notes.push(...(notes?.notes ?? [])); }
        else {
          const row: StepRow = { n: steps.length + 1, stepId: id, msg: e.msg ?? '', t, dur: (t - from - paused) / 1000, tries: notes?.tries ?? 1, notes: notes?.notes ?? [] };
          steps.push(row); byId.set(id, row);
        }
        boundary = t; break;
      }
    }
  }
  const openPause: Pause | null = open;

  // ---- state
  let state: StateName;
  const linkDown = s.link.mode === 'down';
  if (!job) state = s.jobProblem ? 'unavailable' : 'connecting';
  else switch (job.status) {
    case 'queued': state = 'queued'; break;
    case 'running': state = openPause?.kind === 'heat' ? 'cooling' : 'running'; break;
    case 'paused': {
      const k = openPause?.kind ?? 'other';
      state = k === 'heat' ? 'cooling' : k === 'needs' ? 'waiting' : k === 'user' ? 'paused' : k === 'taken' ? 'taken' : 'held'; break;
    }
    case 'taken_over': state = 'taken'; break;
    case 'stuck': state = 'stuck'; break;
    case 'stopped': state = 'stopped'; break;
    case 'done': state = 'done'; break;
    case 'error': state = 'failed'; break;
  }
  const ended = state === 'done' || state === 'stopped';
  const isTerminal = TERMINAL.includes(state);
  const end = isTerminal ? (terminal ? tms(terminal.ts) : ev.length ? tms(ev[ev.length - 1]!.ts) : null) : null;
  const clockEnd = end ?? now;

  // ---- readings
  const lastS = samples[samples.length - 1], lastB = batts[batts.length - 1];
  const temp: Reading | null = lastS ? { v: lastS.v, t: lastS.t, age: Math.max(0, (now - lastS.t) / 1000) } : null;
  const batt: Reading | null = lastB ? { v: lastB.v, t: lastB.t, age: Math.max(0, (now - lastB.t) / 1000) } : null;
  const readingsStale = !ended && (linkDown || (temp ? temp.age > LIMITS.staleS : false));
  const trend = lastS ? slopePerMin(samples, lastS.t - 90_000, lastS.t) : null;
  const heat = nextHeat(prevHeat, trend, temp?.v ?? null);

  // ---- totals (seconds)
  let cool = 0, wait = 0;
  for (const p of pauses) {
    const d = Math.max(0, ((p.end ?? clockEnd) - p.start) / 1000);
    if (p.kind === 'heat') cool += d; else wait += d;
  }
  const total = start != null ? Math.max(0, (clockEnd - start) / 1000) : 0;
  const totals = { work: Math.max(0, total - cool - wait), cool, wait, total };

  // ---- cooling ETA: measured from this pause's own readings, never a made-up rate
  let cooling: Derived['cooling'] = null;
  const coolRates: number[] = [];
  for (const p of pauses) if (p.kind === 'heat' && p.end != null && p.fromTemp != null && p.fromTemp > LIMITS.resumeAt) {
    coolRates.push((p.fromTemp - LIMITS.resumeAt) / ((p.end - p.start) / 60_000));
  }
  if (state === 'cooling' && openPause) {
    const slope = slopePerMin(samples, openPause.start, now);
    const measured = slope != null && slope < -0.01;
    const rate = measured ? -slope! : median(coolRates);
    const v = temp?.v ?? openPause.fromTemp;
    cooling = { ratePerMin: rate, measured, etaS: rate && v != null ? Math.max(0, ((v - LIMITS.resumeAt) / rate) * 60) : null };
  }

  // ---- finish estimate
  const planned = job?.steps_planned ?? null;
  const done = job?.steps_done ?? steps.length;
  const retried = steps.filter(r => r.tries > 1).length;
  let finish: Finish = { kind: 'unknown' };
  if (isTerminal && end != null) finish = { kind: 'ended', t: end };
  else if (planned != null && job) {
    const left = Math.max(0, planned - done);
    const durs = steps.map(r => r.dur).filter((d): d is number => d != null && d > 0);
    const per = durs.length >= 5 ? median(durs.slice(-40)) : null;
    let workS: number | null = per != null ? per * left : null;
    let basis: 'measured' | 'engine' = 'measured';
    if (workS == null) {
      const lastStep = [...ev].reverse().find(e => e.kind === 'step' && typeof e.eta_s === 'number');
      if (lastStep) { workS = Math.max(0, lastStep.eta_s! - (now - tms(lastStep.ts)) / 1000); basis = 'engine'; }
    }
    if (HELD.includes(state)) finish = { kind: 'hold', leftS: workS };
    else if (workS != null) {
      // project cooling pauses from this run's own heating rate while working (°C per working second)
      // measured over the current working stretch only (since the last pause ended), so the cooling
      // inside a pause never cancels out the heating around it; falls back to the first stretch
      let heatRate: number | null = null;
      const lastEnd = pauses.reduce((a, q) => (q.end != null && q.end <= now ? Math.max(a, q.end) : a), start ?? 0);
      const firstStart = pauses.length ? pauses[0]!.start : now;
      const slopeNow = slopePerMin(samples, lastEnd, now) ?? slopePerMin(samples, start ?? 0, firstStart);
      if (slopeNow != null && slopeNow > 0.01) heatRate = slopeNow / 60;
      let extraOpen = 0, T = temp?.v ?? LIMITS.resumeAt, n = 0;
      if (state === 'cooling' && cooling?.etaS != null) { extraOpen = cooling.etaS; T = LIMITS.resumeAt; }
      if (heatRate) { let rem = workS; while (rem > 0 && n < 50) { const toPause = (LIMITS.pauseAt - T) / heatRate; if (toPause >= rem) break; rem -= toPause; n++; T = LIMITS.resumeAt; } }
      const rate = median(coolRates) ?? (cooling?.measured ? cooling.ratePerMin : null);
      const each = rate ? ((LIMITS.pauseAt - LIMITS.resumeAt) / rate) * 60 : null;
      finish = { kind: 'estimate', t: now + (workS + extraOpen + (each ? n * each : 0)) * 1000, basis, pauses: n, pauseEachS: each, includesOpenPause: extraOpen > 0 };
    }
  }

  // ---- the step in flight
  const fm = s.frame?.value ?? null;
  let live: Derived['live'] = null;
  if (job && !ended) {
    const n = Math.min(planned ?? Infinity, done + 1);
    const stepId = stuck?.step_id ?? openPause?.stepId ?? fm?.next_tap?.step_id ?? null;
    const label = fm?.next_tap && fm.next_tap.step_id === stepId ? fm.next_tap.label ?? null : null;
    if (Number.isFinite(n)) live = { n, stepId, label };
  }

  // ---- frame
  let frame: Derived['frame'] = null;
  if (fm) {
    const age = Math.max(0, (now - tms(fm.ts)) / 1000);
    frame = { meta: fm, age, stale: !ended && (linkDown || age > 2 * LIMITS.frameEveryS) };
  }

  const problem = s.jobProblem ? s.jobProblem.why : linkDown ? s.link.why ?? 'the laptop is not answering' : null;

  return {
    state, now, job, planned, done, retried, start, end, lastEventAt: ev.length ? tms(ev[ev.length - 1]!.ts) : null,
    temp, batt, samples, batts, trend, heat, readingsStale, pauses, openPause, cooling, finish, steps, live, lastError, totals,
    terminalEvent: terminal, stuckEvent: stuck, frame, changes: job?.changes_in_force ?? [], problem, linkDown,
  };
}

/** Words for the engine's pause details (crates/bridged/src/runner/popups.rs PauseReason::detail). */
export const PAUSE_DETAIL: Record<string, string> = {
  unknown_overlay: 'a window it does not know is covering the app',
  permission_allow: 'a permission prompt is asking for Allow',
  auto_dismiss_off: 'a pop-up appeared and automatic dismissal is off for this job',
  rule_pause: 'a pop-up rule says to pause here',
  lookalike: 'a pop-up looks like a known one but is not',
  not_state_preserving: 'dismissing this pop-up could lose the app’s state',
  dismiss_failed: 'dismissing a pop-up did not work',
  awaiting_human: 'the flow asked for a person at this step',
  unreadable_screen: 'it cannot read the screen',
  unverified_intent: 'it could not confirm that the last action took effect',
  paused_by_user: 'you paused it',
};

/** "needs_attention: unknown_overlay at s3; lease kept" -> {code:'unknown_overlay', words, rest} */
export function explainPause(msg: string): { code: string | null; words: string | null; rest: string } {
  const m = /^\s*[a-z_]+:\s*([a-z_]+)(.*)$/i.exec(msg);
  if (!m) return { code: null, words: null, rest: msg };
  const code = m[1]!.toLowerCase();
  return { code, words: PAUSE_DETAIL[code] ?? null, rest: m[2]!.replace(/;\s*lease kept\s*$/i, '').trim() };
}
