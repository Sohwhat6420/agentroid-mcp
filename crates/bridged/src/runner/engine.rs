//! The flow engine (task 2.10 slice a): runs a [`Flow`] against a core [`Transport`].
//!
//! Every phone action is a core [`Call`] served by [`Core::call`], so core's device-mode gate sees
//! it: on a `read_only` device every acting step is refused (the engine asks the gate first with
//! [`Core::admit`], so a refused step sends nothing, not even half of a tap-then-type).
//!
//! Per step attempt, in this order:
//! 1. **caps**: the wall cap (4 h on the core's injectable clock) and the step cap (3x planned;
//!    every attempt counts, retries and re-runs included);
//! 2. **observe**: settle with core's contract (200 ms polls, 2 equal signatures, 3 s cap), feed the
//!    screen hash to the **cycle detector** (the same hash seen N times with no new step completed
//!    gives `stuck`), classify pop-ups (dismiss, or pause with a crop rectangle), and relaunch the
//!    app if it left the screen (at most 3 relaunches per run);
//! 3. **execute** the step; `retry` (at most 3, R0/R1 only, schema-enforced) repeats from 1.
//!
//! **Phase 2 refuses R2+ with no exception**: a preflight refuses the whole flow before the phone
//! is touched if any step (including `for_each` bodies) is R2 or higher, and each step checks again.
//!
//! A pause keeps the lease; every other end releases it.
//!
//! **Slice b (journal and resume).** Every journal record the walk writes carries a cursor in
//! `post._at` (`path`, the counters, and `act` for the step's own calls) so `resume.rs` can find the
//! last durable position from the journal alone. An acting step is sent only after its `sent`
//! record is durable ([`RunSink::journal`] returned `Ok`). On resume a step with an intent but no
//! outcome is re-verified ([`ResumeMode::Reverify`]), never re-sent blindly. A screen that cannot be
//! read pauses (`unreadable_screen`) instead of acting blind. `job.pause` / `job.stop` arrive
//! through [`Hooks::control`] and take effect before the next step attempt.

use crate::runner::flow::{subst, Flow, Kind, Risk, Select, Step};
use crate::runner::popups::Dismissal;
use crate::runner::popups::{self, Context, Crop, PauseReason, Press, Rulebook, Screen, Verdict};
use crate::runner::sink::{
    rfc3339_utc, Checkpoint, CheckpointStore, EventKind, Interrupt, JobControl, JournalRecord,
    JournalStatus, LeaseHandle, ProgressEvent, RunSink,
};
use agentroid_adb_link::runner::Clock;
use agentroid_core::gate::{Call, Refusal, Target};
use agentroid_core::require::{Required, MAX_TIMEOUT_MS};
use agentroid_core::selector::{resolve, Chosen, Resolution, Selector, Status};
use agentroid_core::settle::{signature, Settle, SettleOpts};
use agentroid_core::transport::{Core, CoreError, Outcome, Transport};
use agentroid_core::tree::parse;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Step cap: at most this many times the planned steps.
pub const STEP_CAP_FACTOR: u32 = 3;
/// Wall cap: 4 h.
pub const WALL_CAP_MS: u64 = 4 * 3600 * 1000;
/// Same screen hash this many times with no new step completed gives `stuck` (assumed; must exceed
/// 1 + the retry cap of 3, so a step's own retries alone never trip it).
pub const CYCLE_THRESHOLD: u32 = 5;
/// App relaunches per run.
pub const RELAUNCH_CAP: u32 = 3;
/// `wait until` / `require` timeout when the step names none.
pub const DEFAULT_WAIT_MS: u64 = 5000;

/// What the engine needs from the phone side. [`Core`] implements it directly; the daemon implements
/// it over its shared backend so other clients are served between engine calls (task 2.10b).
pub trait Driver {
    fn now_ms(&self) -> u64;
    fn settle(&mut self, serial: Option<&str>, opts: SettleOpts) -> Result<Settle, CoreError>;
    /// The gate only; nothing is served.
    fn admit(&mut self, serial: Option<&str>, call: &Call) -> Result<(), CoreError>;
    fn call(&mut self, serial: Option<&str>, call: &Call) -> Result<Outcome, CoreError>;
}

impl<T: Transport, C: Clock> Driver for Core<T, C> {
    fn now_ms(&self) -> u64 {
        self.clock().now_ms()
    }
    fn settle(&mut self, serial: Option<&str>, opts: SettleOpts) -> Result<Settle, CoreError> {
        Core::settle(self, serial, opts)
    }
    fn admit(&mut self, serial: Option<&str>, call: &Call) -> Result<(), CoreError> {
        Core::admit(self, serial, call)
    }
    fn call(&mut self, serial: Option<&str>, call: &Call) -> Result<Outcome, CoreError> {
        Core::call(self, serial, call)
    }
}

/// Per-job options. [`RunOpts::new`] gives the planned defaults.
#[derive(Debug, Clone)]
pub struct RunOpts {
    pub run_id: String,
    /// Device to drive (adb or hardware serial); `None` is core's default pick.
    pub device: Option<String>,
    pub step_cap_factor: u32,
    pub wall_cap_ms: u64,
    pub cycle_threshold: u32,
    pub relaunch_cap: u32,
    /// The per-job pop-up switch: `false` makes every dismissable overlay pause instead.
    pub auto_dismiss: bool,
    pub settle: SettleOpts,
    pub oem: Option<String>,
    pub sdk: Option<u32>,
    /// Packages this job expects on screen besides `app.package` (not overlays).
    pub extra_packages: Vec<String>,
    /// Unix ms at the run's start, for the RFC 3339 `ts` fields (injectable for tests).
    pub start_unix_ms: u64,
    /// The lease epoch, written to every journal record.
    pub epoch: u64,
    /// Display id, written to every journal record and progress event.
    pub display: u32,
    /// Flight-recorder frames (2.11a): when the same screen has been seen this many sightings
    /// short of the cycle threshold or closer, a screenshot goes to the sink's `frame`. 0 (the
    /// default) takes none; the daemon sets `flight::FRAMES`, so the last frames before a `stuck`
    /// are the stuck screen itself. A routine run never repeats a screen that often, so it takes no
    /// screenshots.
    pub flight_frames: u32,
}

impl RunOpts {
    pub fn new(run_id: &str) -> RunOpts {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
            .unwrap_or(0);
        RunOpts {
            run_id: run_id.to_string(),
            device: None,
            step_cap_factor: STEP_CAP_FACTOR,
            wall_cap_ms: WALL_CAP_MS,
            cycle_threshold: CYCLE_THRESHOLD,
            relaunch_cap: RELAUNCH_CAP,
            auto_dismiss: true,
            settle: SettleOpts::default(),
            oem: None,
            sdk: None,
            extra_packages: Vec::new(),
            start_unix_ms: now,
            epoch: 0,
            display: 0,
            flight_frames: 0,
        }
    }
}

/// Screenshot width for flight-recorder frames.
pub const FLIGHT_FRAME_WIDTH: i32 = 720;

