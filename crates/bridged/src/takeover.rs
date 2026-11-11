//! Agent mode and human mode in the daemon: task 2.15 (`crates/handover`) attached to running
//! jobs.
//!
//! * One [`handover::mode::ModeMachine`] per job on a `full` phone. Its settings seam
//!   ([`SettingsLeases`]) is care's lease manager: the agent profile (`profiles/agent.json`) is
//!   leased job-scoped in `leases.json` when the job starts; a human touch hands back by restoring
//!   those leases (compare-and-swap) and writing `profiles/human.json` exactly.
//! * The job's journal tells the machine which step is in flight (`sent` opens it; `done`,
//!   `failed` or `interrupted` closes it), so a touch reports the right step.
//! * A human touch from Detector 1 (`getevent`, [`crate::server::Daemon::feed_getevent`]) or the
//!   overlay raises `take_over`: the job's control turns `TakeOver`, the engine journals the
//!   in-flight step `interrupted` (`post.interrupted_by: "human"`; resume re-checks its
//!   post-condition) and the job pauses `taken_over`. Phase 2 has no keep-working path (S1 is not
//!   passed), so every take-over pauses.
//! * **The injection log.** Detector 1 never sees the hub's own `input tap` (it reads physical
//!   touchscreens only), but a touch report from the overlay can be the hub's own injection. Every
//!   acting step's `sent`..`done` window (plus [`INJECTION_SLACK_MS`]) is logged, and an overlay
//!   touch inside one is dropped as the hub's own.

use crate::jobcare::CareCore;
use agentroid_adb_link::runner::Clock;
use care::clock::CareClock;
use care::settings::ScopeReq;
use handover::clash::Clash;
use handover::mode::{
    Action, Caps, LeaseError, Mode, ModeMachine, Planner, SettingsLeases, Source,
};
use handover::profile::{Profiles, SettingWrite};
use handover::vdisplay::Plan;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// An overlay touch this close to one of the hub's own injection windows is the hub's own.
pub const INJECTION_SLACK_MS: u64 = 150;
/// Closed injection windows kept.
const WINDOWS: usize = 64;

/// The hub's own injections: one window per acting step, on the hub's monotonic clock.
#[derive(Debug, Default)]
pub struct InjectionLog {
    open: Option<(String, u64)>,
    closed: VecDeque<(u64, u64)>,
}

impl InjectionLog {
    pub fn sent(&mut self, step: &str, at: u64) {
        self.open = Some((step.to_string(), at));
    }
    pub fn ended(&mut self, step: &str, at: u64) {
        if let Some((s, from)) = self.open.take() {
            if s == step {
                while self.closed.len() >= WINDOWS {
                    self.closed.pop_front();
                }
                self.closed.push_back((from, at));
            } else {
                self.open = Some((s, from));
            }
        }
    }
    /// Was something of ours on its way to the screen at `at`?
    pub fn covers(&self, at: u64) -> bool {
        let near =
            |from: u64, to: u64| at + INJECTION_SLACK_MS >= from && at <= to + INJECTION_SLACK_MS;
        self.open
            .as_ref()
            .is_some_and(|(_, from)| at + INJECTION_SLACK_MS >= *from)
            || self.closed.iter().any(|(f, t)| near(*f, *t))
    }
}

/// care's monotonic clock as adb-link's [`Clock`] (the mode machine's timers).
pub struct CareMono(pub Arc<dyn CareClock>);

impl Clock for CareMono {
    fn now_ms(&self) -> u64 {
        self.0.mono_ms()
    }
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// The mode machine's settings seam, filled by care's lease manager.
pub struct CareLeases(pub Arc<CareCore>);

impl SettingsLeases for CareLeases {
    fn lease_agent(
        &mut self,
        owner: &str,
        writes: &[SettingWrite],
        _dnd_rule: bool,
    ) -> Result<(), LeaseError> {
        // the DND rule belongs to the helper app (2.16), which is not reachable from here yet: CP-D
        let core = &self.0;
        let phone = core.phone();
        let who = core.owner(owner);
        let mut mgr = core.mgr();
        for w in writes {
            mgr.apply(
                &phone,
                core.mode,
                &who,
                &w.ns,
                &w.key,
                &w.value,
                ScopeReq::Job,
            )
            .map_err(|e| LeaseError(format!("{}: {e}", w.id())))?;
        }
        Ok(())
    }
    fn hand_back(
        &mut self,
        owner: &str,
        human: &[SettingWrite],
        _dnd_rule: bool,
    ) -> Result<(), LeaseError> {
        let core = &self.0;
        let phone = core.phone();
        let mut mgr = core.mgr();
        mgr.restore_job(&phone, core.mode, owner)
            .map_err(|e| LeaseError(e.to_string()))?;
        // the fixed human profile, written exactly (never compare-and-swap)
        for w in human {
            phone
                .settings_put(&w.ns, &w.key, &w.value)
                .map_err(|e| LeaseError(format!("{}: {e}", w.id())))?;
        }
        Ok(())
    }
}

/// Phase 2: S1 has not passed, so `Caps::keep_working` is false and the machine never asks for a
/// plan. Should it ever, the answer is a clash, so the job pauses.
struct NoVirtualDisplay;

impl Planner for NoVirtualDisplay {
    fn plan(&mut self) -> Result<Plan, Clash> {
        Err(Clash::TaskLeftVirtualDisplay {
            package: "(no virtual display in Phase 2)".into(),
        })
    }
}

/// What a touch did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TouchOutcome {
    /// An overlay report inside one of the hub's own injection windows: dropped.
    pub own_injection: bool,
    /// The machine raised `take_over`.
    pub take_over: bool,
    /// The step that was in flight.
    pub interrupted: Option<String>,
    /// Every action, in words (for the journal and tests).
    pub actions: Vec<String>,
}

