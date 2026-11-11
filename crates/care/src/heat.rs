//! The heat guard (port of phone-guard `thermal_guard`, made a non-blocking state
//! machine so one daemon can drive many phones on an injected clock).
//!
//! * **Battery** temperature alone pauses and resumes (the default): pause at
//!   `pause_battery_dc` (45.0 °C for bench jobs), resume only **strictly below** `resume_below_dc`.
//! * The screen sleeps while resting (and is put back to sleep if something wakes it). It is never
//!   slept while working: only the `Working → Resting` transition and a woken rest send sleep.
//! * A rest that reaches `rest_cap_ms` pushes through the [`Notifier`] and stops, screen left asleep.
//! * Skin temperature and thermal headroom only slow work, in proportion, down to at most one call
//!   per `max_pace_ms` (1 call a second).
//!
//! Divergence from phone-guard, on purpose: phone-guard paused on the max of the battery and the
//! thermalservice skin/battery readings; the plan's default is battery only, skin only paces.

use crate::phone::Reading;
use crate::thresholds::THRESHOLDS;
use crate::CareError;

/// The screen, as the guard needs it.
pub trait Screen {
    /// `KEYCODE_SLEEP`.
    fn sleep(&self) -> Result<(), CareError>;
    /// `KEYCODE_WAKEUP`, then dismiss the keyguard if no lock is set. Returns whether it is still locked.
    fn wake(&self) -> Result<bool, CareError>;
    /// Awake now? `None` when unknown.
    fn awake(&self) -> Option<bool>;
}

/// The push channel (a phone push or a message to the user, task 2.11a). Injected.
pub trait Notifier {
    fn push(&self, serial: &str, title: &str, body: &str) -> Result<(), String>;
}

/// Ordinary jobs pause at 42 °C; bench jobs at 45 °C. Both resume below 37 °C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Normal,
    Bench,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Still hot after the 60 min rest cap.
    RestCap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeatState {
    Working,
    Resting { since_ms: u64, peak_dc: i32 },
    Stopped { reason: StopReason },
}

/// What the caller (the flow engine) must do after a poll.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardAction {
    /// Keep working, at least `pace_ms` between calls (0 = no slowdown).
    Work { pace_ms: u64 },
    /// Just paused: the screen was sent to sleep. Emit a `pause` event with `heat:`.
    Paused { temp_dc: i32 },
    /// Still resting.
    Resting { temp_dc: i32, rested_ms: u64 },
    /// Cooled down: screen woken. Continue the job.
    Resumed {
        temp_dc: i32,
        peak_dc: i32,
        rested_ms: u64,
        locked: bool,
    },
    /// Rest cap reached: pushed, stop the job.
    Stop {
        reason: StopReason,
        temp_dc: i32,
        peak_dc: i32,
        rested_ms: u64,
        pushed: bool,
    },
}

/// Proportional slowdown from skin temperature and headroom: 0 below the start of each ramp,
/// `max_pace_ms` at its end, linear in between; the larger of the two wins.
pub fn pace_ms(skin_dc: Option<i32>, headroom: Option<f32>) -> u64 {
    let t = &THRESHOLDS;
    let skin = skin_dc.map_or(0.0, |s| {
        ((s - t.skin_slow_from_dc) as f64 / (t.skin_slow_full_dc - t.skin_slow_from_dc) as f64)
            .clamp(0.0, 1.0)
    });
    let head = headroom.filter(|h| h.is_finite()).map_or(0.0, |h| {
        (((h - t.headroom_slow_from) / (t.headroom_slow_full - t.headroom_slow_from)) as f64)
            .clamp(0.0, 1.0)
    });
    (skin.max(head) * t.max_pace_ms as f64).round() as u64
}

/// One phone's guard. Each phone has its own instance; nothing here is shared.
#[derive(Debug, Clone)]
pub struct HeatGuard {
    serial: String,
    kind: JobKind,
    state: HeatState,
    next_poll_ms: u64,
}

