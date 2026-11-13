//! Jobs and `run_script` in the daemon (task 2.10 slice b).
//!
//! JSON-RPC methods, **all lease-holder only** (the same rule and messages as `device.call`):
//! `job.start`, `job.status`, `job.pause`, `job.resume`, `job.stop`, `job.receipt`, `job.dry_run`
//! and `job.run_script`.
//!
//! * `job.start` validates the flow (slice a's loader) and refuses R2+ before anything exists. The
//!   job runs on its own thread under the **caller's** lease: the engine checks that lease before
//!   every acting call and stops if it is gone. The caller acquired the lease, so the caller
//!   releases it; the job never does.
//! * Each job keeps `<jobs dir>/<run_id>/` (journal, progress, checkpoint, `flow.json`, `job.json`,
//!   and `receipt.json` at the end), so `job.resume` works after a daemon restart too: the journal
//!   is read back and the resume point planned by `runner::resume`.
//! * The engine reaches the phone through [`SharedDriver`]: the backend lock is held per call, not
//!   per job.
//! * `job.run_script` runs the slice-c sandbox with [`DaemonBridge`]: `call` goes through
//!   `Core::call` with the device's mode (the backend's gate), `shell` is re-classified with the
//!   slice-c classifier and then run by adb-link's `GuardedRunner`. Every bridge call has its own
//!   timeout. Every refusal the sandbox returns is written to the script run's journal.
//! * Supervision (2.11a): progress events reach `<run>/progress.ndjson` as they happen (the
//!   journal writer, through the [`Tee`]); the Tee keeps the job's flight recorder (10 trees,
//!   3 frames) and dumps it to `<run>/flight/` on `stuck`; `job.start` prunes run directories
//!   older than 7 days by [`JobsConfig::clock`].
//! * Care and take-over: with [`JobsConfig::care`] set, each job on a phone
//!   gets a [`JobCare`] (heat guard, battery floor, job-scoped settings leases, and with profiles
//!   the 2.15 mode machine). Its waits run in the engine's control hook before every step; care's
//!   entries go into the job's journal; a human touch turns the job `taken_over`.

use crate::jobcare::{CareConfig, JobCare};
use crate::proto::*;
use crate::runner::dryrun::{dry_run, SavedTree, Verdict2};
use crate::runner::engine::{self, Hooks, Resume, RunOpts, RunReport, Stop};
use crate::runner::flight::{prune_runs_capped, FlightRecorder, FRAMES, MAX_RUNS, RETENTION_MS};
use crate::runner::flow::{load_value, parse_yaml_value, Flow, Risk};
use crate::runner::journal::{
    read_checkpoint, read_progress, run_dir, valid_run_id, write_atomic, FileCheckpoints,
    GroupCommit, JournalWriter, JOURNAL_FILE, PROGRESS_FILE,
};
use crate::runner::popups::{PauseReason, Rulebook};
use crate::runner::receipt::{self, ReceiptInput};
use crate::runner::record;
use crate::runner::resume::{load_journal, plan, LoadError, Start};
use crate::runner::sink::{
    rfc3339_utc, ControlNote, EventKind, Interrupt, JobControl, JournalRecord, JournalStatus,
    LeaseHandle, ProgressEvent, RunSink,
};
use crate::server::{device_key, parse_params, to_value, Daemon, SharedDriver};
use agentroid_adb_link::guard::GuardedRunner;
use agentroid_adb_link::runner::{RunError, RunOutput, Runner};
use agentroid_core::gate::Call;
use agentroid_core::shell::sh_quote;
use handover::touch::{parse_line, Detector, Line, PhoneClock, PhoneClocks};
use sandbox::shell::{classify, ShellVerdict};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long `job.pause` / `job.stop` wait for a running job to reach its next step boundary.
const CONTROL_WAIT: Duration = Duration::from_secs(10);

/// An adb [`Runner`] behind an `Arc`, so the daemon can own one and wrap it in a [`GuardedRunner`].
#[derive(Clone)]
pub struct DynRunner(pub Arc<dyn Runner + Send + Sync>);

impl Runner for DynRunner {
    fn run(&self, args: &[String], timeout_ms: u64) -> Result<RunOutput, RunError> {
        self.0.run(args, timeout_ms)
    }
    fn run_bytes(
        &self,
        args: &[String],
        timeout_ms: u64,
    ) -> Result<agentroid_adb_link::runner::BytesOutput, RunError> {
        self.0.run_bytes(args, timeout_ms)
    }
    fn run_marked(
        &self,
        args: &[String],
        timeout_ms: u64,
        mark: &mut dyn FnMut(),
    ) -> Result<RunOutput, RunError> {
        self.0.run_marked(args, timeout_ms, mark)
    }
    fn deny(&self, token: &str) {
        self.0.deny(token);
    }
}

/// Where jobs live and how `run_script` is wired.
pub struct JobsConfig {
    /// The jobs directory: one `<run_id>/` per job and per script run.
    pub dir: PathBuf,
    /// `agentroid-sandbox.exe`; default: next to the running executable.
    pub sandbox_exe: Option<PathBuf>,
    /// adb for `agentroid.shell`, always behind the command guard. `None`: shell calls fail, unsent.
    pub shell: Option<Arc<GuardedRunner<DynRunner>>>,
    /// Per `agentroid.call` (plus the call's own `timeout_ms` / `wait` time).
    pub call_timeout_ms: u64,
    /// Per `agentroid.shell`.
    pub shell_timeout_ms: u64,
    pub limits: sandbox::Limits,
    pub group_commit: GroupCommit,
    /// Wall clock in Unix ms, for the retention prune at job start (injectable for tests, 2.11a).
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Run directories whose last activity is older than this are pruned at `job.start`.
    pub retention_ms: u64,
    /// At most this many ended run directories are kept (the oldest go first); live jobs do not count.
    pub max_runs: usize,
    /// Care during jobs (2.13) and, with profiles, take-over (2.15). `None`: no heat guard, no
    /// floor, no settings leases (the fake-device daemon).
    pub care: Option<CareConfig>,
}

impl JobsConfig {
    pub fn new(dir: impl Into<PathBuf>) -> JobsConfig {
        JobsConfig {
            dir: dir.into(),
            sandbox_exe: None,
            shell: None,
            call_timeout_ms: 20_000,
            shell_timeout_ms: 15_000,
            limits: sandbox::Limits::default(),
            group_commit: GroupCommit::default(),
            clock: Arc::new(unix_ms),
            retention_ms: RETENTION_MS,
            max_runs: MAX_RUNS,
            care: None,
        }
    }
    /// Run `agentroid.shell` through `runner`, wrapped in adb-link's [`GuardedRunner`].
    pub fn with_shell_runner(mut self, runner: Arc<dyn Runner + Send + Sync>) -> JobsConfig {
        self.shell = Some(Arc::new(GuardedRunner::new(DynRunner(runner))));
        self
    }
}

/// `job.pause` / `job.stop` requests and take-overs, polled by the engine before every step
/// attempt (and, for a take-over, between the calls of a step); plus the job's care waits.
#[derive(Default)]
pub(crate) struct Control {
    flag: AtomicU8,
    care: Mutex<Option<Arc<JobCare>>>,
}

impl Control {
    pub(crate) fn set(&self, i: Option<Interrupt>) {
        self.flag.store(
            match i {
                None => 0,
                Some(Interrupt::Pause) => 1,
                Some(Interrupt::Stop) => 2,
                Some(Interrupt::TakeOver) => 3,
            },
            Ordering::SeqCst,
        );
    }
    pub(crate) fn care(&self) -> Option<Arc<JobCare>> {
        self.care
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn set_care(&self, c: Option<Arc<JobCare>>) {
        *self
            .care
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = c;
    }
}

impl JobControl for Control {
    fn interrupt(&self) -> Option<Interrupt> {
        match self.flag.load(Ordering::SeqCst) {
            1 => Some(Interrupt::Pause),
            2 => Some(Interrupt::Stop),
            3 => Some(Interrupt::TakeOver),
            _ => None,
        }
    }
    /// care's heat rest and battery floor (2.13), before every step attempt.
    fn wait_ready(&self, _step_id: &str, emit: &mut dyn FnMut(ControlNote)) -> Option<Interrupt> {
        self.care()?.wait(&|| self.interrupt(), emit)
    }
}

/// `job.json` in the run directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobMeta {
    run_id: String,
    device: String,
    flow: String,
    created_unix_ms: u64,
    #[serde(default)]
    oem: Option<String>,
    #[serde(default)]
    sdk: Option<u32>,
    #[serde(default = "yes")]
    auto_dismiss: bool,
    /// Started with `record`: a `done` end writes the recording.
    #[serde(default)]
    record: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default)]
pub(crate) struct JobState {
    /// `running`, `paused`, `stuck`, `stopped`, `done`, `error`.
    status: &'static str,
    running: bool,
    step_id: Option<String>,
    last_msg: Option<String>,
    last_error: Option<String>,
    pause_reason: Option<String>,
    planned: u32,
    done: u32,
    retries: u32,
    retried_steps: u32,
    /// Engine work time (cumulative across resumes).
    work_ms: u64,
    /// CPU of this daemon's job threads; `None` until measured.
    cpu_ms: Option<u64>,
    receipt: Option<Value>,
    journal_seq: u64,
    /// The connection whose lease the job last ran under (the job view's controls resume with it).
    conn: u64,
}

