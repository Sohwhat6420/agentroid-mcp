// The reducer: validation at the edge, dedupe by seq, foreign runs dropped, a bad payload never replaces good data.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { initialState, reduce, mergeEvents } from '../.test-build/model/reducer.js';
import { V } from './helpers.mjs';

const T = Date.parse('2026-10-20T05:00:00Z');
const job = (o = {}) => ({ job_id: 'j1', run_id: 'r1', device: 'poco-x6', flow: 'f', status: 'running', steps_planned: 10, steps_done: 0, ...o });
const ev = (seq, o = {}) => JSON.stringify({ v: 1, ts: new Date(T + seq * 1000).toISOString(), run_id: 'r1', seq, kind: 'step', step_id: `s${seq}`, msg: 'ok', ...o });

test('a valid job is taken; an invalid one sets a problem and keeps the last good job', () => {
  let s = reduce(initialState(T), { type: 'job', at: T, payload: job() }, V);
  assert.equal(s.job.value.job_id, 'j1');
  assert.equal(s.jobProblem, null);
  s = reduce(s, { type: 'job', at: T + 1, payload: job({ status: 'exploded' }) }, V);
  assert.equal(s.job.value.status, 'running', 'the bad payload did not replace the good one');
  assert.match(s.jobProblem.why, /job-view\.json/);
});

test('the server Date header sets the clock skew', () => {
  const s = reduce(initialState(T), { type: 'job', at: T + 5000, payload: job(), serverDate: T }, V);
  assert.equal(s.skew, 5000);
});

test('events are deduplicated by seq and kept in seq order (an SSE reconnect replays)', () => {
  let s = reduce(initialState(T), { type: 'job', at: T, payload: job() }, V);
  s = reduce(s, { type: 'events', at: T, text: [ev(1), ev(2), ev(3)].join('\n') }, V);
  s = reduce(s, { type: 'events', at: T, text: [ev(2), ev(3), ev(5), ev(4)].join('\n') }, V);
  assert.deepEqual(s.events.map(e => e.seq), [1, 2, 3, 4, 5]);
  assert.equal(s.raw.length, 5, 'raw keeps exactly the accepted lines');
});

test('mergeEvents sorts an out-of-order batch and reports only what was added', () => {
  const a = [{ seq: 1 }, { seq: 3 }];
  const r = mergeEvents(a, [{ seq: 2 }, { seq: 3 }]);
  assert.deepEqual(r.events.map(e => e.seq), [1, 2, 3]);
  assert.deepEqual(r.added.map(e => e.seq), [2]);
});

test('events from another run are dropped and counted', () => {
  let s = reduce(initialState(T), { type: 'job', at: T, payload: job() }, V);
  s = reduce(s, { type: 'events', at: T, text: [ev(1), ev(2, { run_id: 'other' })].join('\n') }, V);
  assert.equal(s.events.length, 1);
  assert.equal(s.foreign, 1);
});

test('bad lines are rejected, counted and kept for the raw view; good lines around them are used', () => {
  let s = reduce(initialState(T), { type: 'job', at: T, payload: job() }, V);
  s = reduce(s, { type: 'events', at: T, text: [ev(1), '{"v":1,', ev(2, { batt: 140 }), ev(3)].join('\n') }, V);
  assert.deepEqual(s.events.map(e => e.seq), [1, 3]);
  assert.equal(s.rejectedCount, 2);
  assert.equal(s.rejected.length, 2);
});

test('a new run resets events, frame and recorder', () => {
  let s = reduce(initialState(T), { type: 'job', at: T, payload: job() }, V);
  s = reduce(s, { type: 'events', at: T, text: ev(1) }, V);
  s = reduce(s, { type: 'job', at: T, payload: job({ run_id: 'r2' }) }, V);
  assert.equal(s.events.length, 0);
  assert.equal(s.frame, null);
});

test('a frame from another run is refused', () => {
  let s = reduce(initialState(T), { type: 'job', at: T, payload: job() }, V);
  const fm = { v: 1, run_id: 'r9', ts: '2026-10-20T05:00:00Z', bytes: 100, w: 610, h: 1356, screen_w: 1220, screen_h: 2712, secure: false };
  s = reduce(s, { type: 'frame', at: T, payload: fm }, V);
  assert.equal(s.frame, null);
  assert.match(s.frameProblem.why, /another run/);
});

test('the link goes down and comes back; a repeat of the same state is the same object', () => {
  let s = initialState(T);
  s = reduce(s, { type: 'link', at: T, mode: 'down', why: 'x' }, V);
  assert.equal(s.link.mode, 'down');
  assert.equal(reduce(s, { type: 'link', at: T + 1, mode: 'down', why: 'x' }, V), s);
});
