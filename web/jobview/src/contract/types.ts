// TypeScript shapes for the payloads the view reads. They mirror, field for field:
//   schemas/job-view.json     (Job, ChangeInForce)
//   schemas/progress.json     (ProgressEvent)
//   contract/frame-meta.schema.json and contract/recorder.schema.json (proposed by 2.11b)
// The runtime check is schema.ts against those files; these types only describe what passed it.

export type JobStatus = 'queued' | 'running' | 'paused' | 'taken_over' | 'stuck' | 'stopped' | 'done' | 'error';
export type Namespace = 'system' | 'secure' | 'global' | 'vendor';
export type LeaseScope = 'session' | 'job' | 'until' | 'permanent';

export interface ChangeInForce {
  key: string;
  ns?: Namespace;
  before: string;
  applied: string;
  owner: { pid: number; job_id: string; session: string };
  scope?: LeaseScope;
  expires?: string;
  ts?: string;
}

export interface Job {
  job_id: string;
  run_id: string;
  device: string;
  flow: string;
  status: JobStatus;
  steps_planned?: number;
  steps_done?: number;
  events_url?: string;
  frame_url?: string;
  changes_in_force?: ChangeInForce[];
}

export type EventKind = 'step' | 'progress' | 'pause' | 'resume' | 'stuck' | 'taken_over' | 'done' | 'error';

export interface ProgressEvent {
  v: 1;
  ts: string;
  run_id: string;
  seq: number;
  kind: EventKind;
  step_id?: string;
  msg?: string;
  temp_c?: number;
  batt?: number;
  eta_s?: number;
  display?: number;
}

export interface TapMark { step_id: string; x: number; y: number; selector: string; label?: string }
export interface MissMark { step_id: string; x: number; y: number; selector: string; reason: string }

export interface FrameMeta {
  v: 1;
  run_id: string;
  ts: string;
  bytes: number;
  w: number;
  h: number;
  screen_w: number;
  screen_h: number;
  secure: boolean;
  display?: number;
  step_id?: string;
  next_tap?: TapMark;
  miss?: MissMark;
}

export interface Recorder {
  v: 1;
  run_id: string;
  ts: string;
  step_id: string;
  reason: string;
  trees: number;
  frames: number;
  crop: { url: string; x: number; y: number; w: number; h: number; bytes: number };
}
