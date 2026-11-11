// The view's single state store. Pure: (state, action, validators) -> state. Every payload is
// validated here, at the edge; a payload that fails is counted and described, never used.

import type { FrameMeta, Job, ProgressEvent, Recorder } from '../contract/types.js';
import { parseNdjson, type Validators } from '../contract/validate.js';

export type LinkMode = 'connecting' | 'stream' | 'poll' | 'fixture' | 'down';

export interface Problem { at: number; why: string; kind: 'invalid' | 'unreachable' }
export interface Received<T> { value: T; at: number }

export interface ViewState {
  job: Received<Job> | null;
  jobProblem: Problem | null;
  others: Job[];
  events: ProgressEvent[];
  /** the accepted NDJSON lines, newest last (bounded), for the raw view */
  raw: string[];
  rejected: { at: number; why: string; text: string }[];
  rejectedCount: number;
  /** events whose run_id was not this job's run */
  foreign: number;
  frame: Received<FrameMeta> | null;
  frameProblem: Problem | null;
  recorder: Received<Recorder> | null;
  link: { mode: LinkMode; since: number; lastOk: number | null; why: string | null };
  /** client ms minus server ms, from the HTTP Date header of the last good job fetch (0 in fixture mode) */
  skew: number;
}

export type Action =
  | { type: 'job'; at: number; payload: unknown; serverDate?: number }
  | { type: 'job-failed'; at: number; why: string }
  | { type: 'jobs'; at: number; payload: unknown }
  | { type: 'events'; at: number; text: string }
  | { type: 'frame'; at: number; payload: unknown }
  | { type: 'frame-failed'; at: number; why: string }
  | { type: 'recorder'; at: number; payload: unknown | null }
  | { type: 'link'; at: number; mode: LinkMode; why?: string };

export const RAW_KEEP = 400;
export const REJECT_KEEP = 20;

export function initialState(at: number): ViewState {
  return {
    job: null, jobProblem: null, others: [], events: [], raw: [], rejected: [], rejectedCount: 0, foreign: 0,
    frame: null, frameProblem: null, recorder: null,
    link: { mode: 'connecting', since: at, lastOk: null, why: null }, skew: 0,
  };
}

function ok(s: ViewState, at: number): ViewState['link'] {
  return { ...s.link, lastOk: at, why: s.link.mode === 'down' ? null : s.link.why };
}

/** Insert events by seq; a seq seen before is dropped (a reconnect replays from Last-Event-ID). */
export function mergeEvents(have: ProgressEvent[], incoming: ProgressEvent[]): { events: ProgressEvent[]; added: ProgressEvent[] } {
  const seen = new Set(have.map(e => e.seq));
  const added: ProgressEvent[] = [];
  for (const e of incoming) if (!seen.has(e.seq)) { seen.add(e.seq); added.push(e); }
  if (!added.length) return { events: have, added };
  // sort unless the batch is itself in order and starts after what we have (the common, cheap case)
  const sortedTail = (have.length === 0 || added[0]!.seq > have[have.length - 1]!.seq) && added.every((e, i) => i === 0 || e.seq > added[i - 1]!.seq);
  const events = have.concat(added);
  if (!sortedTail) events.sort((a, b) => a.seq - b.seq);
  return { events, added };
}

export function reduce(s: ViewState, a: Action, v: Validators): ViewState {
  switch (a.type) {
    case 'job': {
      const c = v.job(a.payload);
      if (!c.ok) return { ...s, jobProblem: { at: a.at, why: `job data did not match job-view.json: ${c.why}`, kind: 'invalid' }, link: ok(s, a.at) };
      const runChanged = s.job && s.job.value.run_id !== c.value.run_id;
      const events = runChanged ? [] : s.events.filter(e => e.run_id === c.value.run_id);
      return {
        ...s, job: { value: c.value, at: a.at }, jobProblem: null, link: ok(s, a.at),
        events, raw: runChanged ? [] : s.raw, foreign: s.foreign + (runChanged ? 0 : s.events.length - events.length),
        skew: a.serverDate != null && Number.isFinite(a.serverDate) ? a.at - a.serverDate : s.skew,
        ...(runChanged ? { frame: null, recorder: null } : {}),
      };
    }
    case 'job-failed':
      return { ...s, jobProblem: { at: a.at, why: a.why, kind: 'unreachable' } };
    case 'jobs': {
      const c = v.jobList(a.payload);
      if (!c.ok) return s; // the other-jobs list is a nicety; a bad list is ignored, never half-used
      return { ...s, others: c.value.filter(j => j.job_id !== s.job?.value.job_id) };
    }
    case 'events': {
      const parsed = parseNdjson(a.text, v);
      const run = s.job?.value.run_id;
      const mine = run ? parsed.events.filter(e => e.run_id === run) : parsed.events;
      const foreign = parsed.events.length - mine.length;
      const { events, added } = mergeEvents(s.events, mine);
      const addedSeqs = new Set(added.map(e => e.seq));
      // keep the raw text of exactly the lines that were accepted
      const rawAdded: string[] = [];
      for (const line of a.text.split(/\r?\n/)) {
        const t = line.trim(); if (!t) continue;
        try { const o = JSON.parse(t) as { seq?: number }; if (typeof o.seq === 'number' && addedSeqs.has(o.seq)) { rawAdded.push(t); addedSeqs.delete(o.seq); } } catch { /* rejected above */ }
      }
      const rejected = parsed.rejected.length
        ? s.rejected.concat(parsed.rejected.map(r => ({ at: a.at, why: r.why, text: r.text }))).slice(-REJECT_KEEP)
        : s.rejected;
      return {
        ...s, events, raw: rawAdded.length ? s.raw.concat(rawAdded).slice(-RAW_KEEP) : s.raw,
        rejected, rejectedCount: s.rejectedCount + parsed.rejected.length, foreign: s.foreign + foreign,
        link: ok(s, a.at),
      };
    }
    case 'frame': {
      const c = v.frameMeta(a.payload);
      if (!c.ok) return { ...s, frameProblem: { at: a.at, why: `frame data did not match frame-meta: ${c.why}`, kind: 'invalid' } };
      if (s.job && c.value.run_id !== s.job.value.run_id) return { ...s, frameProblem: { at: a.at, why: 'frame belongs to another run', kind: 'invalid' } };
      return { ...s, frame: { value: c.value, at: a.at }, frameProblem: null };
    }
    case 'frame-failed':
      return { ...s, frameProblem: { at: a.at, why: a.why, kind: 'unreachable' } };
    case 'recorder': {
      if (a.payload == null) return { ...s, recorder: null };
      const c = v.recorder(a.payload);
      return c.ok ? { ...s, recorder: { value: c.value, at: a.at } } : s;
    }
    case 'link':
      if (s.link.mode === a.mode && s.link.why === (a.why ?? null)) return s;
      return { ...s, link: { mode: a.mode, since: a.at, lastOk: s.link.lastOk, why: a.why ?? null } };
  }
}
