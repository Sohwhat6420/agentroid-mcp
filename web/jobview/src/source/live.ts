// Live mode: the endpoints in WIRING.md, served by bridged on the same origin as this page.
//   GET /jobs/:id            job object (job-view.json); its Date header sets the clock skew
//   GET /jobs                the job list (job-view.json, list form)
//   GET /jobs/:id/events     NDJSON backlog (?after=<seq>); with Accept: text/event-stream, SSE
//   GET /jobs/:id/frame.json frame metadata, polled at most once per 30 s
//   GET /jobs/:id/frame.jpg  the frame itself (<= 60 KB), fetched only when frame.json changes
//   GET /jobs/:id/recorder.json  flight-recorder summary (404 when there is none)
//   POST /jobs/:id/control   {"action": ...} with the page token (proposed; see WIRING.md)
// The page only ever talks to its own origin: no third-party host, no CDN.

import type { Action, ViewState } from '../model/reducer.js';
import type { FrameMeta, Recorder } from '../contract/types.js';
import { LIMITS } from '../model/derive.js';
import type { ControlAction, ControlResult, Source } from './types.js';

const JOB_POLL_MS = 5000, EVENTS_POLL_MS = 2000, DOWN_AFTER_MS = 15000, FETCH_TIMEOUT_MS = 8000;

async function get(url: string, accept: string): Promise<Response> {
  const ctl = new AbortController();
  const t = setTimeout(() => ctl.abort(), FETCH_TIMEOUT_MS);
  try { return await fetch(url, { headers: { Accept: accept }, cache: 'no-store', credentials: 'same-origin', signal: ctl.signal }); }
  finally { clearTimeout(t); }
}