pub(crate) struct Job {
    meta: JobMeta,
    flow: Flow,
    dir: PathBuf,
    control: Control,
    st: Mutex<JobState>,
    /// The last 10 trees and 3 frames of this job, across resumes (2.11a).
    flight: Mutex<FlightRecorder>,
}

impl Job {
    fn new(meta: JobMeta, flow: Flow, dir: PathBuf, st: JobState) -> Job {
        Job {
            meta,
            flow,
            dir,
            control: Control::default(),
            st: Mutex::new(st),
            flight: Mutex::new(FlightRecorder::default()),
        }
    }
    fn st(&self) -> MutexGuard<'_, JobState> {
        self.st
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn flight(&self) -> MutexGuard<'_, FlightRecorder> {
        self.flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The longest a getevent chunk or an overlay report is believed to take to reach the hub (adb, Wi-Fi,
/// a busy laptop). A mapped touch time further back than this is not trusted (see `Daemon::place_touch`).
pub const MAX_PLAUSIBLE_DELAY_MS: u64 = 15_000;

/// The daemon's jobs, by run id.
#[derive(Default)]
pub struct Jobs {
    map: Mutex<HashMap<String, Arc<Job>>>,
    next: AtomicU64,
    /// Phone clock -> hub clock offsets, per device (wiring 2, 25 Oct): the least-delayed offset
    /// of every (phone time, hub arrival time) pair seen, kept across jobs.
    clocks: Mutex<HashMap<String, PhoneClocks>>,
    /// Detector 1 on the live getevent stream, one per device, kept across calls: a touch frame
    /// (`BTN_TOUCH DOWN` ... `SYN_REPORT`) can be split across two reads. The map text it was built
    /// from is kept so a changed device map starts a fresh one.
    detectors: Mutex<HashMap<String, (String, Detector)>>,
}

impl Jobs {
    fn map(&self) -> MutexGuard<'_, HashMap<String, Arc<Job>>> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn clocks(&self) -> MutexGuard<'_, HashMap<String, PhoneClocks>> {
        self.clocks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn detectors(&self) -> MutexGuard<'_, HashMap<String, (String, Detector)>> {
        self.detectors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap_or(0)
}

fn state_err(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(STATE_ERROR, e.to_string())
}

/// CPU time (user + kernel) of the calling thread, in ms.
#[cfg(windows)]
fn thread_cpu_ms() -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: GetCurrentThread returns a pseudo-handle valid for this thread; the four pointers are
    // to live, writable FILETIMEs.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    Some((t(kernel) + t(user)) / 10_000)
}

#[cfg(not(windows))]
fn thread_cpu_ms() -> Option<u64> {
    None
}

/// The flow as a JSON value: an object as given, or YAML text converted; `vars` merged in.
fn flow_json(flow: &Value, vars: Option<Map<String, Value>>) -> Result<Value, RpcError> {
    let mut v = match flow {
        Value::String(text) => {
            parse_yaml_value(text).map_err(|e| RpcError::new(INVALID_PARAMS, e.to_string()))?
        }
        Value::Object(_) => flow.clone(),
        _ => {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "flow must be a flow object or YAML text",
            ))
        }
    };
    if let Some(extra) = vars {
        let Some(obj) = v.as_object_mut() else {
            return Err(RpcError::new(INVALID_PARAMS, "flow must be an object"));
        };
        let entry = obj.entry("vars").or_insert_with(|| json!({}));
        let Some(m) = entry.as_object_mut() else {
            return Err(RpcError::new(INVALID_PARAMS, "flow vars must be an object"));
        };
        for (k, val) in extra {
            m.insert(k, val);
        }
    }
    Ok(v)
}

/// `run_steps`: the flow object for a bare list of steps in `app`. A step with no `id` is numbered
/// `s<position>`; everything else is the caller's, checked by the same schema and semantic rules as
/// any flow (so a step's `risk`, `retry` and selectors are held to them).
fn steps_flow(steps: &[Value], app: Option<&str>) -> Result<Value, RpcError> {
    let app = app.map(str::trim).unwrap_or_default();
    if !is_package_name(app) {
        return Err(RpcError::new(INVALID_PARAMS, format!("steps need app: the package they run in, such as com.android.settings (got {app:?})")));
    }
    if steps.is_empty() {
        return Err(RpcError::new(INVALID_PARAMS, "steps is empty"));
    }
    // the caller's own ids first, so a generated `s<position>` never takes one of them
    let taken: Vec<&str> = steps
        .iter()
        .filter_map(|s| s.get("id").and_then(Value::as_str))
        .collect();
    let mut out = Vec::with_capacity(steps.len());
    for (i, s) in steps.iter().enumerate() {
        let Some(o) = s.as_object() else {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!("steps[{i}] must be an object"),
            ));
        };
        let mut o = o.clone();
        if !o.contains_key("id") {
            let mut n = i + 1;
            let mut id = format!("s{n}");
            while taken.contains(&id.as_str()) {
                n += steps.len();
                id = format!("s{n}");
            }
            o.insert("id".into(), json!(id));
        }
        out.push(Value::Object(o));
    }
    Ok(
        json!({"flow": "run_steps", "version": 1, "app": {"package": app, "version_range": "*"}, "steps": out}),
    )
}

/// An Android application id: two or more dot-separated segments, each starting with a letter and
/// made of letters, digits and underscores (what the engine compares with a screen's packages).
fn is_package_name(s: &str) -> bool {
    let mut segments = 0;
    for seg in s.split('.') {
        let mut c = seg.chars();
        if !c.next().is_some_and(|f| f.is_ascii_alphabetic())
            || !c.all(|x| x.is_ascii_alphanumeric() || x == '_')
        {
            return false;
        }
        segments += 1;
    }
    segments >= 2
}

/// One row per top-level step of the job's flow: `done` for a step that has a completion event in
/// `progress.ndjson` (reads such as `assert` leave no journal record, so the journal alone would
/// call them not run), then, for the first step that has none, how the job left it (`failed` when
/// it ended stuck or in error, `interrupted` when it paused or was stopped, `running` while the job
/// is), then `not_run`. A flow runs its top-level steps in order, so that first step is the one that failed.
/// `note` is what the step did, or why the job stopped at it.
fn step_rows(job: &Job) -> Vec<Value> {
    let events = read_progress(&job.dir);
    let (status, last_error, pause) = {
        let st = job.st();
        (st.status, st.last_error.clone(), st.pause_reason.clone())
    };
    let mut stopped_at = false;
    job.flow
        .steps
        .iter()
        .map(|s| {
            let done = events.iter().rev().find(|e| e.kind == EventKind::Step && e.step_id.as_deref() == Some(s.id.as_str()));
            let (state, note) = match (done, stopped_at) {
                (Some(e), _) => ("done", e.msg.clone().unwrap_or_default()),
                (None, false) => {
                    stopped_at = true;
                    match status {
                        "stuck" | "error" => ("failed", last_error.clone().unwrap_or_default()),
                        "done" => ("not_run", String::new()),
                        "running" => ("running", String::new()),
                        _ => ("interrupted", pause.clone().unwrap_or_default()),
                    }
                }
                (None, true) => ("not_run", String::new()),
            };
            json!({"step_id": s.id, "status": state, "note": note.chars().take(200).collect::<String>()})
        })
        .collect()
}

/// `flow` text that names a file (one line ending .yaml, .yml or .json) rather than holding a flow.
fn looks_like_flow_file(s: &str) -> bool {
    let t = s.trim();
    let l = t.to_ascii_lowercase();
    !t.contains('\n') && t.len() <= 260 && [".yaml", ".yml", ".json"].iter().any(|e| l.ends_with(e))
}

/// The text of a recorded flow file: a bare file name (`job-1-2.flow.yaml`) in the jobs directory,
/// at most 1 MiB. The name is checked before the disk is touched, so a path, a drive, a UNC share or
/// a pipe name is refused without being opened; a run directory's own `flow.json` (which keeps no secret
/// values) is out of reach because it sits one level down.
fn flow_file_text(root: &Path, name: &str) -> Result<String, RpcError> {
    use std::io::Read;
    let bad = |why: String| RpcError::new(INVALID_PARAMS, format!("flow file {name:?}: {why}"));
    let n = name.trim();
    let plain = !n.is_empty()
        && n.len() <= 120
        && !n.starts_with('.')
        && !n.contains("..")
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !plain {
        return Err(bad("give a bare file name from the jobs directory, such as job-1-2.flow.yaml (no path, drive or share)".into()));
    }
    let root_c = root
        .canonicalize()
        .map_err(|e| bad(format!("the jobs directory is unreadable ({e})")))?;
    let full_c = root
        .join(n)
        .canonicalize()
        .map_err(|_| bad("no such file in the jobs directory".into()))?;
    // a link that leads out of the directory fails here
    if full_c.parent() != Some(root_c.as_path()) || !full_c.is_file() {
        return Err(bad(
            "replay reads only files directly inside the jobs directory".into(),
        ));
    }
    let mut text = String::new();
    let f = std::fs::File::open(&full_c).map_err(|e| bad(e.to_string()))?;
    f.take((1 << 20) + 1)
        .read_to_string(&mut text)
        .map_err(|e| bad(e.to_string()))?;
    if text.len() > 1 << 20 {
        return Err(bad("over the 1 MiB limit".into()));
    }
    Ok(text)
}

