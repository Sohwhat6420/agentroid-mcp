//! PQ-1: the scheduled action queue (HANDOVER.md, PQ table). A token-free layer over the 2.10 flow
//! engine: the agent plans a flow with a time and a screen condition, hands it to [`Queue::enqueue`]
//! and goes away. [`Queue::tick`] (called by the daemon's timer, no model involved) starts what is
//! due, runs it through [`engine::run`] (so every step passes core's gate, the R2+ refusal, the caps,
//! the cycle detector and the pop-up policy exactly as a live call does) and journals it.
//!
//! * **Zero model calls.** Nothing in this module talks to a model. The only outbound path is a
//!   [`WakeSender`], and it is used only for a wake-up.
//! * **Exactly one wake-up per stop.** A run that ends in anything but `done` (a failed step, a
//!   refusal, a cap, `stuck`, or a pause on an unexpected screen), a job whose condition never held,
//!   a missed window, or a runner that died mid-run, raises ONE wake-up: `<run>/wakeup-<n>.json`,
//!   created with `create_new`, so a second attempt for the same stop is a no-op. It carries the
//!   reason, a screenshot path (taken through the gate, saved beside the journal) and the journal
//!   tail. Delivery is retried by [`Queue::redeliver`] until the `.delivered` marker exists; the
//!   record itself is never rewritten or duplicated.
//! * **`resume(run_id)`.** [`Queue::resume`] reads the journal back (torn tail cut, corrupt middle
//!   refused), plans from the checkpoint with [`resume::plan`] (an intent without an outcome is
//!   re-verified, never re-sent) and runs on. A resumed run that stops again raises wake-up n+1.
//! * **State on disk.** `<jobs>/queue.json` (atomic rewrite) and `<jobs>/queue-log.ndjson` (append,
//!   fsynced: enqueue, probe, start, wake, resume), so a restart loses nothing.

use crate::runner::engine::{run, Driver, Hooks, Pause, Resume, RunOpts, RunReport, Stop};
use crate::runner::flow::{load_value, Select};
use crate::runner::journal::{
    read_checkpoint, run_dir, valid_run_id, write_atomic, FileCheckpoints, GroupCommit,
    JournalWriter, JOURNAL_FILE,
};
use crate::runner::popups::Rulebook;
use crate::runner::resume::{load_journal, plan, LoadError, Start};
use crate::runner::sink::{rfc3339_utc, JournalRecord};
use agentroid_core::gate::Call;
use agentroid_core::transport::CoreError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const QUEUE_FILE: &str = "queue.json";
pub const QUEUE_LOG: &str = "queue-log.ndjson";
/// Journal records carried in a wake-up.
pub const TAIL_RECORDS: usize = 8;

/// When a job may start, and what the screen must show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct When {
    /// Not before this time (Unix ms).
    pub not_before_unix_ms: u64,
    /// If it has not started by this time (window missed, or its condition never held), wake the
    /// model once instead of running late. `None` waits for ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
    /// A `$defs/select` object (`{"text": "Home"}`, `{"id": "..."}`): the job starts only while
    /// this is on screen. Checked through the gate (`ui_tree` with `require`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    /// Stopped and the model was woken; `resume` may continue it.
    Woken,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedJob {
    pub run_id: String,
    pub flow: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    pub when: When,
    pub state: JobState,
    /// Wake-ups raised so far (also the number of the next one, minus 1).
    #[serde(default)]
    pub wakes: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk: Option<u32>,
    pub enqueued_unix_ms: u64,
    /// How the last run ended (`Stop::code`), or the wake reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
}

/// The one message a stop sends to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WakeUp {
    pub v: u8,
    /// `<run_id>#<n>`; the receiver dedupes on it.
    pub wake_id: String,
    pub run_id: String,
    pub attempt: u32,
    pub ts: String,
    /// `Stop::code` (`failed`, `paused`, `stuck`, `refused`, `step_cap`, ...) or one of
    /// `condition_not_met`, `window_missed`, `runner_died`.
    pub reason: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// Path of the screenshot taken when the run stopped. A path only, never the image.
    pub screenshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_error: Option<String>,
    /// The last [`TAIL_RECORDS`] journal records.
    pub journal_tail: Vec<JournalRecord>,
    /// The overlay rectangle for an unknown-overlay pause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<String>,
    pub resume: Value,
    /// Always 0: the runner never calls a model.
    pub model_calls_before_wake: u32,
}

