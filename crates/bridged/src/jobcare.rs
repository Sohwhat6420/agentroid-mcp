//! Care during jobs: task 2.13 (`crates/care`) wired into the daemon.
//!
//! * **Heat guard and battery floor.** Before every step attempt the engine asks the job's
//!   control to [`JobCare::wait`]. At care's own cadence (every 10 s working, every 60 s resting)
//!   it reads the phone through adb-link's `GuardedRunner` and feeds care's [`HeatGuard`]: pause at
//!   42 °C battery (the screen sleeps), resume only **strictly below** 37 °C. While it rests no step
//!   runs; `pause` / `resume` events start with `heat:` and carry the reading (`temp_c`, `batt`),
//!   and the rest counts as the receipt's cooling time. Still hot after the 60 min rest cap: care
//!   pushes and the job stops. The battery floor (care's [`check_floor`], 12 % when not `powered`)
//!   holds the job the same way.
//! * **Job-scoped settings leases** go through care's [`LeaseManager`] over `leases.json` (the one
//!   store phone-guard shares): the agent bundle at job start (or 2.15's agent profile through the
//!   [`crate::takeover`] mode machine), restored by compare-and-swap at the end. Every care event is
//!   written into the job's journal (`care-*` step ids), and `devices.list` / the job view show
//!   `changes_in_force`.
//! * **read_only and helper devices:** care refuses every lease scope (its own gate, first, with
//!   nothing sent; the refusal is journalled) and no heat or floor read is made: care's readings
//!   are adb shell commands, which only `full` devices get.
//!
//! Nothing here starts adb itself; the daemon hands in the guarded runner. care's
//! `guard-logic-tests` feature is never enabled.

use crate::jobs::DynRunner;
use crate::runner::sink::{ControlNote, EventKind, Interrupt, JournalRecord, JournalStatus};
use crate::takeover::Takeover;
use agentroid_adb_link::guard::GuardedRunner;
use agentroid_core::gate::DeviceMode;
use care::clock::CareClock;
use care::floor::{check_floor, FloorVerdict};
use care::heat::{GuardAction, HeatGuard, JobKind, Notifier};
use care::leases::LeaseStore;
use care::phone::AdbPhone;
use care::pid::PidProbe;
use care::settings::{ChangeInForce, LeaseManager, Owner};
use care::sink::{CareEvent, CareSink};
use care::thresholds::THRESHOLDS;
use care::CareError;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// How long one slice of a rest wait is, so `job.pause` / `job.stop` are noticed quickly.
pub const WAIT_SLICE_MS: u64 = 250;

/// The push channel for "too hot: job stopped" when no other is wired: a line in `bridged.log`.
pub struct LogNotifier(pub PathBuf);

impl Notifier for LogNotifier {
    fn push(&self, serial: &str, title: &str, body: &str) -> Result<(), String> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
            .map_err(|e| e.to_string())?;
        writeln!(f, "push {serial}: {title}: {body}").map_err(|e| e.to_string())
    }
}

/// How the daemon reaches care (part of [`crate::jobs::JobsConfig`]).
pub struct CareConfig {
    /// adb, always behind adb-link's command guard (the protected phone stays read-only there too).
    pub runner: Arc<GuardedRunner<DynRunner>>,
    /// care's `leases.json` (phone-guard's store).
    pub leases: PathBuf,
    pub clock: Arc<dyn CareClock>,
    pub pids: Arc<dyn PidProbe>,
    pub notifier: Arc<dyn Notifier + Send + Sync>,
    /// Waits `ms` (real: a thread sleep; tests: advance a fake clock).
    pub sleep: Arc<dyn Fn(u64) + Send + Sync>,
    /// `profiles/` with `agent.json` and `human.json` (2.15): with it, a job leases the agent
    /// profile through the mode machine and a human touch hands the phone back. Without it, care's
    /// own agent bundle is leased and restored by compare-and-swap.
    pub profiles: Option<PathBuf>,
    /// Battery-floor re-check while held below the floor.
    pub floor_poll_ms: u64,
}

impl CareConfig {
    pub fn new(
        runner: Arc<GuardedRunner<DynRunner>>,
        leases: impl Into<PathBuf>,
        clock: Arc<dyn CareClock>,
        notifier: Arc<dyn Notifier + Send + Sync>,
    ) -> CareConfig {
        CareConfig {
            runner,
            leases: leases.into(),
            clock,
            pids: Arc::new(care::pid::OsPids),
            notifier,
            sleep: Arc::new(|ms| std::thread::sleep(std::time::Duration::from_millis(ms))),
            profiles: None,
            floor_poll_ms: 60_000,
        }
    }
}