/// Write `<run id>.flow.yaml` and `<run id>.maestro.yaml` beside the run directory (pruning removes
/// directories only, so a recording outlives its run), and `recording.json` inside it.
fn write_recording(job: &Job) -> Result<(), String> {
    let text = std::fs::read_to_string(job.dir.join("flow.json"))
        .map_err(|e| format!("flow.json: {e}"))?;
    let stored: Value = serde_json::from_str(&text).map_err(|e| format!("flow.json: {e}"))?;
    // flow.json keeps no secret values; the job in memory still has them, which the leak check needs
    let fv = record::with_secret_values(&stored, &job.flow.vars);
    let done: HashSet<String> = read_progress(&job.dir)
        .iter()
        .filter(|e| e.kind == EventKind::Step)
        .filter_map(|e| e.step_id.clone())
        .collect();
    let rec = match record::record(&fv, &done, &job.meta.run_id) {
        Ok(Some(r)) => r,
        Ok(None) => return Err("no step was completed".into()),
        Err(e) => return Err(e),
    };
    let root = job.dir.parent().ok_or("the run directory has no parent")?;
    let (flow_name, maestro_name) = record::file_names(&job.meta.run_id);
    let (flow_path, maestro_path) = (root.join(flow_name), root.join(maestro_name));
    write_atomic(&flow_path, rec.flow_yaml.as_bytes())?;
    write_atomic(&maestro_path, rec.maestro_yaml.as_bytes())?;
    let v = record::recording_value(
        &flow_path.display().to_string(),
        &maestro_path.display().to_string(),
        rec.steps,
        &rec.maestro_skipped,
    );
    write_atomic(
        &job.dir.join("recording.json"),
        (v.to_string() + "\n").as_bytes(),
    )
}

/// What `write_recording` saved for this job, if anything.
fn recording_of(job: &Job) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(job.dir.join("recording.json")).ok()?).ok()
}

fn load_flow(v: Value) -> Result<Flow, RpcError> {
    load_value(v).map_err(|e| RpcError::new(INVALID_PARAMS, e.to_string()))
}

fn status_of_stop(stop: &Stop) -> &'static str {
    match stop {
        Stop::Done => "done",
        Stop::Paused(p) if matches!(p.reason, PauseReason::TakenOver) => "taken_over",
        Stop::Paused(_) => "paused",
        Stop::Stuck { .. } => "stuck",
        Stop::Stopped { .. } => "stopped",
        _ => "error",
    }
}

/// The receipt's `outcome` for a job status.
fn outcome_of(status: &str) -> &'static str {
    match status {
        "done" => "done",
        "stopped" => "stopped",
        "stuck" => "stuck",
        "paused" | "taken_over" => "paused",
        "running" => "running",
        _ => "failed",
    }
}

fn status_from_outcome(outcome: &str) -> &'static str {
    match outcome {
        "done" => "done",
        "stopped" => "stopped",
        "stuck" => "stuck",
        _ => "error",
    }
}

fn journal_lines(dir: &Path) -> u64 {
    std::fs::read(dir.join(JOURNAL_FILE))
        .map(|b| b.iter().filter(|c| **c == b'\n').count() as u64)
        .unwrap_or(0)
}

/// The engine's sink in the daemon: persists through the journal writer (progress events reach
/// `progress.ndjson` as they happen), keeps the job's live state (step, last message) for
/// `job.status`, and feeds the job's flight recorder, which it dumps to `<run>/flight/` when the
/// `stuck` event arrives (2.11a), before the job's status turns `stuck`.
struct Tee<'a> {
    w: JournalWriter,
    job: &'a Job,
    /// The job's care: its buffered entries are written into this journal, and acting steps'
    /// `sent` / outcome records feed the take-over machine and its injection log.
    care: Option<Arc<JobCare>>,
    epoch: u64,
}

impl Tee<'_> {
    /// care's entries so far, into the journal (their own `care-*` step ids).
    fn flush_care(&mut self) {
        let Some(c) = self.care.clone() else { return };
        for rec in c.drain_records(&self.job.meta.run_id, self.epoch) {
            if let Err(e) = self.w.journal(&rec) {
                self.job.st().last_error = Some(format!("care entry not journalled: {e}"));
            }
        }
    }
}

impl RunSink for Tee<'_> {
    fn progress(&mut self, ev: &ProgressEvent) {
        {
            let mut st = self.job.st();
            if ev.step_id.is_some() {
                st.step_id.clone_from(&ev.step_id);
            }
            st.last_msg.clone_from(&ev.msg);
        }
        self.w.progress(ev);
        if ev.kind == EventKind::Stuck {
            let dumped = self.job.flight().dump(
                &self.job.dir,
                &self.job.meta.run_id,
                &rfc3339_utc(unix_ms()),
            );
            if let Err(e) = dumped {
                self.job.st().last_error = Some(format!("flight recorder not dumped: {e}"));
            }
        }
    }
    fn journal(&mut self, rec: &JournalRecord) -> Result<(), String> {
        let r = self.w.journal(rec);
        if let Some(t) = self.care.as_ref().and_then(|c| c.takeover.as_ref()) {
            match rec.status {
                // the intent is durable and the calls go out next
                JournalStatus::Sent if r.is_ok() => t.step_sent(&rec.step_id),
                JournalStatus::Done | JournalStatus::Failed | JournalStatus::Interrupted => {
                    t.step_ended(&rec.step_id)
                }
                _ => {}
            }
        }
        self.flush_care();
        r
    }
    fn tree(&mut self, step_id: &str, xml: &str) {
        self.job.flight().tree(rfc3339_utc(unix_ms()), step_id, xml);
    }
    fn frame(&mut self, step_id: &str, image: &[u8]) {
        self.job
            .flight()
            .frame(rfc3339_utc(unix_ms()), step_id, image);
    }
}

/// The caller's lease, as the job sees it. The job never releases it: the caller acquired it.
struct JobLease<'a> {
    daemon: &'a Daemon,
    device: String,
    conn: u64,
}

impl LeaseHandle for JobLease<'_> {
    fn held(&self) -> bool {
        self.daemon
            .registry
            .check_holder(&self.device, self.conn)
            .is_ok()
    }
    fn release(&mut self) {}
}

/// Build (and, for an ended job, store) the receipt from the files on disk and the job's counters.
fn make_receipt(job: &Job, store: bool) -> Value {
    let records = load_journal(&job.dir.join(JOURNAL_FILE))
        .map(|l| l.records)
        .unwrap_or_default();
    let events = read_progress(&job.dir);
    // care's heat rests (2.13); the time rested is not "waiting for a person"
    let cooling_ms = job
        .control
        .care()
        .map_or(0, |c| c.cooling_ms.load(Ordering::SeqCst));
    let mut st = job.st();
    let wall_ms = unix_ms()
        .saturating_sub(job.meta.created_unix_ms)
        .max(st.work_ms);
    let errors: Vec<String> = st.last_error.iter().cloned().collect();
    let r = receipt::build(&ReceiptInput {
        run_id: &job.meta.run_id,
        device: &job.meta.device,
        flow: &job.meta.flow,
        outcome: outcome_of(st.status),
        planned: st.planned,
        done: st.done,
        retries: st.retries,
        retried_steps: st.retried_steps,
        wall_ms,
        // the engine's time includes the heat rests, so work = wall - waiting - cooling
        waiting_ms: wall_ms.saturating_sub(st.work_ms),
        cooling_ms,
        laptop_cpu_ms: st.cpu_ms,
        errors,
        records: &records,
        events: &events,
    });
    let v = r.to_value();
    if store {
        let _ = write_atomic(
            &job.dir.join("receipt.json"),
            (v.to_string() + "\n").as_bytes(),
        );
        st.receipt = Some(v.clone());
    }
    v
}

