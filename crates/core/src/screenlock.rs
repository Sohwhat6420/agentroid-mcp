//! The screen lock: the daemon refuses every screen-changing call unless `ask-screen` allowed it.
//!
//! `ask-screen` (in [`crate::askscreen`]) only asks. A session that skips it could still tap on a
//! phone someone is using. The lock closes that gap. When it is set on a [`crate::transport::Core`],
//! every acting call (tap, long_press, swipe, scroll, type_text, key, unlock, open_app, open_url,
//! start_activity, set_value, the acting steps of flows and jobs) and every `run_script` shell
//! line that is not on the read-only list goes through [`ScreenLock::admit`] first:
//!
//! 1. **A live lease.** A Yes to `ask-screen` for this phone (any session, any purpose) whose
//!    lease has not run out: allowed. The lease is read from `ask-screen`'s own state file, keyed
//!    by the phone's hardware serial or its adb serial.
//! 2. **A No stands.** Inside the 5-minute cool-down after a No (or no answer): refused.
//! 3. **Not in use now.** The phone is read at the moment of the call, with the same rules as
//!    `ask-screen` (screen off or dozing; or on, untouched for 5 minutes, nothing holding it on, no
//!    keyboard, no listed app): allowed, and a **burst** starts.
//! 4. **A burst goes on.** The agent's own taps count as touches and its own app or keyboard
//!    shows as in use, so after the first allowed call the phone would read as in use at once. A
//!    burst therefore goes on while the phone's last touch is no later than the end of the
//!    agent's own last action and that action was under [`BURST_GAP_MS`] ago. The phone states
//!    its last touch as "N ms ago" at the moment it is read, somewhere inside the read, so the
//!    lock dates the touch from the end of the read and allows for the read's own measured length
//!    plus [`OWN_SLACK_MS`]. A touch by anyone else after that ends the burst, and so does a call
//!    screen. A touch that comes while the agent's own action is still running is not told apart.
//! 5. **Otherwise refused**, with the reasons and the `ask-screen` command to run.
//!
//! Emulators (`emulator-<port>`) are not locked: nobody holds them. Nothing that cannot be read
//! is guessed: a phone that cannot be read is refused.

use crate::askscreen::{judge, load_memory, Signals, Use, REASK_AFTER_MS};
use std::collections::HashMap;
use std::path::PathBuf;

/// A burst ends when the agent has done nothing on the phone for this long.
pub const BURST_GAP_MS: u64 = 5 * 60_000;
/// Allowed on top of the read's own measured length when the phone's last touch is compared with
/// the end of the agent's own action (the phone's clock steps; the order of its reads).
pub const OWN_SLACK_MS: u64 = 300;
/// How long a shell line from a script counts as the agent's own activity: the lock admits it
/// before it runs and does not see it end.
pub const SHELL_SPAN_MS: u64 = 10_000;
/// The idle time below which a screen-on phone counts as in use (the `ask-screen` default).
pub const DEFAULT_IDLE_MS: u64 = 5 * 60_000;
/// Apps that end a burst whoever opened them: a ringing or live call.
pub const CALL_APPS: [&str; 5] = [
    "com.android.incallui",
    "com.samsung.android.incallui",
    "com.google.android.dialer",
    "com.android.dialer",
    "com.android.server.telecom",
];

/// Reads the phone's signals for an adb serial (production: `ask-screen`'s own reader over the
/// guarded adb runner). `None`: could not be read.
pub type SignalReader = Box<dyn FnMut(&str) -> Option<Signals> + Send>;
/// Unix milliseconds (tests pass a fake).
pub type UnixNow = Box<dyn Fn() -> u64 + Send>;

/// Why a call was let through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admitted {
    /// Emulators are not locked.
    Emulator,
    /// A live `ask-screen` lease, with the minutes left.
    Lease(u64),
    /// Not in use at the time of the call.
    Free(String),
    /// The agent's own burst goes on: no one else has touched the phone since its last action.
    Burst,
}

pub struct ScreenLock {
    state_dir: PathBuf,
    idle_limit_ms: u64,
    read: SignalReader,
    now: UnixNow,
    /// Per phone (hardware serial, lower case): the end of the agent's last admitted action.
    ours: HashMap<String, u64>,
}

