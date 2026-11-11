// The edge validator against the repo's own schema fixtures (valid-* pass, invalid-* fail), the proposed
// frame/recorder schemas against the generated fixtures, and the validator's refusal of what it cannot check.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { compile, SchemaUnsupported } from '../.test-build/contract/schema.js';
import { parseNdjson } from '../.test-build/contract/validate.js';
import { V, repoFixtures, scenarioNames, scenarioFile } from './helpers.mjs';

const jobOrList = (v) => (Array.isArray(v) ? V.jobList(v) : V.job(v));

for (const f of repoFixtures('job-view')) {
  test(`job-view fixture ${f.name} is ${f.valid ? 'accepted' : 'refused'}`, () => {
    const r = jobOrList(f.value);
    assert.equal(r.ok, f.valid, r.ok ? 'accepted' : r.why);
    if (!r.ok) assert.ok(r.why.length > 0, 'a refusal says why');
  });
}
for (const f of repoFixtures('progress')) {
  test(`progress fixture ${f.name} is ${f.valid ? 'accepted' : 'refused'}`, () => {
    const r = V.event(f.value);
    assert.equal(r.ok, f.valid, r.ok ? 'accepted' : r.why);
  });
}

test('a missing required key is named in the refusal (job-view invalid-change-missing-before)', () => {
  const f = repoFixtures('job-view').find(x => x.name === 'invalid-change-missing-before.json');
  const r = V.job(f.value);
  assert.equal(r.ok, false);
  assert.ok(r.issues.some(i => i.path.endsWith('/before') && /required/.test(i.msg)), JSON.stringify(r.issues));
});

test('a list where an object is expected is refused, and vice versa', () => {
  assert.equal(V.job([]).ok, false);
  assert.equal(V.jobList({}).ok, false);
  assert.equal(V.job(null).ok, false);
  assert.equal(V.job('{"job_id":"x"}').ok, false, 'a JSON string is not a job');
});

for (const name of scenarioNames()) {
  test(`generated scenario ${name}: frame.json and recorder.json match the proposed schemas`, () => {
    const fr = scenarioFile(name, 'frame.json');
    if (fr) { const r = V.frameMeta(JSON.parse(fr)); assert.ok(r.ok, r.ok ? '' : r.why); }
    const rc = scenarioFile(name, 'recorder.json');
    if (rc) { const r = V.recorder(JSON.parse(rc)); assert.ok(r.ok, r.ok ? '' : r.why); }
  });
}

test('frame metadata: over 60 KB, or a secure frame with bytes, is refused', () => {
  const fr = JSON.parse(scenarioFile('running', 'frame.json'));
  assert.equal(V.frameMeta({ ...fr, bytes: 61441 }).ok, false);
  assert.equal(V.frameMeta({ ...fr, bytes: 61440 }).ok, true);
  assert.equal(V.frameMeta({ ...fr, secure: true, bytes: 10 }).ok, false);
  assert.equal(V.frameMeta({ ...fr, secure: true, bytes: 0 }).ok, true);
  assert.equal(V.frameMeta({ ...fr, extra: 1 }).ok, false, 'additionalProperties: false');
});

test('NDJSON: good lines kept, bad lines rejected with a reason, a truncated line does not stop the rest', () => {
  const text = scenarioFile('invalid-events', 'events.ndjson');
  const r = parseNdjson(text, V);
  assert.equal(r.rejected.length, 4);
  assert.ok(r.rejected.some(x => /JSON/i.test(x.why)), 'the truncated line is named as bad JSON');
  const lines = text.split('\n').filter(Boolean).length;
  assert.equal(r.events.length, lines - 4);
});

test('the invalid-job scenario is refused (status not in the enum)', () => {
  const r = V.job(JSON.parse(scenarioFile('invalid-job', 'job.json')));
  assert.equal(r.ok, false);
  assert.match(r.why, /status/);
});

test('a schema keyword the checker does not implement is refused at compile time, never ignored', () => {
  assert.throws(() => compile({ type: 'object', patternProperties: { '^x': { type: 'string' } } }), SchemaUnsupported);
  assert.throws(() => compile({ $ref: 'https://example.invalid/x.json' }), SchemaUnsupported);
});

test('minimum, maximum, pattern, enum and const are enforced', () => {
  const c = compile({ type: 'object', properties: { n: { type: 'integer', minimum: 0, maximum: 3 }, s: { type: 'string', pattern: '^a+$' }, e: { enum: ['x', 'y'] }, k: { const: 1 } } });
  assert.equal(c({ n: 2, s: 'aa', e: 'x', k: 1 }).length, 0);
  assert.equal(c({ n: 4 }).length, 1);
  assert.equal(c({ n: -1 }).length, 1);
  assert.equal(c({ n: 1.5 }).length, 1);
  assert.equal(c({ s: 'ab' }).length, 1);
  assert.equal(c({ e: 'z' }).length, 1);
  assert.equal(c({ k: 2 }).length, 1);
});
