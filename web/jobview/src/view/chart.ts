// "Where the time went": the work / pause lane over the battery temperature line (design v2.1 spine
// chart), drawn from real samples and pauses. The hover tooltip flips by quadrant (CRITIQUE item 4) so
// it never covers the point it explains.

import { LIMITS, TERMINAL, explainPause, type Derived, type Pause, type Sample } from '../model/derive.js';
import { NB, c1, clock, dur, durMin, esc, hm, stepLabel } from './format.js';

const $ = (id: string) => document.getElementById(id)!;

export interface ChartCtx {
  get(): Derived;
  cursor(): number | null;
  setCursor(t: number | null): void;
  /** the time of the selected (non-live) step, for the playhead */
  selT(): number | null;
}
interface Geom { X: (t: number) => number; Y: (v: number) => number; x0: number; x1: number; L: number; Rm: number; W: number; H: number }
let geom: Geom | null = null;

function domain(D: Derived): [number, number] {
  const x0 = D.start ?? D.now - 60_000;
  if (D.end != null) return [x0, Math.max(D.end, x0 + 60_000)];
  const f = D.finish;
  const x1 = f.kind === 'estimate' ? Math.max(f.t, D.now + 60_000) : D.now + 30_000;
  return [x0, Math.max(x1, x0 + 60_000)];
}
const PAT: Record<Pause['kind'], string> = { heat: 'url(#pc)', needs: 'url(#pw)', stuck: 'url(#ps)', user: 'none', taken: 'none', other: 'none' };
const COL: Record<Pause['kind'], string> = { heat: 'var(--cool)', needs: 'var(--wait)', stuck: 'var(--stuck)', user: 'var(--idle)', taken: 'var(--idle)', other: 'var(--idle)' };
const WORD: Record<Pause['kind'], string> = { heat: 'cooling', needs: 'waiting for you', stuck: 'stuck', user: 'paused by you', taken: 'you had the phone', other: 'paused' };

export function nearestSample(sm: Sample[], t: number): Sample | null {
  let best: Sample | null = null, bd = Infinity;
  for (const p of sm) { const d = Math.abs(p.t - t); if (d < bd) { bd = d; best = p; } }
  return best;
}