fn phone_key(hw_serial: &str, adb_serial: &str) -> String {
    if hw_serial.is_empty() {
        adb_serial
    } else {
        hw_serial
    }
    .to_ascii_lowercase()
}

impl ScreenLock {
    /// `state_dir` is `ask-screen`'s state folder (where its leases are).
    pub fn new(state_dir: PathBuf, read: SignalReader, now: UnixNow) -> Self {
        ScreenLock {
            state_dir,
            idle_limit_ms: DEFAULT_IDLE_MS,
            read,
            now,
            ours: HashMap::new(),
        }
    }

    pub fn with_idle_limit_ms(mut self, ms: u64) -> Self {
        self.idle_limit_ms = ms;
        self
    }

    pub fn state_dir(&self) -> &std::path::Path {
        &self.state_dir
    }

    /// May a screen-changing call go to this phone now? `Err` holds the reason, for the refusal.
    pub fn admit(&mut self, adb_serial: &str, hw_serial: &str) -> Result<Admitted, String> {
        if adb_serial.starts_with("emulator-") {
            return Ok(Admitted::Emulator);
        }
        let t = (self.now)();
        // ask-screen keys its file by the hardware serial when it can read it, else by the
        // `--device` text; look under both
        let mut lease_until = 0;
        let mut denied_until = 0;
        for k in [hw_serial, adb_serial] {
            if k.is_empty() {
                continue;
            }
            let m = load_memory(&self.state_dir, k, t);
            lease_until = m.leases.values().copied().fold(lease_until, u64::max);
            denied_until = denied_until.max(m.denied_until_ms);
        }
        let ask = format!(
            "run `agentroid ask-screen --device {adb_serial} --minutes N --why \"...\"` first (a Yes gives a lease this lock honours)"
        );
        if lease_until > t {
            return Ok(Admitted::Lease((lease_until - t).div_ceil(60_000)));
        }
        if t < denied_until {
            return Err(format!(
                "the owner said No to the last ask-screen (or did not answer): no screen-changing call for {} s",
                (denied_until - t).div_ceil(1000).min(REASK_AFTER_MS / 1000)
            ));
        }
        let read_from = (self.now)();
        let read = (self.read)(adb_serial);
        let read_to = (self.now)().max(read_from);
        let Some(s) = read else {
            return Err(format!(
                "the phone could not be read, so whether it is in use is unknown; {ask}"
            ));
        };
        let reasons = match judge(&s, self.idle_limit_ms, &[]) {
            Use::Free(why) => return Ok(Admitted::Free(why)),
            Use::InUse(r) => r,
        };
        if let Some(&last) = self.ours.get(&phone_key(hw_serial, adb_serial)) {
            let call_up = s.focus.as_deref().is_some_and(|f| CALL_APPS.contains(&f));
            if let (Some(true), Some(idle), false) = (s.screen_on(), s.idle_ms, call_up) {
                // the latest the touch can have been: "idle ms ago" at the end of the read
                let touched_at = read_to.saturating_sub(idle);
                let allowance = (read_to - read_from) + OWN_SLACK_MS;
                if read_to < last.saturating_add(BURST_GAP_MS)
                    && touched_at <= last.saturating_add(allowance)
                {
                    return Ok(Admitted::Burst);
                }
            }
        }
        Err(format!(
            "the phone is in use ({}); {ask}",
            reasons.join(", ")
        ))
    }

    /// The agent's own action on this phone ended at `until_ms` (Unix ms; `None`: now).
    pub fn acted(&mut self, adb_serial: &str, hw_serial: &str, until_ms: Option<u64>) {
        if adb_serial.starts_with("emulator-") {
            return;
        }
        let at = until_ms.unwrap_or_else(|| (self.now)());
        let e = self
            .ours
            .entry(phone_key(hw_serial, adb_serial))
            .or_insert(0);
        *e = (*e).max(at);
    }

