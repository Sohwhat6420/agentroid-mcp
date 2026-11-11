// Edge validation: every payload from bridged (or from a fixture) passes through here before the
// reducer may use it. Nothing downstream of this file sees an unchecked value.

import { compile, describe, type Issue, type Schema } from './schema.js';
import type { FrameMeta, Job, ProgressEvent, Recorder } from './types.js';

export type Checked<T> = { ok: true; value: T } | { ok: false; issues: Issue[]; why: string };

export interface SchemaSet {
  jobView: Schema;
  progress: Schema;
  frameMeta: Schema;
  recorder: Schema;
}

export interface Validators {
  job(v: unknown): Checked<Job>;
  jobList(v: unknown): Checked<Job[]>;
  event(v: unknown): Checked<ProgressEvent>;
  frameMeta(v: unknown): Checked<FrameMeta>;
  recorder(v: unknown): Checked<Recorder>;
}

function wrap<T>(check: (v: unknown) => Issue[], extra?: (v: unknown) => Issue[]) {
  return (v: unknown): Checked<T> => {
    const issues = check(v);
    if (!issues.length && extra) issues.push(...extra(v));
    return issues.length ? { ok: false, issues, why: describe(issues) } : { ok: true, value: v as T };
  };
}

export function makeValidators(s: SchemaSet): Validators {
  const jobView = compile(s.jobView);
  const progress = compile(s.progress);
  const frameMeta = compile(s.frameMeta);
  const recorder = compile(s.recorder);
  const notArray = (want: 'object' | 'array') => (v: unknown): Issue[] =>
    (want === 'object') === Array.isArray(v) ? [{ path: '', msg: want === 'object' ? 'expected one job, got a list' : 'expected a list of jobs, got one job' }] : [];
  return {
    // job-view.json is oneOf(job, [job]); the view asks for one or the other, so it also checks which it got
    job: (v) => { const pre = notArray('object')(v); return pre.length ? { ok: false, issues: pre, why: describe(pre) } : wrap<Job>(jobView)(v); },
    jobList: (v) => { const pre = notArray('array')(v); return pre.length ? { ok: false, issues: pre, why: describe(pre) } : wrap<Job[]>(jobView)(v); },
    event: wrap<ProgressEvent>(progress),
    // the schema caps bytes at 61,440; also refuse a frame whose picture claims to exist while the screen is secure
    frameMeta: wrap<FrameMeta>(frameMeta, (v) => {
      const f = v as FrameMeta;
      return f.secure && f.bytes > 0 ? [{ path: '/bytes', msg: 'must be 0 when secure is true (no picture of a secure screen)' }] : [];
    }),
    recorder: wrap<Recorder>(recorder),
  };
}

export interface NdjsonResult {
  events: ProgressEvent[];
  rejected: { line: number; text: string; why: string }[];
}

/** Parse NDJSON text (one progress event per line). Blank lines are ignored; bad lines are kept as rejects. */
export function parseNdjson(text: string, v: Validators): NdjsonResult {
  const out: NdjsonResult = { events: [], rejected: [] };
  text.split(/\r?\n/).forEach((raw, i) => {
    const line = raw.trim();
    if (!line) return;
    let obj: unknown;
    try { obj = JSON.parse(line); } catch { out.rejected.push({ line: i + 1, text: line, why: 'not valid JSON' }); return; }
    const c = v.event(obj);
    if (c.ok) out.events.push(c.value); else out.rejected.push({ line: i + 1, text: line, why: c.why });
  });
  return out;
}