/// care's journal entries, buffered until the job's journal writer takes them.
#[derive(Clone, Default)]
pub struct CareBuffer(Arc<Mutex<Vec<CareEvent>>>);

impl CareSink for CareBuffer {
    fn record(&mut self, ev: &CareEvent) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ev.clone());
        Ok(())
    }
}

impl CareBuffer {
    pub fn drain(&self) -> Vec<CareEvent> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// One care event as a `journal.json` record (seq is assigned by the writer).
pub fn care_record(ev: &CareEvent, run_id: &str, epoch: u64) -> JournalRecord {
    let status = if ev.status() == "failed" {
        JournalStatus::Failed
    } else {
        JournalStatus::Done
    };
    JournalRecord {
        v: 1,
        run_id: run_id.to_string(),
        seq: 0,
        step_id: ev.step_id(),
        intent: ev.intent(),
        post: ev.post(),
        status,
        ts_phone: ev.ts.clone(),
        ts_hub: ev.ts.clone(),
        epoch,
        display: 0,
    }
}

/// What every care call on one phone shares (the mode machine's leases use it too).
pub struct CareCore {
    pub runner: Arc<GuardedRunner<DynRunner>>,
    /// adb serial (the `-s` token).
    pub serial: String,
    /// `ro.serialno` (the lease key).
    pub ro: String,
    pub mode: DeviceMode,
    pub session: String,
    mgr: Mutex<LeaseManager<CareBuffer>>,
    pub buffer: CareBuffer,
}

impl CareCore {
    pub fn mgr(&self) -> MutexGuard<'_, LeaseManager<CareBuffer>> {
        self.mgr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub fn phone(&self) -> AdbPhone<'_, DynRunner> {
        AdbPhone::new(&self.runner, self.serial.clone())
    }
    pub fn owner(&self, job_id: &str) -> Owner {
        Owner::here(job_id, self.session.clone())
    }
}

/// The settings leases in force on one phone (`ro.serialno`), straight from `leases.json`.
pub fn changes_in_force(
    leases: &std::path::Path,
    ro: &str,
) -> Result<Vec<ChangeInForce>, CareError> {
    Ok(LeaseStore::new(leases)
        .entries()?
        .iter()
        .filter(|e| e.is_setting() && e.ident() == ro)
        .map(ChangeInForce::of)
        .collect())
}

/// `changes_in_force` as `schemas/job-view.json` items.
pub fn change_json(c: &ChangeInForce) -> Value {
    let mut v = json!({
        "key": c.key, "ns": c.ns, "before": c.before, "applied": c.applied,
        "owner": {"pid": c.owner.pid, "job_id": c.owner.job_id, "session": c.owner.session},
        "scope": c.scope.as_str(), "ts": c.ts,
    });
    if let Some(e) = &c.expires {
        v["expires"] = json!(e);
    }
    v
}

/// One job's care: the heat guard, the floor, the leases and (with profiles) the take-over machine.
pub struct JobCare {
    pub core: Arc<CareCore>,
    clock: Arc<dyn CareClock>,
    notifier: Arc<dyn Notifier + Send + Sync>,
    sleep: Arc<dyn Fn(u64) + Send + Sync>,
    floor_poll_ms: u64,
    heat: Mutex<HeatGuard>,
    /// Time spent resting for heat, all resumes together (the receipt's cooling time).
    pub cooling_ms: AtomicU64,
    /// The last reading, `(temp_c, batt)`.
    last: Mutex<Option<(f64, f64)>>,
    reconnected: AtomicBool,
    pub takeover: Option<Takeover>,
}

fn dc(v: i32) -> f64 {
    f64::from(v) / 10.0
}

