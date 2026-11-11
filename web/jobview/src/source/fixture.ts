// Fixture mode (?fixture=<name>): the page reads public/fixtures/<name>/, which is laid out like the
// bridged endpoints, and every payload still goes through the same edge validation as live data.
// The fixture's manifest freezes "now"; the page clock then runs from it (or stays frozen with
// &freeze=1, for screenshots). Controls work locally by appending the event bridged would emit.

import type { Action, ViewState } from '../model/reducer.js';
import type { FrameMeta, Recorder } from '../contract/types.js';
import type { ControlAction, ControlResult, Source } from './types.js';

interface Manifest { name: string; label: string; job_id: string; now: string; link: 'fixture' | 'down'; files?: string[] }

export function fixtureSource(name: string, opts: { freeze: boolean }): Source {
  const base = `fixtures/${encodeURIComponent(name)}/`;
  let dispatch: (a: Action) => void = () => {};
  let getState: () => ViewState = () => { throw new Error('not started'); };
  let fixtureNow = 0, loadedAt = 0, stopped = false;
  const clientNow = () => (opts.freeze ? loadedAt : Date.now());

  const src: Source = {
    kind: 'fixture', jobId: '', label: `Fixture "${name}": recorded data, not live`,
    now: clientNow,
    start(d, g) {
      dispatch = d; getState = g;
      if (name === 'loading') return; // the first-paint skeleton, held for review
      void (async () => {
        try {
          const man = (await (await fetch(base + 'manifest.json')).json()) as Manifest;
          src.jobId = man.job_id; src.label = `Fixture "${man.label}": recorded data, not live`;
          fixtureNow = Date.parse(man.now); loadedAt = Date.now();
          const has = (f: string) => !Array.isArray(man.files) || man.files.includes(f);
          const text = async (f: string) => { if (!has(f)) return null; const r = await fetch(base + f); return r.ok ? r.text() : null; };
          const json = async (f: string) => { const t = await text(f); if (t == null) return undefined; try { return JSON.parse(t) as unknown; } catch { return Symbol.for('bad-json'); } };
          if (stopped) return;
          const job = await json('job.json');
          if (typeof job === 'symbol' || job === undefined) dispatch({ type: 'job-failed', at: clientNow(), why: 'job.json is not valid JSON' });
          else dispatch({ type: 'job', at: clientNow(), payload: job, serverDate: fixtureNow });
          const jobs = await json('jobs.json'); if (jobs !== undefined && typeof jobs !== 'symbol') dispatch({ type: 'jobs', at: clientNow(), payload: jobs });
          const ev = await text('events.ndjson'); if (ev != null) dispatch({ type: 'events', at: clientNow(), text: ev });
          const fr = await json('frame.json'); if (fr !== undefined && typeof fr !== 'symbol') dispatch({ type: 'frame', at: clientNow(), payload: fr });
          const rec = await json('recorder.json'); dispatch({ type: 'recorder', at: clientNow(), payload: rec === undefined || typeof rec === 'symbol' ? null : rec });
          dispatch({ type: 'link', at: clientNow(), mode: man.link === 'down' ? 'down' : 'fixture', ...(man.link === 'down' ? { why: 'the laptop has not answered since the last event' } : {}) });
        } catch (e) {
          dispatch({ type: 'job-failed', at: clientNow(), why: `fixture "${name}" could not be read (${(e as Error).message})` });
        }
      })();
    },
    stop() { stopped = true; },
    async control(action: ControlAction): Promise<ControlResult> {
      const s = getState(); const job = s.job?.value;
      if (!job) return { ok: false, why: 'no job loaded' };
      const now = clientNow() - s.skew;
      const seq = (s.events[s.events.length - 1]?.seq ?? -1) + 1;
      const ts = new Date(now).toISOString();
      const ev = (kind: string, msg: string, step_id?: string) => JSON.stringify({ v: 1, ts, run_id: job.run_id, seq, kind, msg, ...(step_id ? { step_id } : {}) });
      const stuckId = [...s.events].reverse().find(e => e.kind === 'stuck' || e.kind === 'pause')?.step_id;
      const map: Record<ControlAction, [string, string, string]> = {
        pause: ['paused', 'pause', 'paused: paused_by_user from the job view; lease kept'],
        resume: ['running', 'resume', 'resumed from the job view'],
        take_over: ['taken_over', 'taken_over', 'taken over from the job view; lease released to the person'],
        hand_back: ['running', 'resume', 'handed back from the job view; re-reading the screen'],
        retry: ['running', 'resume', 'retry requested from the job view'],
        stop: ['stopped', 'done', `stopped_by_user: stopped from the job view; restored ${job.changes_in_force?.filter(c => c.scope !== 'permanent').length ?? 0} settings`],
      };
      const [status, kind, msg] = map[action];
      const changes = action === 'stop' ? (job.changes_in_force ?? []).filter(c => c.scope === 'permanent') : job.changes_in_force;
      dispatch({ type: 'events', at: clientNow(), text: ev(kind, msg, action === 'retry' ? stuckId : undefined) });
      dispatch({ type: 'job', at: clientNow(), payload: { ...job, status, ...(changes ? { changes_in_force: changes } : {}) }, serverDate: now });
      return { ok: true };
    },
    frameSrc: (m: FrameMeta) => `${base}frame.jpg?ts=${encodeURIComponent(m.ts)}`,
    cropSrc: (_r: Recorder) => `${base}crop.jpg`,
    wantFrame() { /* fixtures have one frame */ },
  };
  return src;
}