impl HeatGuard {
    pub fn new(serial: impl Into<String>, kind: JobKind) -> Self {
        HeatGuard {
            serial: serial.into(),
            kind,
            state: HeatState::Working,
            next_poll_ms: 0,
        }
    }

    pub fn serial(&self) -> &str {
        &self.serial
    }
    pub fn state(&self) -> HeatState {
        self.state
    }
    pub fn is_working(&self) -> bool {
        self.state == HeatState::Working
    }

    /// The battery temperature that pauses this job.
    pub fn pause_at_dc(&self) -> i32 {
        match self.kind {
            JobKind::Normal => THRESHOLDS.pause_battery_dc,
            JobKind::Bench => THRESHOLDS.bench_pause_battery_dc,
        }
    }

    /// Is a poll due at `now_ms`? (Working: every 10 s; resting: every 60 s.)
    pub fn due(&self, now_ms: u64) -> bool {
        now_ms >= self.next_poll_ms && !matches!(self.state, HeatState::Stopped { .. })
    }

    /// Feed one reading. Sends screen commands and the push itself; returns what the job must do.
    pub fn poll(
        &mut self,
        now_ms: u64,
        r: &Reading,
        screen: &dyn Screen,
        notifier: &dyn Notifier,
    ) -> GuardAction {
        let t = &THRESHOLDS;
        let temp = r.battery_dc;
        match self.state {
            HeatState::Working => {
                if temp >= self.pause_at_dc() {
                    let _ = screen.sleep(); // a failed sleep still rests: no work is sent while resting
                    self.state = HeatState::Resting {
                        since_ms: now_ms,
                        peak_dc: temp,
                    };
                    self.next_poll_ms = now_ms + t.poll_resting_ms;
                    GuardAction::Paused { temp_dc: temp }
                } else {
                    self.next_poll_ms = now_ms + t.poll_working_ms;
                    GuardAction::Work {
                        pace_ms: pace_ms(r.skin_dc, r.headroom),
                    }
                }
            }
            HeatState::Resting { since_ms, peak_dc } => {
                let peak_dc = peak_dc.max(temp);
                let rested_ms = now_ms.saturating_sub(since_ms);
                if temp < t.resume_below_dc {
                    // strictly below: 37.0 does not resume, 36.9 does
                    let locked = screen.wake().unwrap_or(true);
                    self.state = HeatState::Working;
                    self.next_poll_ms = now_ms + t.poll_working_ms;
                    return GuardAction::Resumed {
                        temp_dc: temp,
                        peak_dc,
                        rested_ms,
                        locked,
                    };
                }
                if rested_ms >= t.rest_cap_ms {
                    let body = format!(
                        "{} is still {:.1} °C after {} min of rest (peak {:.1} °C): the job is stopped.",
                        self.serial,
                        temp as f64 / 10.0,
                        rested_ms / 60_000,
                        peak_dc as f64 / 10.0
                    );
                    let pushed = notifier
                        .push(&self.serial, "Phone too hot: job stopped", &body)
                        .is_ok();
                    self.state = HeatState::Stopped {
                        reason: StopReason::RestCap,
                    };
                    return GuardAction::Stop {
                        reason: StopReason::RestCap,
                        temp_dc: temp,
                        peak_dc,
                        rested_ms,
                        pushed,
                    };
                }
                if screen.awake() == Some(true) {
                    let _ = screen.sleep(); // something woke it: sleep again
                }
                self.state = HeatState::Resting { since_ms, peak_dc };
                self.next_poll_ms = now_ms + t.poll_resting_ms;
                GuardAction::Resting {
                    temp_dc: temp,
                    rested_ms,
                }
            }
            HeatState::Stopped { reason } => GuardAction::Stop {
                reason,
                temp_dc: temp,
                peak_dc: temp,
                rested_ms: 0,
                pushed: false,
            },
        }
    }
}