impl JobCare {
    pub fn new(
        cfg: &CareConfig,
        serial: &str,
        ro: &str,
        mode: DeviceMode,
        session: &str,
    ) -> Result<JobCare, String> {
        let buffer = CareBuffer::default();
        let mgr = LeaseManager::new(
            LeaseStore::new(&cfg.leases),
            buffer.clone(),
            cfg.clock.clone(),
            cfg.pids.clone(),
        );
        let core = Arc::new(CareCore {
            runner: cfg.runner.clone(),
            serial: serial.to_string(),
            ro: ro.to_string(),
            mode,
            session: session.to_string(),
            mgr: Mutex::new(mgr),
            buffer,
        });
        let takeover = match (&cfg.profiles, mode) {
            (Some(dir), DeviceMode::Full) => {
                Some(Takeover::new(core.clone(), cfg.clock.clone(), dir)?)
            }
            _ => None,
        };
        Ok(JobCare {
            core,
            clock: cfg.clock.clone(),
            notifier: cfg.notifier.clone(),
            sleep: cfg.sleep.clone(),
            floor_poll_ms: cfg.floor_poll_ms.max(WAIT_SLICE_MS),
            heat: Mutex::new(HeatGuard::new(serial, JobKind::Normal)),
            cooling_ms: AtomicU64::new(0),
            last: Mutex::new(None),
            reconnected: AtomicBool::new(false),
            takeover,
        })
    }

