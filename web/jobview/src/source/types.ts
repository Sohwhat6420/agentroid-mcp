import type { Action, ViewState } from '../model/reducer.js';
import type { FrameMeta, Recorder } from '../contract/types.js';

export type ControlAction = 'pause' | 'resume' | 'take_over' | 'hand_back' | 'retry' | 'stop';
export type ControlResult = { ok: true } | { ok: false; why: string; unwired?: boolean };

export interface Source {
  kind: 'fixture' | 'live';
  jobId: string;
  /** shown in the footer so fixture data can never pass for live data */
  label: string;
  start(dispatch: (a: Action) => void, getState: () => ViewState): void;
  stop(): void;
  control(action: ControlAction): Promise<ControlResult>;
  frameSrc(meta: FrameMeta): string;
  cropSrc(rec: Recorder): string;
  /** the client clock; fixture mode can freeze it for screenshots */
  now(): number;
  /** ask for a fresh frame now (the source still refuses more than 1 per 30 s) */
  wantFrame(): void;
}