export function renderChart(ctx: ChartCtx) {
  const D = ctx.get();
  const svg = $('chart'), W = Math.max(280, $('chart-wrap').clientWidth), mobile = W < 520;
  const H = mobile ? 180 : 210, L = 34, Rm = 12, top = 22, laneH = 16, ty0 = top + laneH + 18, ty1 = H - 22;
  svg.setAttribute('viewBox', `0 0 ${W} ${H}`); svg.setAttribute('height', String(H));
  const [x0, x1] = domain(D);
  const ended = TERMINAL.includes(D.state);
  const now = ended && D.end != null ? D.end : D.now;
  const vmin = Math.min(35, ...D.samples.map(s => Math.floor(s.v - 0.5))), vmax = Math.max(44, ...D.samples.map(s => Math.ceil(s.v + 0.5)));
  const X = (t: number) => L + ((t - x0) / (x1 - x0)) * (W - L - Rm);
  const Y = (v: number) => ty1 - ((v - vmin) / (vmax - vmin)) * (ty1 - ty0);
  let g = `<defs><pattern id="pc" width="5" height="5" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="2" height="5" fill="var(--cool)"/></pattern>
    <pattern id="pw" width="5" height="5" patternUnits="userSpaceOnUse"><rect width="2" height="5" fill="var(--wait)"/></pattern>
    <pattern id="ps" width="6" height="6" patternUnits="userSpaceOnUse" patternTransform="rotate(45)"><rect width="2" height="6" fill="var(--stuck)"/><rect width="6" height="2" fill="var(--stuck)"/></pattern></defs>`;
  // time ticks, on SGT minute boundaries
  const span = (x1 - x0) / 1000, stepMin = span > 3000 ? (mobile ? 15 : 10) : span > 900 ? (mobile ? 10 : 5) : (mobile ? 5 : 2);
  const SGT = 8 * 3600_000, stepMs = stepMin * 60_000;
  for (let t = Math.ceil((x0 + SGT) / stepMs) * stepMs - SGT; t < x1; t += stepMs) { const x = X(t); g += `<line x1="${x}" x2="${x}" y1="${ty0 - 6}" y2="${ty1}" stroke="var(--line)"/><text x="${x}" y="${H - 6}" text-anchor="middle">${hm(t)}</text>`; }
  for (const v of [36, 38, 40, 44].filter(v => v >= vmin && v <= vmax)) g += `<text x="${L - 6}" y="${Y(v) + 4}" text-anchor="end">${v}°</text>`;
  const nowX = X(now), lx = ended ? Math.min(W - Rm, nowX - 6) : W - Rm;
  g += `<line x1="${L}" x2="${W - Rm}" y1="${Y(LIMITS.pauseAt)}" y2="${Y(LIMITS.pauseAt)}" stroke="var(--fail)" stroke-dasharray="5 4"/>`;
  let lim = `<text x="${lx}" y="${Y(LIMITS.pauseAt) - 5}" text-anchor="end" class="lbl-strong halo" style="fill:var(--fail)">pauses at ${LIMITS.pauseAt}°</text>`;
  g += `<line x1="${L}" x2="${W - Rm}" y1="${Y(LIMITS.resumeAt)}" y2="${Y(LIMITS.resumeAt)}" stroke="var(--cool)" stroke-dasharray="2 4"/>`;
  lim += `<text x="${lx}" y="${Y(LIMITS.resumeAt) + 15}" text-anchor="end" class="lbl-strong halo" style="fill:var(--cool)">resumes at ${LIMITS.resumeAt}°</text>`;

  // the lane: grey work blocks between pauses, each pause in its own pattern
  g += `<text x="${L}" y="${top - 8}" class="lbl-strong">Work and pauses</text>`;
  if (D.start != null) {
    let cursor = D.start;
    for (const p of D.pauses) {
      if (p.start > now) break;
      if (p.start > cursor) g += `<rect x="${X(cursor)}" y="${top}" width="${Math.max(1, X(p.start) - X(cursor))}" height="${laneH}" fill="var(--ink-3)" rx="2"/>`;
      const pe = Math.min(p.end ?? now, now);
      g += `<rect x="${X(p.start)}" y="${top}" width="${Math.max(2, X(pe) - X(p.start))}" height="${laneH}" fill="${PAT[p.kind]}" stroke="${COL[p.kind]}" rx="2"/>`;
      if (X(pe) - X(p.start) > 70) g += `<text x="${(X(p.start) + X(pe)) / 2}" y="${top + laneH + 13}" text-anchor="middle" fill="${COL[p.kind]}">${WORD[p.kind]} ${Math.round((pe - p.start) / 60_000)}${NB}min</text>`;
      cursor = Math.max(cursor, pe);
    }
    if (cursor < now && D.state !== 'failed') g += `<rect x="${X(cursor)}" y="${top}" width="${Math.max(1, X(now) - X(cursor))}" height="${laneH}" fill="var(--ink-3)" rx="2"/>`;
    if (D.state === 'failed' && D.lastEventAt != null && D.lastEventAt < now) g += `<rect x="${X(D.lastEventAt)}" y="${top}" width="${Math.max(2, X(now) - X(D.lastEventAt))}" height="${laneH}" fill="none" stroke="var(--fail)" stroke-dasharray="3 3" rx="2"/>`;
  }
  // projection (an estimate is drawn dashed, never solid)
  const f = D.finish;
  if (f.kind === 'estimate' && f.t > now) {
    g += `<rect x="${X(now)}" y="${top}" width="${Math.max(0, X(f.t) - X(now))}" height="${laneH}" fill="none" stroke="var(--line-strong)" stroke-dasharray="4 3" rx="2"/>`;
    if (X(f.t) - X(now) > 60) g += `<text x="${(X(now) + X(f.t)) / 2}" y="${top + laneH + 13}" text-anchor="middle">projected</text>`;
  }
  // the temperature line, broken where readings are missing for over a minute (no invented continuity)
  const sm = D.samples;
  if (sm.length) {
    let seg: string[] = [], prev: Sample | null = null;
    const flush = () => { if (seg.length > 1) g += `<polyline points="${seg.join(' ')}" fill="none" stroke="var(--ink)" stroke-width="1.75" stroke-linejoin="round"/>`; else if (seg.length === 1) { const [x, y] = seg[0]!.split(','); g += `<circle cx="${x}" cy="${y}" r="2" fill="var(--ink)"/>`; } seg = []; };
    for (const s of sm) { if (prev && s.t - prev.t > LIMITS.staleS * 1000) flush(); seg.push(`${X(s.t).toFixed(1)},${Y(s.v).toFixed(1)}`); prev = s; }
    flush();
  }
  // cooling projection: measured rate to the resume line
  if (D.state === 'cooling' && D.cooling?.etaS != null && D.temp) {
    g += `<line x1="${X(D.temp.t)}" y1="${Y(D.temp.v)}" x2="${X(D.now + D.cooling.etaS * 1000)}" y2="${Y(LIMITS.resumeAt)}" stroke="var(--cool)" stroke-width="1.75" stroke-dasharray="4 4"/>`;
  }
  for (const q of D.pauses) {
    if (q.kind !== 'heat' || q.start > now) continue;
    if (q.fromTemp != null) { const x = X(q.start), y = Y(q.fromTemp); g += `<path d="M${x - 5},${y - 11} L${x + 5},${y - 11} L${x},${y - 3} Z" fill="var(--fail)"/>`; }
    if (q.end != null) { const at = nearestSample(sm, q.end); g += `<circle cx="${X(q.end)}" cy="${Y(at && Math.abs(at.t - q.end) < 30_000 ? at.v : LIMITS.resumeAt)}" r="3.5" fill="var(--surface)" stroke="var(--cool)" stroke-width="1.75"/>`; }
  }
  const failed = D.state === 'failed', lastT = sm.at(-1)?.t;
  g += `<line x1="${nowX}" x2="${nowX}" y1="${top - 4}" y2="${ty1}" stroke="var(--ink)" stroke-width="1"/><text x="${Math.min(nowX, W - Rm - 6)}" y="${top - 8}" text-anchor="end" class="lbl-strong" ${failed ? 'style="fill:var(--fail)"' : ''}>${ended ? (failed && lastT ? `ended, no readings since ${hm(lastT)}` : 'end') : D.linkDown && lastT ? `now, no readings since ${hm(lastT)}` : 'now'}</text>`;
  g += lim;
  const st = ctx.selT();
  if (st != null) { const x = X(st); g += `<line x1="${x}" x2="${x}" y1="${top}" y2="${ty1}" stroke="var(--focus)" stroke-width="2" stroke-dasharray="2 2"/>`; }
  const c = ctx.cursor();
  if (c != null) { const pt = nearestSample(sm, c); if (pt) { const x = X(pt.t), y = Y(pt.v); g += `<line x1="${x}" x2="${x}" y1="${ty0 - 6}" y2="${ty1}" stroke="var(--ink-2)"/><circle cx="${x}" cy="${y}" r="4.5" fill="var(--surface)" stroke="var(--ink)" stroke-width="2"/>`; } }
  if (!sm.length) g += `<text x="${(L + W - Rm) / 2}" y="${(ty0 + ty1) / 2}" text-anchor="middle">No temperature readings in this run yet</text>`;
  const sig = g + W;
  const e = svg as unknown as SVGElement & { _sig?: string };
  if (e._sig !== sig) { e._sig = sig; svg.innerHTML = g; }
  geom = { X, Y, x0, x1, L, Rm, W, H };

  const tl = D.totals;
  const waitKinds = new Set(D.pauses.filter(p => p.kind !== 'heat').map(p => p.kind));
  const waitSw = waitKinds.has('stuck') ? 'stuck' : waitKinds.has('needs') ? 'wait' : 'idle';
  const legend = `<span><i class="sw work"></i>working ${durMin(tl.work)}</span><span><i class="sw cool"></i>cooling ${durMin(tl.cool)}</span>` +
    (tl.wait >= 1 ? `<span><i class="sw ${waitSw}"></i>held ${dur(tl.wait)}</span>` : '') + (f.kind === 'estimate' && f.t > now ? '<span><i class="sw proj"></i>projected</span>' : '');
  const lg = $('spine-legend') as HTMLElement & { _html?: string };
  if (lg._html !== legend) { lg._html = legend; lg.innerHTML = legend; }
}