/// Where a wake-up goes (the daemon's notification path; a fake in tests).
pub trait WakeSender {
    fn deliver(&mut self, wake: &WakeUp) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct MemoryWakes {
    pub sent: Vec<WakeUp>,
    /// Fail the next N deliveries.
    pub fail: u32,
}

impl WakeSender for MemoryWakes {
    fn deliver(&mut self, wake: &WakeUp) -> Result<(), String> {
        if self.fail > 0 {
            self.fail -= 1;
            return Err("sender down".into());
        }
        self.sent.push(wake.clone());
        Ok(())
    }
}

/// What one job did in one [`Queue::tick`].
#[derive(Debug, Clone, PartialEq)]
pub enum Tick {
    /// Its time has not come.
    NotYet(String),
    /// Its time came but the screen condition does not hold yet (no wake-up).
    Waiting(String),
    /// It ran to the end.
    Done(String),
    /// It stopped and one wake-up was raised.
    Woke { run_id: String, reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueueError {
    Invalid(String),
    Duplicate(String),
    Unknown(String),
    /// `resume` on a job that is not stopped, or whose journal cannot be trusted.
    NotResumable(String),
    Io(String),
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::Invalid(s) => write!(f, "invalid job: {s}"),
            QueueError::Duplicate(s) => write!(f, "run_id {s} is already queued"),
            QueueError::Unknown(s) => write!(f, "no queued job {s}"),
            QueueError::NotResumable(s) => write!(f, "cannot resume: {s}"),
            QueueError::Io(s) => write!(f, "queue i/o: {s}"),
        }
    }
}

pub struct Queue {
    dir: PathBuf,
    jobs: Vec<QueuedJob>,
}

fn io<E: std::fmt::Display>(e: E) -> QueueError {
    QueueError::Io(e.to_string())
}

impl Queue {
    /// Open (or create) the queue in `jobs_dir`.
    pub fn open(jobs_dir: &Path) -> Result<Queue, QueueError> {
        std::fs::create_dir_all(jobs_dir).map_err(io)?;
        let path = jobs_dir.join(QUEUE_FILE);
        let jobs = match std::fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t)
                .map_err(|e| QueueError::Io(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(io(e)),
        };
        Ok(Queue {
            dir: jobs_dir.to_path_buf(),
            jobs,
        })
    }

    pub fn jobs(&self) -> &[QueuedJob] {
        &self.jobs
    }

    pub fn job(&self, run_id: &str) -> Option<&QueuedJob> {
        self.jobs.iter().find(|j| j.run_id == run_id)
    }

    fn save(&self) -> Result<(), QueueError> {
        let text = serde_json::to_string_pretty(&self.jobs).map_err(io)?;
        write_atomic(&self.dir.join(QUEUE_FILE), text.as_bytes()).map_err(QueueError::Io)
    }

    fn log(&self, now: u64, ev: &str, run_id: &str, extra: Value) {
        let mut o = Map::new();
        o.insert("ts".into(), json!(rfc3339_utc(now)));
        o.insert("ev".into(), json!(ev));
        o.insert("run_id".into(), json!(run_id));
        if let Value::Object(m) = extra {
            o.extend(m);
        }
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(QUEUE_LOG))
        {
            let _ = writeln!(f, "{}", Value::Object(o));
            let _ = f.sync_data();
        }
    }