    pub fn last_reading(&self) -> Option<(f64, f64)> {
        *self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The job (re)starts on the phone: lease the working settings. A refusal (read_only, helper)
    /// is journalled by care and the job goes on without leases; nothing was sent.
    pub fn begin(&self, job_id: &str) -> Vec<String> {
        let mut notes = vec![];
        let phone = self.core.phone();
        if self.core.mode == DeviceMode::Full && !self.reconnected.swap(true, Ordering::SeqCst) {
            // first contact from this daemon: leases a dead owner left behind are restored first
            if let Err(e) = self.core.mgr().on_reconnect(&phone, self.core.mode) {
                notes.push(format!("care: stale leases not checked: {e}"));
            }
        }
        if let Some(t) = &self.takeover {
            notes.extend(t.begin(job_id));
            return notes;
        }
        let owner = self.core.owner(job_id);
        let r = self
            .core
            .mgr()
            .apply_agent_bundle(&phone, self.core.mode, &owner, &[]);
        match r {
            Ok(applied) => notes.push(format!(
                "care: {} job-scoped settings leased",
                applied.len()
            )),
            Err(e) => notes.push(format!("care: no settings leased ({e})")),
        }
        notes
    }

    /// The job ended: give the settings back (compare-and-swap; the human profile with 2.15).
    pub fn end(&self, job_id: &str) -> Vec<String> {
        if let Some(t) = &self.takeover {
            return t.end();
        }
        if self.core.mode != DeviceMode::Full {
            return vec![];
        }
        let phone = self.core.phone();
        match self.core.mgr().restore_job(&phone, self.core.mode, job_id) {
            Ok(r) => vec![format!("care: {} job-scoped settings restored", r.len())],
            Err(e) => vec![format!(
                "care: settings not restored yet ({e}); they stay leased for the next reconnect"
            )],
        }
    }

    /// Wait until the phone may work: the heat guard, then the battery floor. `interrupt` is the
    /// job's own control (a pause or stop asked while resting ends the wait at once).
    pub fn wait(
        &self,
        interrupt: &dyn Fn() -> Option<Interrupt>,
        emit: &mut dyn FnMut(ControlNote),
    ) -> Option<Interrupt> {
        if self.core.mode != DeviceMode::Full {
            return None; // care's own gate: adb shell reads are for full devices only
        }
        let phone = self.core.phone();
        let mut polled = false;
        loop {
            let now = self.clock.mono_ms();
            let action = {
                let mut g = self
                    .heat
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !g.due(now) {
                    if g.is_working() {
                        break;
                    }
                    None
                } else {
                    match phone.reading() {
                        Ok(r) => {
                            polled = true;
                            *self
                                .last
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some((dc(r.battery_dc), f64::from(r.level)));
                            Some((g.poll(now, &r, &phone, self.notifier.as_ref()), r))
                        }
                        Err(e) => {
                            let working = g.is_working();
                            drop(g);
                            emit(ControlNote {
                                kind: EventKind::Error,
                                msg: format!("care: no phone reading ({e})"),
                                temp_c: None,
                                batt: None,
                            });
                            if working {
                                break; // unread while working: the next due poll tries again
                            }
                            None
                        }
                    }
                }
            };
            if let Some((a, r)) = action {
                let (t, b) = (Some(dc(r.battery_dc)), Some(f64::from(r.level)));
                let t_s = THRESHOLDS;
                match a {
                    GuardAction::Work { .. } => break,
                    GuardAction::Paused { temp_dc } => emit(ControlNote {
                        kind: EventKind::Pause,
                        msg: format!("heat: battery {:.1} °C reached {:.1} °C; screen asleep, resting until below {:.1} °C", dc(temp_dc), dc(t_s.pause_battery_dc), dc(t_s.resume_below_dc)),
                        temp_c: t,
                        batt: b,
                    }),
                    GuardAction::Resting { temp_dc, rested_ms } => emit(ControlNote { kind: EventKind::Progress, msg: format!("heat: resting, battery {:.1} °C after {} s", dc(temp_dc), rested_ms / 1000), temp_c: t, batt: b }),
                    GuardAction::Resumed { temp_dc, peak_dc, rested_ms, locked } => {
                        self.cooling_ms.fetch_add(rested_ms, Ordering::SeqCst);
                        let lock = if locked { "; the screen is locked" } else { "" };
                        emit(ControlNote { kind: EventKind::Resume, msg: format!("heat: battery {:.1} °C is below {:.1} °C after {} s (peak {:.1} °C); resuming{lock}", dc(temp_dc), dc(t_s.resume_below_dc), rested_ms / 1000, dc(peak_dc)), temp_c: t, batt: b });
                        break;
                    }
                    GuardAction::Stop { temp_dc, peak_dc, rested_ms, pushed, .. } => {
                        self.cooling_ms.fetch_add(rested_ms, Ordering::SeqCst);
                        let p = if pushed { "pushed" } else { "the push failed" };
                        emit(ControlNote { kind: EventKind::Error, msg: format!("heat: still {:.1} °C after {} min of rest (peak {:.1} °C); the job is stopped ({p})", dc(temp_dc), rested_ms / 60_000, dc(peak_dc)), temp_c: t, batt: b });
                        return Some(Interrupt::Stop);
                    }
                }
            }
            // resting: no step runs; the guard's next poll is due in 60 s
            if let Some(i) = self.nap(THRESHOLDS.poll_resting_ms, interrupt) {
                return Some(i);
            }
        }
        if polled {
            return self.floor(&phone, interrupt, emit);
        }
        None
    }

    /// The battery floor, at the heat guard's cadence. Held below it (and not powered): wait.
    fn floor(
        &self,
        phone: &AdbPhone<'_, DynRunner>,
        interrupt: &dyn Fn() -> Option<Interrupt>,
        emit: &mut dyn FnMut(ControlNote),
    ) -> Option<Interrupt> {
        let mut held = false;
        loop {
            let v = check_floor(phone, &self.core.mgr());
            match v {
                Ok(FloorVerdict::Pause { level }) => {
                    if !held {
                        emit(ControlNote { kind: EventKind::Pause, msg: format!("battery: {level} % is below the {} % floor and the phone is not powered; waiting for power", THRESHOLDS.battery_floor_pct), temp_c: None, batt: Some(f64::from(level)) });
                        held = true;
                    }
                }
                Ok(FloorVerdict::Ok { level, powered }) => {
                    if held {
                        let why = if powered {
                            "the phone is powered"
                        } else {
                            "the level is back above the floor"
                        };
                        emit(ControlNote {
                            kind: EventKind::Resume,
                            msg: format!("battery: {level} %, {why}; resuming"),
                            temp_c: None,
                            batt: Some(f64::from(level)),
                        });
                    }
                    return None;
                }
                Ok(FloorVerdict::Frozen { .. }) => return None,
                Err(e) => {
                    emit(ControlNote {
                        kind: EventKind::Error,
                        msg: format!("care: battery floor not checked ({e})"),
                        temp_c: None,
                        batt: None,
                    });
                    return None;
                }
            }
            if let Some(i) = self.nap(self.floor_poll_ms, interrupt) {
                return Some(i);
            }
        }
    }

    /// Sleep `ms` in slices, returning early with the job's interrupt.
    fn nap(&self, ms: u64, interrupt: &dyn Fn() -> Option<Interrupt>) -> Option<Interrupt> {
        let mut left = ms;
        while left > 0 {
            let s = left.min(WAIT_SLICE_MS);
            (self.sleep)(s);
            left -= s;
            if let Some(i) = interrupt() {
                return Some(i);
            }
        }
        None
    }

    /// care's buffered entries as journal records of `run_id`.
    pub fn drain_records(&self, run_id: &str, epoch: u64) -> Vec<JournalRecord> {
        self.core
            .buffer
            .drain()
            .iter()
            .map(|e| care_record(e, run_id, epoch))
            .collect()
    }
}