fn finish(
    job: &Job,
    rep: Option<&RunReport>,
    err: Option<String>,
    journal_seq: u64,
    cpu: Option<u64>,
) {
    // written before the status turns `done`, so a caller that sees `done` finds the recording
    if job.meta.record && rep.is_some_and(|r| matches!(r.stop, Stop::Done)) {
        if let Err(e) = write_recording(job) {
            // the caller asked for a recording: the receipt's `recording` says why there is none
            let _ = write_atomic(
                &job.dir.join("recording.json"),
                (json!({"error": e}).to_string() + "\n").as_bytes(),
            );
            job.st().last_msg = Some(format!("recording not saved: {e}"));
        }
    }
    let ended = {
        let mut st = job.st();
        st.running = false;
        st.journal_seq = journal_seq;
        if let Some(c) = cpu {
            st.cpu_ms = Some(st.cpu_ms.unwrap_or(0) + c);
        }
        match (rep, err) {
            (Some(rep), _) => {
                st.status = status_of_stop(&rep.stop);
                st.planned = rep.planned;
                st.done = rep.done;
                st.retries = rep.retries;
                st.retried_steps = rep.retried_steps;
                st.work_ms = rep.elapsed_ms;
                st.pause_reason = None;
                st.last_error = None;
                match &rep.stop {
                    Stop::Done | Stop::Stopped { .. } => {}
                    Stop::Paused(p) => {
                        st.pause_reason = Some(p.reason.detail().to_string());
                        st.step_id = Some(p.step_id.clone());
                    }
                    other => st.last_error = Some(other.describe()),
                }
            }
            (None, e) => {
                st.status = "error";
                st.last_error = e;
            }
        }
        !is_paused(st.status)
    };
    if ended {
        make_receipt(job, true);
    }
}

/// A paused job stopped with no thread running: care gives the settings back and its entries go
/// into the job's journal through a writer of its own.
fn end_care_outside(job: &Job, cfg: &JobsConfig) {
    let Some(c) = job.control.care() else { return };
    let notes = c.end(&job.meta.run_id);
    if let Ok(mut w) = JournalWriter::open(&cfg.dir, &job.meta.run_id, cfg.group_commit) {
        for rec in c.drain_records(&job.meta.run_id, 0) {
            let _ = w.journal(&rec);
        }
        let _ = w.commit();
    }
    if let Some(n) = notes.last() {
        job.st().last_msg = Some(n.clone());
    }
}

/// Paused, by anyone (a person who took the phone included): resumable, and the device is busy.
pub(crate) fn is_paused(status: &str) -> bool {
    matches!(status, "paused" | "taken_over")
}

/// The device's adb serial, mode and lease holder for a job's care; `None` with no care config.
fn job_care(daemon: &Daemon, job: &Job, cfg: &JobsConfig) -> Result<Option<Arc<JobCare>>, String> {
    if let Some(c) = job.control.care() {
        return Ok(Some(c));
    }
    let Some(care_cfg) = cfg.care.as_ref() else {
        return Ok(None);
    };
    let info = daemon.resolve(&job.meta.device).map_err(|e| e.message)?;
    let mode = daemon.backend().mode_of(&info);
    let session = daemon
        .registry
        .lease_of(&job.meta.device)
        .map(|l| l.holder)
        .unwrap_or_default();
    let ro = if info.hw_serial.is_empty() {
        info.serial.clone()
    } else {
        info.hw_serial.clone()
    };
    let c = Arc::new(JobCare::new(care_cfg, &info.serial, &ro, mode, &session)?);
    job.control.set_care(Some(c.clone()));
    Ok(Some(c))
}

/// One engine run of `job` (a start or a resume), on the job's own thread.
fn run_thread(daemon: Arc<Daemon>, job: Arc<Job>, conn: u64, resume: Option<Resume>) {
    let Some(cfg) = daemon.jobs_cfg.as_ref() else {
        return;
    };
    let cpu0 = thread_cpu_ms();
    let w = match JournalWriter::open(&cfg.dir, &job.meta.run_id, cfg.group_commit) {
        Ok(w) => w,
        Err(e) => {
            finish(
                &job,
                None,
                Some(format!("journal not opened: {e}")),
                journal_lines(&job.dir),
                None,
            );
            return;
        }
    };
    let epoch = daemon
        .registry
        .lease_of(&job.meta.device)
        .map_or(0, |l| l.epoch);
    // care (2.13) and take-over (2.15): leases first, before any step
    let care = match job_care(&daemon, &job, cfg) {
        Ok(c) => c,
        Err(e) => {
            job.st().last_error = Some(format!("care not attached: {e}"));
            None
        }
    };
    let mut tee = Tee {
        w,
        job: &job,
        care: care.clone(),
        epoch,
    };
    if let Some(c) = &care {
        for note in c.begin(&job.meta.run_id) {
            job.st().last_msg = Some(note);
        }
        tee.flush_care();
    }
    let mut cps = FileCheckpoints::new(&job.dir);
    let mut lease = JobLease {
        daemon: &daemon,
        device: job.meta.device.clone(),
        conn,
    };
    let mut opts = RunOpts::new(&job.meta.run_id);
    opts.device = Some(job.meta.device.clone());
    opts.epoch = epoch;
    opts.oem.clone_from(&job.meta.oem);
    opts.sdk = job.meta.sdk;
    opts.auto_dismiss = job.meta.auto_dismiss;
    opts.flight_frames = u32::try_from(FRAMES).unwrap_or(3);
    opts.start_unix_ms = unix_ms().saturating_sub(resume.as_ref().map_or(0, |r| r.from.elapsed_ms));
    let rules = Rulebook::seeded();
    let mut driver = SharedDriver { daemon: &daemon };
    let rep = engine::run(
        &mut driver,
        &job.flow,
        &rules,
        opts,
        Hooks {
            sink: &mut tee,
            checkpoints: &mut cps,
            lease: Some(&mut lease),
            control: Some(&job.control),
        },
        resume,
    );
    // an ended job gives its settings back (a paused one keeps them; a take-over already did)
    if let Some(c) = &care {
        if !matches!(rep.stop, Stop::Paused(_)) {
            for note in c.end(&job.meta.run_id) {
                job.st().last_msg = Some(note);
            }
        }
    }
    tee.flush_care();
    let commit = tee.w.commit();
    let seq = tee.w.next_seq();
    drop(tee);
    let cpu = thread_cpu_ms().zip(cpu0).map(|(b, a)| b.saturating_sub(a));
    finish(&job, Some(&rep), None, seq, cpu);
    if let Err(e) = commit {
        job.st().last_error = Some(e);
    }
    // the run's files are written: bound the directory now, not only at the next start. The run that
    // just ended stays (its receipt is about to be read), whatever the cap or the clock says.
    daemon.prune_runs_after(&job.meta.run_id);
}

impl Daemon {
    fn cfg(&self) -> Result<&JobsConfig, RpcError> {
        self.jobs_cfg.as_ref().ok_or_else(|| {
            RpcError::new(
                STATE_ERROR,
                "this daemon has no jobs directory; job methods are off",
            )
        })
    }