/// Counts screen hashes since the last new step completed.
#[derive(Debug, Clone, Default)]
pub struct CycleDetector {
    pub threshold: u32,
    seen: HashMap<u64, u32>,
}

impl CycleDetector {
    pub fn new(threshold: u32) -> Self {
        CycleDetector {
            threshold: threshold.max(1),
            seen: HashMap::new(),
        }
    }
    /// Record one sighting; returns how many times this hash has now been seen.
    pub fn see(&mut self, hash: u64) -> u32 {
        let c = self.seen.entry(hash).or_insert(0);
        *c += 1;
        *c
    }
    pub fn tripped(&self, count: u32) -> bool {
        count >= self.threshold
    }
    /// A step at a new position completed: forget the counts.
    pub fn progress(&mut self) {
        self.seen.clear();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    /// R2 or higher: refused in Phase 2 with no exception (no approval channel until 3.4/3.5).
    Phase2Risk { risk: Risk },
    /// Core's device-mode gate (`read_only`, helper mode, S24, allowlist).
    Gate(Refusal),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pause {
    pub step_id: String,
    pub reason: PauseReason,
    /// The overlay's rectangle in device px. Only this is reported, never an image.
    pub crop: Option<Crop>,
    pub checkpoint: Checkpoint,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    Done,
    /// Boxed: a pause carries its checkpoint, much larger than every other variant.
    Paused(Box<Pause>),
    Stuck {
        step_id: String,
        hash: u64,
        seen: u32,
    },
    StepCap {
        executed: u32,
        cap: u32,
    },
    WallCap {
        elapsed_ms: u64,
        cap_ms: u64,
    },
    RelaunchCap {
        relaunches: u32,
    },
    Refused {
        step_id: String,
        why: Refused,
    },
    Failed {
        step_id: String,
        why: String,
    },
    LeaseLost {
        step_id: String,
    },
    /// The lease holder stopped the job (`job.stop`) before `step_id`.
    Stopped {
        step_id: String,
    },
}

impl Stop {
    pub fn code(&self) -> &'static str {
        match self {
            Stop::Done => "done",
            Stop::Paused(_) => "paused",
            Stop::Stuck { .. } => "stuck",
            Stop::StepCap { .. } => "step_cap",
            Stop::WallCap { .. } => "wall_cap",
            Stop::RelaunchCap { .. } => "relaunch_cap",
            Stop::Refused { .. } => "refused",
            Stop::Failed { .. } => "failed",
            Stop::LeaseLost { .. } => "lease_lost",
            Stop::Stopped { .. } => "stopped",
        }
    }
    pub fn describe(&self) -> String {
        match self {
            Stop::Done => "done".into(),
            Stop::Paused(p) => format!(
                "paused at {}: {} ({}){}",
                p.step_id,
                p.reason.state(),
                p.reason.detail(),
                p.crop
                    .map(|c| format!(", crop {}", c.describe()))
                    .unwrap_or_default()
            ),
            Stop::Stuck {
                step_id,
                hash,
                seen,
            } => format!(
                "stuck before {step_id}: screen {hash:016x} seen {seen} times with no progress"
            ),
            Stop::StepCap { executed, cap } => {
                format!("step cap: {executed} steps executed, cap {cap} (3x planned)")
            }
            Stop::WallCap { elapsed_ms, cap_ms } => {
                format!("wall cap: {elapsed_ms} ms elapsed, cap {cap_ms} ms")
            }
            Stop::RelaunchCap { relaunches } => {
                format!("relaunch cap: the app left the screen again after {relaunches} relaunches")
            }
            Stop::Refused {
                step_id,
                why: Refused::Phase2Risk { risk },
            } => format!(
                "{step_id} refused: {} is refused in Phase 2, with no exception",
                risk.as_str()
            ),
            Stop::Refused {
                step_id,
                why: Refused::Gate(r),
            } => format!("{step_id} refused: {r}"),
            Stop::Failed { step_id, why } => format!("{step_id} failed: {why}"),
            Stop::LeaseLost { step_id } => {
                format!("lease lost before {step_id}; nothing more was sent")
            }
            Stop::Stopped { step_id } => {
                format!("stopped by the lease holder before {step_id}; nothing more was sent")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub run_id: String,
    pub stop: Stop,
    pub planned: u32,
    pub cap: u32,
    /// Step attempts executed (what the cap counts).
    pub executed: u32,
    /// Steps completed.
    pub done: u32,
    pub retries: u32,
    /// Steps that needed more than one attempt.
    pub retried_steps: u32,
    pub relaunches: u32,
    pub dismissals: Vec<Dismissal>,
    pub reads: BTreeMap<String, String>,
    pub elapsed_ms: u64,
    pub lease_released: bool,
}

/// Restart from a checkpoint (slice b loads it). `human_confirmed` passes a `checkpoint` step that
/// paused awaiting a person.
#[derive(Debug, Clone, PartialEq)]
pub struct Resume {
    pub from: Checkpoint,
    pub human_confirmed: bool,
    pub mode: ResumeMode,
}

/// What to do with the step at the resume path (task 2.10b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResumeMode {
    /// Run it again: a pause or checkpoint resume, where nothing of it was sent yet or a person
    /// has looked.
    #[default]
    Rerun,
    /// The journal says it is done: continue with the step after it.
    Skip,
    /// The journal has its intent and no outcome: check its post-condition (or the `verify` /
    /// `require` step right after it). If that holds the step is done; otherwise pause
    /// (`needs_attention`, `unverified_intent`). Never re-sent.
    Reverify,
}

/// What slice b plugs in.
pub struct Hooks<'h> {
    pub sink: &'h mut dyn RunSink,
    pub checkpoints: &'h mut dyn CheckpointStore,
    pub lease: Option<&'h mut dyn LeaseHandle>,
    /// `job.pause` / `job.stop`, polled before every step attempt.
    pub control: Option<&'h dyn JobControl>,
}

enum Fail {
    /// May be retried (selector missing, post-condition not met, transport hiccup).
    Retry(String),
    /// Will not get better by retrying.
    Fatal(String),
    /// Ends the run (refusal, pause, cap).
    Stop(Stop),
}

struct Obs {
    xml: String,
    hash: u64,
}

struct Run<'r, 'h, D: Driver + ?Sized> {
    core: &'r mut D,
    flow: &'r Flow,
    rules: &'r Rulebook,
    opts: RunOpts,
    hooks: Hooks<'h>,
    start_ms: u64,
    base_elapsed: u64,
    planned: u32,
    cap: u32,
    executed: u32,
    done: u32,
    retries: u32,
    retried_steps: u32,
    relaunches: u32,
    cycle: CycleDetector,
    completed: HashSet<Vec<usize>>,
    reads: BTreeMap<String, String>,
    dismissals: Vec<Dismissal>,
    pseq: u64,
    jseq: u64,
    human_confirmed: bool,
    /// The flow position being worked on (written into every journal record's `post._at`).
    cur: Vec<usize>,
    /// Consumed by the step at the resume path.
    mode: Option<ResumeMode>,
    /// Why the last observation had no readable screen.
    unread_why: String,
}

/// Run `flow` to its end, a pause, a cap or a refusal.
pub fn run<D: Driver + ?Sized>(
    core: &mut D,
    flow: &Flow,
    rules: &Rulebook,
    opts: RunOpts,
    hooks: Hooks<'_>,
    resume: Option<Resume>,
) -> RunReport {
    let planned = flow.planned();
    let cap = planned.saturating_mul(opts.step_cap_factor);
    let start_ms = core.now_ms();
    let cycle = CycleDetector::new(opts.cycle_threshold);
    let mut r = Run {
        core,
        flow,
        rules,
        opts,
        hooks,
        start_ms,
        base_elapsed: 0,
        planned,
        cap,
        executed: 0,
        done: 0,
        retries: 0,
        retried_steps: 0,
        relaunches: 0,
        cycle,
        completed: HashSet::new(),
        reads: BTreeMap::new(),
        dismissals: Vec::new(),
        pseq: 0,
        jseq: 0,
        human_confirmed: false,
        cur: Vec::new(),
        mode: None,
        unread_why: String::new(),
    };
    let mut resume_path: Option<Vec<usize>> = None;
    if let Some(res) = resume {
        r.executed = res.from.executed;
        r.relaunches = res.from.relaunches;
        r.base_elapsed = res.from.elapsed_ms;
        r.reads = res.from.reads.clone();
        r.done = res.from.done;
        r.retries = res.from.retries;
        r.retried_steps = res.from.retried_steps;
        r.human_confirmed = res.human_confirmed;
        r.mode = Some(res.mode);
        r.event(
            EventKind::Resume,
            Some(&res.from.step_id),
            format!("resumed from {} ({})", res.from.step_id, res.from.reason),
        );
        resume_path = Some(res.from.path);
    }
    let outcome = match r.preflight() {
        Err(s) => Err(s),
        Ok(()) => {
            let flow: &Flow = r.flow;
            let mut path = Vec::new();
            r.run_steps(&flow.steps, &flow.vars, &mut path, resume_path.as_deref())
        }
    };
    let stop = match outcome {
        Ok(()) => Stop::Done,
        Err(s) => s,
    };
    r.finish(stop)
}

impl<'r, D: Driver + ?Sized> Run<'r, '_, D> {
    // ------------------------------------------------------------ bookkeeping

    fn elapsed(&self) -> u64 {
        self.base_elapsed + self.core.now_ms().saturating_sub(self.start_ms)
    }

    fn event(&mut self, kind: EventKind, step_id: Option<&str>, msg: String) {
        self.event_with(kind, step_id, msg, None, None);
    }

    /// An event carrying a phone reading (care's heat and battery notes; 2.13). Readings are only
    /// ever what the phone reported: none are invented.
    fn event_with(
        &mut self,
        kind: EventKind,
        step_id: Option<&str>,
        msg: String,
        temp_c: Option<f64>,
        batt: Option<f64>,
    ) {
        let elapsed = self.elapsed();
        let eta_s = (kind == EventKind::Step && self.done > 0).then(|| {
            let per = elapsed as f64 / f64::from(self.done);
            per * f64::from(self.planned.saturating_sub(self.done)) / 1000.0
        });
        let ev = ProgressEvent {
            v: 1,
            ts: rfc3339_utc(self.opts.start_unix_ms + elapsed),
            run_id: self.opts.run_id.clone(),
            seq: self.pseq,
            kind,
            step_id: step_id.map(str::to_string),
            msg: Some(msg),
            temp_c,
            batt,
            eta_s,
            display: Some(self.opts.display),
        };
        self.pseq += 1;
        self.hooks.sink.progress(&ev);
    }

    /// A journal record whose failure to persist is reported but does not stop the run (pop-up and
    /// relaunch records, refusals, failures: nothing is sent *because of* them).
    fn journal(&mut self, step_id: &str, intent: String, post: Value, status: JournalStatus) {
        if let Err(e) = self.journal_at(step_id, intent, post, status, false) {
            self.event(
                EventKind::Error,
                Some(step_id),
                format!("journal write failed: {e}"),
            );
        }
    }

    /// The position cursor every walk record carries in `post._at` (see the module docs).
    fn cursor(&self, act: bool) -> Value {
        let mut c = Map::new();
        c.insert("path".into(), json!(self.cur));
        c.insert("executed".into(), json!(self.executed));
        c.insert("done".into(), json!(self.done));
        c.insert("retries".into(), json!(self.retries));
        c.insert("retried_steps".into(), json!(self.retried_steps));
        c.insert("relaunches".into(), json!(self.relaunches));
        c.insert("elapsed_ms".into(), json!(self.elapsed()));
        c.insert("act".into(), json!(act));
        Value::Object(c)
    }

    /// Write one record; `act` marks the step's own calls (planned / sent / done / failed).
    fn journal_at(
        &mut self,
        step_id: &str,
        intent: String,
        post: Value,
        status: JournalStatus,
        act: bool,
    ) -> Result<(), String> {
        let mut post = post;
        if !self.cur.is_empty() {
            let cursor = self.cursor(act);
            if let Some(o) = post.as_object_mut() {
                o.insert("_at".into(), cursor);
            }
        }
        let ts = rfc3339_utc(self.opts.start_unix_ms + self.elapsed());
        let rec = JournalRecord {
            v: 1,
            run_id: self.opts.run_id.clone(),
            seq: self.jseq,
            step_id: step_id.to_string(),
            intent,
            post,
            status,
            // ASSUMPTION: the v0 transport reads no phone clock, so ts_phone is the hub's time too.
            ts_phone: ts.clone(),
            ts_hub: ts,
            epoch: self.opts.epoch,
            display: self.opts.display,
        };
        self.jseq += 1;
        self.hooks.sink.journal(&rec)
    }

    fn lease_ok(&self) -> bool {
        self.hooks.lease.as_deref().is_none_or(LeaseHandle::held)
    }

    fn device(&self) -> Option<String> {
        self.opts.device.clone()
    }

    fn checkpoint(&self, step: &Step, path: &[usize], reason: &str) -> Checkpoint {
        Checkpoint {
            run_id: self.opts.run_id.clone(),
            step_id: step.id.clone(),
            path: path.to_vec(),
            reason: reason.to_string(),
            executed: self.executed,
            relaunches: self.relaunches,
            elapsed_ms: self.elapsed(),
            reads: self.reads.clone(),
            done: self.done,
            retries: self.retries,
            retried_steps: self.retried_steps,
        }
    }

    fn finish(mut self, stop: Stop) -> RunReport {
        match &stop {
            Stop::Done => self.event(
                EventKind::Done,
                None,
                format!(
                    "done: {} steps completed, {} executed",
                    self.done, self.executed
                ),
            ),
            // the job view reads `done` + `stopped_by_user` as its Stopped state (JOBVIEW-DESIGN §6)
            Stop::Stopped { step_id } => {
                let (id, msg) = (step_id.clone(), stop.describe());
                self.event(
                    EventKind::Done,
                    Some(&id),
                    format!("stopped_by_user: {msg}"),
                );
            }
            Stop::Paused(_) => {} // the pause event went out when it happened
            Stop::Stuck { step_id, .. } => {
                let (id, msg) = (step_id.clone(), stop.describe());
                self.event(EventKind::Stuck, Some(&id), msg);
            }
            other => {
                let msg = other.describe();
                self.event(EventKind::Error, None, format!("{}: {msg}", other.code()));
            }
        }
        let mut lease_released = false;
        if !matches!(stop, Stop::Paused(_)) {
            if let Some(l) = self.hooks.lease.as_mut() {
                l.release();
                lease_released = true;
            }
        }
        RunReport {
            run_id: self.opts.run_id.clone(),
            planned: self.planned,
            cap: self.cap,
            executed: self.executed,
            done: self.done,
            retries: self.retries,
            retried_steps: self.retried_steps,
            relaunches: self.relaunches,
            dismissals: std::mem::take(&mut self.dismissals),
            reads: std::mem::take(&mut self.reads),
            elapsed_ms: self.elapsed(),
            lease_released,
            stop,
        }
    }

    // ------------------------------------------------------------ the walk

    /// R2+ anywhere in the flow refuses it before anything touches the phone. Each refused step
    /// gets one `failed` journal record.
    fn preflight(&mut self) -> Result<(), Stop> {
        let flow = self.flow;
        let refused: Vec<(&Step, Risk)> = flow
            .all_steps()
            .into_iter()
            .map(|s| (s, s.effective_risk()))
            .filter(|(_, r)| r.rank() >= Risk::R2.rank())
            .collect();
        for (s, risk) in &refused {
            self.journal(
                &s.id,
                format!(
                    "refused before start: {} {} is {}; Phase 2 refuses R2+ with no exception",
                    s.kind.as_str(),
                    s.id,
                    risk.as_str()
                ),
                post_of(s),
                JournalStatus::Failed,
            );
        }
        match refused.first() {
            Some((s, risk)) => Err(Stop::Refused {
                step_id: s.id.clone(),
                why: Refused::Phase2Risk { risk: *risk },
            }),
            None => Ok(()),
        }
    }

    fn run_steps(
        &mut self,
        steps: &'r [Step],
        vars: &Map<String, Value>,
        path: &mut Vec<usize>,
        resume: Option<&[usize]>,
    ) -> Result<(), Stop> {
        let start = resume.and_then(|r| r.first().copied()).unwrap_or(0);
        for (i, step) in steps.iter().enumerate().skip(start) {
            path.push(i);
            let inner = if i == start {
                resume.map(|r| r.get(1..).unwrap_or(&[]))
            } else {
                None
            };
            let res = if step.kind == Kind::ForEach {
                if inner.is_some_and(<[usize]>::is_empty) {
                    self.mode = None; // a resume at the loop itself simply enters it
                }
                self.run_for_each(step, vars, path, inner)
            } else if inner.is_some() {
                self.run_resumed(steps, i, vars, path)
            } else {
                self.run_step(step, vars, path, false)
            };
            path.pop();
            res?;
        }
        Ok(())
    }

    fn run_for_each(
        &mut self,
        step: &'r Step,
        vars: &Map<String, Value>,
        path: &mut Vec<usize>,
        inner: Option<&[usize]>,
    ) -> Result<(), Stop> {
        let rows: &'r [Value] = step.table.as_deref().unwrap_or(&[]);
        let (start_row, body_resume) = match inner {
            Some([row, rest @ ..]) => (*row, Some(rest)),
            _ => {
                // entering the loop is one step (resuming into its body is not)
                self.cur = path.clone();
                self.check_caps()?;
                self.check_control(step, path)?;
                self.executed += 1;
                (0, None)
            }
        };
        for (ri, row) in rows.iter().enumerate().skip(start_row) {
            let mut v = vars.clone();
            if let Some(o) = row.as_object() {
                for (k, val) in o {
                    v.insert(k.clone(), val.clone());
                }
            }
            path.push(ri);
            let res = self.run_steps(
                &step.body,
                &v,
                path,
                if ri == start_row { body_resume } else { None },
            );
            path.pop();
            res?;
        }
        self.complete(step, path, format!("for_each over {} rows", rows.len()));
        Ok(())
    }

    /// Before every step attempt: the wall cap, then the step cap (exactly `cap` attempts may run).
    fn check_caps(&self) -> Result<(), Stop> {
        let elapsed = self.elapsed();
        if elapsed >= self.opts.wall_cap_ms {
            return Err(Stop::WallCap {
                elapsed_ms: elapsed,
                cap_ms: self.opts.wall_cap_ms,
            });
        }
        if self.executed >= self.cap {
            return Err(Stop::StepCap {
                executed: self.executed,
                cap: self.cap,
            });
        }
        Ok(())
    }

    /// `job.pause` / `job.stop` / a take-over, asked before every step attempt (after the caps);
    /// then the control's own wait (care's heat rest and battery floor, 2.13), whose pause and
    /// resume events are emitted as they happen.
    fn check_control(&mut self, step: &Step, path: &[usize]) -> Result<(), Stop> {
        let ctl = self.hooks.control;
        let asked = match ctl.and_then(JobControl::interrupt) {
            None => match ctl {
                Some(c) => {
                    let sid = step.id.clone();
                    c.wait_ready(&sid, &mut |n| {
                        self.event_with(n.kind, Some(&sid), n.msg, n.temp_c, n.batt)
                    })
                }
                None => None,
            },
            some => some,
        };
        match asked {
            None => Ok(()),
            Some(Interrupt::Stop) => Err(Stop::Stopped {
                step_id: step.id.clone(),
            }),
            Some(Interrupt::Pause) => Err(self.pause(step, path, PauseReason::ByUser, None)),
            Some(Interrupt::TakeOver) => Err(self.pause(step, path, PauseReason::TakenOver, None)),
        }
    }

    /// Between and after the calls of an acting step: a person took the phone (2.15). The step is
    /// journalled `interrupted` with `post.interrupted_by: "human"` (resume re-checks its
    /// post-condition and never blindly re-sends it), then the job pauses `taken_over`.
    fn taken_over_mid_step(
        &mut self,
        step: &Step,
        intent: &str,
        sent: usize,
        of: usize,
    ) -> Option<Stop> {
        if self.hooks.control.and_then(JobControl::interrupt) != Some(Interrupt::TakeOver) {
            return None;
        }
        let mut post = post_of(step);
        if let Some(o) = post.as_object_mut() {
            o.insert("interrupted_by".into(), json!("human"));
        }
        let why = format!("{intent}: a person took the phone after {sent} of {of} calls; the post-condition is checked on resume");
        if let Err(e) = self.journal_at(&step.id, why, post, JournalStatus::Interrupted, true) {
            self.event(
                EventKind::Error,
                Some(&step.id),
                format!("journal write failed: {e}"),
            );
        }
        let path = self.cur.clone();
        Some(self.pause(step, &path, PauseReason::TakenOver, None))
    }

    /// The step at the resume path, per [`ResumeMode`].
    fn run_resumed(
        &mut self,
        steps: &'r [Step],
        i: usize,
        vars: &Map<String, Value>,
        path: &[usize],
    ) -> Result<(), Stop> {
        let step = &steps[i];
        match self.mode.take().unwrap_or_default() {
            ResumeMode::Rerun => self.run_step(step, vars, path, true),
            ResumeMode::Skip => {
                self.cur = path.to_vec();
                self.complete(
                    step,
                    path,
                    format!(
                        "{} was done before the restart (journal); not sent again",
                        step.id
                    ),
                );
                Ok(())
            }
            ResumeMode::Reverify => self.reverify(steps, i, vars, path),
        }
    }

    /// A step whose intent is in the journal with no outcome: re-verify it, never re-send it.
    fn reverify(
        &mut self,
        steps: &'r [Step],
        i: usize,
        vars: &Map<String, Value>,
        path: &[usize],
    ) -> Result<(), Stop> {
        let step = &steps[i];
        self.cur = path.to_vec();
        self.check_caps()?;
        let post_kind = step
            .post
            .as_ref()
            .and_then(|p| p.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_string();
        let next_check = steps
            .get(i + 1)
            .filter(|n| matches!(n.kind, Kind::Verify | Kind::Require));
        let verdict: Result<String, String> = if post_kind != "none" {
            match self.check_post(step, vars) {
                Ok(()) => Ok(format!("its post-condition {post_kind} holds")),
                Err(Fail::Stop(s)) => return Err(s),
                Err(Fail::Retry(why) | Fail::Fatal(why)) => Err(why),
            }
        } else if let Some(n) = next_check {
            let obs = self.observe(step)?;
            let mut missing = Vec::new();
            for s in n.selectors.clone().unwrap_or_default() {
                match s.to_core(vars) {
                    Ok(sel) => match self.find(obs.as_ref(), &sel, 0) {
                        Ok(res) if res.status != Status::Ambiguous => {}
                        _ => missing.push(sel.to_string()),
                    },
                    Err(e) => missing.push(e),
                }
            }
            if missing.is_empty() {
                Ok(format!(
                    "every selector of {} {} is on screen",
                    n.kind.as_str(),
                    n.id
                ))
            } else {
                Err(format!(
                    "{} {} not satisfied, missing {}",
                    n.kind.as_str(),
                    n.id,
                    missing.join(", ")
                ))
            }
        } else {
            Err("it has no post-condition and no verify/require step after it".into())
        };
        match verdict {
            Ok(how) => {
                let intent = format!(
                    "resume: {} {} re-verified, {how}; not sent again",
                    step.kind.as_str(),
                    step.id
                );
                if let Err(e) =
                    self.journal_at(&step.id, intent, post_of(step), JournalStatus::Done, true)
                {
                    self.event(
                        EventKind::Error,
                        Some(&step.id),
                        format!("journal write failed: {e}"),
                    );
                }
                self.complete(step, path, format!("re-verified after the restart: {how}"));
                Ok(())
            }
            Err(why) => {
                let intent = format!("resume: {} {} had an intent and no outcome and could not be re-verified ({why}); nothing sent again", step.kind.as_str(), step.id);
                if let Err(e) = self.journal_at(
                    &step.id,
                    intent,
                    post_of(step),
                    JournalStatus::Interrupted,
                    true,
                ) {
                    self.event(
                        EventKind::Error,
                        Some(&step.id),
                        format!("journal write failed: {e}"),
                    );
                }
                Err(self.pause(
                    step,
                    path,
                    PauseReason::UnverifiedIntent {
                        step: step.id.clone(),
                        why,
                    },
                    None,
                ))
            }
        }
    }

    fn complete(&mut self, step: &Step, path: &[usize], msg: String) {
        self.done += 1;
        if self.completed.insert(path.to_vec()) {
            self.cycle.progress();
        }
        self.event(EventKind::Step, Some(&step.id), msg);
    }

    fn run_step(
        &mut self,
        step: &'r Step,
        vars: &Map<String, Value>,
        path: &[usize],
        resumed_here: bool,
    ) -> Result<(), Stop> {
        let attempts = step.attempts();
        self.cur = path.to_vec();
        for attempt in 0..attempts {
            self.check_caps()?;
            self.check_control(step, path)?;
            self.executed += 1;
            if attempt > 0 {
                self.retries += 1;
            }
            if attempt == 1 {
                self.retried_steps += 1;
            }
            let obs = self.prepare(step, path)?;
            match self.exec(step, vars, path, obs.as_ref(), resumed_here && attempt == 0) {
                Ok(msg) => {
                    self.complete(step, path, msg);
                    return Ok(());
                }
                Err(Fail::Retry(why)) if attempt + 1 < attempts => {
                    self.event(
                        EventKind::Progress,
                        Some(&step.id),
                        format!(
                            "attempt {} of {attempts} failed: {why}; retrying",
                            attempt + 1
                        ),
                    );
                }
                Err(Fail::Retry(why) | Fail::Fatal(why)) => {
                    return Err(Stop::Failed {
                        step_id: step.id.clone(),
                        why,
                    })
                }
                Err(Fail::Stop(s)) => return Err(s),
            }
        }
        Err(Stop::Failed {
            step_id: step.id.clone(),
            why: "no attempts left".into(),
        })
    }

    // ------------------------------------------------------------ observe: settle, cycle, pop-ups, relaunch

    fn observe(&mut self, step: &Step) -> Result<Option<Obs>, Stop> {
        let dev = self.device();
        match self.core.settle(dev.as_deref(), self.opts.settle) {
            Ok(Settle::Settled { signature, xml, .. }) => {
                self.hooks.sink.tree(&step.id, &xml); // the flight recorder's ring (2.11a)
                Ok(Some(Obs {
                    xml,
                    hash: signature,
                }))
            }
            Ok(Settle::Unsettled {
                last_xml: Some(xml),
                polls,
                ..
            }) => {
                self.hooks.sink.tree(&step.id, &xml);
                self.event(
                    EventKind::Progress,
                    Some(&step.id),
                    format!("unsettled after {polls} polls; using the last tree"),
                );
                let hash = signature(&xml).unwrap_or(0);
                Ok(Some(Obs { xml, hash }))
            }
            Ok(Settle::Unsettled {
                last_error, polls, ..
            }) => {
                self.unread_why = format!(
                    "no tree parsed in {polls} reads: {}",
                    last_error.unwrap_or_else(|| "no error given".into())
                );
                Ok(None)
            }
            Err(CoreError::Refused(r)) => Err(Stop::Refused {
                step_id: step.id.clone(),
                why: Refused::Gate(r),
            }),
            Err(e) => {
                self.unread_why = format!("the screen could not be read: {e}");
                Ok(None)
            }
        }
    }

    fn prepare(&mut self, step: &Step, path: &[usize]) -> Result<Option<Obs>, Stop> {
        loop {
            let Some(obs) = self.observe(step)? else {
                // Fail closed: with no readable tree no overlay can be ruled out, so nothing is sent.
                let why = std::mem::take(&mut self.unread_why);
                return Err(self.pause(step, path, PauseReason::UnreadableScreen { why }, None));
            };
            let seen = self.cycle.see(obs.hash);
            self.flight_frame(step, seen);
            if self.cycle.tripped(seen) {
                return Err(Stop::Stuck {
                    step_id: step.id.clone(),
                    hash: obs.hash,
                    seen,
                });
            }
            let verdict = {
                let ctx = Context {
                    app: &self.flow.app.package,
                    extra_packages: &self.opts.extra_packages,
                    oem: self.opts.oem.as_deref(),
                    sdk: self.opts.sdk,
                    auto_dismiss: self.opts.auto_dismiss,
                };
                popups::classify(
                    self.rules,
                    &Screen {
                        xml: &obs.xml,
                        activity: None,
                    },
                    &ctx,
                )
            };
            match verdict {
                Verdict::Clear => {}
                Verdict::Dismiss(d) => {
                    self.dismiss(step, path, d)?;
                    continue;
                }
                Verdict::Pause { reason, crop } => return Err(self.pause(step, path, reason, crop)),
            }
            if let Some(locked) = popups::app_gone(&obs.xml, &self.flow.app.package) {
                self.relaunch(step, locked)?;
                continue;
            }
            return Ok(Some(obs));
        }
    }

    /// A flight-recorder frame when this screen has now been seen within `flight_frames` sightings
    /// of the cycle threshold (2.11a). A read through the gate like any other; a failure only
    /// means no frame.
    fn flight_frame(&mut self, step: &Step, seen: u32) {
        let want = self.opts.flight_frames;
        if want == 0 || seen.saturating_add(want) <= self.cycle.threshold {
            return;
        }
        let dev = self.device();
        if let Ok(Outcome {
            image: Some(img), ..
        }) = self.core.call(
            dev.as_deref(),
            &Call::Screenshot {
                max_width: FLIGHT_FRAME_WIDTH,
            },
        ) {
            self.hooks.sink.frame(&step.id, &img);
        }
    }

    fn pause(
        &mut self,
        step: &Step,
        path: &[usize],
        reason: PauseReason,
        crop: Option<Crop>,
    ) -> Stop {
        let cp = self.checkpoint(step, path, reason.detail());
        if let Err(e) = self.hooks.checkpoints.save(&cp) {
            self.event(
                EventKind::Error,
                Some(&step.id),
                format!("checkpoint not saved: {e}"),
            );
        }
        let at = crop
            .map(|c| format!(", crop {}", c.describe()))
            .unwrap_or_default();
        let kind = if matches!(reason, PauseReason::TakenOver) {
            EventKind::TakenOver
        } else {
            EventKind::Pause
        };
        self.event(
            kind,
            Some(&step.id),
            format!("{}: {}{at}; lease kept", reason.state(), reason.detail()),
        );
        Stop::Paused(Box::new(Pause {
            step_id: step.id.clone(),
            reason,
            crop,
            checkpoint: cp,
        }))
    }

    fn dismiss(&mut self, step: &Step, path: &[usize], d: Dismissal) -> Result<(), Stop> {
        if !self.lease_ok() {
            return Err(Stop::LeaseLost {
                step_id: step.id.clone(),
            });
        }
        let call = match d.press {
            Press::Tap { x, y } => Call::Tap {
                target: Target::Point {
                    x: f64::from(x),
                    y: f64::from(y),
                    space: Some("device".into()),
                    image_width: None,
                },
            },
            Press::Back => Call::Key {
                name: "back".into(),
            },
        };
        let dev = self.device();
        match self.core.call(dev.as_deref(), &call) {
            Ok(_) => {
                let how = match d.press {
                    Press::Tap { x, y } => format!("tap {:?} at ({x},{y})", d.button),
                    Press::Back => "back".to_string(),
                };
                // exactly one journal entry per automatic dismissal
                self.journal(
                    &step.id,
                    format!("popup_dismiss: {} via {how}", d.rule),
                    json!({"kind": "popup_gone", "package": d.package}),
                    JournalStatus::Done,
                );
                self.event(
                    EventKind::Progress,
                    Some(&step.id),
                    format!("pop-up dismissed: {} via {how}", d.rule),
                );
                self.dismissals.push(d);
                Ok(())
            }
            Err(e) => {
                let crop = Some(d.crop);
                Err(self.pause(
                    step,
                    path,
                    PauseReason::DismissFailed {
                        package: d.package,
                        why: e.to_string(),
                    },
                    crop,
                ))
            }
        }
    }

    fn relaunch(&mut self, step: &Step, locked: bool) -> Result<(), Stop> {
        if self.relaunches >= self.opts.relaunch_cap {
            return Err(Stop::RelaunchCap {
                relaunches: self.relaunches,
            });
        }
        if !self.lease_ok() {
            return Err(Stop::LeaseLost {
                step_id: step.id.clone(),
            });
        }
        self.relaunches += 1;
        let pkg = self.flow.app.package.clone();
        let mut calls = Vec::new();
        if locked {
            calls.push(Call::Unlock);
        }
        calls.push(Call::OpenApp {
            package: pkg.clone(),
        });
        let dev = self.device();
        for c in &calls {
            match self.core.call(dev.as_deref(), c) {
                Ok(_) => {}
                Err(CoreError::Refused(r)) => {
                    return Err(Stop::Refused {
                        step_id: step.id.clone(),
                        why: Refused::Gate(r),
                    })
                }
                Err(e) => {
                    return Err(Stop::Failed {
                        step_id: step.id.clone(),
                        why: format!("relaunch of {pkg} failed: {e}"),
                    })
                }
            }
        }
        let n = self.relaunches;
        let cap = self.opts.relaunch_cap;
        self.journal(
            &step.id,
            format!(
                "relaunch {pkg} ({n} of {cap}){}",
                if locked { " after unlock" } else { "" }
            ),
            json!({"kind": "present", "package": pkg}),
            JournalStatus::Done,
        );
        self.event(
            EventKind::Progress,
            Some(&step.id),
            format!("{pkg} had left the screen; relaunched ({n} of {cap})"),
        );
        Ok(())
    }

    // ------------------------------------------------------------ execute

    fn refused(&mut self, step: &Step, e: CoreError) -> Fail {
        match e {
            CoreError::Refused(r) => {
                self.journal(
                    &step.id,
                    format!("refused by the gate: {r}"),
                    post_of(step),
                    JournalStatus::Failed,
                );
                Fail::Stop(Stop::Refused {
                    step_id: step.id.clone(),
                    why: Refused::Gate(r),
                })
            }
            CoreError::Resolve(_)
            | CoreError::Ambiguous(_)
            | CoreError::RequireTimeout { .. }
            | CoreError::Transport(_) => Fail::Retry(e.to_string()),
            CoreError::Invalid(_) | CoreError::InputRefused(_) | CoreError::Frame(_) => {
                Fail::Fatal(e.to_string())
            }
        }
    }

    /// Gate every call first, then send them in order, journalled planned / sent / done.
    fn act(&mut self, step: &Step, intent: String, calls: &[Call]) -> Result<(), Fail> {
        let dev = self.device();
        for c in calls {
            if let Err(e) = self.core.admit(dev.as_deref(), c) {
                return Err(self.refused(step, e));
            }
        }
        if !self.lease_ok() {
            return Err(Fail::Stop(Stop::LeaseLost {
                step_id: step.id.clone(),
            }));
        }
        // The intent must be durable before anything reaches the phone: the sink group-commits
        // `planned` + `sent` with one fsync and answers `Ok` for `sent` only after it.
        let durable = self
            .journal_at(
                &step.id,
                intent.clone(),
                post_of(step),
                JournalStatus::Planned,
                true,
            )
            .and_then(|()| {
                self.journal_at(
                    &step.id,
                    intent.clone(),
                    post_of(step),
                    JournalStatus::Sent,
                    true,
                )
            });
        if let Err(e) = durable {
            return Err(Fail::Stop(Stop::Failed {
                step_id: step.id.clone(),
                why: format!("the intent record is not durable ({e}); nothing was sent"),
            }));
        }
        for (i, c) in calls.iter().enumerate() {
            if let Err(e) = self.core.call(dev.as_deref(), c) {
                if let Err(je) = self.journal_at(
                    &step.id,
                    format!("{intent}: {e}"),
                    post_of(step),
                    JournalStatus::Failed,
                    true,
                ) {
                    self.event(
                        EventKind::Error,
                        Some(&step.id),
                        format!("journal write failed: {je}"),
                    );
                }
                return Err(self.refused(step, e));
            }
            // a person may have taken the phone while this call ran: never send the next one, and
            // never journal the step done
            if let Some(stop) = self.taken_over_mid_step(step, &intent, i + 1, calls.len()) {
                return Err(Fail::Stop(stop));
            }
        }
        if let Err(e) = self.journal_at(&step.id, intent, post_of(step), JournalStatus::Done, true)
        {
            self.event(
                EventKind::Error,
                Some(&step.id),
                format!("journal write failed: {e}"),
            );
        }
        Ok(())
    }

    /// An acting kind the v0 engine has no executor for: the gate still answers first.
    fn unsupported(&mut self, step: &Step, representative: Call) -> Result<String, Fail> {
        let dev = self.device();
        if let Err(e) = self.core.admit(dev.as_deref(), &representative) {
            return Err(self.refused(step, e));
        }
        Err(Fail::Fatal(format!(
            "step kind {} is not supported by the v0 engine; nothing was sent",
            step.kind.as_str()
        )))
    }

    fn selector(
        step: &Step,
        s: Option<&Select>,
        vars: &Map<String, Value>,
    ) -> Result<Selector, Fail> {
        let s = s.ok_or_else(|| Fail::Fatal(format!("{} needs a selector", step.kind.as_str())))?;
        s.to_core(vars).map_err(Fail::Fatal)
    }

    /// Resolve `sel` on the observed tree, or poll for it through core when `timeout_ms` > 0.
    fn find(
        &mut self,
        obs: Option<&Obs>,
        sel: &Selector,
        timeout_ms: u64,
    ) -> Result<Resolution, String> {
        if timeout_ms > 0 {
            let dev = self.device();
            let call = Call::UiTree {
                interactive_only: false,
                require: vec![sel.clone()],
                timeout_ms: Some(timeout_ms.min(MAX_TIMEOUT_MS)),
            };
            return match self.core.call(dev.as_deref(), &call) {
                Ok(o) => match o.required {
                    Some(Required::Met {
                        mut resolutions, ..
                    }) if !resolutions.is_empty() => Ok(resolutions.remove(0)),
                    _ => Err(format!("{sel} not found")),
                },
                Err(e) => Err(e.to_string()),
            };
        }
        let obs = obs.ok_or("no readable screen")?;
        let tree = parse(&obs.xml, false).map_err(|e| e.to_string())?;
        resolve(&tree, sel).map_err(|e| e.to_string())
    }

    fn label(res: &Resolution) -> Option<String> {
        match &res.chosen {
            Some(Chosen::Element(a)) => Some(a.label.clone()),
            Some(Chosen::Point { fx, fy }) => Some(format!("({fx:.3},{fy:.3})")),
            None => None,
        }
    }

    fn check_post(&mut self, step: &Step, vars: &Map<String, Value>) -> Result<(), Fail> {
        let Some(post) = &step.post else {
            return Ok(());
        };
        let kind = post.get("kind").and_then(Value::as_str).unwrap_or("none");
        if kind == "none" {
            return Ok(());
        }
        let obs = self
            .observe(step)
            .map_err(Fail::Stop)?
            .ok_or_else(|| Fail::Retry("post-condition: no readable screen".into()))?;
        let met = match kind {
            "present" | "absent" => {
                let sel = post
                    .get("select")
                    .map(Select::from_value)
                    .transpose()
                    .map_err(Fail::Fatal)?
                    .ok_or_else(|| Fail::Fatal("post needs select".into()))?;
                let sel = sel.to_core(vars).map_err(Fail::Fatal)?;
                let found = self.find(Some(&obs), &sel, 0).is_ok();
                found == (kind == "present")
            }
            "text_equals" => {
                let want = subst(post.get("text").and_then(Value::as_str).unwrap_or(""), vars)
                    .map_err(Fail::Fatal)?;
                parse(&obs.xml, false)
                    .map(|t| t.elements.iter().any(|e| e.text == want.as_str()))
                    .unwrap_or(false)
            }
            other => return Err(Fail::Fatal(format!("post kind {other:?} is not supported"))),
        };
        if met {
            Ok(())
        } else {
            Err(Fail::Retry(format!("post-condition {kind} not met")))
        }
    }

    fn exec(
        &mut self,
        step: &Step,
        vars: &Map<String, Value>,
        path: &[usize],
        obs: Option<&Obs>,
        resumed_here: bool,
    ) -> Result<String, Fail> {
        let risk = step.effective_risk();
        if risk.rank() >= Risk::R2.rank() {
            self.journal(
                &step.id,
                format!("refused: {} is refused in Phase 2", risk.as_str()),
                post_of(step),
                JournalStatus::Failed,
            );
            return Err(Fail::Stop(Stop::Refused {
                step_id: step.id.clone(),
                why: Refused::Phase2Risk { risk },
            }));
        }
        match step.kind {
            Kind::Tap => {
                let sel = Self::selector(step, step.select.as_ref(), vars)?;
                self.act(
                    step,
                    format!("tap {sel}"),
                    &[Call::Tap {
                        target: Target::Select(sel.clone()),
                    }],
                )?;
                self.check_post(step, vars)?;
                Ok(format!("tapped {sel}"))
            }
            Kind::Type => {
                let raw = step
                    .value
                    .as_ref()
                    .and_then(Value::as_str)
                    .ok_or_else(|| Fail::Fatal("type needs a string value".into()))?;
                let text = subst(raw, vars).map_err(Fail::Fatal)?;
                let n = text.chars().count();
                let mut calls = Vec::new();
                let mut into = String::new();
                if let Some(s) = &step.select {
                    let sel = s.to_core(vars).map_err(Fail::Fatal)?;
                    into = format!(" into {sel}");
                    calls.push(Call::Tap {
                        target: Target::Select(sel),
                    });
                }
                calls.push(Call::TypeText {
                    text,
                    index: None,
                    submit: false,
                });
                // the text itself never goes into the journal or the events (it may be a secret)
                self.act(step, format!("type {n} characters{into}"), &calls)?;
                self.check_post(step, vars)?;
                Ok(format!("typed {n} characters{into}"))
            }
            Kind::Scroll => {
                let dir = step.direction.clone().unwrap_or_else(|| "down".into());
                self.act(
                    step,
                    format!("scroll {dir}"),
                    &[Call::Scroll {
                        direction: dir.clone(),
                    }],
                )?;
                self.check_post(step, vars)?;
                Ok(format!("scrolled {dir}"))
            }
            Kind::Back => {
                self.act(
                    step,
                    "key back".into(),
                    &[Call::Key {
                        name: "back".into(),
                    }],
                )?;
                self.check_post(step, vars)?;
                Ok("pressed back".into())
            }
            Kind::SetValue => self.unsupported(
                step,
                Call::Swipe {
                    x1: 0.0,
                    y1: 0.0,
                    x2: 0.0,
                    y2: 0.0,
                    ms: 300,
                    space: Some("device".into()),
                    image_width: None,
                },
            ),
            Kind::Route | Kind::Push | Kind::SettingsSet => self.unsupported(
                step,
                Call::StartActivity {
                    component: format!("{}/.Unsupported", self.flow.app.package),
                    action: None,
                    data: None,
                },
            ),
            Kind::Wait => {
                if let Some(u) = step.until_select() {
                    let sel = u.to_core(vars).map_err(Fail::Fatal)?;
                    let t = step.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).max(1);
                    return match self.find(obs, &sel, t) {
                        Ok(res) => Ok(format!("waited for {sel}: {}", res.describe())),
                        Err(e) => Err(Fail::Retry(format!("wait until {sel}: {e}"))),
                    };
                }
                let ms = step
                    .value
                    .as_ref()
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(MAX_TIMEOUT_MS);
                let dev = self.device();
                let mut left = ms;
                while left > 0 {
                    let chunk = left.min(10_000);
                    self.core
                        .call(
                            dev.as_deref(),
                            &Call::Wait {
                                seconds: chunk as f64 / 1000.0,
                            },
                        )
                        .map_err(|e| Fail::Fatal(e.to_string()))?;
                    left -= chunk;
                }
                Ok(format!("waited {ms} ms"))
            }
            Kind::Assert => {
                let sel = Self::selector(step, step.select.as_ref(), vars)?;
                let res = self
                    .find(obs, &sel, step.timeout_ms.unwrap_or(0))
                    .map_err(|e| Fail::Retry(format!("assert {sel}: {e}")))?;
                if res.status == Status::Ambiguous {
                    return Err(Fail::Retry(format!("assert {sel}: {}", res.describe())));
                }
                if let Some(want) = step.value.as_ref().and_then(Value::as_str) {
                    let want = subst(want, vars).map_err(Fail::Fatal)?;
                    let got = Self::label(&res).unwrap_or_default();
                    if got != want {
                        return Err(Fail::Retry(format!(
                            "assert {sel}: label is {got:?}, expected {want:?}"
                        )));
                    }
                }
                Ok(format!("assert held: {}", res.describe()))
            }
            Kind::Read => {
                let sel = Self::selector(step, step.select.as_ref(), vars)?;
                let res = self
                    .find(obs, &sel, step.timeout_ms.unwrap_or(0))
                    .map_err(|e| Fail::Retry(format!("read {sel}: {e}")))?;
                let Some(v) = Self::label(&res) else {
                    return Err(Fail::Retry(format!("read {sel}: {}", res.describe())));
                };
                self.reads.insert(step.id.clone(), v.clone());
                Ok(format!("read {v:?} via {}", res.describe()))
            }
            Kind::Verify => {
                let sels = step.selectors.clone().unwrap_or_default();
                let mut parts = Vec::new();
                for s in &sels {
                    let sel = s.to_core(vars).map_err(Fail::Fatal)?;
                    match self.find(obs, &sel, 0) {
                        Ok(res) if res.status != Status::Ambiguous => parts.push(format!(
                            "{sel}: yes {:?}",
                            Self::label(&res).unwrap_or_default()
                        )),
                        _ => parts.push(format!("{sel}: no")),
                    }
                }
                let v = parts.join("; ");
                self.reads.insert(step.id.clone(), v.clone());
                Ok(format!("verify: {v}"))
            }
            Kind::Require => {
                let sels: Vec<Selector> = step
                    .selectors
                    .clone()
                    .unwrap_or_default()
                    .iter()
                    .map(|s| s.to_core(vars))
                    .collect::<Result<_, _>>()
                    .map_err(Fail::Fatal)?;
                let t = step
                    .timeout_ms
                    .unwrap_or(DEFAULT_WAIT_MS)
                    .clamp(1, MAX_TIMEOUT_MS);
                let dev = self.device();
                match self.core.call(
                    dev.as_deref(),
                    &Call::UiTree {
                        interactive_only: false,
                        require: sels,
                        timeout_ms: Some(t),
                    },
                ) {
                    Ok(_) => Ok("require met".into()),
                    Err(e) => Err(self.refused(step, e)),
                }
            }
            Kind::Checkpoint => {
                let cp = self.checkpoint(step, path, "checkpoint");
                if let Err(e) = self.hooks.checkpoints.save(&cp) {
                    return Err(Fail::Fatal(format!("checkpoint not saved: {e}")));
                }
                if resumed_here && self.human_confirmed {
                    return Ok("checkpoint passed: confirmed by a person on resume".into());
                }
                let mut seen_auto = None;
                if let Some(a) = step.auto_success_select() {
                    let sel = a.to_core(vars).map_err(Fail::Fatal)?;
                    match self.find(obs, &sel, step.timeout_ms.unwrap_or(0)) {
                        Ok(res) if res.status != Status::Ambiguous => {
                            return Ok(format!(
                                "checkpoint passed automatically: {}",
                                res.describe()
                            ))
                        }
                        _ => seen_auto = Some(sel),
                    }
                }
                if step.awaiting_human != Some(false) {
                    return Err(Fail::Stop(self.pause(
                        step,
                        path,
                        PauseReason::AwaitingHuman {
                            step: step.id.clone(),
                        },
                        None,
                    )));
                }
                match seen_auto {
                    Some(sel) => Err(Fail::Retry(format!("auto_success {sel} not seen"))),
                    None => Ok("checkpoint saved".into()),
                }
            }
            Kind::ForEach => Err(Fail::Fatal("for_each is run by the walker".into())),
        }
    }
}

fn post_of(step: &Step) -> Value {
    step.post.clone().unwrap_or_else(|| json!({"kind": "none"}))
}