export function tAtX(clientX: number): number {
  if (!geom) return Date.now();
  const rect = $('chart').getBoundingClientRect();
  const x = (clientX - rect.left) * (geom.W / rect.width);
  return geom.x0 + ((x - geom.L) / (geom.W - geom.L - geom.Rm)) * (geom.x1 - geom.x0);
}

function whyAt(D: Derived, pt: Sample): string {
  const t = pt.t;
  const b = nearestBatt(D, t);
  let line = `<b>${clock(t)}</b> ${c1(pt.v)}${NB}°C${b != null ? `, battery ${Math.round(b)}${NB}%` : ''}<br>`;
  const q = D.pauses.find(p => t >= p.start && t < (p.end ?? Infinity));
  if (q) {
    if (q.kind === 'heat') {
      line += `<b>Cooling.</b> Paused at ${clock(q.start)} after step ${q.afterStep}${q.fromTemp != null ? `, because the battery read ${c1(q.fromTemp)}${NB}°C (the limit is ${LIMITS.pauseAt}${NB}°C)` : ''}. `;
      line += q.end != null ? `Resumed at ${clock(q.end)} (${dur((q.end - q.start) / 1000)}).` : `Resumes by itself at ${LIMITS.resumeAt}${NB}°C.`;
    } else {
      const ex = explainPause(q.msg);
      line += `<b>${esc(WORD[q.kind].replace(/^./, c => c.toUpperCase()))}.</b> ${esc(ex.words ?? q.msg)}. Nothing is tapped, so the phone cools slowly.`;
    }
  } else {
    const s = D.steps.find(r => r.t >= t);
    if (s) line += `Working on step ${s.n}: ${esc(stepLabel(s.msg) || s.stepId)}.`;
    else if (D.live) line += `Working on step ${D.live.n}${D.live.label ? `: ${esc(D.live.label)}` : ''}.`;
    if (pt.v >= LIMITS.nearOn && pt.v < LIMITS.pauseAt) line += ` ${c1(LIMITS.pauseAt - pt.v)}${NB}°C below the pause limit.`;
  }
  return line;
}
function nearestBatt(D: Derived, t: number): number | null {
  const b = nearestSample(D.batts, t);
  return b && Math.abs(b.t - t) < 60_000 ? b.v : null; // a reading over a minute away is not "at" this time
}