    /// A job by run id: in memory, else loaded from its run directory (after a daemon restart).
    fn job(&self, run_id: &str) -> Result<Arc<Job>, RpcError> {
        if let Some(j) = self.jobs.map().get(run_id) {
            return Ok(j.clone());
        }
        let cfg = self.cfg()?;
        let unknown = || RpcError::new(UNKNOWN_JOB, format!("no job {run_id:?}"));
        if !valid_run_id(run_id) {
            return Err(unknown());
        }
        let dir = run_dir(&cfg.dir, run_id).map_err(|_| unknown())?;
        let meta: JobMeta = std::fs::read_to_string(dir.join("job.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .ok_or_else(unknown)?;
        let fv: Value = std::fs::read_to_string(dir.join("flow.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .ok_or_else(unknown)?;
        let flow = load_flow(fv)?;
        let mut st = JobState {
            planned: flow.planned(),
            journal_seq: journal_lines(&dir),
            ..JobState::default()
        };
        match std::fs::read_to_string(dir.join("receipt.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            Some(r) => {
                st.status = status_from_outcome(r["detail"]["outcome"].as_str().unwrap_or(""));
                st.done =
                    u32::try_from(r["receipt"]["steps_done"].as_u64().unwrap_or(0)).unwrap_or(0);
                st.receipt = Some(r);
            }
            None => {
                // it was running or paused when the daemon went away: resumable
                st.status = "paused";
                st.pause_reason = Some(
                    read_checkpoint(&dir)
                        .ok()
                        .flatten()
                        .map_or_else(|| "daemon_restart".to_string(), |c| c.reason),
                );
            }
        }
        let job = Arc::new(Job::new(meta, flow, dir, st));
        self.jobs.map().insert(run_id.to_string(), job.clone());
        Ok(job)
    }

    fn spawn_run(&self, job: Arc<Job>, conn: u64, resume: Option<Resume>) -> Result<(), RpcError> {
        let me = self
            .handle()
            .ok_or_else(|| RpcError::new(INTERNAL_ERROR, "daemon is shutting down"))?;
        job.control.set(None);
        {
            let mut st = job.st();
            st.running = true;
            st.status = "running";
            st.pause_reason = None;
            st.conn = conn;
        }
        let name = format!("job {}", job.meta.run_id);
        let j = job.clone();
        std::thread::Builder::new()
            .name(name)
            .spawn(move || run_thread(me, j, conn, resume))
            .map(|_| ())
            .map_err(|e| {
                let mut st = job.st();
                st.running = false;
                st.status = "error";
                RpcError::new(INTERNAL_ERROR, format!("job thread not started: {e}"))
            })
    }

    fn status_result(job: &Job) -> JobStatusResult {
        let st = job.st();
        let step = st.step_id.clone().unwrap_or_else(|| "-".into());
        let why = match (&st.pause_reason, &st.last_error) {
            (Some(p), _) => format!("; paused: {p}"),
            (None, Some(e)) => format!("; last error: {e}"),
            _ => String::new(),
        };
        JobStatusResult {
            run_id: job.meta.run_id.clone(),
            device: job.meta.device.clone(),
            flow: job.meta.flow.clone(),
            status: st.status.to_string(),
            step_id: st.step_id.clone(),
            steps_planned: st.planned,
            steps_done: st.done,
            retries: st.retries,
            line: format!(
                "{} at {step}, {} of {} steps done, {} retries{why}",
                st.status, st.done, st.planned, st.retries
            ),
            last_error: st.last_error.clone(),
            pause_reason: st.pause_reason.clone(),
            journal_seq: if st.running {
                journal_lines(&job.dir)
            } else {
                st.journal_seq
            },
        }
    }

    /// Wait (blocking, bounded) until the job's thread has ended.
    fn wait_idle(job: &Job, limit: Duration) {
        let t = Instant::now();
        while job.st().running && t.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The `job.*` methods. Every one of them needs the caller to hold the device's lease.
    pub(crate) fn job_dispatch(
        &self,
        conn: u64,
        method: &str,
        params: &Value,
    ) -> Result<Value, RpcError> {
        self.cfg()?;
        match method {
            "job.start" => self.job_start(conn, parse_params(params)?),
            "job.status" => {
                let p: JobRef = parse_params(params)?;
                let job = self.job(&p.run_id)?;
                self.require_holder(&job.meta.device, conn)?;
                to_value(&Self::status_result(&job))
            }
            "job.pause" => {
                let p: JobRef = parse_params(params)?;
                let job = self.job(&p.run_id)?;
                self.require_holder(&job.meta.device, conn)?;
                let (running, status) = {
                    let st = job.st();
                    (st.running, st.status)
                };
                if running {
                    job.control.set(Some(Interrupt::Pause));
                    Self::wait_idle(&job, CONTROL_WAIT);
                } else if !is_paused(status) {
                    return Err(RpcError::new(
                        JOB_ERROR,
                        format!(
                            "job {} is {status}; only a running job can be paused",
                            p.run_id
                        ),
                    ));
                }
                to_value(&Self::status_result(&job))
            }
            "job.resume" => {
                let p: JobResumeParams = parse_params(params)?;
                let job = self.job(&p.run_id)?;
                self.require_holder(&job.meta.device, conn)?;
                let (running, status) = {
                    let st = job.st();
                    (st.running, st.status)
                };
                if running || !is_paused(status) {
                    return Err(RpcError::new(
                        JOB_ERROR,
                        format!(
                            "job {} is {status}; only a paused job can be resumed",
                            p.run_id
                        ),
                    ));
                }
                self.resume_job(&job, conn, p.human_confirmed.unwrap_or(false))?;
                to_value(&Self::status_result(&job))
            }
            "job.stop" => {
                let p: JobRef = parse_params(params)?;
                let job = self.job(&p.run_id)?;
                self.require_holder(&job.meta.device, conn)?;
                let (running, status) = {
                    let st = job.st();
                    (st.running, st.status)
                };
                if running {
                    job.control.set(Some(Interrupt::Stop));
                    Self::wait_idle(&job, CONTROL_WAIT);
                } else if is_paused(status) {
                    end_care_outside(&job, self.cfg()?);
                    {
                        let mut st = job.st();
                        st.status = "stopped";
                        st.pause_reason = None;
                        st.journal_seq = journal_lines(&job.dir);
                    }
                    make_receipt(&job, true);
                } else {
                    return Err(RpcError::new(
                        JOB_ERROR,
                        format!("job {} has already ended ({status})", p.run_id),
                    ));
                }
                let st = job.st();
                to_value(&JobStopResult {
                    run_id: p.run_id,
                    status: st.status.to_string(),
                    journal_seq: st.journal_seq,
                })
            }
            "job.receipt" => {
                let p: JobRef = parse_params(params)?;
                let job = self.job(&p.run_id)?;
                self.require_holder(&job.meta.device, conn)?;
                let stored = job.st().receipt.clone();
                let v = stored.unwrap_or_else(|| make_receipt(&job, false));
                to_value(&JobReceiptResult {
                    run_id: p.run_id,
                    receipt: v["receipt"].clone(),
                    detail: v["detail"].clone(),
                    steps: Some(step_rows(&job)),
                    recording: recording_of(&job),
                })
            }
            "job.dry_run" => {
                let p: JobDryRunParams = parse_params(params)?;
                let info = self.resolve(&p.device)?;
                let key = device_key(&info);
                self.require_holder(&key, conn)?;
                let mode = self.backend().mode_of(&info);
                let flow = load_flow(flow_json(&p.flow, None)?)?;
                let trees: Vec<SavedTree<'_>> = p
                    .trees
                    .iter()
                    .map(|t| SavedTree {
                        name: &t.name,
                        xml: &t.xml,
                    })
                    .collect();
                let rep = dry_run(&flow, &trees, mode, &Rulebook::seeded());
                let rows = rep
                    .rows
                    .iter()
                    .map(|r| DryRunRow {
                        step_id: r.step_id.clone(),
                        kind: r.kind.to_string(),
                        field: r.field.clone(),
                        selector: r.selector.clone(),
                        row: r.row.clone(),
                        verdict: match &r.verdict {
                            Verdict2::Match { tree, describe, .. } => {
                                format!("match in {tree}: {describe}")
                            }
                            Verdict2::Ambiguous { tree, describe } => {
                                format!("ambiguous in {tree}: {describe}")
                            }
                            Verdict2::NoMatch { tried } => {
                                format!("no match: {}", tried.join("; "))
                            }
                            Verdict2::NoSelector => "no selector".into(),
                            Verdict2::Refused(w) => format!("refused: {w}"),
                            Verdict2::Error(e) => format!("error: {e}"),
                        },
                    })
                    .collect();
                let overlays = rep
                    .overlays
                    .iter()
                    .map(|(t, v)| DryRunOverlay {
                        tree: t.clone(),
                        verdict: v.clone(),
                    })
                    .collect();
                to_value(&JobDryRunResult {
                    flow: rep.flow.clone(),
                    matched: rep.matched(),
                    rows,
                    overlays,
                    text: rep.render(),
                })
            }
            "job.run_script" => self.run_script(conn, parse_params(params)?),
            other => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            )),
        }
    }

    /// Restart a paused job from the journal's resume plan (an `interrupted` step a person cut off
    /// is re-verified, never blindly re-sent).
    fn resume_job(&self, job: &Arc<Job>, conn: u64, human_confirmed: bool) -> Result<(), RpcError> {
        let loaded = match load_journal(&job.dir.join(JOURNAL_FILE)) {
            Ok(l) => l.records,
            Err(LoadError::Missing) => vec![],
            Err(e) => return Err(state_err(e)),
        };
        let cp = read_checkpoint(&job.dir).map_err(state_err)?;
        let resume = match plan(&job.meta.run_id, &loaded, cp.as_ref(), human_confirmed) {
            Start::Fresh => None,
            Start::From(r) => Some(r),
        };
        self.spawn_run(job.clone(), conn, resume)
    }

    /// A human touch on `device` (hub time `at`): the running job's take-over machine decides.
    /// `None` when no job with take-over runs there.
    pub fn human_touch(
        &self,
        device: &str,
        at: u64,
        source: handover::mode::Source,
    ) -> Option<crate::takeover::TouchOutcome> {
        let job = self
            .jobs
            .map()
            .values()
            .find(|j| j.meta.device == device && j.st().running)
            .cloned()?;
        let care = job.control.care()?;
        let t = care.takeover.as_ref()?;
        let out = t.touch(at, source);
        if out.take_over {
            job.control.set(Some(Interrupt::TakeOver));
            job.st().last_msg = Some(format!(
                "taken over by a person{}",
                out.interrupted
                    .as_ref()
                    .map(|s| format!(" during {s}"))
                    .unwrap_or_default()
            ));
        }
        Some(out)
    }

    /// Hub time now on `device`'s take-over machine's clock, or `None` when no job there has one.
    fn takeover_now(&self, device: &str) -> Option<u64> {
        self.jobs
            .map()
            .values()
            .find(|j| j.meta.device == device)
            .and_then(|j| j.control.care())
            .and_then(|c| c.takeover.as_ref().map(crate::takeover::Takeover::now))
    }

    /// One (phone time, hub time) pair for a phone clock: the stored offset keeps the least-delayed
    /// one (`touch::ClockMap`), so transport delay never moves a touch later than it was.
    pub fn observe_phone_clock(&self, device: &str, clock: PhoneClock, phone_us: u64, hub_ms: u64) {
        self.jobs
            .clocks()
            .entry(device.to_string())
            .or_default()
            .observe(clock, phone_us, hub_ms);
    }

    /// A phone timestamp (microseconds on `clock`) on the hub's clock. `None` until a pair was seen:
    /// the two clocks start at unrelated zeros, so an unmapped phone time is never hub time.
    pub fn phone_to_hub_ms(&self, device: &str, clock: PhoneClock, phone_us: u64) -> Option<u64> {
        self.jobs
            .clocks()
            .get(device)
            .and_then(|c| c.try_to_hub_ms(clock, phone_us))
    }

    /// A person's touch stamped on a phone clock: `touch_us` is when it happened, `sent_us` the same
    /// clock's reading when the report left the phone (the newest touchscreen `getevent` line; the
    /// overlay's `elapsedRealtime` at send). The report just arrived, so (`sent_us`, hub now) is a
    /// sample; the touch is then placed on the hub clock and goes to [`Daemon::human_touch`]. Both the
    /// injection windows and the mode machine's touch log run on the hub clock.
    pub fn phone_touch(
        &self,
        device: &str,
        clock: PhoneClock,
        touch_us: u64,
        sent_us: u64,
        source: handover::mode::Source,
    ) -> Option<crate::takeover::TouchOutcome> {
        let now = self.takeover_now(device)?;
        self.observe_phone_clock(device, clock, sent_us, now);
        self.place_touch(device, clock, touch_us, sent_us, now, source)
    }

    /// Places a touch on the hub clock without sampling (the caller already did); see
    /// [`Daemon::touch_time_on_hub`] for how it is dated.
    fn place_touch(
        &self,
        device: &str,
        clock: PhoneClock,
        touch_us: u64,
        sent_us: u64,
        now: u64,
        source: handover::mode::Source,
    ) -> Option<crate::takeover::TouchOutcome> {
        let at = self.touch_time_on_hub(device, clock, touch_us, sent_us, now);
        self.human_touch(device, at, source)
    }

    /// When a person's touch happened, on the hub clock. The stored offset answers only when the report
    /// itself agrees with it: the report's own sample (`sent_us` against `now`) must sit within
    /// [`handover::touch::CLOCK_BREAK_MS`] of the stored offset, and the mapped time must fall in
    /// `[hi - MAX_PLAUSIBLE_DELAY_MS, hi]`, where `hi = now - (sent - touch)` is the latest the touch can
    /// have been. Otherwise the phone's clock has moved since the offset was stored (it suspended, even for a
    /// few seconds, or it rebooted), and the touch is dated `hi`, from the arrival. Dating a real touch too
    /// early would put it before the agent's hold and the person would be ignored; dating it up to a
    /// transport delay late costs nothing.
    pub fn touch_time_on_hub(
        &self,
        device: &str,
        clock: PhoneClock,
        touch_us: u64,
        sent_us: u64,
        now: u64,
    ) -> u64 {
        let lead_ms = sent_us.saturating_sub(touch_us) / 1000;
        let hi = now.saturating_sub(lead_ms);
        let lo = hi.saturating_sub(MAX_PLAUSIBLE_DELAY_MS);
        let agrees = self
            .jobs
            .clocks()
            .get(device)
            .is_none_or(|c| c.plausible(clock, sent_us, now, handover::touch::CLOCK_BREAK_MS));
        match self.phone_to_hub_ms(device, clock, touch_us) {
            Some(t) if agrees && (lo..=hi).contains(&t) => t,
            _ => hi,
        }
    }

    /// Detector 1 on a chunk of the LIVE `getevent -lt` stream (the device map from `getevent -pl`).
    /// Unlike [`Daemon::feed_getevent`] it keeps the phone's own timing: the chunk's newest
    /// TOUCHSCREEN line time is the sample for the kernel-clock offset (other input nodes, a stray
    /// line, a garbled timestamp are not), and each touch-down is placed on the hub clock from its
    /// own kernel timestamp, so a chunk that arrived late still dates a touch to when it happened.
    /// The detector is kept per device, so call it per line or per read, as the stream delivers.
    pub fn feed_getevent_live(
        &self,
        device: &str,
        map: &str,
        lines: &str,
    ) -> Vec<crate::takeover::TouchOutcome> {
        let (touches, screen_times) = {
            let mut dets = self.jobs.detectors();
            let slot = dets
                .entry(device.to_string())
                .or_insert_with(|| (map.to_string(), Detector::from_map(map)));
            if slot.0 != map {
                *slot = (map.to_string(), Detector::from_map(map));
            }
            let d = &mut slot.1;
            let touches = d.feed_all(lines);
            let times: Vec<u64> = lines
                .lines()
                .filter_map(|l| {
                    if let Line::Event { t_us, node, .. } = parse_line(l) {
                        d.is_touchscreen(&node).then_some(t_us)
                    } else {
                        None
                    }
                })
                .collect();
            (touches, times)
        };
        let Some(now) = self.takeover_now(device) else {
            return vec![];
        };
        // prefer a line that agrees with the clock we already know: one wild timestamp must not become the sample
        let believable = |t_us: u64| {
            self.jobs.clocks().get(device).is_none_or(|c| {
                c.plausible(
                    PhoneClock::Kernel,
                    t_us,
                    now,
                    i64::try_from(MAX_PLAUSIBLE_DELAY_MS).unwrap_or(i64::MAX),
                )
            })
        };
        let Some(sent_us) = screen_times
            .iter()
            .copied()
            .filter(|t| believable(*t))
            .max()
            .or_else(|| screen_times.iter().copied().max())
        else {
            return vec![];
        };
        // a chunk with no touch in it still calibrates the clock: the next touch needs the offset
        self.observe_phone_clock(device, PhoneClock::Kernel, sent_us, now);
        touches
            .iter()
            .filter_map(|t| {
                self.place_touch(
                    device,
                    PhoneClock::Kernel,
                    t.at_us,
                    sent_us,
                    now,
                    handover::mode::Source::Getevent,
                )
            })
            .collect()
    }

    /// Detector 1 on `getevent -lt` text (the device map from `getevent -pl`): every human
    /// touch-down it finds goes to [`Daemon::human_touch`], stamped on ARRIVAL with the hub clock,
    /// whatever the lines say. Right for a replayed or one-shot capture; the live stream uses
    /// [`Daemon::feed_getevent_live`], which maps phone time through the stored clock offset.
    pub fn feed_getevent(
        &self,
        device: &str,
        map: &str,
        lines: &str,
    ) -> Vec<crate::takeover::TouchOutcome> {
        let mut d = handover::touch::Detector::from_map(map);
        let mut out = vec![];
        for _touch in d.feed_all(lines) {
            let now = self
                .jobs
                .map()
                .values()
                .find(|j| j.meta.device == device)
                .and_then(|j| j.control.care())
                .and_then(|c| c.takeover.as_ref().map(crate::takeover::Takeover::now));
            if let Some(o) =
                now.and_then(|at| self.human_touch(device, at, handover::mode::Source::Getevent))
            {
                out.push(o);
            }
        }
        out
    }

    // ---------------------------------------------------------------- the job view (2.11b's HTTP half)

    /// Every job this daemon knows: in memory, plus the run directories on disk (after a restart).
    fn all_jobs(&self) -> Vec<Arc<Job>> {
        if let Some(cfg) = self.jobs_cfg.as_ref() {
            if let Ok(rd) = std::fs::read_dir(&cfg.dir) {
                for e in rd.flatten() {
                    let id = e.file_name().to_string_lossy().to_string();
                    if id.starts_with("job-") && e.path().join("job.json").is_file() {
                        let _ = self.job(&id);
                    }
                }
            }
        }
        let mut v: Vec<Arc<Job>> = self.jobs.map().values().cloned().collect();
        v.sort_by_key(|j| std::cmp::Reverse(j.meta.created_unix_ms));
        v
    }

    /// One job as `schemas/job-view.json` (object form), with care's `changes_in_force`.
    fn view_of(&self, job: &Job) -> Value {
        let (status, planned, done) = {
            let st = job.st();
            (
                if st.status.is_empty() {
                    "queued"
                } else {
                    st.status
                },
                st.planned,
                st.done,
            )
        };
        let id = &job.meta.run_id;
        let changes: Vec<Value> = self
            .changes_in_force(&job.meta.device)
            .iter()
            .map(crate::jobcare::change_json)
            .collect();
        json!({
            "job_id": id, "run_id": id, "device": job.meta.device, "flow": job.meta.flow, "status": status,
            "steps_planned": planned, "steps_done": done,
            "events_url": format!("/jobs/{id}/events"), "frame_url": format!("/jobs/{id}/frame.jpg"),
            "changes_in_force": changes,
        })
    }

    /// `GET /jobs`: newest first.
    pub fn view_jobs(&self) -> Value {
        Value::Array(self.all_jobs().iter().map(|j| self.view_of(j)).collect())
    }

    /// `GET /jobs/:id`.
    pub fn view_job(&self, run_id: &str) -> Option<Value> {
        if !run_id.starts_with("job-") {
            return None;
        }
        self.job(run_id).ok().map(|j| self.view_of(&j))
    }

    /// The job's progress lines (`schemas/progress.json`, one per line) with `seq` above `after`,
    /// and whether the job has ended (no more will come). `None`: no such job.
    pub fn view_events(
        &self,
        run_id: &str,
        after: Option<u64>,
    ) -> Option<(Vec<(u64, String)>, bool)> {
        if !run_id.starts_with("job-") {
            return None;
        }
        let job = self.job(run_id).ok()?;
        let text = std::fs::read_to_string(job.dir.join(PROGRESS_FILE)).unwrap_or_default();
        let mut out = vec![];
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Some(seq) = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| v["seq"].as_u64())
            else {
                continue;
            };
            if after.is_none_or(|a| seq > a) {
                out.push((seq, line.to_string()));
            }
        }
        let ended = {
            let st = job.st();
            !st.running && !is_paused(st.status)
        };
        Some((out, ended))
    }

    /// The newest flight-recorder frame of the job, if it is a JPEG (2.11a records frames only
    /// while a screen keeps coming back).
    pub fn view_frame(&self, run_id: &str) -> Option<Vec<u8>> {
        let job = self.job(run_id).ok()?;
        let f = job.flight().latest_frame()?;
        (f.starts_with(&[0xff, 0xd8])).then_some(f)
    }

    /// `POST /jobs/:id/control`: `Ok` is 202; `Err((status, why))`.
    pub fn view_control(&self, run_id: &str, action: &str) -> Result<(), (u16, String)> {
        let job = self.job(run_id).map_err(|e| (404, e.message))?;
        let (running, status, conn) = {
            let st = job.st();
            (st.running, st.status, st.conn)
        };
        let wrong = |what: &str| {
            Err((
                409,
                format!("job {run_id} is {status}; it cannot {what} now"),
            ))
        };
        match action {
            "pause" if running => job.control.set(Some(Interrupt::Pause)),
            "pause" => return wrong("pause"),
            "stop" if running => job.control.set(Some(Interrupt::Stop)),
            "stop" if is_paused(status) => {
                let cfg = self.cfg().map_err(|e| (500, e.message))?;
                end_care_outside(&job, cfg);
                {
                    let mut st = job.st();
                    st.status = "stopped";
                    st.pause_reason = None;
                    st.journal_seq = journal_lines(&job.dir);
                }
                make_receipt(&job, true);
            }
            "stop" => return wrong("stop"),
            "take_over" if running => {
                // the person's own click counts as a touch Detector 1 saw: the same take-over path
                let at = job
                    .control
                    .care()
                    .and_then(|c| c.takeover.as_ref().map(crate::takeover::Takeover::now));
                let handled = at
                    .and_then(|at| {
                        self.human_touch(&job.meta.device, at, handover::mode::Source::Getevent)
                    })
                    .is_some_and(|o| o.take_over);
                if !handled {
                    job.control.set(Some(Interrupt::TakeOver));
                }
            }
            "take_over" => return wrong("be taken over"),
            "resume" | "hand_back" if !running && is_paused(status) => {
                if self.registry.check_holder(&job.meta.device, conn).is_err() {
                    return Err((409, format!("the session that ran job {run_id} no longer holds {}; resume it from that session", job.meta.device)));
                }
                self.resume_job(&job, conn, false)
                    .map_err(|e| (409, e.message))?;
            }
            "resume" | "hand_back" => return wrong("resume"),
            "retry" => {
                return Err((
                    501,
                    "retry is not wired: start the flow again from the session".into(),
                ))
            }
            other => return Err((400, format!("unknown action {other:?}"))),
        }
        Ok(())
    }

    fn job_start(&self, conn: u64, p: JobStartParams) -> Result<Value, RpcError> {
        let cfg = self.cfg()?;
        let info = self.resolve(&p.device)?;
        let key = device_key(&info);
        self.require_holder(&key, conn)?;
        let given = match (&p.flow, &p.steps) {
            (Some(f), None) if p.app.is_none() => f.clone(),
            (Some(_), None) => {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "app goes with steps; a flow names its own app",
                ))
            }
            (None, Some(steps)) => steps_flow(steps, p.app.as_deref())?,
            (Some(_), Some(_)) => {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "give flow or steps, not both",
                ))
            }
            (None, None) => {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "job.start needs a flow or a list of steps",
                ))
            }
        };
        // replay: `flow` may name a recorded file inside the jobs directory
        let given = match &given {
            Value::String(s) if looks_like_flow_file(s) => {
                Value::String(flow_file_text(&cfg.dir, s)?)
            }
            _ => given,
        };
        let fv = flow_json(&given, p.vars)?;
        let flow = load_flow(fv.clone())?;
        // Phase 2 refuses R2+ with no exception (the engine's preflight would too; here nothing is created)
        if let Some(s) = flow
            .all_steps()
            .into_iter()
            .find(|s| s.effective_risk().rank() >= Risk::R2.rank())
        {
            let risk = s.effective_risk().as_str();
            return Err(RpcError::new(REFUSED, format!("job refused before start: step {} is {risk}; Phase 2 refuses R2 and above with no exception", s.id)).with_data(json!({"step_id": s.id, "risk": risk})));
        }
        if let Some(busy) = self.jobs.map().values().find(|j| {
            j.meta.device == key && (j.st().status == "running" || is_paused(j.st().status))
        }) {
            let status = busy.st().status;
            return Err(RpcError::new(
                JOB_ERROR,
                format!(
                    "device {key} already has job {} ({status}); stop it first",
                    busy.meta.run_id
                ),
            ));
        }
        // 7-day local retention, pruned at job start (2.11a); jobs live in this daemon are kept
        self.prune_old_runs(cfg, None);
        let n = self.jobs.next.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("job-{}-{n}", unix_ms());
        let dir = run_dir(&cfg.dir, &run_id).map_err(state_err)?;
        std::fs::create_dir_all(&dir).map_err(|e| state_err(format!("{}: {e}", dir.display())))?;
        let meta = JobMeta {
            run_id: run_id.clone(),
            device: key,
            flow: flow.flow.clone(),
            created_unix_ms: unix_ms(),
            oem: p.oem,
            sdk: p.sdk,
            auto_dismiss: p.auto_dismiss.unwrap_or(true),
            record: p.record.unwrap_or(false),
        };
        write_atomic(
            &dir.join("flow.json"),
            (record::without_secret_values(&fv).to_string() + "\n").as_bytes(),
        )
        .map_err(state_err)?;
        write_atomic(
            &dir.join("job.json"),
            (serde_json::to_string(&meta).unwrap_or_default() + "\n").as_bytes(),
        )
        .map_err(state_err)?;
        let st = JobState {
            status: "running",
            planned: flow.planned(),
            ..JobState::default()
        };
        let job = Arc::new(Job::new(meta, flow, dir.clone(), st));
        self.jobs.map().insert(run_id.clone(), job.clone());
        self.spawn_run(job, conn, None)?;
        to_value(&JobStartResult {
            run_id,
            status: "running".into(),
            journal: dir.join(JOURNAL_FILE).display().to_string(),
            progress: dir.join(PROGRESS_FILE).display().to_string(),
        })
    }

    /// Prune the jobs directory now (age and count), as `job.start` does; the daemon also calls it
    /// when a job ends, so the directory is bounded without a start. A no-op without a jobs dir.
    pub fn prune_runs_now(&self) -> crate::runner::flight::PruneReport {
        self.jobs_cfg
            .as_ref()
            .map(|c| self.prune_old_runs(c, None))
            .unwrap_or_default()
    }

    fn prune_runs_after(&self, ended_run_id: &str) {
        if let Some(c) = self.jobs_cfg.as_ref() {
            self.prune_old_runs(c, Some(ended_run_id));
        }
    }

    /// Delete run directories older than `cfg.retention_ms` by `cfg.clock` (2.11a), then the oldest
    /// beyond `cfg.max_runs`. Jobs running
    /// or paused in this daemon are kept whatever their age; pruned ended jobs leave the map too.
    fn prune_old_runs(
        &self,
        cfg: &JobsConfig,
        also_keep: Option<&str>,
    ) -> crate::runner::flight::PruneReport {
        let live: Vec<String> = self
            .jobs
            .map()
            .iter()
            .filter(|(_, j)| j.st().status == "running" || is_paused(j.st().status))
            .map(|(id, _)| id.clone())
            .collect();
        let rep = prune_runs_capped(
            &cfg.dir,
            (cfg.clock)(),
            cfg.retention_ms,
            cfg.max_runs,
            &|id| also_keep == Some(id) || live.iter().any(|l| l == id),
        );
        if !rep.pruned.is_empty() {
            let mut map = self.jobs.map();
            for id in &rep.pruned {
                map.remove(id);
            }
        }
        rep
    }

    fn run_script(&self, conn: u64, p: RunScriptParams) -> Result<Value, RpcError> {
        let cfg = self.cfg()?;
        let info = self.resolve(&p.device)?;
        let key = device_key(&info);
        self.require_holder(&key, conn)?;
        let (mode, allowed) = {
            let b = self.backend();
            (b.mode_of(&info), b.allowed(&info))
        };
        let exe = match &cfg.sandbox_exe {
            Some(p) => p.clone(),
            None => sandbox::default_child_exe().map_err(state_err)?,
        };
        let n = self.jobs.next.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("script-{}-{n}", unix_ms());
        let w = JournalWriter::open(&cfg.dir, &run_id, cfg.group_commit).map_err(state_err)?;
        let journal_path = w.journal_path();
        let epoch = self.registry.lease_of(&key).map_or(0, |l| l.epoch);
        let me = self
            .handle()
            .ok_or_else(|| RpcError::new(INTERNAL_ERROR, "daemon is shutting down"))?;
        let mut bridge = DaemonBridge {
            daemon: me,
            device: key,
            serial: info.serial.clone(),
            w,
            run_id: run_id.clone(),
            epoch,
            call_timeout_ms: cfg.call_timeout_ms,
            shell_timeout_ms: cfg.shell_timeout_ms,
            shell: cfg.shell.clone(),
            calls: 0,
        };
        let device = sandbox::Device {
            serial: info.serial.clone(),
            hw_serial: info.hw_serial.clone(),
            allowed,
            mode,
        };
        let res = sandbox::Sandbox::new(exe)
            .with_limits(cfg.limits.clone())
            .run(&p.script, &device, &mut bridge);
        // every refusal the sandbox returns goes into the journal, in the sandbox's order
        let mut journalled = 0;
        for r in &res.refusals {
            let class = r.class.map(|c| c.as_str()).unwrap_or("-");
            let intent = format!(
                "refused {} {:?}: {} ({class}): {}",
                r.binding, r.request, r.kind, r.reason
            );
            let post = json!({"kind": "none", "refusal": {"seq": r.seq, "binding": r.binding, "kind": r.kind, "class": class, "at_ms": r.at_ms}});
            let rec = bridge.record(
                &format!("script-refusal-{}", r.seq),
                intent,
                post,
                JournalStatus::Failed,
            );
            if bridge.w.journal(&rec).is_ok() {
                journalled += 1;
            }
        }
        let outcome = serde_json::to_value(res.outcome)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let end = bridge.record(
            "script",
            format!(
                "run_script ended: {outcome}, {} binding requests, {} refused",
                res.calls,
                res.refusals.len()
            ),
            json!({"kind": "none"}),
            if outcome == "ok" {
                JournalStatus::Done
            } else {
                JournalStatus::Failed
            },
        );
        let _ = bridge.w.journal(&end);
        bridge.w.commit().map_err(state_err)?;
        to_value(&RunScriptResult {
            run_id,
            outcome,
            value: res.value.clone(),
            error: res.error.clone(),
            stdout: res.stdout.clone(),
            stdout_truncated: res.stdout_truncated,
            refusals: res
                .refusals
                .iter()
                .filter_map(|r| serde_json::to_value(r).ok())
                .collect(),
            calls: res.calls,
            wall_ms: res.wall_ms,
            cpu_ms: res.cpu_ms,
            journal: journal_path.display().to_string(),
            journalled_refusals: journalled,
        })
    }
}

