//! What the engine hands out, behind traits that slice b plugs into:
//!
//! * [`RunSink`]: progress events shaped like `schemas/progress.json` and journal records shaped
//!   like `schemas/journal.json`. Slice b persists both (NDJSON, group-commit fsync); this slice only
//!   emits them. [`MemorySink`] keeps them in memory for tests.
//! * [`CheckpointStore`]: a [`Checkpoint`] at every `checkpoint` step and at every pause, so
//!   `resume(run_id)` can restart from it. [`MemoryCheckpoints`] for tests.
//! * [`LeaseHandle`]: the 2.8 lease the job holds. The engine keeps it on a pause and releases it
//!   when the run ends any other way. [`RegistryLease`] adapts the daemon's registry.

use crate::registry::Registry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// `progress.json` `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Step,
    Progress,
    Pause,
    Resume,
    Stuck,
    TakenOver,
    Done,
    Error,
}

/// One NDJSON line (`schemas/progress.json`). Optional fields are left out when unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub v: u8,
    pub ts: String,
    pub run_id: String,
    pub seq: u64,
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batt: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_s: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<u32>,
}

/// `journal.json` `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStatus {
    Planned,
    Sent,
    Done,
    Interrupted,
    Failed,
}

/// One journal record (`schemas/journal.json`). Every field is required there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub v: u8,
    pub run_id: String,
    pub seq: u64,
    pub step_id: String,
    pub intent: String,
    pub post: Value,
    pub status: JournalStatus,
    pub ts_phone: String,
    pub ts_hub: String,
    pub epoch: u64,
    pub display: u32,
}

/// Where the engine's events and journal records go. Slice b implements this over the run directory
/// ([`crate::runner::journal::JournalWriter`]).
pub trait RunSink {
    fn progress(&mut self, ev: &ProgressEvent);
    /// `Ok` once the record is accepted. For a `sent` record `Ok` means **durable on disk** (the
    /// engine sends nothing to the phone until it has that); an `Err` stops the step unsent.
    fn journal(&mut self, rec: &JournalRecord) -> Result<(), String>;
    /// Every UI tree the engine observed, for the flight recorder (2.11a). Default: ignored.
    fn tree(&mut self, _step_id: &str, _xml: &str) {}
    /// A screenshot taken for the flight recorder (2.11a; only while the screen keeps coming
    /// back, see `RunOpts::flight_frames`). Default: ignored.
    fn frame(&mut self, _step_id: &str, _image: &[u8]) {}
}

/// Keeps everything in memory (tests, dry runs).
#[derive(Debug, Default, Clone)]
pub struct MemorySink {
    pub events: Vec<ProgressEvent>,
    pub records: Vec<JournalRecord>,
}

impl RunSink for MemorySink {
    fn progress(&mut self, ev: &ProgressEvent) {
        self.events.push(ev.clone());
    }
    fn journal(&mut self, rec: &JournalRecord) -> Result<(), String> {
        self.records.push(rec.clone());
        Ok(())
    }
}

/// Where the run can restart. `path` indexes the flow: `[step]` at the top level, and
/// `[step, row, step, ...]` inside `for_each` bodies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub run_id: String,
    pub step_id: String,
    pub path: Vec<usize>,
    /// Why it was taken: `checkpoint` (a checkpoint step) or a pause code such as `needs_attention`.
    pub reason: String,
    /// Steps executed so far (the step cap continues from here).
    pub executed: u32,
    pub relaunches: u32,
    /// Wall time used so far (the 4 h cap continues from here).
    pub elapsed_ms: u64,
    /// Values read so far, by step id.
    pub reads: BTreeMap<String, String>,
    /// Steps completed so far (2.10b; absent in older checkpoints).
    #[serde(default)]
    pub done: u32,
    /// Retry attempts so far (2.10b).
    #[serde(default)]
    pub retries: u32,
    /// Steps that needed more than one attempt (2.10b).
    #[serde(default)]
    pub retried_steps: u32,
}

/// Slice b's checkpoint persistence.
pub trait CheckpointStore {
    fn save(&mut self, cp: &Checkpoint) -> Result<(), String>;
}

#[derive(Debug, Default, Clone)]
pub struct MemoryCheckpoints {
    pub saved: Vec<Checkpoint>,
}

impl CheckpointStore for MemoryCheckpoints {
    fn save(&mut self, cp: &Checkpoint) -> Result<(), String> {
        self.saved.push(cp.clone());
        Ok(())
    }
}

/// What the lease holder asked for between steps (`job.pause`, `job.stop`), or what the phone's
/// person did (`TakeOver`: a human touch, task 2.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    Pause,
    Stop,
    /// A person touched the phone: the in-flight step is journalled `interrupted` (with
    /// `post.interrupted_by: "human"`, so resume re-checks its post-condition) and the job pauses
    /// with `taken_over`. Polled before every step and between and after the calls of a step.
    TakeOver,
}

/// One event a [`JobControl::wait_ready`] wait asks the engine to emit (care's heat pause and
/// resume, the battery floor), with the reading that caused it.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlNote {
    pub kind: EventKind,
    pub msg: String,
    pub temp_c: Option<f64>,
    pub batt: Option<f64>,
}

/// Polled by the engine before every step attempt.
pub trait JobControl {
    fn interrupt(&self) -> Option<Interrupt>;
    /// Called before every step attempt once `interrupt` said carry on. It may block (care's heat
    /// rest: pause at 42 °C, resume strictly below 37 °C; the battery floor), emitting what it does
    /// through `emit` as it happens. `Some` ends the wait with that interrupt instead. Default: no wait.
    fn wait_ready(&self, _step_id: &str, _emit: &mut dyn FnMut(ControlNote)) -> Option<Interrupt> {
        None
    }
}

/// The device lease a job holds (task 2.8).
pub trait LeaseHandle {
    /// Still ours? Checked before every acting step.
    fn held(&self) -> bool;
    fn release(&mut self);
}

/// A lease in the daemon's [`Registry`], owned by connection `conn`.
pub struct RegistryLease<'a> {
    pub registry: &'a Registry,
    pub device: String,
    pub conn: u64,
}

impl LeaseHandle for RegistryLease<'_> {
    fn held(&self) -> bool {
        self.registry.check_holder(&self.device, self.conn).is_ok()
    }
    fn release(&mut self) {
        self.registry.release(&self.device, self.conn);
    }
}

/// Unix milliseconds to RFC 3339 UTC with milliseconds (`2001-09-09T01:46:40.000Z`), which matches
/// the `ts` pattern of `progress.json` and `journal.json`. No date crate is in the offline cache.
pub fn rfc3339_utc(unix_ms: u64) -> String {
    let secs = unix_ms / 1000;
    let ms = unix_ms % 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX / 2);
    let sod = secs % 86_400;
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}
