// Writes the job view's scenario fixtures to public/fixtures/<name>/, laid out exactly like the
// HTTP endpoints bridged will serve (WIRING.md):
//   job.json  -> GET /jobs/:id          (schemas/job-view.json)
//   jobs.json -> GET /jobs              (schemas/job-view.json, list form)
//   events.ndjson -> GET /jobs/:id/events (schemas/progress.json, one event per line)
//   frame.json -> GET /jobs/:id/frame.json (contract/frame-meta.schema.json, proposed)
//   frame.jpg  -> GET /jobs/:id/frame.jpg  (written by render-frames.mjs from scenes/<name>.json)
//   recorder.json -> GET /jobs/:id/recorder.json (contract/recorder.schema.json, proposed)
//   manifest.json -> fixture-only: the frozen "now", the link mode and a label
//
// The numbers come from the same deterministic simulation as design/v2.html (one 200-step camera
// sweep on the POCO X6 Pro, seed 20261020), so the build can be compared with the approved mock.
// Every file is validated against its schema by the unit tests, not here.
import { mkdirSync, writeFileSync, readFileSync, rmSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const app = join(here, '..');
const OUT = join(app, 'public', 'fixtures');
const SCENES = join(app, 'scenes');
const REPO_FIX = join(app, '..', '..', 'schemas', 'fixtures');

const T0 = Date.parse('2026-10-20T13:02:07+08:00') / 1000;
const JOB_ID = '7f3a', RUN_ID = 'r-20261020-1302-7f3a';
const OWNER = { pid: 4127, job_id: JOB_ID, session: 'cli-0930' };
const CHANGES = [
  { key: 'screen_brightness', ns: 'system', before: '128', applied: '1', owner: OWNER, scope: 'job', ts: '2026-10-20T05:02:05Z' },
  { key: 'peak_refresh_rate', ns: 'system', before: '120', applied: '60', owner: OWNER, scope: 'job', ts: '2026-10-20T05:02:05Z' },
  { key: 'screen_off_timeout', ns: 'system', before: '30000', applied: '600000', owner: OWNER, scope: 'job', ts: '2026-10-20T05:02:06Z' },
];
const PAUSE_AT = 42, RESUME_AT = 37, COOL_RATE = 0.55 / 60;
let seed = 20261020; const rnd = () => (seed = (seed * 1664525 + 1013904223) >>> 0) / 4294967296; const R = (a, b) => a + (b - a) * rnd();
const ISOS = [100, 200, 400, 800, 1600], SHUT = ['1/500', '1/250', '1/125', '1/60', '1/30', '1/15', '1/8', '1/4'];
const ISO_X = { 100: 200, 200: 405, 400: 610, 800: 815, 1600: 1020 };
const KIND = {
  open_iso: { label: () => 'Open the ISO wheel', sel: () => 'id:camera:id/iso_chip', tap: () => [260, 2003] },
  set_iso: { label: s => `ISO to ${s.iso}`, sel: s => `text:"${s.iso}" in id:camera:id/iso_wheel`, tap: s => [ISO_X[s.iso], 1820] },
  set_shutter: { label: s => `Shutter to ${s.sh} s`, sel: () => 'near_text:"S" > id:camera:id/shutter_dial', tap: () => [520, 2003] },
  shoot: { label: s => `Shoot (ISO ${s.iso}, ${s.sh} s)`, sel: () => 'id:camera:id/shutter_button', tap: () => [610, 2500] },
  verify: { label: () => 'Check the new JPEG', sel: () => 'pull_new DCIM/Camera (no tap)', tap: () => null },
};
const RETRY = { 47: 'the S chip still showed the old value after the first turn', 104: 'no new file on the first check (the camera was still saving)', 151: 'the ISO wheel was still scrolling' };
const steps = [];
for (let c = 0; c < 40; c++) for (const k of ['open_iso', 'set_iso', 'set_shutter', 'shoot', 'verify']) steps.push({ k, combo: c, iso: ISOS[Math.floor(c / 8)], sh: SHUT[c % 8] });
const pauses = [], bp = [];
(function simulate() {
  let t = T0, temp = 38.6; bp.push([t, temp]);
  steps.forEach((s, i) => {
    s.i = i; s.n = i + 1; s.id = 's' + String(i + 1).padStart(3, '0');
    if (temp >= PAUSE_AT) { const p = { start: t, from: temp, afterIdx: i - 1 }; t += (temp - RESUME_AT) / COOL_RATE; temp = RESUME_AT; p.end = t; pauses.push(p); bp.push([t, temp]); }
    const think = R(1.8, 3.2);
    let act = { open_iso: R(.6, 1.1), set_iso: R(.7, 1.3), set_shutter: R(.9, 1.6), shoot: R(.3, .5), verify: R(1.5, 3) }[s.k];
    let settle = { open_iso: R(.5, .9), set_iso: R(.6, 1), set_shutter: R(.6, 1), shoot: R(3.5, 7), verify: R(.2, .4) }[s.k];
    s.tries = RETRY[i] ? 2 : 1; act *= s.tries; settle *= s.tries;
    s.start = t; s.actStart = t + think; s.end = t + think + act + settle;
    temp += (s.end - t) * (s.k === 'shoot' ? 0.017 : 0.0085); t = s.end; bp.push([t, temp]);
  });
})();
const END = steps[199].end;
function tempAt(t) { if (t <= bp[0][0]) return bp[0][1]; for (let j = 1; j < bp.length; j++) if (t <= bp[j][0]) { const [a, ta] = bp[j - 1], [b, tb] = bp[j]; return ta + (tb - ta) * ((t - a) / ((b - a) || 1)); } return bp[bp.length - 1][1]; }
const noise = t => (Math.sin(t * 12.9898) * 43758.5453 % 1) * 0.05;
const battAt = t => Math.min(80, 72.4 + 0.28 * (t - T0) / 60);
const pauseAt = t => pauses.find(p => t >= p.start && t < p.end);
const iso = t => new Date(Math.round(t * 1000)).toISOString().replace(/\.\d{3}Z$/, (m) => (m === '.000Z' ? 'Z' : m));
const r1 = v => Math.round(v * 10) / 10;
function settingsAt(t) {
  let isoV = 'AUTO', sh = 'AUTO', dial = false;
  for (const s of steps) { if (s.end > t) { if (s.k === 'set_iso' && steps[s.i - 1].end <= t) dial = true; break; } if (s.k === 'set_iso') isoV = s.iso; if (s.k === 'set_shutter') sh = s.sh; }
  return { iso: isoV, sh, dial };
}

/**
 * Build the events of a run up to `simNow` (the last moment the engine acted) and, for held states,
 * idle readings up to `readUntil`. `ov` is the overlay: what happened at simNow.
 */
function eventsFor({ simNow, readUntil = simNow, ov = null }) {
  const out = [];
  const push = (t, o) => out.push({ t, o });
  push(T0, { kind: 'progress', msg: 'job started: camera-sweep-200 on POCO X6 Pro', temp_c: r1(tempAt(T0)), batt: r1(battAt(T0)), display: 0 });
  // readings every 30 s (2.11a: progress at least every 30 s), screen state in display
  for (let t = T0 + 30; t <= readUntil; t += 30) {
    const working = t <= simNow;
    const v = working ? tempAt(t) + noise(t) : Math.max(35.5, tempAt(simNow) - 0.25 * (t - simNow) / 60) + noise(t);
    push(t, { kind: 'progress', temp_c: r1(v), batt: r1(battAt(Math.min(t, simNow))), display: 0 });
  }
  let done = 0;
  for (const s of steps) {
    if (s.end > simNow) break;
    if (s.tries > 1) push(s.actStart + 1, { kind: 'progress', step_id: s.id, msg: `attempt 1 of 3 failed: ${RETRY[s.i]}; retrying` });
    done++;
    const elapsed = s.end - T0;
    const eta = elapsed / done * (200 - done);
    push(s.end, { kind: 'step', step_id: s.id, msg: `${KIND[s.k].label(s)}: ok`, eta_s: Math.round(eta), display: 0 });
  }
  for (const p of pauses) {
    if (p.start > simNow) break;
    push(p.start, { kind: 'pause', step_id: steps[p.afterIdx + 1].id, msg: `heat: battery ${p.from.toFixed(1)} °C >= ${PAUSE_AT} °C; screen asleep; lease kept`, temp_c: r1(p.from) });
    if (p.end <= simNow) push(p.end, { kind: 'resume', step_id: steps[p.afterIdx + 1].id, msg: `heat: battery ${RESUME_AT.toFixed(1)} °C; resumed`, temp_c: RESUME_AT });
  }
  if (ov) {
    const s = steps[ov.idx];
    if (ov.type === 'waiting') push(simNow, { kind: 'pause', step_id: s.id, msg: 'needs_attention: unknown_overlay at ' + s.id + ' (com.google.android.permissioncontroller); lease kept' });
    if (ov.type === 'stuck') {
      push(simNow - 110, { kind: 'progress', step_id: s.id, msg: 'attempt 1 of 3 failed: selector id:camera:id/iso_chip matched 0 nodes; retrying' });
      push(simNow - 70, { kind: 'progress', step_id: s.id, msg: 'attempt 2 of 3 failed: selector id:camera:id/iso_chip matched 0 nodes; retrying' });
      push(simNow, { kind: 'stuck', step_id: s.id, msg: 'selector id:camera:id/iso_chip matched 0 nodes after 3 tries' });
    }
    if (ov.type === 'failed') push(simNow, { kind: 'error', step_id: s.id, msg: 'link lost: no response from the device for 30 s' });
    if (ov.type === 'stopped') push(simNow + 4, { kind: 'done', step_id: s.id, msg: 'stopped_by_user: stopped from the job view; restored 3 settings' });
    if (ov.type === 'done') push(simNow + 4, { kind: 'done', msg: 'done: 200 steps completed, 203 executed; restored 3 settings' });
  }
  // the reading cadence must not run past the end of a terminal run
  const stopAt = ov && ['failed', 'stopped', 'done'].includes(ov.type) ? simNow + 4 : Infinity;
  const kept = out.filter(e => e.t <= stopAt || e.o.kind !== 'progress');
  kept.sort((a, b) => a.t - b.t);
  return kept.map((e, seq) => ({ v: 1, ts: iso(e.t), run_id: RUN_ID, seq, ...e.o }));
}

function frameFor({ t, scene, nextTap = null, miss = null, secure = false, stepId }) {
  const meta = { v: 1, run_id: RUN_ID, ts: iso(t), bytes: 0, w: 610, h: 1356, screen_w: 1220, screen_h: 2712, secure, display: 0 };
  if (stepId) meta.step_id = stepId;
  if (nextTap) meta.next_tap = nextTap;
  if (miss) meta.miss = miss;
  return { meta, scene: secure ? null : { t, ...settingsAt(t), ...scene } };
}
const tapOf = s => { const tp = KIND[s.k].tap(s); return tp ? { step_id: s.id, x: tp[0], y: tp[1], selector: KIND[s.k].sel(s), label: KIND[s.k].label(s) } : null; };
const lastFrameT = (t) => { let f = null; for (let x = T0 + 4; x <= t; x += 30) if (!pauseAt(x)) f = x; return f; };

function job(status, done, extra = {}) {
  return { job_id: JOB_ID, run_id: RUN_ID, device: 'POCO X6 Pro', flow: 'camera-sweep-200', status, steps_planned: 200, steps_done: done, events_url: `/jobs/${JOB_ID}/events`, frame_url: `/jobs/${JOB_ID}/frame.jpg`, changes_in_force: CHANGES, ...extra };
}
const OTHER = { job_id: '9c1e', run_id: 'r-20261020-0915-9c1e', device: 'POCO F5 Pro', flow: 'settings-audit', status: 'done', steps_planned: 24, steps_done: 24 };

const scenarios = {};
const doneBy = t => steps.filter(s => s.end <= t).length;
{ // running: step 128 in flight, the battery near the limit
  const simNow = steps[127].actStart + 0.5, s = steps[127];
  scenarios.running = { label: 'Running', now: simNow, job: job('running', doneBy(simNow)), events: eventsFor({ simNow }), frame: frameFor({ t: lastFrameT(simNow), nextTap: tapOf(s), stepId: steps[126].id, scene: {} }) };
}
{ // cooling: 250 s into the second heat pause
  const p = pauses[1], simNow = p.start + 250, s = steps[p.afterIdx + 1];
  const until = { ...CHANGES[0], key: 'stay_on_while_plugged_in', ns: 'global', before: '0', applied: '7', scope: 'until', expires: '2026-10-20T16:00:00+08:00', ts: '2026-10-20T05:02:06Z' };
  scenarios.cooling = { label: 'Cooling (heat pause)', now: simNow, job: job('paused', doneBy(simNow), { changes_in_force: [...CHANGES, until] }), events: eventsFor({ simNow }), frame: frameFor({ t: lastFrameT(p.start), stepId: steps[p.afterIdx].id, scene: {} }), extraNote: s.id };
}
{ // waiting for you: a permission pop-up at step 163
  const idx = 162, simNow = steps[idx].actStart, now = simNow + 48;
  scenarios.waiting = { label: 'Waiting for you', now, job: job('paused', idx), events: eventsFor({ simNow, readUntil: now, ov: { type: 'waiting', idx } }), frame: frameFor({ t: simNow + 2 + Math.floor(46 / 30) * 30, stepId: steps[idx - 1].id, scene: { popup: true } }) };
}
{ // stuck: step 171 cannot find the ISO chip; the recorder has a crop
  const idx = 170, simNow = steps[idx].actStart, now = simNow + 131, s = steps[idx];
  const miss = { step_id: s.id, x: 260, y: 2003, selector: 'id:camera:id/iso_chip', reason: 'not in the accessibility tree' };
  scenarios.stuck = {
    label: 'Stuck', now, job: job('stuck', idx), events: eventsFor({ simNow, readUntil: now, ov: { type: 'stuck', idx } }),
    frame: frameFor({ t: simNow + 2 + Math.floor(129 / 30) * 30, miss, stepId: steps[idx - 1].id, scene: { photoMode: true } }),
    recorder: { v: 1, run_id: RUN_ID, ts: iso(simNow), step_id: s.id, reason: 'selector id:camera:id/iso_chip matched 0 nodes after 3 tries', trees: 10, frames: 3, crop: { url: `/jobs/${JOB_ID}/recorder/crop.jpg`, x: 60, y: 1700, w: 1100, h: 520, bytes: 0 } },
  };
}
{ // stopped by the user after step 120
  const idx = 119, simNow = steps[idx].end, now = simNow + 65;
  scenarios.stopped = { label: 'Stopped', now, job: job('stopped', idx + 1, { changes_in_force: [] }), events: eventsFor({ simNow, ov: { type: 'stopped', idx } }), frame: frameFor({ t: lastFrameT(simNow + 4), stepId: steps[idx].id, scene: {} }) };
}
{ // failed: the phone stopped answering during step 184; the page is 161 s past the last event
  const idx = 183, simNow = steps[idx].actStart, now = simNow + 161;
  scenarios.failed = { label: 'Failed', now, job: job('error', idx), events: eventsFor({ simNow, ov: { type: 'failed', idx } }), frame: frameFor({ t: lastFrameT(simNow), stepId: steps[idx - 1].id, scene: {} }) };
}
{ // done; one permanent change was asked for and stays (R2, shown, not restored)
  const simNow = END, now = END + 185;
  const perm = { key: 'stay_on_while_plugged_in', ns: 'global', before: '0', applied: '3', owner: OWNER, scope: 'permanent', ts: '2026-10-20T05:02:06Z' };
  scenarios.done = { label: 'Done', now, job: job('done', 200, { changes_in_force: [perm] }), events: eventsFor({ simNow, ov: { type: 'done', idx: 199 } }), frame: frameFor({ t: lastFrameT(END + 4), stepId: steps[199].id, scene: {} }) };
}
{ // a secure screen: the app in front blocks screenshots (FLAG_SECURE), so there is no picture
  const simNow = steps[61].actStart + 0.5, s = steps[61];
  scenarios.secure = { label: 'Secure screen', now: simNow, job: job('running', doneBy(simNow)), events: eventsFor({ simNow }), frame: frameFor({ t: lastFrameT(simNow), nextTap: tapOf(s), stepId: steps[60].id, secure: true }) };
}
{ // stale: running, but the laptop stopped answering 3 min ago
  const simNow = steps[90].actStart, now = simNow + 190;
  scenarios.stale = { label: 'Stale (laptop not answering)', now, link: 'down', job: job('running', doneBy(simNow)), events: eventsFor({ simNow }), frame: frameFor({ t: lastFrameT(simNow), nextTap: tapOf(steps[90]), stepId: steps[89].id, scene: {} }) };
}

// contract cases built from the repo's own fixtures (schemas/fixtures), so the page is shown against them too
const readRepo = p => JSON.parse(readFileSync(join(REPO_FIX, p), 'utf8'));
scenarios['invalid-job'] = { label: 'Invalid job payload (repo fixture)', now: T0 + 60, jobRaw: readRepo('job-view/invalid-status-not-in-enum.json'), events: [], frame: null };
{
  const simNow = steps[40].end;
  const evs = eventsFor({ simNow });
  const bad = [readRepo('progress/invalid-battery-over-100.json'), readRepo('progress/invalid-kind-not-in-enum.json'), readRepo('progress/invalid-missing-seq.json')]
    .map(o => ({ ...o, run_id: RUN_ID, seq: (o.seq ?? 0) + 90000 }));
  delete bad[2].seq; // keep the repo fixture's defect: no seq
  scenarios['invalid-events'] = { label: 'Some invalid event lines (repo fixtures)', now: simNow + 5, job: job('running', doneBy(simNow)), events: evs, badLines: bad, frame: frameFor({ t: lastFrameT(simNow), nextTap: tapOf(steps[41]), stepId: steps[40].id, scene: {} }) };
}
{
  const j = readRepo('job-view/valid-stopped-with-changes.json');
  const ev = [readRepo('progress/valid-step.json'), readRepo('progress/valid-pause-minimal.json')].map(o => ({ ...o, run_id: j.run_id }));
  scenarios['repo-stopped'] = { label: 'Repo fixture: stopped with a change still in force', now: Date.parse('2026-10-19T10:06:00Z') / 1000, jobRaw: j, events: ev, frame: null, jobId: j.job_id };
}
scenarios.empty = { label: 'Queued, no events yet', now: T0 - 20, job: { ...job('queued', 0), changes_in_force: [] }, events: [], frame: null };

// ---------- write
if (existsSync(OUT)) rmSync(OUT, { recursive: true });
if (existsSync(SCENES)) rmSync(SCENES, { recursive: true });
mkdirSync(OUT, { recursive: true }); mkdirSync(SCENES, { recursive: true });
const index = [];
for (const [name, sc] of Object.entries(scenarios)) {
  const dir = join(OUT, name); mkdirSync(dir, { recursive: true });
  const jobObj = sc.jobRaw ?? sc.job;
  writeFileSync(join(dir, 'job.json'), JSON.stringify(jobObj, null, 1) + '\n');
  writeFileSync(join(dir, 'jobs.json'), JSON.stringify([jobObj, OTHER].filter(j => j.job_id), null, 1) + '\n');
  const lines = sc.events.map(e => JSON.stringify(e));
  if (sc.badLines) { lines.splice(10, 0, JSON.stringify(sc.badLines[0])); lines.splice(20, 0, JSON.stringify(sc.badLines[1])); lines.splice(30, 0, JSON.stringify(sc.badLines[2])); lines.splice(40, 0, '{"v":1,"ts":"2026-10-20T05:10:00Z",'); }
  writeFileSync(join(dir, 'events.ndjson'), lines.join('\n') + (lines.length ? '\n' : ''));
  if (sc.frame) {
    writeFileSync(join(dir, 'frame.json'), JSON.stringify(sc.frame.meta, null, 1) + '\n');
    if (sc.frame.scene) writeFileSync(join(SCENES, `${name}.json`), JSON.stringify({ name, kind: 'frame', ...sc.frame.scene, batt: battAt(sc.frame.scene.t) }) + '\n');
  }
  if (sc.recorder) {
    writeFileSync(join(dir, 'recorder.json'), JSON.stringify(sc.recorder, null, 1) + '\n');
    writeFileSync(join(SCENES, `${name}.crop.json`), JSON.stringify({ name, kind: 'crop', crop: sc.recorder.crop, ...sc.frame.scene, batt: battAt(sc.frame.scene.t) }) + '\n');
  }
  // the files this scenario has, so the page never asks for one that is absent (a 404 here is noise;
  // a 404 from live bridged for recorder.json is normal and is not logged as an error there either)
  const files = ['job.json', 'jobs.json', 'events.ndjson', ...(sc.frame ? ['frame.json'] : []), ...(sc.recorder ? ['recorder.json'] : [])];
  const manifest = { name, label: sc.label, job_id: sc.jobId ?? JOB_ID, now: iso(sc.now), link: sc.link ?? 'fixture', files };
  writeFileSync(join(dir, 'manifest.json'), JSON.stringify(manifest, null, 1) + '\n');
  index.push({ name, label: sc.label });
  console.log(name.padEnd(16), `${lines.length} lines`, sc.frame ? (sc.frame.meta.secure ? 'secure frame' : 'frame') : 'no frame');
}
writeFileSync(join(OUT, 'index.json'), JSON.stringify(index, null, 1) + '\n');
console.log(`${index.length} scenarios written to ${OUT}`);