/// Run `f` on its own thread and give up waiting after `ms` (the sandbox parent cannot pre-empt a
/// blocked bridge call, so every bridge call bounds its own time).
pub(crate) fn with_timeout<T: Send + 'static>(
    ms: u64,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("bridge call".into())
        .spawn(move || {
            let _ = tx.send(f());
        });
    if let Err(e) = spawned {
        return Err(format!("bridge call thread not started: {e}"));
    }
    match rx.recv_timeout(Duration::from_millis(ms)) {
        Ok(r) => r,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "timed out after {ms} ms; the call may still complete on the phone"
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("the bridge call ended without an answer".into())
        }
    }
}

/// The daemon side of `run_script` (slice c's [`sandbox::Bridge`]).
pub struct DaemonBridge {
    daemon: Arc<Daemon>,
    /// Lease key (hardware serial).
    device: String,
    /// adb serial, for `adb -s <serial> shell`.
    serial: String,
    w: JournalWriter,
    run_id: String,
    epoch: u64,
    call_timeout_ms: u64,
    shell_timeout_ms: u64,
    shell: Option<Arc<GuardedRunner<DynRunner>>>,
    calls: u64,
}

impl DaemonBridge {
    fn record(
        &self,
        step_id: &str,
        intent: String,
        post: Value,
        status: JournalStatus,
    ) -> JournalRecord {
        let ts = rfc3339_utc(unix_ms());
        JournalRecord {
            v: 1,
            run_id: self.run_id.clone(),
            seq: 0,
            step_id: step_id.to_string(),
            intent,
            post,
            status,
            ts_phone: ts.clone(),
            ts_hub: ts,
            epoch: self.epoch,
            display: 0,
        }
    }
}