export function showTip(ctx: ChartCtx, t: number, keyboard = false) {
  const D = ctx.get();
  const pt = nearestSample(D.samples, t); if (!pt) return;
  ctx.setCursor(pt.t); renderChart(ctx);
  if (!geom) return;
  const tip = $('tip'), wrap = $('chart-wrap');
  const html = whyAt(D, pt);
  if ((tip as HTMLElement & { _html?: string })._html !== html) { (tip as HTMLElement & { _html?: string })._html = html; tip.innerHTML = html; }
  tip.classList.add('on');
  const scale = wrap.clientWidth / geom.W;
  const x = geom.X(pt.t) * scale, y = geom.Y(pt.v) * scale, tw = Math.min(300, wrap.clientWidth - 16);
  tip.style.maxWidth = tw + 'px';
  // quadrant flip: right half -> tip to the left of the point; lower half -> tip above it
  const right = x > wrap.clientWidth / 2, lower = y > (geom.H * scale) / 2;
  const w = Math.min(tw, tip.offsetWidth || tw), h = tip.offsetHeight || 60;
  const left = Math.max(0, Math.min(wrap.clientWidth - w, right ? x - w - 12 : x + 12));
  const topPx = lower ? Math.max(0, y - h - 12) : y + 12 + (keyboard ? 4 : 0);
  tip.dataset.q = (lower ? 'b' : 't') + (right ? 'r' : 'l');
  tip.style.transform = `translate(${left}px, ${topPx}px)`;
}
export function hideTip(ctx: ChartCtx) { ctx.setCursor(null); $('tip').classList.remove('on'); renderChart(ctx); }