    /// Now, on the lock's clock (Unix ms).
    pub fn now_ms(&self) -> u64 {
        (self.now)()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::askscreen::{save_memory, Memory};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    const T0: u64 = 1_800_000_000_000;

    fn awake(focus: &str, ime: bool, idle_ms: u64) -> Signals {
        Signals {
            wakefulness: Some("Awake".into()),
            idle_ms: Some(idle_ms),
            screen_kept_on: Some(false),
            focus: Some(focus.into()),
            ime_shown: Some(ime),
            keyguard: Some(false),
        }
    }

    struct Rig {
        lock: ScreenLock,
        clock: Arc<AtomicU64>,
        phone: Arc<Mutex<Option<Signals>>>,
        dir: PathBuf,
    }

    fn rig(name: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "agentroid-screenlock-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let clock = Arc::new(AtomicU64::new(T0));
        let phone = Arc::new(Mutex::new(None));
        let (c, p) = (clock.clone(), phone.clone());
        let lock = ScreenLock::new(
            dir.clone(),
            Box::new(move |_| p.lock().unwrap().clone()),
            Box::new(move || c.load(Ordering::SeqCst)),
        );
        Rig {
            lock,
            clock,
            phone,
            dir,
        }
    }

    impl Rig {
        fn set(&self, s: Option<Signals>) {
            *self.phone.lock().unwrap() = s;
        }
        fn at(&self, t: u64) {
            self.clock.store(t, Ordering::SeqCst);
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn a_phone_in_use_is_refused_with_the_ask_screen_command() {
        let mut r = rig("inuse");
        r.set(Some(awake("com.anthropic.claude", true, 3_000)));
        let e = r.lock.admit("192.0.2.9:5555", "HW1").unwrap_err();
        assert!(e.contains("in use"), "{e}");
        assert!(e.contains("com.anthropic.claude is open"), "{e}");
        assert!(
            e.contains("agentroid ask-screen --device 192.0.2.9:5555"),
            "{e}"
        );
    }

    #[test]
    fn an_unreadable_phone_is_refused() {
        let mut r = rig("unreadable");
        r.set(None);
        assert!(r
            .lock
            .admit("s1", "HW1")
            .unwrap_err()
            .contains("could not be read"));
        // unknown wakefulness is in use too
        r.set(Some(Signals::default()));
        assert!(r.lock.admit("s1", "HW1").is_err());
    }

    #[test]
    fn a_phone_not_in_use_is_allowed_and_starts_a_burst() {
        let mut r = rig("free");
        r.set(Some(Signals {
            wakefulness: Some("Dozing".into()),
            ..Signals::default()
        }));
        assert!(matches!(r.lock.admit("s1", "HW1"), Ok(Admitted::Free(_))));
        r.set(Some(awake("com.example.notes", false, 10 * 60_000)));
        assert!(matches!(r.lock.admit("s1", "HW1"), Ok(Admitted::Free(_))));
    }

    #[test]
    fn a_burst_survives_the_agents_own_touches_but_not_anyone_elses() {
        let mut r = rig("burst");
        r.set(Some(awake("com.example.notes", false, 10 * 60_000)));
        assert!(matches!(r.lock.admit("s1", "HW1"), Ok(Admitted::Free(_))));
        // the agent's tap ends at T0+500; the phone dates its last touch then
        r.lock.acted("s1", "HW1", Some(T0 + 500));
        r.at(T0 + 4_000);
        // its own tap opened the app and the keyboard: in use, but only by the agent
        r.set(Some(awake("com.whatsapp", true, 3_500)));
        assert_eq!(r.lock.admit("s1", "HW1"), Ok(Admitted::Burst));
        // a touch 1 s ago, after the agent's last action ended: someone else
        r.set(Some(awake("com.whatsapp", true, 1_000)));
        let e = r.lock.admit("s1", "HW1").unwrap_err();
        assert!(e.contains("touched 1 s ago"), "{e}");
        // a burst is per phone: another phone's burst does not carry over
        r.set(Some(awake("com.whatsapp", true, 3_500)));
        assert!(r.lock.admit("s2", "HW2").is_err());
    }

    #[test]
    fn a_burst_ends_after_the_gap_and_on_a_call_screen() {
        let mut r = rig("gap");
        r.lock.acted("s1", "HW1", Some(T0));
        r.at(T0 + BURST_GAP_MS + 1);
        r.set(Some(awake("com.whatsapp", false, BURST_GAP_MS + 1)));
        assert!(r.lock.admit("s1", "HW1").is_err());
        r.at(T0 + 2_000);
        r.set(Some(awake("com.android.incallui", false, 2_000)));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // ... and with the same signals but no call screen, it would go on
        r.set(Some(awake("com.whatsapp", false, 2_000)));
        assert_eq!(r.lock.admit("s1", "HW1"), Ok(Admitted::Burst));
    }

    #[test]
    fn a_live_lease_under_either_serial_lets_it_through_and_a_no_stands() {
        let mut r = rig("lease");
        r.set(Some(awake("com.anthropic.claude", true, 1_000)));
        let mut m = Memory::default();
        m.leases.insert("0123456789abcdef".into(), T0 + 90_000);
        save_memory(&r.dir, "hw1", &m).unwrap();
        assert_eq!(
            r.lock.admit("192.0.2.9:5555", "HW1"),
            Ok(Admitted::Lease(2))
        );
        // keyed by the adb serial (ask-screen given --device ip:port, serial unreadable)
        save_memory(&r.dir, "192.0.2.10:5555", &m).unwrap();
        assert_eq!(
            r.lock.admit("192.0.2.10:5555", "HW9"),
            Ok(Admitted::Lease(2))
        );
        // the lease runs out: in use again
        r.at(T0 + 90_001);
        assert!(r.lock.admit("192.0.2.9:5555", "HW1").is_err());
        // a No stands for its cool-down even when the phone is put down
        let no = Memory {
            denied_until_ms: T0 + 200_000,
            ..Memory::default()
        };
        save_memory(&r.dir, "hw1", &no).unwrap();
        r.set(Some(Signals {
            wakefulness: Some("Asleep".into()),
            ..Signals::default()
        }));
        assert!(r
            .lock
            .admit("192.0.2.9:5555", "HW1")
            .unwrap_err()
            .contains("said No"));
    }

    #[test]
    fn the_touch_is_dated_from_the_end_of_the_read_and_the_reads_length_is_allowed_for() {
        // a read that takes 800 ms; the phone states "idle" at the start of it
        let dir =
            std::env::temp_dir().join(format!("agentroid-screenlock-slow-{}", std::process::id()));
        let clock = Arc::new(AtomicU64::new(T0));
        let touch = Arc::new(AtomicU64::new(T0));
        let (c1, c2, tt) = (clock.clone(), clock.clone(), touch.clone());
        let mut lock = ScreenLock::new(
            dir.clone(),
            Box::new(move |_| {
                let at = c1.load(Ordering::SeqCst);
                let idle = at - tt.load(Ordering::SeqCst);
                c1.store(at + 800, Ordering::SeqCst);
                Some(awake("com.whatsapp", true, idle))
            }),
            Box::new(move || c2.load(Ordering::SeqCst)),
        );
        // the agent's own action ended at T0 + 1000 and its touch was at T0 + 900
        lock.acted("s1", "HW1", Some(T0 + 1_000));
        touch.store(T0 + 900, Ordering::SeqCst);
        clock.store(T0 + 5_000, Ordering::SeqCst);
        assert_eq!(lock.admit("s1", "HW1"), Ok(Admitted::Burst));
        // someone else at T0 + 1500, 500 ms after the agent's action ended: refused despite the
        // 800 ms read (dated T0 + 2300 at the latest; allowed up to T0 + 1000 + 800 + 300)
        touch.store(T0 + 1_500, Ordering::SeqCst);
        clock.store(T0 + 9_000, Ordering::SeqCst);
        assert!(lock.admit("s1", "HW1").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emulators_are_not_locked() {
        let mut r = rig("emu");
        r.set(Some(awake("com.anthropic.claude", true, 0)));
        assert_eq!(
            r.lock.admit("emulator-5554", "EMULATOR35X1"),
            Ok(Admitted::Emulator)
        );
        // a phone whose hardware serial merely looks like an emulator's is locked
        assert!(r.lock.admit("192.0.2.9:5555", "emulator-5554").is_err());
    }
}
