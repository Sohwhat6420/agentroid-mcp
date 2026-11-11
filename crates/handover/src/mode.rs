//! The agent-mode / human-mode state machine:
//! `Idle → Agent → HumanSeen → (OutOfSight | Paused) → ReclaimWarn → Agent`.
//!
//! * In agent mode the agent profile is leased while a job holds the phone.
//! * A human touch stops injection first (the decision is stamped before anything slow runs),
//!   raises `take_over`, reports the interrupted step for a post-condition check, and restores
//!   `profiles/human.json` exactly through [`SettingsLeases`] within 2 s.
//! * Then keep-working on the virtual display (only when S1 passed and the SDK is 30 or higher, and
//!   the handover has no clash), otherwise pause-on-touch.
//! * Reclaim from out-of-sight: display 0 off or locked, and 2 min with no touch, then the 10 s dot
//!   warning; a touch, media, a call, or the screen coming back on and unlocked cancels it.
//! * Resume from the pause path: screen off and 5 min with no touch, then the same 10 s warning.
//! * Reclaim re-applies the agent profile within 2 s.
//! * Both detectors feed one `last_human_touch`: the latest timestamp wins, duplicates within
//!   50 ms merge.
//!
//! Timers run on the hub clock ([`Clock`]); phone timestamps are mapped onto it by the caller
//! (`touch::ClockMap`). Every transition is journalled.

use agentroid_adb_link::runner::Clock;

use crate::clash::Clash;
use crate::profile::{Profiles, SettingWrite};
use crate::vdisplay::Plan;

/// A human touch must stop injection within this long of the event.
pub const DECISION_BUDGET_MS: u64 = 300;
/// The human profile must be back within this long of the event.
pub const RESTORE_BUDGET_MS: u64 = 2_000;
/// The agent profile must be back within this long of the reclaim decision.
pub const REAPPLY_BUDGET_MS: u64 = 2_000;
/// No touch for this long (and display 0 off or locked) starts the reclaim warning.
pub const RECLAIM_IDLE_MS: u64 = 120_000;
/// The measured tolerance on [`RECLAIM_IDLE_MS`].
pub const RECLAIM_TOLERANCE_MS: u64 = 5_000;
/// The corner-dot warning before a reclaim or resume.
pub const WARN_MS: u64 = 10_000;
/// The pause path resumes after this long with no touch (and the screen off).
pub const PAUSE_RESUME_IDLE_MS: u64 = 300_000;
/// Touches closer than this are one touch (the two detectors see the same finger).
pub const MERGE_MS: u64 = 50;

/// Where a touch was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Detector 1, hub side (`getevent`).
    Getevent,
    /// Detector 2, phone side (the accessibility overlay).
    Overlay,
}

/// The one `last_human_touch`, fed by both detectors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TouchLog {
    last: Option<u64>,
    pub merged: u32,
    pub distinct: u32,
}

impl TouchLog {
    /// Record a touch at hub time `at`. Returns false when it merges with the last one (within
    /// [`MERGE_MS`]). The latest timestamp always wins: an older touch never moves `last` back.
    pub fn record(&mut self, at: u64) -> bool {
        match self.last {
            Some(l) if at.abs_diff(l) <= MERGE_MS => {
                self.last = Some(l.max(at));
                self.merged += 1;
                false
            }
            Some(l) => {
                self.last = Some(l.max(at));
                self.distinct += 1;
                true
            }
            None => {
                self.last = Some(at);
                self.distinct += 1;
                true
            }
        }
    }

    pub fn last(&self) -> Option<u64> {
        self.last
    }
}

/// Why a lease call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseError(pub String);

/// The settings-lease seam. `crates/care` (2.13) implements it over `leases.json`; tests use a fake.
pub trait SettingsLeases {
    /// Take the job's lease on every write (care records each before-value for compare-and-swap),
    /// apply the values, and set the helper's DND rule on or off.
    fn lease_agent(
        &mut self,
        owner: &str,
        writes: &[SettingWrite],
        dnd_rule: bool,
    ) -> Result<(), LeaseError>;
    /// Hand back: write `human` exactly (the fixed profile, never compare-and-swap), restore every
    /// other key this owner leased by compare-and-swap, set the DND rule, and release the leases.
    fn hand_back(
        &mut self,
        owner: &str,
        human: &[SettingWrite],
        dnd_rule: bool,
    ) -> Result<(), LeaseError>;
}

