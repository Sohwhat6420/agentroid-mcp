// Shared test setup: validators built from the repo's own schemas, and fixture loaders.
import { readFileSync, readdirSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import { makeValidators } from '../.test-build/contract/validate.js';
import { initialState, reduce } from '../.test-build/model/reducer.js';

export const ROOT = fileURLToPath(new URL('..', import.meta.url));
export const SCHEMAS = join(ROOT, '..', '..', 'schemas');
const readJson = (p) => JSON.parse(readFileSync(p, 'utf8'));

export const schemaSet = () => ({
  jobView: readJson(join(SCHEMAS, 'job-view.json')),
  progress: readJson(join(SCHEMAS, 'progress.json')),
  frameMeta: readJson(join(ROOT, 'contract', 'frame-meta.schema.json')),
  recorder: readJson(join(ROOT, 'contract', 'recorder.schema.json')),
});
export const V = makeValidators(schemaSet());

/** every file of a repo fixture folder: [{name, valid, value}] */
export function repoFixtures(kind) {
  const dir = join(SCHEMAS, 'fixtures', kind);
  return readdirSync(dir).filter(f => f.endsWith('.json')).map(f => ({ name: f, valid: f.startsWith('valid-'), value: readJson(join(dir, f)) }));
}

const FX = join(ROOT, 'public', 'fixtures');
export const scenarioNames = () => readJson(join(FX, 'index.json')).map(x => x.name);
export function scenarioFile(name, file) { const p = join(FX, name, file); return existsSync(p) ? readFileSync(p, 'utf8') : null; }

/** Feed a generated scenario through the reducer exactly as fixture mode does; returns {state, now}. */
export function loadScenario(name) {
  const man = readJson(join(FX, name, 'manifest.json'));
  const now = Date.parse(man.now);
  let s = initialState(now);
  const j = scenarioFile(name, 'job.json'); if (j) s = reduce(s, { type: 'job', at: now, payload: JSON.parse(j), serverDate: now }, V);
  const js = scenarioFile(name, 'jobs.json'); if (js) s = reduce(s, { type: 'jobs', at: now, payload: JSON.parse(js) }, V);
  const ev = scenarioFile(name, 'events.ndjson'); if (ev != null) s = reduce(s, { type: 'events', at: now, text: ev }, V);
  const fr = scenarioFile(name, 'frame.json'); if (fr) s = reduce(s, { type: 'frame', at: now, payload: JSON.parse(fr) }, V);
  const rc = scenarioFile(name, 'recorder.json'); if (rc) s = reduce(s, { type: 'recorder', at: now, payload: JSON.parse(rc) }, V);
  if (man.link === 'down') s = reduce(s, { type: 'link', at: now, mode: 'down', why: 'test' }, V);
  return { state: s, now, manifest: man };
}
