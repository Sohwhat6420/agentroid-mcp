// Entry point. Chooses the data source, builds the edge validators from the repo's own schemas (the
// schemas stay the single source of truth), and starts the view.
//   ?fixture=<name>[&freeze=1]   recorded data from public/fixtures/<name>/ (labelled FIXTURE, never live)
//   ?job=<id>                    live: this job from bridged on the same origin
//   (nothing)                    live: the newest unfinished job in GET /jobs, else the newest job

import jobViewSchema from '../../../schemas/job-view.json';
import progressSchema from '../../../schemas/progress.json';
import frameMetaSchema from '../contract/frame-meta.schema.json';
import recorderSchema from '../contract/recorder.schema.json';
import { makeValidators } from './contract/validate.js';
import type { Schema } from './contract/schema.js';
import { initialState } from './model/reducer.js';
import { fixtureSource } from './source/fixture.js';
import { liveSource } from './source/live.js';
import type { Source } from './source/types.js';
import { startApp } from './view/app.js';

const validators = makeValidators({
  jobView: jobViewSchema as Schema, progress: progressSchema as Schema,
  frameMeta: frameMetaSchema as Schema, recorder: recorderSchema as Schema,
});

async function pickLiveJob(): Promise<string> {
  try {
    const r = await fetch('/jobs', { headers: { Accept: 'application/json' }, cache: 'no-store', credentials: 'same-origin' });
    if (!r.ok) return '';
    const c = validators.jobList(await r.json());
    if (!c.ok || !c.value.length) return '';
    const open = c.value.find(j => !['done', 'stopped', 'error'].includes(j.status));
    return (open ?? c.value[0]!).job_id;
  } catch { return ''; }
}

async function boot() {
  const q = new URLSearchParams(location.search);
  const fixture = q.get('fixture');
  let source: Source;
  let fixtures: { name: string; label: string }[] | null = null;
  if (fixture != null) {
    source = fixtureSource(/^[a-z0-9-]{1,40}$/.test(fixture) ? fixture : 'invalid-name', { freeze: q.get('freeze') === '1' });
    try { fixtures = ((await (await fetch('fixtures/index.json')).json()) as { name: string; label: string }[]).filter(f => typeof f.name === 'string'); } catch { fixtures = []; }
  } else {
    const id = q.get('job') ?? (await pickLiveJob());
    source = liveSource(id);
  }
  const app = startApp(source, validators, initialState(source.now()), fixtures);
  if (fixture == null && !source.jobId) {
    app.dispatch({ type: 'job-failed', at: Date.now(), why: 'bridged lists no job on this laptop (GET /jobs gave none)' });
    return;
  }
  source.start(app.dispatch, app.getState);
}

void boot();