impl sandbox::Bridge for DaemonBridge {
    fn call(&mut self, call: &Call, _raw_args: &Value) -> Result<Value, String> {
        self.calls += 1;
        let step = format!("script-call-{}", self.calls);
        let acting = call.is_acting();
        if acting {
            // the same rule as flow steps: the intent is durable before the phone is touched
            let planned = self.record(
                &step,
                format!("script {}", call.name()),
                json!({"kind": "none"}),
                JournalStatus::Planned,
            );
            let sent = self.record(
                &step,
                format!("script {}", call.name()),
                json!({"kind": "none"}),
                JournalStatus::Sent,
            );
            self.w
                .journal(&planned)
                .and_then(|()| self.w.journal(&sent))
                .map_err(|e| format!("not sent: the intent record is not durable ({e})"))?;
        }
        let extra = match call {
            Call::UiTree {
                timeout_ms: Some(t),
                ..
            } => *t,
            Call::Wait { seconds } if seconds.is_finite() => {
                (seconds.clamp(0.0, 10.0) * 1000.0) as u64
            }
            _ => 0,
        };
        let (daemon, device, c) = (self.daemon.clone(), self.device.clone(), call.clone());
        // Core::call with the device's mode: the backend's gate runs again inside
        let res = with_timeout(self.call_timeout_ms + extra, move || {
            let r = daemon.backend().call(&device, &c);
            r.map(|o| json!({"text": o.text, "image_bytes": o.image.map(|i| i.len())}))
                .map_err(|e| e.to_string())
        });
        if acting {
            let (status, intent) = match &res {
                Ok(_) => (JournalStatus::Done, format!("script {}", call.name())),
                Err(e) => (
                    JournalStatus::Failed,
                    format!("script {}: {e}", call.name()),
                ),
            };
            let rec = self.record(&step, intent, json!({"kind": "none"}), status);
            let _ = self.w.journal(&rec);
        }
        res
    }