/// What can run on this phone (2.16's profile and S1's result).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub s1_passed: bool,
    pub sdk: u32,
}

impl Caps {
    /// Keep-working needs S1 and the API 30 multi-display APIs; otherwise every touch pauses.
    pub fn keep_working(&self) -> bool {
        self.s1_passed && self.sdk >= 30
    }
}

/// The phone's state on each tick (from the link or adb; media and calls are injected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhoneState {
    pub display0_on: bool,
    pub locked: bool,
    pub media_playing: bool,
    pub in_call: bool,
}

impl PhoneState {
    fn busy(&self) -> bool {
        self.media_playing || self.in_call
    }
}

/// Plans the virtual-display handover at the moment of a takeover.
pub trait Planner {
    fn plan(&mut self) -> Result<Plan, Clash>;
}

/// Where the job rests after a takeover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resting {
    OutOfSight,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No job: the human profile.
    Idle,
    /// A job holds the phone: the agent profile is leased.
    Agent,
    /// A human touch was seen: injection stopped, the human profile is being restored.
    HumanSeen,
    /// The job keeps working on the virtual display.
    OutOfSight,
    /// The job is paused.
    Paused,
    /// The 10 s dot warning before a reclaim (or a resume).
    ReclaimWarn { from: Resting, until: u64 },
}

/// Where the job may inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Injection {
    Display0,
    VirtualDisplayOnly,
    Stopped,
}

/// Lease attempts per call before the retry moves to the next tick.
const LEASE_ATTEMPTS: u32 = 3;

/// Why the job paused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PauseReason {
    /// S1 failed or the SDK is below 30.
    NoKeepWorking,
    /// The handover (or the out-of-sight work) clashed.
    Clash(Clash),
}

/// What the machine asks of the job and the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The agent profile is leased (job start or reclaim), at hub time `at`.
    AgentApplied {
        at: u64,
    },
    /// Stop every acting send now.
    StopInjection {
        event_at: u64,
        decided_at: u64,
        source: Source,
    },
    /// The job's `take_over` callback.
    TakeOver,
    /// This step was in flight: check its post-condition, never re-run it blindly.
    InterruptedStep {
        step: String,
    },
    /// The human profile is back, at `at`.
    HumanRestored {
        at: u64,
    },
    /// A lease call failed (it is retried on the next tick).
    LeaseTrouble {
        what: &'static str,
        why: String,
    },
    /// Move the job's app to the virtual display and keep working there.
    MoveToVirtualDisplay(Plan),
    PauseJob(PauseReason),
    ShowDot {
        until: u64,
    },
    HideDot,
    /// The warning ran out: the agent has the phone again.
    Reclaimed {
        at: u64,
    },
    /// The pause path resumes; check this step first, if any.
    ResumeJob {
        check_first: Option<String>,
    },
}

/// One journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub at: u64,
    pub from: Mode,
    pub to: Mode,
    pub why: String,
}

/// The machine.
pub struct ModeMachine<L: SettingsLeases, C: Clock> {
    pub leases: L,
    pub clock: C,
    pub profiles: Profiles,
    pub caps: Caps,
    /// The device's `settings_keys` (2.16), when known: keys it lacks are skipped.
    pub present_keys: Option<Vec<String>>,
    mode: Mode,
    owner: String,
    touches: TouchLog,
    in_flight: Option<String>,
    interrupted: Option<String>,
    /// Touches at or before this time cannot take the phone (they predate the agent's hold).
    agent_since: u64,
    pending_restore: bool,
    pending_agent: bool,
    pub journal: Vec<Transition>,
}

