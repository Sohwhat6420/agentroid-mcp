// derive(): the state shown, the cooling ETA, hysteresis, the finish estimate, staleness.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { derive, classifyPause, nextHeat, slopePerMin, explainPause, HEAT0, LIMITS } from '../.test-build/model/derive.js';
import { initialState, reduce } from '../.test-build/model/reducer.js';
import { V, loadScenario } from './helpers.mjs';

const expected = {
  running: 'running', cooling: 'cooling', waiting: 'waiting', stuck: 'stuck', stopped: 'stopped', failed: 'failed',
  done: 'done', secure: 'running', stale: 'running', 'invalid-job': 'unavailable', 'invalid-events': 'running',
  'repo-stopped': 'stopped', empty: 'queued',
};
for (const [name, state] of Object.entries(expected)) {
  test(`scenario ${name} shows "${state}"`, () => {
    const { state: s, now } = loadScenario(name);
    assert.equal(derive(s, now).state, state);
  });
}

test('connecting before any answer, unavailable once a payload has failed', () => {
  const T = Date.now();
  assert.equal(derive(initialState(T), T).state, 'connecting');
  const s = reduce(initialState(T), { type: 'job-failed', at: T, why: 'x' }, V);
  assert.equal(derive(s, T).state, 'unavailable');
});

test('cooling: the ETA is measured from this pause\'s own readings and is plausible', () => {
  const { state: s, now } = loadScenario('cooling');
  const d = derive(s, now);
  assert.ok(d.cooling, 'cooling info present');
  assert.equal(d.cooling.measured, true);
  assert.ok(d.cooling.etaS > 0 && d.cooling.etaS < 20 * 60, `eta ${d.cooling.etaS}`);
  const expectS = ((d.temp.v - LIMITS.resumeAt) / d.cooling.ratePerMin) * 60;
  assert.ok(Math.abs(expectS - d.cooling.etaS) < 1);
});

test('running near the limit: the finish estimate includes a cooling pause still to come', () => {
  const { state: s, now } = loadScenario('running');
  const d = derive(s, now);
  assert.equal(d.finish.kind, 'estimate');
  assert.ok(d.finish.pauses >= 1, `pauses expected: ${d.finish.pauses}`);
  assert.ok(d.finish.t > now);
});

test('held and ended states never show a finish time that moves', () => {
  assert.equal(derive(loadScenario('stuck').state, loadScenario('stuck').now).finish.kind, 'hold');
  const done = loadScenario('done');
  assert.equal(derive(done.state, done.now).finish.kind, 'ended');
});

test('stale: link down marks readings and the frame stale', () => {
  const { state: s, now } = loadScenario('stale');
  const d = derive(s, now);
  assert.equal(d.linkDown, true);
  assert.equal(d.readingsStale, true);
  assert.equal(d.frame.stale, true);
});

test('a reading older than 60 s is stale even with the link up', () => {
  const { state: s, now } = loadScenario('running');
  assert.equal(derive(s, now).readingsStale, false);
  assert.equal(derive(s, now + 61_000 + 30_000).readingsStale, true);
});

test('classifyPause reads the engine\'s pause message prefixes', () => {
  assert.equal(classifyPause('heat: 42.1 °C; lease kept'), 'heat');
  assert.equal(classifyPause('needs_attention: unknown_overlay at s3; lease kept'), 'needs');
  assert.equal(classifyPause('awaiting_human: awaiting_human; lease kept'), 'needs');
  assert.equal(classifyPause('paused: paused_by_user; lease kept'), 'user');
  assert.equal(classifyPause('something else'), 'other');
  assert.equal(classifyPause(), 'other');
});

test('explainPause gives words for the engine detail code', () => {
  const r = explainPause('needs_attention: permission_allow at s12; lease kept');
  assert.equal(r.code, 'permission_allow');
  assert.match(r.words, /permission/);
  assert.equal(r.rest, 'at s12');
});

test('hysteresis: "near the limit" turns on at 40.5 and off only at 40.0', () => {
  let h = nextHeat(HEAT0, 0, 40.4); assert.equal(h.near, false);
  h = nextHeat(h, 0, 40.5); assert.equal(h.near, true);
  h = nextHeat(h, 0, 40.2); assert.equal(h.near, true, 'still on between the thresholds');
  h = nextHeat(h, 0, 40.0); assert.equal(h.near, false);
});

test('hysteresis: "rising" needs 0.08 °C/min to start and falls back only under 0.03', () => {
  let h = nextHeat(HEAT0, 0.05, 38); assert.equal(h.dir, 'steady');
  h = nextHeat(h, 0.09, 38); assert.equal(h.dir, 'rising');
  h = nextHeat(h, 0.04, 38); assert.equal(h.dir, 'rising');
  h = nextHeat(h, 0.02, 38); assert.equal(h.dir, 'steady');
  h = nextHeat(h, -0.1, 38); assert.equal(h.dir, 'falling');
});

test('slopePerMin needs 3 readings across 45 s; otherwise it says nothing', () => {
  const t0 = 0;
  assert.equal(slopePerMin([{ t: t0, v: 1 }, { t: t0 + 60_000, v: 2 }], 0, 1e9), null);
  assert.equal(slopePerMin([{ t: 0, v: 1 }, { t: 10_000, v: 1 }, { t: 20_000, v: 1 }], 0, 1e9), null);
  const s = slopePerMin([{ t: 0, v: 36 }, { t: 30_000, v: 36.5 }, { t: 60_000, v: 37 }], 0, 1e9);
  assert.ok(Math.abs(s - 1) < 1e-9);
});

test('step durations exclude paused time, and retries are counted from "attempt N of M failed"', () => {
  const T = Date.parse('2026-10-20T05:00:00Z');
  const line = (seq, sec, o) => JSON.stringify({ v: 1, ts: new Date(T + sec * 1000).toISOString(), run_id: 'r1', seq, ...o });
  let s = reduce(initialState(T), { type: 'job', at: T, payload: { job_id: 'j', run_id: 'r1', device: 'd', flow: 'f', status: 'running', steps_planned: 3, steps_done: 1 } }, V);
  s = reduce(s, { type: 'events', at: T, text: [
    line(0, 0, { kind: 'progress', msg: 'started' }), // "started" is not a kind in progress.json
    line(1, 5, { kind: 'progress', step_id: 's1', msg: 'attempt 1 of 3 failed: no node; retrying' }),
    line(2, 10, { kind: 'pause', msg: 'heat: 42.0 °C; lease kept', temp_c: 42 }),
    line(3, 70, { kind: 'resume', temp_c: 37 }),
    line(4, 80, { kind: 'step', step_id: 's1', msg: 'Tap: ok' }),
  ].join('\n') }, V);
  const d = derive(s, T + 90_000);
  assert.equal(d.steps.length, 1);
  assert.equal(d.steps[0].dur, 20, '80 s minus the 60 s pause');
  assert.equal(d.steps[0].tries, 2);
  assert.equal(d.retried, 1);
  assert.equal(d.totals.cool, 60);
});