    fn shell(&mut self, argv: &[String]) -> Result<String, String> {
        // the slice-c classifier again, on exactly this argv (defence in depth), then the guarded runner
        match classify(&argv.join(" ")) {
            ShellVerdict::Allowed { argv: a } if a == argv => {}
            ShellVerdict::Allowed { .. } => {
                return Err(
                    "refused by the daemon's re-check: the argv changed when re-split".into(),
                )
            }
            ShellVerdict::Refused { class, reason } => {
                return Err(format!(
                    "refused by the daemon's re-check ({}): {reason}",
                    class.as_str()
                ))
            }
        }
        // D3b: core's shell guard (deny-by-default families, approval for `asked`, audit log).
        // Nothing reaches the runner unless it says yes. `Some(ticket)`: the screen lock admitted
        // this very line (a gate that answers after the timeout keeps its ticket to itself, and a
        // newer call on the phone makes it count for nothing)
        let ticket = {
            let (daemon, serial, line) = (self.daemon.clone(), self.serial.clone(), argv.join(" "));
            with_timeout(SHELL_GATE_TIMEOUT_MS, move || {
                daemon
                    .backend()
                    .shell_gate(&serial, &line)
                    .map_err(|e| format!("refused by the shell guard: {e}"))
            })?
        };
        // no runner: nothing is sent, and the ticket is dropped unused
        let runner = self
            .shell
            .clone()
            .ok_or("no adb runner is configured in this daemon; nothing was sent")?;
        let line = argv
            .iter()
            .map(|a| sh_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        let args = vec![
            "-s".to_string(),
            self.serial.clone(),
            "shell".to_string(),
            line,
        ];
        let t = self.shell_timeout_ms;
        let out = with_timeout(t + 1_000, move || match runner.run(&args, t) {
            Ok(o) if o.ok() => Ok(o.stdout),
            Ok(o) => Err(format!(
                "adb shell exited {:?}: {}",
                o.code,
                o.stderr.trim()
            )),
            Err(e) => Err(e.to_string()),
        });
        // a line the lock admitted has run (or timed out running): only now is its end the
        // agent's own activity for the screen lock's burst. A read-only line has no ticket.
        if let Some(t) = ticket {
            self.daemon.backend().lock_done(t);
        }
        out
    }

    fn shell_refused(&mut self, cmd: &str, reason: &str) {
        self.daemon
            .backend()
            .shell_refused(&self.serial, cmd, reason);
    }
}

/// How long `DaemonBridge::shell` waits for the shell guard: the approval window (90 s) plus a
/// margin for the ask itself. A line the guard allows outright returns in microseconds.
const SHELL_GATE_TIMEOUT_MS: u64 = agentroid_core::approval::DEFAULT_EXPIRY_MS + 10_000;
