// The job view: v2.1's regions (band, receipt, frame, time chart, flight recorder, vitals, palette),
// ported from design/v2.html's mock onto real, validated data. Every number shown comes from
// derive(); nothing here invents a reading.

import { derive, explainPause, HEAT0, HELD, LIMITS, TERMINAL, type Derived, type HeatState, type Pause, type StateName, type StepRow } from '../model/derive.js';
import { reduce, type Action, type ViewState } from '../model/reducer.js';
import type { Validators } from '../contract/validate.js';
import type { ControlAction, Source } from '../source/types.js';
import { breakable, c1, clock, dur, durMin, durShort, esc, glyph, hm, kb, NB, plural, setHTML, stepLabel } from './format.js';
import { renderChart, hideTip, showTip, tAtX, type ChartCtx } from './chart.js';
import { initPalette, type PaletteCommand } from './palette.js';

const $ = (id: string) => document.getElementById(id)!;

const ST: Record<StateName, { w: string; g: string; c: string; tint: string }> = {
  connecting: { w: 'Connecting…', g: 'queued', c: '--idle', tint: '--idle-tint' },
  unavailable: { w: 'Data unavailable', g: 'failed', c: '--idle', tint: '--idle-tint' },
  queued: { w: 'Queued', g: 'queued', c: '--idle', tint: '--idle-tint' },
  running: { w: 'Running', g: 'run', c: '--run', tint: '--run-tint' },
  cooling: { w: 'Cooling', g: 'cool', c: '--cool', tint: '--cool-tint' },
  waiting: { w: 'Waiting for you', g: 'wait', c: '--wait', tint: '--wait-tint' },
  stuck: { w: 'Stuck', g: 'stuck', c: '--stuck', tint: '--stuck-tint' },
  paused: { w: 'Paused by you', g: 'paused', c: '--idle', tint: '--idle-tint' },
  held: { w: 'Paused', g: 'paused', c: '--idle', tint: '--idle-tint' },
  taken: { w: 'You have the phone', g: 'taken', c: '--idle', tint: '--idle-tint' },
  stopped: { w: 'Stopped', g: 'stopped', c: '--idle', tint: '--idle-tint' },
  failed: { w: 'Failed', g: 'failed', c: '--fail', tint: '--fail-tint' },
  done: { w: 'Done', g: 'done', c: '--done', tint: '--done-tint' },
};
const STEP_STYLE = {
  done: { g: 'done', w: 'Done', c: 'var(--ink-2)' }, retried: { g: 'retry', w: 'Done, 2 tries', c: 'var(--retry)' },
  acting: { g: 'run', w: 'Acting', c: 'var(--run)' }, queued: { g: 'queued', w: 'Queued', c: 'var(--ink-3)' },
  waiting: { g: 'wait', w: 'Needs you', c: 'var(--wait)' }, stuck: { g: 'stuck', w: 'Stuck', c: 'var(--stuck)' },
  failed: { g: 'failed', w: 'Failed', c: 'var(--fail)' }, cooling: { g: 'cool', w: 'Waits to cool', c: 'var(--cool)' },
  paused: { g: 'paused', w: 'Paused', c: 'var(--idle)' }, taken: { g: 'taken', w: 'You have it', c: 'var(--idle)' },
} as const;
type StepStyle = keyof typeof STEP_STYLE;

const ACT_TO_CONTROL: Record<string, ControlAction> = { pause: 'pause', resume: 'resume', take: 'take_over', handback: 'hand_back', retry: 'retry', 'stop-yes': 'stop' };
const PENDING_WORD: Record<ControlAction, string> = { pause: 'Pausing…', resume: 'Resuming…', take_over: 'Letting go…', hand_back: 'Handing back…', retry: 'Retrying…', stop: 'Stopping…' };

export interface App { dispatch(a: Action): void; getState(): ViewState }