/// One job's take-over machinery.
pub struct Takeover {
    machine: Mutex<ModeMachine<CareLeases, CareMono>>,
    log: Mutex<InjectionLog>,
    clock: Arc<dyn CareClock>,
    pub own_injections_dropped: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Takeover {
    pub fn new(
        core: Arc<CareCore>,
        clock: Arc<dyn CareClock>,
        profiles: &Path,
    ) -> Result<Takeover, String> {
        let p = Profiles::load(profiles)
            .map_err(|e| format!("profiles {}: {}", profiles.display(), e.0))?;
        let m = ModeMachine::new(
            CareLeases(core),
            CareMono(clock.clone()),
            p,
            Caps {
                s1_passed: false,
                sdk: 0,
            },
        );
        Ok(Takeover {
            machine: Mutex::new(m),
            log: Mutex::new(InjectionLog::default()),
            clock,
            own_injections_dropped: AtomicU64::new(0),
        })
    }

    pub fn now(&self) -> u64 {
        self.clock.mono_ms()
    }

    pub fn mode(&self) -> Mode {
        lock(&self.machine).mode()
    }

    /// A job (re)takes the phone. After a take-over the person handed back: their profile is
    /// written again (idempotent), then the agent profile is leased.
    pub fn begin(&self, job_id: &str) -> Vec<String> {
        let mut m = lock(&self.machine);
        let mut acts = vec![];
        if m.mode() != Mode::Idle && m.mode() != Mode::Agent {
            acts.extend(m.end_job());
        }
        acts.extend(m.start_job(job_id));
        acts.iter().map(|a| format!("handover: {a:?}")).collect()
    }

    pub fn end(&self) -> Vec<String> {
        lock(&self.machine)
            .end_job()
            .iter()
            .map(|a| format!("handover: {a:?}"))
            .collect()
    }

    /// From the job's journal: an acting step's intent went out.
    pub fn step_sent(&self, step: &str) {
        let now = self.now();
        lock(&self.log).sent(step, now);
        lock(&self.machine).step_started(step);
    }

    /// From the job's journal: the step's outcome is in.
    pub fn step_ended(&self, step: &str) {
        let now = self.now();
        lock(&self.log).ended(step, now);
        lock(&self.machine).step_finished(step);
    }

    /// A human touch at hub time `at`.
    pub fn touch(&self, at: u64, source: Source) -> TouchOutcome {
        let mut out = TouchOutcome::default();
        if source == Source::Overlay && lock(&self.log).covers(at) {
            self.own_injections_dropped.fetch_add(1, Ordering::SeqCst);
            out.own_injection = true;
            out.actions.push("dropped: the hub's own injection".into());
            return out;
        }
        let acts = lock(&self.machine).human_touch(at, source, &mut NoVirtualDisplay);
        for a in &acts {
            match a {
                Action::TakeOver => out.take_over = true,
                Action::InterruptedStep { step } => out.interrupted = Some(step.clone()),
                _ => {}
            }
            out.actions.push(format!("{a:?}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_injection_log_covers_open_and_recent_windows_only() {
        let mut l = InjectionLog::default();
        assert!(!l.covers(1_000));
        l.sent("tap-1", 1_000);
        assert!(l.covers(1_000) && l.covers(5_000), "open window");
        assert!(l.covers(1_000 - INJECTION_SLACK_MS) && !l.covers(1_000 - INJECTION_SLACK_MS - 1));
        l.ended("tap-1", 1_200);
        assert!(l.covers(1_200 + INJECTION_SLACK_MS));
        assert!(
            !l.covers(1_200 + INJECTION_SLACK_MS + 1),
            "after the window: a person"
        );
        l.ended("other", 9_000);
        assert!(!l.covers(9_000), "an end without its start opens nothing");
    }
}