impl<L: SettingsLeases, C: Clock> ModeMachine<L, C> {
    pub fn new(leases: L, clock: C, profiles: Profiles, caps: Caps) -> Self {
        Self {
            leases,
            clock,
            profiles,
            caps,
            present_keys: None,
            mode: Mode::Idle,
            owner: String::new(),
            touches: TouchLog::default(),
            in_flight: None,
            interrupted: None,
            agent_since: 0,
            pending_restore: false,
            pending_agent: false,
            journal: Vec::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn last_human_touch(&self) -> Option<u64> {
        self.touches.last()
    }

    pub fn touch_log(&self) -> TouchLog {
        self.touches
    }

    /// May the job send an acting call now, and where.
    pub fn injection(&self) -> Injection {
        match self.mode {
            Mode::Agent => Injection::Display0,
            Mode::OutOfSight
            | Mode::ReclaimWarn {
                from: Resting::OutOfSight,
                ..
            } => Injection::VirtualDisplayOnly,
            _ => Injection::Stopped,
        }
    }

    fn go(&mut self, to: Mode, why: impl Into<String>) {
        let at = self.clock.now_ms();
        self.journal.push(Transition {
            at,
            from: self.mode,
            to,
            why: why.into(),
        });
        self.mode = to;
    }

    fn human_writes(&self) -> Vec<SettingWrite> {
        self.profiles.human.writes(self.present_keys.as_deref())
    }

    fn agent_writes(&self) -> Vec<SettingWrite> {
        self.profiles.agent.writes(self.present_keys.as_deref())
    }

    /// Lease the agent profile, retrying within the budget.
    fn apply_agent(&mut self, out: &mut Vec<Action>) -> bool {
        let start = self.clock.now_ms();
        let writes = self.agent_writes();
        let dnd = self.profiles.agent.dnd_zen_rule.unwrap_or(false);
        let mut attempts = 0;
        loop {
            match self.leases.lease_agent(&self.owner, &writes, dnd) {
                Ok(()) => {
                    self.pending_agent = false;
                    out.push(Action::AgentApplied {
                        at: self.clock.now_ms(),
                    });
                    return true;
                }
                Err(e) => {
                    out.push(Action::LeaseTrouble {
                        what: "agent profile",
                        why: e.0,
                    });
                    attempts += 1;
                    if attempts >= LEASE_ATTEMPTS
                        || self.clock.now_ms().saturating_sub(start) >= REAPPLY_BUDGET_MS / 2
                    {
                        self.pending_agent = true;
                        return false;
                    }
                }
            }
        }
    }

    /// Hand back to the fixed human profile, retrying within the budget.
    fn restore_human(&mut self, out: &mut Vec<Action>) -> bool {
        let start = self.clock.now_ms();
        let writes = self.human_writes();
        let dnd = self.profiles.human.dnd_zen_rule.unwrap_or(false);
        let mut attempts = 0;
        loop {
            match self.leases.hand_back(&self.owner, &writes, dnd) {
                Ok(()) => {
                    self.pending_restore = false;
                    out.push(Action::HumanRestored {
                        at: self.clock.now_ms(),
                    });
                    return true;
                }
                Err(e) => {
                    out.push(Action::LeaseTrouble {
                        what: "human profile",
                        why: e.0,
                    });
                    attempts += 1;
                    if attempts >= LEASE_ATTEMPTS
                        || self.clock.now_ms().saturating_sub(start) >= RESTORE_BUDGET_MS / 2
                    {
                        self.pending_restore = true;
                        return false;
                    }
                }
            }
        }
    }

    /// A job takes the phone: lease the agent profile.
    pub fn start_job(&mut self, job_id: &str) -> Vec<Action> {
        let mut out = Vec::new();
        if self.mode != Mode::Idle {
            return out;
        }
        self.owner = job_id.to_string();
        self.agent_since = self.clock.now_ms();
        self.interrupted = None;
        self.go(Mode::Agent, format!("job {job_id} holds the phone"));
        self.apply_agent(&mut out);
        out
    }

    /// The job ends (done, stopped or failed): hand back to the human profile.
    pub fn end_job(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        if self.mode == Mode::Idle {
            return out;
        }
        if matches!(self.mode, Mode::ReclaimWarn { .. }) {
            out.push(Action::HideDot);
        }
        self.restore_human(&mut out);
        self.go(Mode::Idle, "job ended");
        self.owner.clear();
        self.in_flight = None;
        out
    }

    pub fn step_started(&mut self, step: &str) {
        self.in_flight = Some(step.to_string());
    }

    pub fn step_finished(&mut self, step: &str) {
        if self.in_flight.as_deref() == Some(step) {
            self.in_flight = None;
        }
        if self.interrupted.as_deref() == Some(step) {
            self.interrupted = None;
        }
    }

    /// A human touch from either detector, at hub time `at`.
    pub fn human_touch(
        &mut self,
        at: u64,
        source: Source,
        planner: &mut dyn Planner,
    ) -> Vec<Action> {
        let mut out = Vec::new();
        let decided_at = self.clock.now_ms();
        let fresh = self.touches.record(at);
        match self.mode {
            Mode::Agent if fresh && at > self.agent_since => {
                // 1. stop injection before anything slow
                out.push(Action::StopInjection {
                    event_at: at,
                    decided_at,
                    source,
                });
                self.go(Mode::HumanSeen, format!("human touch ({source:?})"));
                out.push(Action::TakeOver);
                if let Some(step) = self.in_flight.take() {
                    self.interrupted = Some(step.clone());
                    out.push(Action::InterruptedStep { step });
                }
                // 2. the fixed human profile
                self.restore_human(&mut out);
                // 3. keep working out of sight, or pause
                if !self.caps.keep_working() {
                    self.go(Mode::Paused, "pause-on-touch (S1 failed or SDK below 30)");
                    out.push(Action::PauseJob(PauseReason::NoKeepWorking));
                } else {
                    match planner.plan() {
                        Ok(plan) => {
                            self.go(Mode::OutOfSight, "keep working on the virtual display");
                            out.push(Action::MoveToVirtualDisplay(plan));
                        }
                        Err(c) => {
                            self.go(Mode::Paused, format!("clash: {c}"));
                            out.push(Action::PauseJob(PauseReason::Clash(c)));
                        }
                    }
                }
            }
            Mode::ReclaimWarn { from, .. } if fresh => {
                out.push(Action::HideDot);
                self.go(resting(from), "touch during the warning: reclaim cancelled");
            }
            _ => {}
        }
        out
    }

    /// The job hit a clash while working out of sight: pause.
    pub fn clash(&mut self, c: Clash) -> Vec<Action> {
        let mut out = Vec::new();
        match self.mode {
            Mode::OutOfSight
            | Mode::ReclaimWarn {
                from: Resting::OutOfSight,
                ..
            } => {
                if matches!(self.mode, Mode::ReclaimWarn { .. }) {
                    out.push(Action::HideDot);
                }
                self.go(Mode::Paused, format!("clash: {c}"));
                out.push(Action::PauseJob(PauseReason::Clash(c)));
            }
            _ => {}
        }
        out
    }

    fn idle_for(&self, now: u64) -> u64 {
        now.saturating_sub(self.touches.last().unwrap_or(self.agent_since))
    }

    fn may_warn(&self, from: Resting, now: u64, p: &PhoneState) -> bool {
        if p.busy() {
            return false;
        }
        match from {
            Resting::OutOfSight => {
                (!p.display0_on || p.locked) && self.idle_for(now) >= RECLAIM_IDLE_MS
            }
            Resting::Paused => !p.display0_on && self.idle_for(now) >= PAUSE_RESUME_IDLE_MS,
        }
    }

    /// Hub timer tick with the phone's current state.
    pub fn tick(&mut self, p: PhoneState) -> Vec<Action> {
        let mut out = Vec::new();
        let now = self.clock.now_ms();
        if self.pending_restore && !matches!(self.mode, Mode::Agent) {
            self.restore_human(&mut out);
        }
        if self.pending_agent && self.mode == Mode::Agent {
            self.apply_agent(&mut out);
        }
        match self.mode {
            Mode::OutOfSight | Mode::Paused => {
                let from = if self.mode == Mode::OutOfSight {
                    Resting::OutOfSight
                } else {
                    Resting::Paused
                };
                if self.may_warn(from, now, &p) {
                    let until = now + WARN_MS;
                    self.go(
                        Mode::ReclaimWarn { from, until },
                        "idle and screen off or locked: 10 s warning",
                    );
                    out.push(Action::ShowDot { until });
                }
            }
            Mode::ReclaimWarn { from, until } => {
                // the idle rule is not re-checked here: a touch cancels through human_touch
                let still = !p.busy()
                    && match from {
                        Resting::OutOfSight => !p.display0_on || p.locked,
                        Resting::Paused => !p.display0_on,
                    };
                if !still {
                    out.push(Action::HideDot);
                    self.go(
                        resting(from),
                        "media, a call or the screen came back: reclaim cancelled",
                    );
                } else if now >= until {
                    out.push(Action::HideDot);
                    self.agent_since = now;
                    self.go(Mode::Agent, "warning ran out: reclaim");
                    out.push(Action::Reclaimed { at: now });
                    self.apply_agent(&mut out);
                    if from == Resting::Paused {
                        out.push(Action::ResumeJob {
                            check_first: self.interrupted.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
        out
    }
}

fn resting(from: Resting) -> Mode {
    match from {
        Resting::OutOfSight => Mode::OutOfSight,
        Resting::Paused => Mode::Paused,
    }
}