export function startApp(source: Source, validators: Validators, initial: ViewState, fixtures: { name: string; label: string }[] | null): App {
  let S = initial;
  let D: Derived;
  let heat: HeatState = HEAT0;
  const params = new URLSearchParams(location.search);
  let sel: number | null = null;
  let filter: 'all' | 'problems' | 'pauses' = (['all', 'problems', 'pauses'] as const).find(f => f === params.get('filter')) ?? 'all';
  let chartCursor: number | null = null;
  let lastStateName: StateName | null = null, ctlSig = '', listSig = '', confirmOpen = false, lastFrameTs: string | null = null;
  let pending: { action: ControlAction; since: number; from: string } | null = null;
  let ctlError: string | null = null;
  let booted = false, wantHashStep: number | null = null;

  const m = /^#step-(\d+)$/.exec(location.hash); if (m) wantHashStep = +m[1]!;
  if (params.get('scrub')) wantHashStep = +params.get('scrub')!;

  const dispatch = (a: Action) => { S = reduce(S, a, validators); render(); };

  // ---------------------------------------------------------------- selection helpers
  const maxSel = () => D.live ? D.steps.length : D.steps.length - 1;
  const selIdx = () => sel == null ? maxSel() : Math.max(0, Math.min(sel, maxSel()));
  const isLiveIdx = (i: number) => !!D.live && i === D.steps.length;
  const numberAt = (i: number) => isLiveIdx(i) ? D.live!.n : D.steps[i]?.n ?? i + 1;
  function setSel(i: number | null) {
    const max = maxSel();
    sel = i == null || (i >= max && !!D.live) ? null : Math.max(0, Math.min(max, i));
    listSig = ''; render();
  }
  function lastProblem(): number | null {
    for (let i = D.steps.length - 1; i >= 0; i--) if (D.steps[i]!.tries > 1) return i;
    return null;
  }
  const problemLive = () => !!D.live && ['stuck', 'failed', 'waiting'].includes(D.state);
  function jumpToProblem() { if (problemLive()) setSel(null); else { const p = lastProblem(); if (p != null) setSel(p); } }

  // ---------------------------------------------------------------- band
  function sentence(name: StateName): string {
    const job = D.job, planned = D.planned, n = D.live?.n ?? D.done;
    const label = D.live?.label ? `: ${esc(D.live.label.replace(/^./, c => c.toLowerCase()))}` : '';
    const of = planned != null ? ` of ${planned}` : '';
    const changes = D.changes.filter(c => c.scope !== 'permanent');
    switch (name) {
      case 'connecting': return `Reading job <span class="mono">${esc(source.jobId || '…')}</span> from the laptop. The page fills in when the first answer arrives.`;
      case 'unavailable': return (source.jobId ? `There is no usable data for job <span class="mono">${esc(source.jobId)}</span> yet: ` : 'There is no job to show yet: ') + `${esc(S.jobProblem?.why ?? 'unknown problem')}. Nothing below is shown until a payload passes the check.` + (source.kind === 'live' && source.jobId ? ' It keeps asking.' : ' Reload once a job has started.');
      case 'queued': return `Waiting to start on ${esc(job?.device ?? 'the phone')}. Nothing has been tapped yet.`;
      case 'running': return `Step ${n}${of}${label}.` + (D.heat.near && D.temp ? ` The battery is at ${c1(D.temp.v)}${NB}°C, close to the ${LIMITS.pauseAt}${NB}°C limit; it will pause by itself to cool.` : '');
      case 'cooling': {
        const p = D.openPause!;
        const from = p.fromTemp != null ? ` because the battery reached ${c1(p.fromTemp)}${NB}°C (the limit is ${LIMITS.pauseAt}${NB}°C)` : '';
        const eta = D.cooling?.etaS != null ? `, about ${dur(D.cooling.etaS)} from now${D.cooling.measured ? '' : ' at the rate of its earlier pauses'}` : '; how long that takes is still being measured';
        return `Paused after step ${p.afterStep}${from}. It resumes by itself at ${LIMITS.resumeAt}${NB}°C${eta}. The screen sleeps while it cools.`;
      }
      case 'waiting': {
        const ex = explainPause(D.openPause?.msg ?? '');
        return `It paused at step ${n} and needs you: ${esc(ex.words ?? ex.rest ?? 'no reason given')}${ex.words && ex.rest ? ` <span class="mono">${breakable(ex.rest)}</span>` : ''}. It never answers a permission for you. Take over, deal with it on the phone, then hand back.`;
      }
      case 'stuck': {
        const miss = D.frame?.meta.miss;
        return `Step ${n} is stuck: ${esc(D.stuckEvent?.msg ?? 'no reason given')}.` + (miss ? ` The latest frame marks where it expected <span class="mono">${breakable(miss.selector)}</span>.` : '') + ' Take over and fix it on the phone, or retry the step.';
      }
      case 'paused': return `Held after step ${D.done}. Nothing is tapped until you resume. Settings stay changed while paused.`;
      case 'held': return `Paused after step ${D.done}: ${esc(explainPause(D.openPause?.msg ?? '').words ?? D.openPause?.msg ?? 'no reason given')}. Nothing is tapped while it is paused.`;
      case 'taken': return `The agent has let go after step ${D.done}. Do what you need on the phone, then hand back; it re-reads the screen before its next tap.`;
      case 'stopped': {
        const t = D.terminalEvent ? clock(Date.parse(D.terminalEvent.ts)) : null;
        const restored = changes.length ? `${plural(changes.length, 'changed setting')} ${changes.length === 1 ? 'is' : 'are'} still applied.` : `The changed settings were put back${t ? ` by ${t}` : ''}.`;
        return `You stopped it after step ${D.done}. ${restored}` + (planned != null && D.done < planned ? ` Steps ${D.done + 1} to ${planned} were not run.` : '');
      }
      case 'failed': {
        const e = D.lastError;
        const silent = D.lastEventAt != null ? ` No events for ${dur((D.now - D.lastEventAt) / 1000)}.` : '';
        const still = changes.length ? ` The ${plural(changes.length, 'changed setting')} ${changes.length === 1 ? 'is' : 'are'} still applied; bridged puts ${changes.length === 1 ? 'it' : 'them'} back when the phone reconnects.` : '';
        return `It stopped with an error${e ? ` during step ${D.done + 1} at ${clock(e.t)}: ${esc(e.msg)}` : ''}.${silent} Every reading below is from before that.${still}`;
      }
      case 'done': {
        const n2 = D.pauses.filter(p => p.kind === 'heat').length;
        const perm = D.changes.filter(c => c.scope === 'permanent').length;
        return `All ${planned ?? D.done} steps done in ${dur(D.totals.total)}. ${durMin(D.totals.cool)} of that was cooling (${plural(n2, 'pause')}).` + (changes.length ? ` ${plural(changes.length, 'setting')} still applied.` : ' The job’s settings were put back.') + (perm ? ` ${plural(perm, 'permanent change')} stays, as asked.` : '');
      }
    }
  }

  function notice(): string | null {
    if (D.state === 'connecting' || D.state === 'unavailable') return null;
    const ended = D.state === 'done' || D.state === 'stopped';
    if (S.jobProblem && S.job) return `Showing the last good data from ${clock(S.job.at - S.skew)}, ${dur((D.now - (S.job.at - S.skew)) / 1000)} old: ${S.jobProblem.why}.`;
    if (D.linkDown && !ended) return `Stale: ${S.link.why ?? 'the laptop is not answering'}. Showing data from ${D.lastEventAt ? clock(D.lastEventAt) : 'before'}, ${D.lastEventAt ? dur((D.now - D.lastEventAt) / 1000) : '?'} old.`;
    if (S.rejectedCount) return `${plural(S.rejectedCount, 'event line')} did not match progress.json and ${S.rejectedCount === 1 ? 'was' : 'were'} left out (Raw events lists ${S.rejectedCount === 1 ? 'it' : 'them'}).`;
    return null;
  }

  function renderBand(name: StateName) {
    const st = ST[name];
    const root = document.documentElement.style;
    root.setProperty('--st', `var(${st.c})`); root.setProperty('--st-tint', `var(${st.tint})`);
    root.setProperty('--st-ink2', `var(${st.c}-ink2)`); root.setProperty('--st-ink3', `var(${st.c}-ink3)`);
    $('state-glyph').setAttribute('href', '#g-' + st.g);
    if (name !== lastStateName) {
      $('state-word').textContent = st.w;
      const w = $('state-wrap'); w.classList.remove('enter'); void w.offsetWidth; if (lastStateName && lastStateName !== 'connecting') w.classList.add('enter');
      if (lastStateName && lastStateName !== 'connecting') $('announce').textContent = `Job state: ${st.w}.`;
    }
    setHTML($('state-sentence'), sentence(name));
    const nt = notice(); const ne = $('notice'); ne.hidden = !nt; setHTML(ne, nt ? esc(nt) : '');
    if (name === 'connecting' || name === 'unavailable') return;

    const d = D.done, planned = D.planned;
    const ruler = $('ruler');
    if (planned) {
      ruler.setAttribute('aria-valuemax', String(planned)); ruler.setAttribute('aria-valuenow', String(d)); ruler.setAttribute('aria-valuetext', `${d} of ${planned} steps done`);
      let h = `<div class="fill" style="width:${(d / planned) * 100}%"></div>`;
      if (!TERMINAL.includes(name) || name === 'failed') h += `<div class="cur" style="left:calc(${(d / planned) * 100}% - 2px)"></div>`;
      for (const p of D.pauses) if (p.kind === 'heat') h += `<div class="mk" style="left:${(p.afterStep / planned) * 100}%" title="cooling pause after step ${p.afterStep}"></div>`;
      setHTML(ruler, h);
    } else { ruler.removeAttribute('aria-valuemax'); ruler.setAttribute('aria-valuetext', `${d} steps done; the total was not reported`); setHTML(ruler, ''); }
    setHTML($('ruler-legend'), `<span>${planned ? `${d} of ${planned} done` : `${d} done, total not reported`}</span>` + (D.pauses.some(p => p.kind === 'heat') ? '<span>white gaps: cooling pauses</span>' : ''));

    const term = TERMINAL.includes(name), held = HELD.includes(name);
    let figs = `<div><dt>Steps</dt><dd>${d}${planned ? `<span class="u">${NB}/${NB}${planned}</span>` : ''}<small>${planned ? `${Math.round((d / planned) * 100)}${NB}%, ` : ''}${D.retried} retried</small></dd></div>`;
    figs += `<div><dt>${term ? 'Took' : 'Elapsed'}</dt><dd>${D.start != null ? durMin(D.totals.total) : 'not started'}<small>${D.start != null ? `from ${hm(D.start)}, ${Math.round((D.totals.cool / Math.max(1, D.totals.total)) * 100)}${NB}% cooling` : 'no events yet'}</small></dd></div>`;
    const f = D.finish;
    if (f.kind === 'ended') figs += `<div><dt>${name === 'done' ? 'Finished' : 'Ended'}</dt><dd>${hm(f.t)}<small>${clock(f.t)} SGT</small></dd></div>`;
    else if (f.kind === 'hold' || held) figs += `<div><dt>Finish (estimate)</dt><dd>On hold<small>${f.kind === 'hold' && f.leftS != null ? `≈${NB}${Math.max(1, Math.round(f.leftS / 60))}${NB}min of work left once it continues` : 'nothing runs until it continues'}</small></dd></div>`;
    else if (f.kind === 'estimate') {
      const why = f.pauses ? `+${NB}${plural(f.pauses, 'cooling pause')} expected${f.pauseEachS ? ` (≈${NB}${Math.round(f.pauseEachS / 60)}${NB}min each)` : ', length not measured yet'}` : f.includesOpenPause ? 'includes the rest of this pause' : 'no more cooling expected';
      figs += `<div><dt>Finish (estimate)</dt><dd>≈${NB}${hm(f.t)}<small>${why}${f.basis === 'engine' ? '; the engine’s estimate' : ''}</small></dd></div>`;
    } else figs += `<div><dt>Finish (estimate)</dt><dd>Not yet<small>needs 5 finished steps to estimate</small></dd></div>`;
    if (D.temp) figs += `<div class="m-only"><dt>Battery temp</dt><dd>${c1(D.temp.v)}<span class="u">${NB}°C</span><small>${D.readingsStale ? '<b class="stale-tag">stale</b>, ' : ''}read ${dur(D.temp.age)} ago</small></dd></div>`;
    setHTML($('figs'), figs);
    const sig = [name, D.live?.stepId, pending?.action, ctlError].join('|');
    if ((!confirmOpen && sig !== ctlSig) || name !== lastStateName) renderControls(name);
  }

  function renderControls(name: StateName) {
    confirmOpen = false; ctlSig = [name, D.live?.stepId, pending?.action, ctlError].join('|');
    const b = (id: string, label: string, cls = '') => {
      const ctl = ACT_TO_CONTROL[id];
      if (pending && ctl === pending.action) return `<button class="btn ${cls}" type="button" data-act="${id}" aria-busy="true" disabled>${PENDING_WORD[ctl]}</button>`;
      return `<button class="btn ${cls}" type="button" data-act="${id}"${pending ? ' disabled' : ''}>${label}</button>`;
    };
    let row = '', note = '';
    const n = D.live?.n;
    switch (name) {
      case 'running': row = b('pause', 'Pause') + b('take', 'Take over') + b('stop', 'Stop…', 'danger-text'); break;
      case 'cooling': row = b('take', 'Take over') + b('stop', 'Stop…', 'danger-text'); note = `No pause button: it is already paused for heat and resumes by itself at ${LIMITS.resumeAt}${NB}°C.`; break;
      case 'waiting': row = b('take', glyph('taken') + 'Take over', 'primary wait') + b('stop', 'Stop…', 'danger-text'); note = 'Answering permissions from this page comes later (approvals inbox).'; break;
      case 'stuck': row = b('take', glyph('taken') + 'Take over', 'primary stuck') + b('retry', `<span>Retry <span class="m-hide">step </span>${n ?? ''}</span>`) + b('stop', 'Stop…', 'danger-text'); break;
      case 'paused': case 'held': row = b('resume', 'Resume', 'primary') + b('take', 'Take over') + b('stop', 'Stop…', 'danger-text'); break;
      case 'taken': row = b('handback', 'Hand back', 'primary') + b('stop', 'Stop…', 'danger-text'); note = 'It re-reads the screen before its next tap.'; break;
      case 'queued': row = b('stop', 'Stop…', 'danger-text'); break;
      default: row = `<button class="btn" type="button" data-act="copy">Copy receipt</button><button class="btn" type="button" data-act="raw">Raw events</button>`;
        note = name === 'failed' ? 'Stop and restore runs automatically when the phone reconnects.' : '';
    }
    if (ctlError) note = `<span role="alert">${esc(ctlError)}</span>`;
    setHTML($('controls'), `<div class="ctl-row">${row}</div>${note ? `<p class="note${ctlError ? ' err' : ''}">${note}</p>` : ''}`);
  }
  function openConfirm() {
    confirmOpen = true;
    const name = D.state, planned = D.planned;
    const ch = D.changes.filter(c => c.scope !== 'permanent').length;
    const from = D.done + (name === 'running' ? 2 : 1);
    $('controls').insertAdjacentHTML('beforeend', `<div class="confirm" role="group" aria-label="Confirm stop"><p>Stop ${name === 'running' ? 'after the current step' : 'now'}${ch ? ` and put back the ${plural(ch, 'changed setting')}` : ''}? ${planned && from <= planned ? `Steps ${from} to ${planned} will not run. ` : ''}This can't be undone.</p>
      <div class="ctl-row"><button class="btn danger" type="button" data-act="stop-yes">Stop and restore</button><button class="btn" type="button" data-act="stop-no">Keep ${name === 'running' ? 'running' : 'it'}</button></div></div>`);
    // the markup changed outside setHTML, so drop its cache: cancelling must really rewrite the controls
    ($('controls') as HTMLElement & { _html?: string })._html = undefined;
    ($('controls').querySelector('[data-act="stop-no"]') as HTMLElement).focus();
  }

  // ---------------------------------------------------------------- receipt
  function receiptText() { return [...document.querySelectorAll('#receipt .rc-row')].map(r => r.children[0]!.textContent + ': ' + r.children[1]!.textContent).join('\n'); }
  function renderReceipt(name: StateName) {
    const el = $('receipt');
    if (!TERMINAL.includes(name)) { el.hidden = true; return; }
    el.hidden = false;
    const planned = D.planned, d = D.done, job = D.job!;
    const peak = D.samples.reduce<{ t: number; v: number } | null>((a, b) => (!a || b.v > a.v ? b : a), null);
    const batts = S.events.filter(e => typeof e.batt === 'number');
    const changes = D.changes.filter(c => c.scope !== 'permanent'), perm = D.changes.length - changes.length;
    const outcome = name === 'done' ? `Done: all ${planned ?? d} steps` : name === 'stopped' ? `Stopped by you after step ${d}` : `Failed at step ${d + 1}: ${D.lastError?.msg ?? 'error'}`;
    const settings = changes.length
      ? `<span class="bad">${plural(changes.length, 'setting')} still applied.</span> ${name === 'failed' ? 'Restore runs on reconnect.' : 'Check the changes list.'}`
      : `<span class="ok">None still in force.</span>${perm ? ` ${plural(perm, 'permanent change')} stays, as asked.` : ''}`;
    const heatN = D.pauses.filter(p => p.kind === 'heat' && p.start <= D.now).length;
    const rows1: [string, string][] = [
      ['Outcome', esc(outcome)],
      ['Steps', `${d}${planned ? ` of ${planned}` : ''} done, ${D.retried} needed a second try${planned ? `, ${planned - d} not run` : ''}`],
      ['Wall time', D.start != null && D.end != null ? `${dur(D.totals.total)} from ${clock(D.start)} to ${clock(D.end)} SGT` : 'not known'],
      ['Where it went', `working ${dur(D.totals.work)}, cooling ${dur(D.totals.cool)} (${plural(heatN, 'pause')}), waiting ${dur(D.totals.wait)}`],
    ];
    const rows2: [string, string][] = [
      ['Peak temperature', peak ? `${c1(peak.v)}${NB}°C at ${clock(peak.t)} (pause limit ${LIMITS.pauseAt}${NB}°C)` : 'no readings'],
      ['Battery', batts.length ? `${Math.round(batts[0]!.batt!)} → ${Math.round(batts[batts.length - 1]!.batt!)}${NB}%` : 'no readings'],
      ['Settings', settings],
      ['Retries', D.steps.filter(r => r.tries > 1).map(r => `step ${r.n}`).join(', ') || 'none'],
      ['Raw events', `<span class="mono">${esc(job.events_url ?? '/jobs/' + job.job_id + '/events')}</span> (${S.events.length} lines, NDJSON)`],
    ];
    const rr = (rows: [string, string][]) => rows.map(([k, v]) => `<div class="rc-row"><dt>${k}</dt><dd>${v}</dd></div>`).join('');
    setHTML(el, `<div class="sec-h" style="border-top:0;padding-top:0"><h2 id="rc-h">Receipt</h2><span class="meta">run <span class="mono">${esc(job.run_id)}</span></span></div>
      <div class="rc-grid"><dl>${rr(rows1)}</dl><dl>${rr(rows2)}</dl></div>`);
  }

  // ---------------------------------------------------------------- frame
  function ghost(x: number, y: number, kind: 'plan' | 'miss', label: string, W: number, H: number): string {
    const col = kind === 'miss' ? '#ff9b8a' : '#ffffff';
    let s = `<svg class="ghost" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-hidden="true"><g stroke="${col}" fill="none">`;
    s += `<line x1="${x}" y1="0" x2="${x}" y2="${H}" stroke-width="3" stroke-dasharray="14 14" opacity=".75"/><line x1="0" y1="${y}" x2="${W}" y2="${y}" stroke-width="3" stroke-dasharray="14 14" opacity=".75"/>`;
    s += `<circle cx="${x}" cy="${y}" r="80" stroke="#000" stroke-width="16" opacity=".45"/><circle cx="${x}" cy="${y}" r="80" stroke-width="9" stroke-dasharray="26 16"/>`;
    s += kind === 'miss' ? `<path d="M${x - 34} ${y - 34} L${x + 34} ${y + 34} M${x + 34} ${y - 34} L${x - 34} ${y + 34}" stroke-width="10"/>` : `<circle cx="${x}" cy="${y}" r="10" fill="${col}" stroke="none"/>`;
    s += `</g>`;
    const ly = y > 600 ? y - 150 : y + 120, lw = 30 * label.length + 40, lx = Math.max(20, Math.min(W - 20 - lw, x - lw / 2));
    s += `<rect x="${lx}" y="${ly - 62}" width="${lw}" height="86" rx="16" fill="#0f1113" stroke="${col}" stroke-width="4"/><text x="${lx + lw / 2}" y="${ly - 4}" text-anchor="middle" fill="${col}" font-size="48" font-family="system-ui" font-weight="600">${esc(label)}</text>`;
    return s + '</svg>';
  }
  function renderFrame(name: StateName) {
    const phone = $('phone'), stamp = $('stamp'), ageEl = $('frame-age');
    const f = D.frame;
    const ended = name === 'done' || name === 'stopped';
    if (!f) {
      phone.classList.remove('stale');
      setHTML($('screen'), `<div class="no-frame">${S.frameProblem ? `Frame unavailable: ${esc(S.frameProblem.why)}.` : ended ? 'No frame was kept for this job.' : 'No frame yet: the first one arrives within 30 s of the start.'}</div>`);
      stamp.hidden = true; setHTML(ageEl, '<span>no frame</span>');
      setHTML($('frame-cap'), `<span>At most 1 frame per 30${NB}s and 60${NB}KB.</span>`);
      return;
    }
    const meta = f.meta, stale = f.stale;
    phone.style.aspectRatio = `${meta.screen_w}/${meta.screen_h}`;
    phone.classList.toggle('stale', stale && !meta.secure);
    const n = D.live?.n;
    const tap = !TERMINAL.includes(name) && name !== 'waiting' && name !== 'cooling' && meta.next_tap && !meta.miss ? meta.next_tap : null;
    let over = '';
    if (meta.miss && name === 'stuck') over = ghost(meta.miss.x, meta.miss.y, 'miss', `Not found${n ? ` · step ${n}` : ''}`, meta.screen_w, meta.screen_h);
    else if (tap) over = ghost(tap.x, tap.y, 'plan', `Next tap${n ? ` · step ${n}` : ''}`, meta.screen_w, meta.screen_h);
    const pic = meta.secure
      ? '<div class="secure-well"></div>'
      : `<img class="shot" src="${esc(source.frameSrc(meta))}" width="${meta.w}" height="${meta.h}" alt="Phone screen at ${clock(Date.parse(meta.ts))}" decoding="async">`;
    // the secure note sits above the ghost, on its own ground, so the crosshair never runs through the words
    const note = meta.secure ? `<div class="secure-note">${glyph('lock')}<b>Secure screen</b><span>The app in front blocks screenshots, so there is no picture.</span></div>` : '';
    setHTML($('screen'), pic + over + note);
    if (lastFrameTs != null && meta.ts !== lastFrameTs && !stale && !meta.secure) { phone.classList.remove('blink'); void phone.offsetWidth; phone.classList.add('blink'); }
    lastFrameTs = meta.ts;
    const span = stamp.firstChild as HTMLElement;
    if (meta.secure) { stamp.hidden = false; span.textContent = 'Secure screen: no picture'; }
    else if (stale) { stamp.hidden = false; span.textContent = name === 'cooling' ? 'Stale: screen off' : D.linkDown || name === 'failed' ? 'Stale: phone not answering' : `Stale, ${dur(f.age)} old`; }
    else if (ended) { stamp.hidden = false; span.textContent = 'Final frame'; }
    else stamp.hidden = true;
    const frac = Math.min(1, f.age / LIMITS.frameEveryS), a = frac * 2 * Math.PI, ex = 8 + 6 * Math.sin(a), ey = 8 - 6 * Math.cos(a);
    const arc = frac >= 1 ? '<circle cx="8" cy="8" r="6" fill="none" stroke="currentColor" stroke-width="2"/>' : `<circle cx="8" cy="8" r="6" fill="none" stroke="var(--line-strong)" stroke-width="2"/><path d="M8 2 A6 6 0 ${frac > .5 ? 1 : 0} 1 ${ex.toFixed(2)} ${ey.toFixed(2)}" fill="none" stroke="currentColor" stroke-width="2"/>`;
    ageEl.classList.toggle('is-stale', stale && !ended);
    const nextNote = TERMINAL.includes(name) ? 'final frame' : name === 'cooling' ? 'no frames while cooling' : `next in ≤${NB}${Math.max(0, Math.ceil(LIMITS.frameEveryS - f.age))}${NB}s`;
    setHTML(ageEl, `<svg viewBox="0 0 16 16" aria-hidden="true">${arc}</svg><span>${stale && !ended ? 'Stale: ' : ''}${dur(f.age)} old, ${nextNote}</span>`);
    let cap = meta.secure
      ? `<span>Taken ${clock(Date.parse(meta.ts))} SGT. The window in front is marked secure, so bridged sends no picture; the ghost still shows the next tap.</span>`
      : `<span>Taken ${clock(Date.parse(meta.ts))} SGT, ${kb(meta.bytes)}. At most 1 frame per 30${NB}s and 60${NB}KB.</span>`;
    if (tap) cap += `<span>Dashed ring: the next tap${n ? `, step ${n}` : ''}${tap.label ? ` (${esc(tap.label)})` : ''} at ${tap.x},${NB}${tap.y}${NB}px. Dashed means not done yet.</span><span class="mono">${breakable(tap.selector)}</span>`;
    if (meta.miss && name === 'stuck') cap += `<span>Crossed ring: where <span class="mono">${breakable(meta.miss.selector)}</span> should be; ${esc(meta.miss.reason)}.</span>`;
    if (name === 'waiting') cap += '<span>No tap planned: this pop-up is not in the recipe, and permissions are never tapped for you.</span>';
    if (name === 'cooling') cap += '<span>This is the last picture before the pause. No new frames until it resumes, so the image greys out.</span>';
    setHTML($('frame-cap'), cap);
  }

  // ---------------------------------------------------------------- chart
  const chartCtx: ChartCtx = { get: () => D, cursor: () => chartCursor, setCursor: (t) => { chartCursor = t; }, selT: () => { const i = selIdx(); return sel == null ? null : isLiveIdx(i) ? null : D.steps[i]?.t ?? null; } };

  // ---------------------------------------------------------------- recorder
  function renderScrub() {
    const r = $('scrub') as HTMLInputElement, max = maxSel() + 1;
    r.max = String(Math.max(1, max)); r.value = String(selIdx() + 1); r.disabled = max < 1;
    r.setAttribute('aria-valuetext', `Step ${numberAt(selIdx())}${sel == null && D.live ? ', live' : ''}`);
    $('scrub-start').textContent = `Step ${D.steps[0]?.n ?? 1}`;
    $('scrub-end').textContent = D.live ? (sel == null ? `Step ${D.live.n}, live` : `Step ${D.live.n}, live is here`) : `Step ${numberAt(max - 1)}`;
    const pct = (i: number) => (max <= 1 ? 0 : (i / (max - 1)) * 100);
    let ticks = '';
    D.steps.forEach((s, i) => { if (s.tries > 1) ticks += `<i class="r" style="left:${pct(i)}%" title="retry"></i>`; });
    D.pauses.forEach(p => { if (p.kind === 'heat') ticks += `<i class="p" style="left:${pct(Math.max(0, p.afterStep - .5))}%"></i>`; });
    setHTML($('scrub-track'), ticks);
    setHTML($('rec-meta'), `<span class="tick-key"><i class="tk r"></i>retries</span> <span class="tick-key"><i class="tk p"></i>cooling</span>`);
  }
  function liveStyle(name: StateName): StepStyle {
    return ({ running: 'acting', cooling: 'cooling', waiting: 'waiting', stuck: 'stuck', failed: 'failed', paused: 'paused', held: 'paused', taken: 'taken', queued: 'queued' } as Partial<Record<StateName, StepStyle>>)[name] ?? 'acting';
  }
  function notesHtml(notes: string[]) {
    if (!notes.length) return '';
    return `<ul class="notes">${notes.map(n => `<li>${/^attempt /i.test(n) ? glyph('retry') : glyph('skip')}<span>${esc(n)}</span></li>`).join('')}</ul>`;
  }
  function renderDetail() {
    const i = selIdx(), live = isLiveIdx(i), name = D.state;
    const back = sel != null ? `<button class="btn quiet" type="button" data-act="live">Back to live <kbd>j</kbd></button>` : '';
    const nav = `<div class="detail-actions"><button class="btn" type="button" data-act="prev" ${i <= 0 ? 'disabled' : ''}>Previous <kbd>[</kbd></button><button class="btn" type="button" data-act="next" ${i >= maxSel() ? 'disabled' : ''}>Next <kbd>]</kbd></button>${back}`;
    if (maxSel() < 0) { setHTML($('detail'), `<p class="empty">No steps have finished yet.</p>`); return; }
    if (live) {
      const L = D.live!, st = STEP_STYLE[liveStyle(name)];
      const fm = D.frame?.meta, tap = fm?.next_tap?.step_id === L.stepId ? fm.next_tap : null, miss = fm?.miss && fm.miss.step_id === L.stepId ? fm.miss : null;
      const selector = miss?.selector ?? tap?.selector;
      const startedAt = D.steps.length ? D.steps[D.steps.length - 1]!.t : D.start;
      const notes = S.events.filter(e => e.kind === 'progress' && e.step_id && e.step_id === L.stepId && e.msg).map(e => e.msg!);
      let why = '';
      if (name === 'waiting' && D.openPause) { const ex = explainPause(D.openPause.msg); why = `<p class="meta">${esc(ex.words ?? 'Paused')}${ex.rest ? `: <span class="mono">${breakable(ex.rest)}</span>` : ''}.</p>`; }
      if (name === 'stuck' && D.stuckEvent) why = `<p class="meta">${esc(D.stuckEvent.msg ?? '')}${notes.length ? `; ${plural(notes.filter(n => /^attempt /i.test(n)).length + 1, 'try', 'tries')} in all` : ''}.</p>`;
      if (name === 'failed' && D.lastError) why = `<p class="meta">${esc(D.lastError.msg)} at ${clock(D.lastError.t)}. Whether this step's action took effect is unknown.</p>`;
      const rec = S.recorder?.value;
      const crop = rec && (name === 'stuck' || name === 'failed') && (!L.stepId || rec.step_id === L.stepId)
        ? `<figure class="crop"><img src="${esc(source.cropSrc(rec))}" alt="Flight recorder crop of the screen around where it looked, ${rec.crop.w} by ${rec.crop.h} px" width="${rec.crop.w}" height="${rec.crop.h}" loading="lazy"><figcaption>Flight recorder, ${clock(Date.parse(rec.ts))}: the last frame, cropped to ${rec.crop.x},${NB}${rec.crop.y} (${rec.crop.w}×${rec.crop.h}${NB}px). ${plural(rec.trees, 'tree')} and ${plural(rec.frames, 'frame')} were saved.</figcaption></figure>`
        : '';
      setHTML($('detail'), `<div class="detail-h"><h3>Step ${L.n}${L.label ? ` · ${esc(L.label)}` : ''}</h3><span class="pill ${name === 'running' || name === 'queued' ? 'dashed' : ''}" style="--st:${st.c}">${glyph(st.g)}${st.w}${sel == null ? ', live' : ''}</span></div>
        <div class="sel-line mono" title="Selector">${selector ? breakable(selector) : 'selector not reported for this step yet'}</div>
        <dl class="kv"><div><dt>Step id</dt><dd class="mono">${esc(L.stepId ?? 'not reported')}</dd></div>
          <div><dt>Tap</dt><dd>${tap ? `${tap.x},${NB}${tap.y}${NB}px` : miss ? `expected near ${miss.x},${NB}${miss.y}${NB}px` : 'not reported'}</dd></div>
          <div><dt>Started</dt><dd>${startedAt != null ? clock(startedAt) : 'not started'}, not finished</dd></div></dl>
        ${why}${notesHtml(name === 'stuck' ? [] : notes)}${crop}${nav}</div>`);
      return;
    }
    const r = D.steps[i]!, st = STEP_STYLE[r.tries > 1 ? 'retried' : 'done'];
    const w = r.tries > 1 ? `Done, ${r.tries} tries` : st.w;
    setHTML($('detail'), `<div class="detail-h"><h3>Step ${r.n} · ${esc(stepLabel(r.msg) || r.stepId)}</h3><span class="pill" style="--st:${st.c}">${glyph(st.g)}${w}</span></div>
      <div class="sel-line mono" title="Engine message">${breakable(r.msg || '(no message)')}</div>
      <dl class="kv"><div><dt>Step id</dt><dd class="mono">${esc(r.stepId)}</dd></div>
        <div><dt>Finished</dt><dd>${clock(r.t)}</dd></div>
        <div><dt>Took</dt><dd>${r.dur != null ? `${durShort(r.dur)} <span class="meta">(pauses not counted)</span>` : 'not known'}</dd></div>
        <div><dt>Tries</dt><dd>${r.tries}</dd></div></dl>
      ${notesHtml(r.notes)}${nav}<button class="btn quiet" type="button" data-act="copylink">${glyph('link')}Copy link to step ${r.n}</button></div>`);
  }
  function pauseRow(p: Pause) {
    const on = p.end == null && !TERMINAL.includes(D.state);
    const d = ((p.end ?? D.end ?? D.now) - p.start) / 1000;
    const k = p.kind;
    const word = k === 'heat' ? (on ? 'Cooling' : 'Cooled') : k === 'needs' ? (on ? 'Needs you' : 'Waited') : k === 'stuck' ? 'Stuck' : k === 'taken' ? 'You had it' : 'Paused';
    const title = k === 'heat' ? `Cooling pause${on ? ' (now)' : ''}` : k === 'needs' ? `Waiting for you${on ? ' (now)' : ''}` : k === 'stuck' ? 'Stuck' : k === 'taken' ? 'You took over' : k === 'user' ? 'Paused by you' : 'Paused';
    const col = k === 'heat' ? 'var(--cool)' : k === 'needs' ? 'var(--wait)' : k === 'stuck' ? 'var(--stuck)' : 'var(--idle)';
    const mono = k === 'heat' ? `${p.fromTemp != null ? c1(p.fromTemp) : '?'} → ${on && D.temp ? `${c1(D.temp.v)} (target ${c1(LIMITS.resumeAt)})` : c1(LIMITS.resumeAt)}${NB}°C, screen asleep` : (explainPause(p.msg).words ?? p.msg);
    const g = k === 'heat' ? 'cool' : k === 'needs' ? 'wait' : k === 'stuck' ? 'stuck' : k === 'taken' ? 'taken' : 'paused';
    return `<li><div class="row event" role="note"><span style="color:${col}">${glyph(g)}</span><span class="n"></span><span class="what"><b>${title}</b><span class="mono">${esc(mono)}</span></span><span class="st"><span style="--sc:${col}">${word}</span><em>${dur(d)}</em></span></div></li>`;
  }
  function renderList() {
    const cur = selIdx();
    const sig = [D.state, D.steps.length, D.pauses.length, D.pauses.at(-1)?.end, cur, filter, D.live?.stepId, Math.floor(D.now / 5000)].join('|');
    if (sig === listSig) return; listSig = sig;
    const items: string[] = [];
    const pausesAt = (k: number) => D.pauses.filter(p => p.afterStep === k && p.kind !== 'stuck');
    const showPauses = filter !== 'problems';
    D.steps.forEach((r, i) => {
      if (showPauses) for (const p of pausesAt(i)) items.push(pauseRow(p));
      if (filter === 'pauses' || (filter === 'problems' && r.tries < 2)) return;
      const st = STEP_STYLE[r.tries > 1 ? 'retried' : 'done'];
      items.push(`<li><button type="button" class="row" id="step-${r.n}" data-i="${i}" aria-current="${i === cur}" style="--sc:${st.c}">
        <span style="color:${st.c}">${glyph(st.g)}</span><span class="n">${r.n}</span><span class="what"><b>${esc(stepLabel(r.msg) || r.stepId)}</b><span class="mono">${esc(r.stepId)}</span></span>
        <span class="st"><span>${r.tries > 1 ? `Done, ${r.tries} tries` : st.w}</span><em>${r.dur != null ? durShort(r.dur) : ''}</em></span></button></li>`);
    });
    if (showPauses) for (const p of pausesAt(D.steps.length)) items.push(pauseRow(p));
    if (D.live && filter !== 'pauses' && (filter === 'all' || problemLive())) {
      const st = STEP_STYLE[liveStyle(D.state)], i = D.steps.length;
      items.push(`<li><button type="button" class="row ${D.state === 'running' || D.state === 'cooling' ? 'live' : 'live'}" id="step-${D.live.n}" data-i="${i}" aria-current="${i === cur}" style="--sc:${st.c}">
        <span style="color:${st.c}">${glyph(st.g)}</span><span class="n">${D.live.n}</span><span class="what"><b>${esc(D.live.label ?? 'In flight')}</b><span class="mono">${esc(D.live.stepId ?? 'step id not reported yet')}</span></span>
        <span class="st"><span>${st.w}</span><em>now</em></span></button></li>`);
    }
    if (filter === 'all' && D.planned != null) {
      const from = (D.live ? D.live.n : D.done) + 1;
      if (from <= D.planned) items.push(`<li><div class="row more" role="note"><span style="color:var(--ink-3)">${glyph(TERMINAL.includes(D.state) ? 'skip' : 'queued')}</span><span class="n"></span><span class="what"><b>Steps ${from} to ${D.planned}</b><span class="mono">${TERMINAL.includes(D.state) ? 'not run' : 'queued; their names arrive as they run'}</span></span><span class="st"><span style="--sc:var(--ink-3)">${TERMINAL.includes(D.state) ? 'Not run' : 'Queued'}</span><em>${D.planned - from + 1}</em></span></div></li>`);
    }
    const empty = filter === 'problems' ? 'No retries or problems so far: every step passed on its first try.' : filter === 'pauses' ? `No pauses yet. The job pauses by itself at ${LIMITS.pauseAt}${NB}°C.` : 'No steps yet.';
    $('list').innerHTML = items.length ? `<ol>${items.join('')}</ol>` : `<p class="empty">${empty}</p>`;
    const row = document.getElementById('step-' + numberAt(cur));
    if (row) {
      const list = $('list'); const prev = row.parentElement!.previousElementSibling as HTMLElement | null;
      const top = prev ? prev.offsetTop + prev.offsetHeight : 0; // the live li is sticky, so measure from its neighbour
      if (top < list.scrollTop || top > list.scrollTop + list.clientHeight - 60) list.scrollTop = top - list.clientHeight / 2;
    }
  }

  // ---------------------------------------------------------------- vitals
  const HUMAN: Record<string, (v: string) => string> = {
    screen_brightness: v => v === '1' ? '1 (dim)' : v,
    peak_refresh_rate: v => `${v}${NB}Hz`, min_refresh_rate: v => `${v}${NB}Hz`,
    screen_off_timeout: v => { const n = +v; return Number.isFinite(n) ? (n >= 60000 ? `${Math.round(n / 60000)}${NB}min` : `${Math.round(n / 1000)}${NB}s`) : v; },
  };
  const human = (key: string, v: string) => (HUMAN[key] ? HUMAN[key]!(v) : v);
  function renderVitals(name: StateName) {
    const ended = name === 'done' || name === 'stopped';
    let html = '';
    const t = D.temp;
    if (t) {
      const stale = D.readingsStale;
      const tline = ended ? `Final reading, taken ${clock(t.t)}` : stale ? `<span class="stale-tag">Stale</span>, last read ${dur(t.age)} ago at ${clock(t.t)}` : `${D.heat.dir === 'steady' || D.trend == null ? 'steady' : `${D.heat.dir} ${c1(Math.abs(D.trend))}${NB}°C/min`}, read ${dur(t.age)} ago`;
      let gap = '';
      if (!stale && name === 'cooling') gap = `${c1(t.v - LIMITS.resumeAt)}${NB}°C above resume${D.cooling?.etaS != null ? `, about ${dur(D.cooling.etaS)} to go` : ''}`;
      else if (!stale && name === 'running' && D.heat.dir === 'rising' && D.trend && D.trend > 0) gap = `${c1(LIMITS.pauseAt - t.v)}${NB}°C below the pause limit, about ${dur(((LIMITS.pauseAt - t.v) / D.trend) * 60)} at this rate`;
      else if (!stale && !TERMINAL.includes(name)) gap = `${c1(LIMITS.pauseAt - t.v)}${NB}°C below the pause limit`;
      const lo = Math.min(34, Math.floor(t.v) - 1), hi = Math.max(44, Math.ceil(t.v) + 1);
      const tx = (v: number) => 8 + ((v - lo) / (hi - lo)) * 264;
      const thermo = `<svg class="thermo" viewBox="0 0 280 46" preserveAspectRatio="none" aria-hidden="true">
        <defs><pattern id="thpat" width="4" height="4" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="1.6" height="4" fill="var(--fail)"/></pattern></defs>
        <rect x="8" y="14" width="264" height="8" rx="2" fill="var(--surface-2)" stroke="var(--line-strong)"/>
        <rect x="${tx(LIMITS.pauseAt)}" y="14" width="${tx(hi) - tx(LIMITS.pauseAt)}" height="8" fill="url(#thpat)"/>
        <line x1="${tx(LIMITS.resumeAt)}" x2="${tx(LIMITS.resumeAt)}" y1="10" y2="26" stroke="var(--cool)" stroke-width="2"/><line x1="${tx(LIMITS.pauseAt)}" x2="${tx(LIMITS.pauseAt)}" y1="10" y2="26" stroke="var(--fail)" stroke-width="2"/>
        <path d="M${tx(t.v) - 6} 4 L${tx(t.v) + 6} 4 L${tx(t.v)} 13 Z" fill="${stale ? 'var(--ink-3)' : 'var(--ink)'}"/>
        <text x="8" y="40">${lo}°</text><text x="${tx(LIMITS.resumeAt)}" y="40" text-anchor="middle">${LIMITS.resumeAt} resume</text><text x="${tx(LIMITS.pauseAt)}" y="40" text-anchor="middle">${LIMITS.pauseAt} pause</text></svg>`;
      html += `<div class="vital ${stale ? 'stale' : ''}"><h3>Battery temperature</h3><div class="big">${c1(t.v)}<span class="u">°C</span></div><p class="meta">${tline}</p>${thermo}<p class="meta">${gap}</p></div>`;
    } else html += `<div class="vital"><h3>Battery temperature</h3><div class="big">No reading</div><p class="meta">No event in this run has carried <span class="mono">temp_c</span> yet.</p></div>`;
    const b = D.batt;
    if (b) html += `<div class="vital ${D.readingsStale ? 'stale' : ''}"><h3>Battery</h3><div class="big">${Math.round(b.v)}<span class="u">%</span></div><p class="meta">${D.readingsStale ? '<span class="stale-tag">Stale:</span> ' : ''}read ${dur(b.age)} ago. It stops at ${LIMITS.batteryFloor}${NB}% when unpowered; charging is capped at ${LIMITS.chargeCap}${NB}%.</p></div>`;
    else html += `<div class="vital"><h3>Battery</h3><div class="big">No reading</div><p class="meta">No event has carried <span class="mono">batt</span> yet.</p></div>`;
    const e = D.lastError;
    html += `<div class="vital"><h3>Last error</h3>${e ? `<p class="meta">${clock(e.t)}${e.stepId ? `, step <span class="mono">${esc(e.stepId)}</span>` : ''}</p><p class="err-msg">${esc(e.msg)}</p>` : '<p class="meta">None in this run.</p>'}</div>`;
    const rows = D.changes.map(c => `<tr><td><span class="mono">${esc(c.key)}</span><span class="own">${c.ns ? esc(c.ns) + ', ' : ''}${c.scope === 'permanent' ? '<span class="scope-perm">permanent (R2), stays after the job</span>' : c.scope === 'until' && c.expires ? `until ${clock(Date.parse(c.expires))}` : c.scope ? `${esc(c.scope)}-scoped` : 'scope not given'}. Owner: job <span class="mono">${esc(c.owner.job_id)}</span>, pid ${c.owner.pid}, session <span class="mono">${esc(c.owner.session)}</span></span></td><td>${esc(human(c.key, c.before))}</td><td>${esc(human(c.key, c.applied))}</td></tr>`).join('');
    const tempScoped = D.changes.filter(c => c.scope !== 'permanent').length;
    const foot = !D.changes.length ? (ended || name === 'failed' ? 'None in force: every change was put back.' : 'None: this job has not changed any setting.')
      : name === 'failed' && tempScoped ? '<span class="stale-tag">Still applied</span>: put back when the phone reconnects.'
      : ended && tempScoped ? `<span class="stale-tag">Still applied</span> after the job ended.`
      : 'Put back when the job ends, however it ends' + (D.changes.some(c => c.scope === 'permanent') ? ', except permanent changes.' : '.');
    html += `<div class="vital"><h3>Changes in force</h3>${D.changes.length ? `<table class="changes"><caption class="sr-only">Settings changed on the phone, with their owners</caption><thead><tr><th scope="col">Setting</th><th scope="col">Before</th><th scope="col">Applied</th></tr></thead><tbody>${rows}</tbody></table>` : ''}<p class="meta" style="margin-top:var(--s2)">${foot}</p></div>`;
    html += `<div class="vital"><h3>Other jobs on this laptop</h3>${S.others.length ? `<ul class="others">${S.others.map(o => `<li>${esc(o.flow)} on ${esc(o.device)}: ${esc(o.status.replace('_', ' '))}</li>`).join('')}</ul>` : '<p class="meta">None.</p>'}</div>`;
    setHTML($('vitals'), html);
    const dev = D.job?.device ?? '';
    setHTML($('link-meta'), D.linkDown ? `<span class="stale-tag">link lost${D.lastEventAt ? ` ${dur((D.now - D.lastEventAt) / 1000)} ago` : ''}</span>` : `${esc(dev)}${D.lastEventAt ? `, last event ${dur((D.now - D.lastEventAt) / 1000)} ago` : ''}`);
  }

  // ---------------------------------------------------------------- render loop
  let skeletonCleared = false;
  function render() {
    D = derive(S, source.now(), heat); heat = D.heat;
    const name = D.state;
    if (pending && (D.job?.status !== pending.from || Date.now() - pending.since > 10000)) {
      if (D.job?.status === pending.from) ctlError = 'No change seen after 10 s: the request may not have reached the job.';
      pending = null;
    }
    renderBand(name);
    if (name !== 'connecting' && name !== 'unavailable') {
      if (!skeletonCleared) { skeletonCleared = true; $('chart-sk').hidden = true; booted = true; }
      $('main').removeAttribute('aria-busy');
      renderReceipt(name); renderFrame(name); renderChart(chartCtx); renderScrub(); renderDetail(); renderList(); renderVitals(name);
      if (!$('raw').hidden) $('raw').textContent = S.raw.slice(-16).join('\n') + (S.rejected.length ? `\n\n# ${S.rejectedCount} rejected line(s), newest last:\n` + S.rejected.slice(-5).map(r => `# ${r.why}\n# ${r.text}`).join('\n') : '');
      if (wantHashStep != null && D.steps.length) { const want = wantHashStep; wantHashStep = null; const i = D.steps.findIndex(r => r.n === want); if (i >= 0) setSel(i); }
      setHTML($('crumb-flow'), esc(D.job!.flow));
      setHTML($('crumb-meta'), `${D.planned != null ? `${D.planned} steps` : 'steps'} on ${esc(D.job!.device)}, job <span class="mono">${esc(D.job!.job_id)}</span>`);
    } else {
      $('main').setAttribute('aria-busy', name === 'connecting' ? 'true' : 'false');
    }
    document.title = name === 'connecting' ? 'Connecting · Agentroid job' : name === 'unavailable' ? 'Data unavailable · Agentroid job' : `${ST[name].w} ${D.done}${D.planned ? '/' + D.planned : ''} · ${D.job!.flow}`;
    lastStateName = name;
  }

  // ---------------------------------------------------------------- interaction
  function copyText(text: string, btn: HTMLElement | null, okMsg: string, failMsg: string) {
    const done = (okay: boolean) => {
      if (btn) { const was = btn.innerHTML; btn.classList.toggle('done-flash', okay); btn.textContent = okay ? 'Copied' : 'Copy failed'; setTimeout(() => { if (btn.isConnected) { btn.innerHTML = was; btn.classList.remove('done-flash'); } }, 1600); }
      $('announce').textContent = okay ? okMsg : failMsg;
    };
    if (navigator.clipboard?.writeText) navigator.clipboard.writeText(text).then(() => done(true), () => done(false)); else done(false);
  }
  async function control(act: string) {
    const ctl = ACT_TO_CONTROL[act]; if (!ctl || pending) return;
    pending = { action: ctl, since: Date.now(), from: D.job?.status ?? '' }; ctlError = null; ctlSig = ''; render();
    const res = await source.control(ctl);
    if (!res.ok) { pending = null; ctlError = res.why; ctlSig = ''; render(); return; }
    if (source.kind === 'fixture') pending = null; // fixture answers at once; a fake delay would be theatre
    ctlSig = ''; listSig = ''; render(); ($('controls').querySelector('.btn:not([disabled])') as HTMLElement | null)?.focus();
  }
  function act(a: string) {
    if (!booted) return;
    switch (a) {
      case 'stop': openConfirm(); return;
      case 'stop-no': renderControls(D.state); ($('controls').querySelector('[data-act="stop"]') as HTMLElement | null)?.focus(); return;
      case 'copy': copyText(receiptText(), $('controls').querySelector('[data-act="copy"]'), 'Receipt copied.', 'Copy failed: select the receipt text instead.'); return;
      case 'raw': ($('raw-toggle') as HTMLElement).click(); $('raw').focus(); return;
      case 'live': setSel(null); return;
      case 'prev': setSel(selIdx() - 1); return;
      case 'next': setSel(selIdx() + 1); return;
      case 'copylink': {
        const n = numberAt(selIdx()); const u = location.href.split('#')[0] + '#step-' + n; history.replaceState(null, '', '#step-' + n);
        copyText(u, $('detail').querySelector('[data-act="copylink"]'), `Link to step ${n} copied.`, 'Copy failed: the link is in the address bar.'); return;
      }
      default: void control(a);
    }
  }
  document.addEventListener('click', e => {
    const t = e.target as HTMLElement;
    const b = t.closest<HTMLButtonElement>('[data-act]'); if (b && !b.disabled) act(b.dataset.act!);
    const r = t.closest<HTMLElement>('.row[data-i]'); if (r) setSel(+r.dataset.i!);
  });
  $('scrub').addEventListener('input', e => setSel(+(e.target as HTMLInputElement).value - 1));
  document.querySelectorAll<HTMLButtonElement>('.chip[data-f]').forEach(c => {
    c.setAttribute('aria-pressed', String(c.dataset.f === filter));
    c.addEventListener('click', () => {
      filter = c.dataset.f as typeof filter;
      document.querySelectorAll('.chip[data-f]').forEach(x => x.setAttribute('aria-pressed', String(x === c)));
      // CRITIQUE rule 19: the filter is in the URL, so a "Retries and problems" view can be shared
      const u = new URL(location.href); if (filter === 'all') u.searchParams.delete('filter'); else u.searchParams.set('filter', filter); history.replaceState(null, '', u);
      listSig = ''; render();
    });
  });
  $('raw-toggle').addEventListener('click', e => { const r = $('raw'); r.hidden = !r.hidden; (e.currentTarget as HTMLElement).setAttribute('aria-pressed', String(!r.hidden)); render(); });

  const cw = $('chart-wrap');
  cw.addEventListener('pointermove', e => { if (booted) showTip(chartCtx, tAtX(e.clientX)); });
  cw.addEventListener('pointerleave', e => { if (booted && e.pointerType === 'mouse') hideTip(chartCtx); });
  cw.addEventListener('click', e => { if (!booted) return; const t = tAtX(e.clientX); showTip(chartCtx, t); const i = stepAtTime(t); if (i != null) setSel(i); });
  cw.addEventListener('focus', () => { if (booted) showTip(chartCtx, chartCursor ?? lastReadT(), true); });
  cw.addEventListener('blur', () => { if (booted) hideTip(chartCtx); });
  cw.addEventListener('keydown', e => {
    if (!booted || !['ArrowLeft', 'ArrowRight', 'Enter', 'Home', 'End'].includes(e.key)) return; e.preventDefault();
    let t = chartCursor ?? lastReadT(); const d = e.shiftKey ? 300_000 : 30_000;
    if (e.key === 'ArrowLeft') t -= d; if (e.key === 'ArrowRight') t += d; if (e.key === 'Home') t = D.start ?? t; if (e.key === 'End') t = lastReadT();
    t = Math.max(D.start ?? t, Math.min(lastReadT(), t));
    if (e.key === 'Enter') { const i = stepAtTime(t); if (i != null) setSel(i); }
    showTip(chartCtx, t, true);
  });
  const lastReadT = () => D.samples.at(-1)?.t ?? D.lastEventAt ?? D.now;
  function stepAtTime(t: number): number | null {
    const i = D.steps.findIndex(r => r.t >= t);
    if (i >= 0) return i;
    return D.steps.length ? maxSel() : null;
  }
  window.addEventListener('resize', () => { if (booted) renderChart(chartCtx); });

  // palette
  const pal = initPalette({
    booted: () => booted,
    commands(q: string): PaletteCommand[] {
      const out: PaletteCommand[] = []; const name = D?.state;
      if (booted) {
        const n = /^\s*(?:step\s*)?(\d{1,4})\s*$/i.exec(q);
        if (n) {
          const want = +n[1]!; const i = D.steps.findIndex(r => r.n === want);
          if (i >= 0) out.push({ label: `Go to step ${want}`, hint: stepLabel(D.steps[i]!.msg), run: () => { setSel(i); document.getElementById('step-' + want)?.focus(); } });
          else if (D.live && want === D.live.n) out.push({ label: `Go to step ${want}`, hint: 'the live one', run: () => setSel(null) });
          else if (D.planned && want >= 1 && want <= D.planned) out.push({ label: `Step ${want} has not run yet`, hint: D.live ? `go to step ${D.live.n}, the live one` : 'nothing to show', run: () => setSel(null) });
        }
        if (name === 'running') out.push({ label: 'Pause', hint: 'holds after the current step', run: () => act('pause') });
        if (name === 'paused' || name === 'held') out.push({ label: 'Resume', run: () => act('resume') });
        if (name && ['running', 'cooling', 'waiting', 'stuck', 'paused', 'held'].includes(name)) out.push({ label: 'Take over', hint: 'the agent lets go of the phone', run: () => act('take') });
        if (name === 'taken') out.push({ label: 'Hand back', hint: 'it re-reads the screen first', run: () => act('handback') });
        if (name === 'stuck' && D.live) out.push({ label: `Retry step ${D.live.n}`, run: () => act('retry') });
        if (name && !TERMINAL.includes(name)) out.push({ label: 'Stop…', hint: 'asks first, then puts settings back', cls: 'danger', run: () => act('stop') });
        if (sel != null) out.push({ label: 'Back to live', key: 'j', run: () => setSel(null) });
        out.push({ label: 'Last problem', key: 'f', run: jumpToProblem });
        if (D.steps.length && !isLiveIdx(selIdx())) out.push({ label: `Copy link to step ${numberAt(selIdx())}`, run: () => act('copylink') });
        if (name && TERMINAL.includes(name)) out.push({ label: 'Copy receipt', run: () => act('copy') });
        out.push({ label: $('raw').hidden ? 'Show raw events' : 'Hide raw events', run: () => ($('raw-toggle') as HTMLElement).click() });
      }
      out.push({ label: `Switch to ${document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark'} theme`, run: () => ($('theme') as HTMLElement).click() });
      const words = q.toLowerCase().split(/\s+/).filter(w => w && !/^\d+$/.test(w));
      return out.filter(c => /^(Go to step|Step \d)/.test(c.label) || words.every(w => (c.label + ' ' + (c.hint ?? '')).toLowerCase().includes(w)));
    },
  });

  document.addEventListener('keydown', e => {
    if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === 'k') { e.preventDefault(); pal.toggle(); return; }
    if (pal.isOpen()) return;
    const tgt = e.target as HTMLElement;
    if (tgt.matches('input,select,textarea') || e.metaKey || e.ctrlKey || e.altKey) { if (e.key === 'Escape' && confirmOpen) act('stop-no'); return; }
    if (!booted) return;
    if (e.key === 'j') setSel(null);
    else if (e.key === 'f') jumpToProblem();
    else if (e.key === '[') setSel(selIdx() - 1);
    else if (e.key === ']') setSel(selIdx() + 1);
    else if (e.key === 'Escape') { if (confirmOpen) act('stop-no'); else if (sel != null) setSel(null); }
  });

  // theme
  function applyTheme(t: string, save: boolean) {
    document.documentElement.dataset.theme = t; const dark = t === 'dark';
    $('theme').innerHTML = `<svg class="g" aria-hidden="true"><use href="#g-${dark ? 'sun' : 'moon'}"/></svg><span>${dark ? 'Light' : 'Dark'}</span>`;
    $('theme').setAttribute('aria-label', `Switch to ${dark ? 'light' : 'dark'} theme`);
    if (save) { try { localStorage.setItem('agentroid-jobview-theme', t); } catch { /* storage blocked: the choice lasts for this visit */ } }
  }
  applyTheme(document.documentElement.dataset.theme === 'dark' ? 'dark' : 'light', false);
  $('theme').addEventListener('click', () => { applyTheme(document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark', true); if (booted) render(); });

  // fixture switcher (fixture mode only: never shown on live data)
  if (fixtures) {
    const fx = $('fix'); fx.hidden = false;
    const selEl = $('fix-select') as HTMLSelectElement;
    const cur = params.get('fixture') ?? '';
    selEl.innerHTML = [{ name: 'loading', label: 'Loading (first paint)' }, ...fixtures].map(f => `<option value="${esc(f.name)}"${f.name === cur ? ' selected' : ''}>${esc(f.label)}</option>`).join('');
    selEl.addEventListener('change', () => { const u = new URL(location.href); u.searchParams.set('fixture', selEl.value); u.hash = ''; location.assign(u.toString()); });
  }
  $('source-label').textContent = source.label;

  // the 1 s tick: ages and the cooling countdown move; skipped while the tab is hidden
  setInterval(() => { if (!document.hidden && booted) { render(); $('source-label').textContent = source.label; } }, 1000);
  // the slow-connection line, in words, after 5 s
  setTimeout(() => { if (!booted && !S.jobProblem && source.jobId !== '') { setHTML($('state-sentence'), 'Still connecting: the laptop has not answered for 5 s. Check that bridged is running on it.'); } else if (!booted && source.kind === 'fixture' && new URLSearchParams(location.search).get('fixture') === 'loading') { setHTML($('state-sentence'), 'Still connecting: the laptop has not answered for 5 s. Check that bridged is running on it.'); } }, 5000);

  return { dispatch, getState: () => S };
}