export function liveSource(jobId: string): Source {
  const id = encodeURIComponent(jobId);
  let dispatch: (a: Action) => void = () => {};
  let getState: () => ViewState = () => { throw new Error('not started'); };
  let es: EventSource | null = null;
  const timers: number[] = [];
  let lastFrameFetch = 0, frameTs: string | null = null, lastSuccess = Date.now(), recorderFor: string | null = null;

  const fail = (why: string) => {
    if (Date.now() - lastSuccess > DOWN_AFTER_MS) dispatch({ type: 'link', at: Date.now(), mode: 'down', why });
  };
  const success = () => { lastSuccess = Date.now(); };

  async function pollJob() {
    try {
      const r = await get(`/jobs/${id}`, 'application/json');
      if (!r.ok) { dispatch({ type: 'job-failed', at: Date.now(), why: `GET /jobs/${jobId} answered ${r.status}` }); fail(`GET /jobs/${jobId} answered ${r.status}`); return; }
      const date = Date.parse(r.headers.get('Date') ?? '');
      let payload: unknown;
      try { payload = await r.json(); } catch { dispatch({ type: 'job-failed', at: Date.now(), why: 'the job answer was not JSON' }); return; }
      success();
      dispatch({ type: 'job', at: Date.now(), payload, ...(Number.isFinite(date) ? { serverDate: date } : {}) });
      const st = getState().job?.value.status;
      if ((st === 'stuck' || st === 'error') && recorderFor !== st) { recorderFor = st; void pollRecorder(); }
    } catch (e) {
      dispatch({ type: 'job-failed', at: Date.now(), why: `the laptop did not answer (${(e as Error).name === 'AbortError' ? 'timed out' : 'network error'})` });
      fail('the laptop is not answering');
    }
  }
  async function pollList() {
    try { const r = await get('/jobs', 'application/json'); if (r.ok) dispatch({ type: 'jobs', at: Date.now(), payload: await r.json() }); } catch { /* the list is optional */ }
  }
  async function pollEvents() {
    const after = getState().events.at(-1)?.seq;
    try {
      const r = await get(`/jobs/${id}/events${after != null ? `?after=${after}` : ''}`, 'application/x-ndjson');
      if (!r.ok) { fail(`GET events answered ${r.status}`); return; }
      success();
      dispatch({ type: 'events', at: Date.now(), text: await r.text() });
    } catch { fail('the laptop is not answering'); }
  }
  async function pollFrame(force = false) {
    const now = Date.now();
    if (!force && now - lastFrameFetch < LIMITS.frameEveryS * 1000) return; // at most 1 frame per 30 s
    lastFrameFetch = now;
    try {
      const r = await get(`/jobs/${id}/frame.json`, 'application/json');
      if (r.status === 404) return; // no frame yet
      if (!r.ok) { dispatch({ type: 'frame-failed', at: now, why: `frame.json answered ${r.status}` }); return; }
      const meta = await r.json() as FrameMeta;
      if (meta && meta.ts === frameTs) return;
      frameTs = meta?.ts ?? null;
      dispatch({ type: 'frame', at: now, payload: meta });
    } catch { dispatch({ type: 'frame-failed', at: now, why: 'the frame could not be fetched' }); }
  }
  async function pollRecorder() {
    try {
      const r = await get(`/jobs/${id}/recorder.json`, 'application/json');
      dispatch({ type: 'recorder', at: Date.now(), payload: r.ok ? await r.json() : null });
    } catch { /* optional */ }
  }
  function openStream() {
    if (typeof EventSource === 'undefined') { dispatch({ type: 'link', at: Date.now(), mode: 'poll' }); return; }
    const after = getState().events.at(-1)?.seq;
    es = new EventSource(`/jobs/${id}/events${after != null ? `?after=${after}` : ''}`);
    es.onopen = () => { success(); dispatch({ type: 'link', at: Date.now(), mode: 'stream' }); };
    es.onmessage = (m) => { success(); dispatch({ type: 'events', at: Date.now(), text: m.data }); };
    es.addEventListener('job', (m) => { success(); try { dispatch({ type: 'job', at: Date.now(), payload: JSON.parse((m as MessageEvent).data) }); } catch { /* the next poll fixes it */ } });
    es.onerror = () => {
      // EventSource reconnects by itself with Last-Event-ID; meanwhile poll so nothing is missed
      dispatch({ type: 'link', at: Date.now(), mode: 'poll' });
      void pollEvents();
    };
  }

  return {
    kind: 'live', jobId, label: 'Live from bridged on this laptop',
    now: () => Date.now(),
    start(d, g) {
      dispatch = d; getState = g;
      void (async () => {
        await pollJob(); void pollList(); await pollEvents(); openStream(); void pollFrame(true);
        timers.push(window.setInterval(() => { if (!document.hidden) void pollJob(); }, JOB_POLL_MS));
        timers.push(window.setInterval(() => { if (!document.hidden && getState().link.mode !== 'stream') void pollEvents(); }, EVENTS_POLL_MS));
        timers.push(window.setInterval(() => { if (!document.hidden) void pollFrame(); }, 5000));
        timers.push(window.setInterval(() => { if (!document.hidden) void pollList(); }, 30000));
      })();
    },
    stop() { es?.close(); timers.forEach(t => clearInterval(t)); },
    async control(action: ControlAction): Promise<ControlResult> {
      const token = document.querySelector<HTMLMetaElement>('meta[name="agentroid-token"]')?.content ?? '';
      try {
        const r = await fetch(`/jobs/${id}/control`, {
          method: 'POST', credentials: 'same-origin', cache: 'no-store',
          headers: { 'Content-Type': 'application/json', 'X-Agentroid-Token': token }, body: JSON.stringify({ action }),
        });
        if (r.status === 202 || r.ok) { void pollJob(); return { ok: true }; }
        if ([404, 405, 501].includes(r.status)) return { ok: false, unwired: true, why: 'Controls are not wired on this laptop yet. Use the phone tile, or ask Claude.' };
        if (r.status === 403) return { ok: false, why: 'Refused: this page is not allowed to control the job (token or origin check).' };
        if (r.status === 409) return { ok: false, why: `Refused: the job is no longer in a state that allows "${action.replace('_', ' ')}".` };
        return { ok: false, why: `The laptop answered ${r.status}; nothing was changed.` };
      } catch { return { ok: false, why: 'The laptop did not answer; nothing was changed.' }; }
    },
    frameSrc: (m: FrameMeta) => `/jobs/${id}/frame.jpg?ts=${encodeURIComponent(m.ts)}`,
    cropSrc: (rec: Recorder) => rec.crop.url,
    wantFrame() { void pollFrame(); },
  };
}