    /// Queue a flow. Refused up front (nothing is stored) for a bad run id, a duplicate, a flow that
    /// fails the schema, any step above R1 (the engine refuses those anyway, see `engine::preflight`;
    /// refusing now means the agent hears about it while it is still there to fix it), or a bad
    /// condition selector.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue(
        &mut self,
        run_id: &str,
        flow: Value,
        device: Option<&str>,
        when: When,
        oem: Option<&str>,
        sdk: Option<u32>,
        now_unix_ms: u64,
    ) -> Result<(), QueueError> {
        if !valid_run_id(run_id) {
            return Err(QueueError::Invalid(format!("bad run_id {run_id:?}")));
        }
        if self.job(run_id).is_some() {
            return Err(QueueError::Duplicate(run_id.into()));
        }
        let f = load_value(flow.clone()).map_err(|e| QueueError::Invalid(e.to_string()))?;
        if let Some(s) = f
            .all_steps()
            .into_iter()
            .find(|s| s.effective_risk().rank() >= 2)
        {
            return Err(QueueError::Invalid(format!(
                "step {} is {}: R2 and above need the device owner's per-action approval and never run unattended",
                s.id,
                s.effective_risk().as_str()
            )));
        }
        if let Some(p) = &when.present {
            Select::from_value(p).map_err(QueueError::Invalid)?;
        }
        if when
            .deadline_unix_ms
            .is_some_and(|d| d < when.not_before_unix_ms)
        {
            return Err(QueueError::Invalid("deadline is before not_before".into()));
        }
        self.jobs.push(QueuedJob {
            run_id: run_id.into(),
            flow,
            device: device.map(str::to_string),
            when,
            state: JobState::Queued,
            wakes: 0,
            oem: oem.map(str::to_string),
            sdk,
            enqueued_unix_ms: now_unix_ms,
            last: None,
        });
        self.save()?;
        self.log(now_unix_ms, "enqueue", run_id, json!({}));
        Ok(())
    }

    /// Cancel a job that has not started. No wake-up (the agent asked for it).
    pub fn cancel(&mut self, run_id: &str, now_unix_ms: u64) -> Result<(), QueueError> {
        let j = self
            .jobs
            .iter_mut()
            .find(|j| j.run_id == run_id)
            .ok_or_else(|| QueueError::Unknown(run_id.into()))?;
        if j.state != JobState::Queued {
            return Err(QueueError::NotResumable(format!(
                "{run_id} is {:?}, only a queued job can be cancelled",
                j.state
            )));
        }
        j.state = JobState::Cancelled;
        self.save()?;
        self.log(now_unix_ms, "cancel", run_id, json!({}));
        Ok(())
    }

    /// Start what is due. Jobs run one at a time, earliest `not_before` first. Never calls a model.
    pub fn tick<D: Driver + ?Sized>(
        &mut self,
        driver: &mut D,
        rules: &Rulebook,
        now_unix_ms: u64,
        sender: &mut dyn WakeSender,
    ) -> Result<Vec<Tick>, QueueError> {
        let mut order: Vec<(u64, String)> = self
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Queued)
            .map(|j| (j.when.not_before_unix_ms, j.run_id.clone()))
            .collect();
        order.sort();
        let mut out = Vec::new();
        for (_, id) in order {
            let j = self.job(&id).cloned().expect("listed above");
            if now_unix_ms < j.when.not_before_unix_ms {
                out.push(Tick::NotYet(id));
                continue;
            }
            let late = j.when.deadline_unix_ms.is_some_and(|d| now_unix_ms > d);
            if let Some(sel) = &j.when.present {
                let met = probe(driver, j.device.as_deref(), sel);
                self.log(now_unix_ms, "probe", &id, json!({"present": met}));
                if !met {
                    if late {
                        self.wake(
                            driver,
                            &id,
                            "condition_not_met",
                            "the screen condition did not hold before the deadline; the job never started".into(),
                            None,
                            None,
                            now_unix_ms,
                            sender,
                        )?;
                        out.push(Tick::Woke {
                            run_id: id,
                            reason: "condition_not_met".into(),
                        });
                    } else {
                        out.push(Tick::Waiting(id));
                    }
                    continue;
                }
            } else if late {
                self.wake(
                    driver,
                    &id,
                    "window_missed",
                    "the job was due but the runner did not start it before its deadline".into(),
                    None,
                    None,
                    now_unix_ms,
                    sender,
                )?;
                out.push(Tick::Woke {
                    run_id: id,
                    reason: "window_missed".into(),
                });
                continue;
            }
            out.push(self.execute(driver, rules, &id, None, now_unix_ms, sender)?);
        }
        Ok(out)
    }

    /// Continue a stopped job from its journal and checkpoint. `human_confirmed` lets a
    /// `checkpoint` step that waits for a person pass.
    pub fn resume<D: Driver + ?Sized>(
        &mut self,
        driver: &mut D,
        rules: &Rulebook,
        run_id: &str,
        human_confirmed: bool,
        now_unix_ms: u64,
        sender: &mut dyn WakeSender,
    ) -> Result<Tick, QueueError> {
        let j = self
            .job(run_id)
            .ok_or_else(|| QueueError::Unknown(run_id.into()))?;
        if j.state != JobState::Woken {
            return Err(QueueError::NotResumable(format!(
                "{run_id} is {:?}; only a job that stopped and woke the model can be resumed",
                j.state
            )));
        }
        let dir = run_dir(&self.dir, run_id).map_err(QueueError::Io)?;
        let records = match load_journal(&dir.join(JOURNAL_FILE)) {
            Ok(l) => l.records,
            Err(LoadError::Missing) => Vec::new(),
            Err(e) => return Err(QueueError::NotResumable(e.to_string())),
        };
        let cp = read_checkpoint(&dir).map_err(QueueError::NotResumable)?;
        let start = match plan(run_id, &records, cp.as_ref(), human_confirmed) {
            Start::Fresh => None,
            Start::From(r) => Some(r),
        };
        self.log(
            now_unix_ms,
            "resume",
            run_id,
            json!({"from_checkpoint": start.is_some()}),
        );
        self.execute(driver, rules, run_id, start, now_unix_ms, sender)
    }

    /// After a crash: a job still `running` died with the runner. Wake the model once for each.
    pub fn recover<D: Driver + ?Sized>(
        &mut self,
        driver: &mut D,
        now_unix_ms: u64,
        sender: &mut dyn WakeSender,
    ) -> Result<Vec<Tick>, QueueError> {
        let ids: Vec<String> = self
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Running)
            .map(|j| j.run_id.clone())
            .collect();
        let mut out = Vec::new();
        for id in ids {
            self.wake(
                driver,
                &id,
                "runner_died",
                "the runner stopped while this job was running; resume re-checks the step it was on, it does not re-send it".into(),
                None,
                None,
                now_unix_ms,
                sender,
            )?;
            out.push(Tick::Woke {
                run_id: id,
                reason: "runner_died".into(),
            });
        }
        Ok(out)
    }

    /// Deliver any wake-up whose delivery failed earlier. Returns how many were delivered. The
    /// record on disk is untouched; only the `.delivered` marker is added.
    pub fn redeliver(&self, sender: &mut dyn WakeSender) -> usize {
        let mut n = 0;
        for j in &self.jobs {
            let Ok(dir) = run_dir(&self.dir, &j.run_id) else {
                continue;
            };
            for k in 1..=j.wakes {
                let rec = dir.join(format!("wakeup-{k}.json"));
                let mark = dir.join(format!("wakeup-{k}.delivered"));
                if mark.exists() {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&rec) else {
                    continue;
                };
                let Ok(w) = serde_json::from_str::<WakeUp>(&text) else {
                    continue;
                };
                if sender.deliver(&w).is_ok() && std::fs::write(&mark, b"1").is_ok() {
                    n += 1;
                }
            }
        }
        n
    }

    fn set_state(&mut self, run_id: &str, state: JobState, last: &str) -> Result<(), QueueError> {
        let j = self
            .jobs
            .iter_mut()
            .find(|j| j.run_id == run_id)
            .ok_or_else(|| QueueError::Unknown(run_id.into()))?;
        j.state = state;
        j.last = Some(last.into());
        self.save()
    }

    fn execute<D: Driver + ?Sized>(
        &mut self,
        driver: &mut D,
        rules: &Rulebook,
        run_id: &str,
        resume: Option<Resume>,
        now_unix_ms: u64,
        sender: &mut dyn WakeSender,
    ) -> Result<Tick, QueueError> {
        let j = self.job(run_id).cloned().expect("checked by callers");
        let flow = load_value(j.flow.clone()).map_err(|e| QueueError::Invalid(e.to_string()))?;
        // `running` is durable before anything is sent, so a crash is noticed by `recover`.
        self.set_state(run_id, JobState::Running, "running")?;
        self.log(
            now_unix_ms,
            "start",
            run_id,
            json!({"resumed": resume.is_some()}),
        );
        let mut opts = RunOpts::new(run_id);
        opts.device = j.device.clone();
        opts.start_unix_ms = now_unix_ms;
        opts.oem = j.oem.clone();
        opts.sdk = j.sdk;
        let report: RunReport = {
            let mut w = JournalWriter::open(&self.dir, run_id, GroupCommit::default())
                .map_err(QueueError::Io)?;
            let mut cps = FileCheckpoints::new(w.dir());
            run(
                driver,
                &flow,
                rules,
                opts,
                Hooks {
                    sink: &mut w,
                    checkpoints: &mut cps,
                    lease: None,
                    control: None,
                },
                resume,
            )
            // `w` drops here and commits the last journal records
        };
        match &report.stop {
            Stop::Done => {
                self.set_state(run_id, JobState::Done, "done")?;
                self.log(now_unix_ms, "done", run_id, json!({"steps": report.done}));
                Ok(Tick::Done(run_id.into()))
            }
            stop => {
                let (step, crop) = stop_details(stop);
                let reason = stop.code();
                self.wake(
                    driver,
                    run_id,
                    reason,
                    stop.describe(),
                    step,
                    crop,
                    now_unix_ms,
                    sender,
                )?;
                Ok(Tick::Woke {
                    run_id: run_id.into(),
                    reason: reason.into(),
                })
            }
        }
    }

    /// Raise wake-up n+1 for `run_id`: record, state, delivery. Idempotent for the same stop: if the
    /// record file exists nothing is written or sent again.
    #[allow(clippy::too_many_arguments)]
    fn wake<D: Driver + ?Sized>(
        &mut self,
        driver: &mut D,
        run_id: &str,
        reason: &str,
        detail: String,
        step_id: Option<String>,
        crop: Option<String>,
        now_unix_ms: u64,
        sender: &mut dyn WakeSender,
    ) -> Result<(), QueueError> {
        let (device, wakes) = {
            let j = self
                .job(run_id)
                .ok_or_else(|| QueueError::Unknown(run_id.into()))?;
            (j.device.clone(), j.wakes)
        };
        let n = wakes + 1;
        let dir = run_dir(&self.dir, run_id).map_err(QueueError::Io)?;
        std::fs::create_dir_all(&dir).map_err(io)?;
        let rec_path = dir.join(format!("wakeup-{n}.json"));
        if rec_path.exists() {
            // already raised for this stop (a retry after a crash between record and state)
            self.set_state(run_id, JobState::Woken, reason)?;
            return Ok(());
        }
        // the screenshot goes through the gate like any live call; a failure is reported, not hidden
        let (shot, shot_err) =
            match driver.call(device.as_deref(), &Call::Screenshot { max_width: 720 }) {
                Ok(o) => match o.image {
                    Some(img) => {
                        let p = dir.join(format!("wake-{n}.jpg"));
                        match std::fs::write(&p, img) {
                            Ok(()) => (Some(p.to_string_lossy().replace('\\', "/")), None),
                            Err(e) => (None, Some(format!("not saved: {e}"))),
                        }
                    }
                    None => (None, Some("the screenshot call returned no image".into())),
                },
                Err(e) => (None, Some(describe_err(&e))),
            };
        let tail = match load_journal(&dir.join(JOURNAL_FILE)) {
            Ok(l) => {
                let k = l.records.len().saturating_sub(TAIL_RECORDS);
                l.records[k..].to_vec()
            }
            Err(_) => Vec::new(),
        };
        let w = WakeUp {
            v: 1,
            wake_id: format!("{run_id}#{n}"),
            run_id: run_id.into(),
            attempt: n,
            ts: rfc3339_utc(now_unix_ms),
            reason: reason.into(),
            detail,
            step_id,
            screenshot: shot,
            screenshot_error: shot_err,
            journal_tail: tail,
            crop,
            resume: json!({"tool": "resume", "run_id": run_id}),
            model_calls_before_wake: 0,
        };
        let text = serde_json::to_string_pretty(&w).map_err(io)?;
        // create_new: the exactly-once guard
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&rec_path)
        {
            Ok(mut f) => {
                f.write_all(text.as_bytes()).map_err(io)?;
                f.sync_all().map_err(io)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(e) => return Err(io(e)),
        }
        let j = self
            .jobs
            .iter_mut()
            .find(|j| j.run_id == run_id)
            .expect("looked up above");
        j.wakes = n;
        j.state = JobState::Woken;
        j.last = Some(reason.into());
        self.save()?;
        self.log(
            now_unix_ms,
            "wake",
            run_id,
            json!({"attempt": n, "reason": reason}),
        );
        if sender.deliver(&w).is_ok() {
            let _ = std::fs::write(dir.join(format!("wakeup-{n}.delivered")), b"1");
        }
        Ok(())
    }
}

fn describe_err(e: &CoreError) -> String {
    format!("{e:?}")
}

fn stop_details(stop: &Stop) -> (Option<String>, Option<String>) {
    match stop {
        Stop::Paused(p) => {
            let p: &Pause = p;
            (Some(p.step_id.clone()), p.crop.map(|c| c.describe()))
        }
        Stop::Stuck { step_id, .. }
        | Stop::Refused { step_id, .. }
        | Stop::Failed { step_id, .. }
        | Stop::LeaseLost { step_id }
        | Stop::Stopped { step_id } => (Some(step_id.clone()), None),
        _ => (None, None),
    }
}

/// Is `sel` on screen right now? Asked through the gate; any error counts as "not yet".
fn probe<D: Driver + ?Sized>(driver: &mut D, device: Option<&str>, sel: &Value) -> bool {
    let Ok(s) = Select::from_value(sel) else {
        return false;
    };
    let Ok(core_sel) = s.to_core(&Map::new()) else {
        return false;
    };
    driver
        .call(
            device,
            &Call::UiTree {
                interactive_only: false,
                require: vec![core_sel],
                timeout_ms: Some(0),
            },
        )
        .is_ok()
}
