//! The screen lock: the daemon refuses every screen-changing call unless `ask-screen` allowed it.
//!
//! `ask-screen` (in [`crate::askscreen`]) only asks. A session that skips it could still tap on a
//! phone someone is using. The lock closes that gap. When it is set on a [`crate::transport::Core`],
//! every acting call (tap, long_press, swipe, scroll, type_text, key, unlock, open_app, open_url,
//! start_activity, set_value, the acting steps of flows and jobs), every link segment and every
//! `run_script` shell line that is not on the read-only list goes through [`ScreenLock::admit`]
//! first:
//!
//! 1. **A live lease.** A Yes to `ask-screen` for this phone (any session, any purpose) whose
//!    lease has not run out: allowed. The lease is read from `ask-screen`'s own state file, keyed
//!    by the phone's hardware serial or its adb serial. A call a lease let through never starts
//!    or carries on a burst, and the lease ends any burst from before it.
//! 2. **A No stands.** Inside the 5-minute cool-down after a No (or no answer): refused.
//! 3. **Not in use now.** The phone is read at the moment of the call, with the same rules as
//!    `ask-screen` (screen off or dozing; or on, untouched for 5 minutes, nothing holding it on, no
//!    keyboard, no listed app; and in both cases no call, read whatever the screen shows): allowed,
//!    and a **burst** starts. A call screen counts as a listed app here.
//! 4. **A burst goes on.** The agent's own taps count as touches and its own app or keyboard
//!    shows as in use, so after the first allowed call the phone would read as in use at once. A
//!    burst therefore goes on while the phone's last touch is no later than the end of the
//!    agent's own last action and that action was under [`BURST_GAP_MS`] ago. The phone states
//!    its last touch as "N ms ago" while `dumpsys power` runs, so the lock times that one read,
//!    dates the touch from its end and allows for its length plus [`OWN_SLACK_MS`]. A power read
//!    longer than [`MAX_POWER_READ_MS`] cannot date the touch, so it does not continue a burst. A
//!    touch by anyone else ends the burst, and so does a call.
//! 5. **The agent's own left-overs.** After the gap, when nobody has touched the phone since the
//!    agent's last action and it has been idle for the idle limit, the app and keyboard that
//!    action left on screen do not count (a held screen and a call still do).
//! 6. **Otherwise refused**, with the reasons and the `ask-screen` command to run.
//!
//! The burst and its times run on a monotonic clock, so a step of the wall clock cannot widen it;
//! leases and the No cool-down are on the wall clock, as `ask-screen` writes them.
//!
//! An allowed verdict is kept for [`VERDICT_TTL_MS`] (until the agent next acts), so one call that
//! is checked more than once (a tap with `require` and `verify`) reads the phone once.
//!
//! Emulators are not locked: nobody holds them. An adb name of `emulator-<port>` is not enough (a
//! real phone behind a forwarded port gets one); the device must also say it is one
//! (`ro.kernel.qemu` or `ro.boot.qemu` is 1, or its hardware serial is an emulator's). Nothing that
//! cannot be read is guessed: a phone that cannot be read is refused.

use crate::askscreen::{is_generic_serial, judge, load_memory, Signals, Use, REASK_AFTER_MS};
use agentroid_adb_link::runner::Runner;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A burst ends when the agent has done nothing on the phone for this long.
pub const BURST_GAP_MS: u64 = 5 * 60_000;
/// Allowed on top of the power read's own measured length when the phone's last touch is compared
/// with the end of the agent's own action (the phone's clock steps; the order of its reads).
pub const OWN_SLACK_MS: u64 = 300;
/// The longest `dumpsys power` read that can still date the last touch. A slower one would widen
/// the allowance past what tells the agent's touch from someone else's.
pub const MAX_POWER_READ_MS: u64 = 1_000;
/// The longest the lock's whole read of the phone may take. Past this the phone may have changed
/// since it was read, so the call is refused.
pub const LOCK_READ_BUDGET_MS: u64 = 5_000;
/// How long an allowed verdict is kept for the same phone (cleared when the agent acts).
pub const VERDICT_TTL_MS: u64 = 1_000;
/// A link segment the lock let through because the phone is not in use now (not a lease) is
/// checked once, before it is sent, so it must be short: at most this many steps ...
pub const FREE_SEGMENT_MAX_STEPS: usize = 10;
/// ... and at most this many seconds to live (a segment that asks for longer is refused; one that
/// does not say is given this).
pub const FREE_SEGMENT_MAX_S: u64 = 10;
/// The idle time below which a screen-on phone counts as in use (the `ask-screen` default).
pub const DEFAULT_IDLE_MS: u64 = 5 * 60_000;
/// Apps that mean a ringing or live call: in use when the phone is read free, and the end of a
/// burst, whoever opened them.
pub const CALL_APPS: [&str; 5] = [
    "com.android.incallui",
    "com.samsung.android.incallui",
    "com.google.android.dialer",
    "com.android.dialer",
    "com.android.server.telecom",
];

/// One read of the phone: the signals, and the times on the lock's monotonic clock just before
/// and just after the `dumpsys power` read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub signals: Signals,
    pub power_from: u64,
    pub power_to: u64,
}

/// Reads the phone for an adb serial, timing the power read on the clock it is given
/// (production: [`crate::askscreen::read_signals_timed`] over the guarded adb runner). `None`:
/// could not be read.
pub type SignalReader = Box<dyn FnMut(&str, &dyn Fn() -> u64) -> Option<Reading> + Send>;
/// Milliseconds on a clock (tests pass fakes).
pub type Now = Box<dyn Fn() -> u64 + Send>;
/// Whether the device behind an `emulator-<port>` adb name says it is an emulator. `None`: could
/// not be read (then it is locked).
pub type EmulatorProbe = Box<dyn FnMut(&str) -> Option<bool> + Send>;

/// The lock's monotonic clock, shared with the tickets that stamp when their work ended (outside
/// the daemon's lock, so a ticket handed back late is still dated by its own end).
#[derive(Clone)]
pub struct MonoClock(Arc<Mutex<Now>>);

impl MonoClock {
    pub fn new(now: Now) -> Self {
        MonoClock(Arc::new(Mutex::new(now)))
    }

    /// Milliseconds on the lock's monotonic clock.
    pub fn now(&self) -> u64 {
        let f = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (*f)()
    }
}

impl std::fmt::Debug for MonoClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MonoClock")
    }
}

/// Why a call was let through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admitted {
    /// Emulators are not locked.
    Emulator,
    /// A live `ask-screen` lease, with the ms left on it (on the wall clock).
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
    /// Unix ms: leases and the No cool-down.
    wall: Now,
    /// Monotonic ms: the burst, the read timings and the verdict cache.
    mono: MonoClock,
    probe: EmulatorProbe,
    /// (adb serial, hardware serial) -> what the device said about being an emulator.
    emulators: HashMap<(String, String), bool>,
    /// Per phone: the end of the agent's last admitted action, on the monotonic clock.
    ours: HashMap<String, u64>,
    /// Per phone: the last allowed verdict and when it was reached (monotonic).
    verdicts: HashMap<String, (u64, Admitted)>,
}

/// The key a phone's burst is kept under: its hardware serial, unless that is missing or a junk
/// serial many phones share (then its adb serial).
pub(crate) fn phone_key(hw_serial: &str, adb_serial: &str) -> String {
    if hw_serial.is_empty() || is_generic_serial(hw_serial) {
        adb_serial
    } else {
        hw_serial
    }
    .to_ascii_lowercase()
}

/// An emulator's own hardware serial (`EMULATOR35X1X11X0`). The adb serial is never taken for it.
fn emulator_hw_serial(hw_serial: &str) -> bool {
    hw_serial.len() > "EMULATOR".len()
        && hw_serial.starts_with("EMULATOR")
        && hw_serial.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The production [`EmulatorProbe`]: `ro.kernel.qemu` or `ro.boot.qemu` is 1. `None` when
/// neither could be read.
pub fn probe_emulator(r: &dyn Runner, adb_serial: &str) -> Option<bool> {
    let mut read_any = false;
    for key in ["ro.kernel.qemu", "ro.boot.qemu"] {
        if let Some(v) = crate::askscreen::shell(r, adb_serial, &format!("getprop {key}")) {
            read_any = true;
            if v.trim() == "1" {
                return Some(true);
            }
        }
    }
    read_any.then_some(false)
}

/// The app and keyboard a phone shows, taken away (a call screen stays): what the phone would
/// read as if the agent had not left them there.
fn without_left_overs(s: &Signals) -> Signals {
    let mut t = s.clone();
    if !t.focus.as_deref().is_some_and(|f| CALL_APPS.contains(&f)) {
        t.focus = Some(String::new());
    }
    t.ime_shown = Some(false);
    t
}

impl ScreenLock {
    /// `state_dir` is `ask-screen`'s state folder (where its leases are); `wall` is Unix ms,
    /// `mono` a monotonic clock in ms.
    pub fn new(state_dir: PathBuf, read: SignalReader, wall: Now, mono: Now) -> Self {
        ScreenLock {
            state_dir,
            idle_limit_ms: DEFAULT_IDLE_MS,
            read,
            wall,
            mono: MonoClock::new(mono),
            probe: Box::new(|_| None),
            emulators: HashMap::new(),
            ours: HashMap::new(),
            verdicts: HashMap::new(),
        }
    }

    pub fn with_idle_limit_ms(mut self, ms: u64) -> Self {
        self.idle_limit_ms = ms;
        self
    }

    /// How to ask an `emulator-<port>` device whether it is one (default: it cannot be asked, so
    /// only an emulator's own hardware serial exempts it).
    pub fn with_emulator_probe(mut self, probe: EmulatorProbe) -> Self {
        self.probe = probe;
        self
    }

    pub fn state_dir(&self) -> &std::path::Path {
        &self.state_dir
    }

    /// The lock's monotonic clock, for stamping when admitted work ended.
    pub fn clock(&self) -> MonoClock {
        self.mono.clone()
    }

    /// A real emulator: an `emulator-<port>` adb name, and the device says so. Asked once per
    /// (adb serial, hardware serial); an answer that could not be read is asked again next time,
    /// and so is any answer for a device whose hardware serial could not be read (the daemon then
    /// passes the adb serial, which a different device on the same port would share).
    fn is_emulator(&mut self, adb_serial: &str, hw_serial: &str) -> bool {
        if !adb_serial.starts_with("emulator-") {
            return false;
        }
        if emulator_hw_serial(hw_serial) {
            return true;
        }
        let k = (adb_serial.to_string(), hw_serial.to_string());
        let known = !hw_serial.is_empty()
            && !hw_serial.eq_ignore_ascii_case(adb_serial)
            && !is_generic_serial(hw_serial);
        if known {
            if let Some(&e) = self.emulators.get(&k) {
                return e;
            }
        }
        match (self.probe)(adb_serial) {
            Some(e) => {
                if known {
                    self.emulators.insert(k, e);
                }
                e
            }
            None => false,
        }
    }

    /// May a screen-changing call go to this phone now? `Err` holds the reason, for the refusal.
    pub fn admit(&mut self, adb_serial: &str, hw_serial: &str) -> Result<Admitted, String> {
        if self.is_emulator(adb_serial, hw_serial) {
            return Ok(Admitted::Emulator);
        }
        let t = (self.wall)();
        // ask-screen keys its file by the hardware serial when it can read it (and it is not a
        // junk serial), else by the `--device` text; look under both. A junk hardware serial is
        // never a key, but the adb serial always is, even a junk one: a USB phone whose adb serial
        // is the junk one is asked about (and said No to) under that name, and adb cannot reach two
        // connected devices under one serial
        let mut lease_until = 0;
        let mut denied_until = 0;
        let mut keys: Vec<&str> = vec![];
        if !hw_serial.is_empty() && !is_generic_serial(hw_serial) {
            keys.push(hw_serial);
        }
        if !adb_serial.is_empty() && !keys.iter().any(|k| k.eq_ignore_ascii_case(adb_serial)) {
            keys.push(adb_serial);
        }
        for k in keys {
            let m = load_memory(&self.state_dir, k, t);
            lease_until = m.leases.values().copied().fold(lease_until, u64::max);
            denied_until = denied_until.max(m.denied_until_ms);
        }
        let ask = format!(
            "run `agentroid ask-screen --device {adb_serial} --minutes N --why \"...\"` first (a Yes gives a lease this lock honours)"
        );
        let key = phone_key(hw_serial, adb_serial);
        if lease_until > t {
            // the owner's Yes covers this call; a burst from before the lease does not carry on
            // past it (the caller records no burst for a call a lease let through)
            self.ours.remove(&key);
            self.verdicts.remove(&key);
            return Ok(Admitted::Lease(lease_until - t));
        }
        if t < denied_until {
            return Err(format!(
                "the owner said No to the last ask-screen (or did not answer): no screen-changing call for {} s",
                (denied_until - t).div_ceil(1000).min(REASK_AFTER_MS / 1000)
            ));
        }
        let start = self.mono.now();
        if let Some((at, v)) = self.verdicts.get(&key) {
            if start.saturating_sub(*at) <= VERDICT_TTL_MS {
                return Ok(v.clone());
            }
        }
        let clock = self.mono.clone();
        let reading = (self.read)(adb_serial, &move || clock.now());
        let end = self.mono.now().max(start);
        let Some(Reading {
            signals: s,
            power_from,
            power_to,
        }) = reading
        else {
            return Err(format!(
                "the phone could not be read, so whether it is in use is unknown; {ask}"
            ));
        };
        if end - start > LOCK_READ_BUDGET_MS {
            return Err(format!(
                "the phone took {} ms to read (over {LOCK_READ_BUDGET_MS} ms), so it may have changed since; try again, or {ask}",
                end - start
            ));
        }
        let power_ms = power_to.saturating_sub(power_from);
        // the touch can be dated only from a quick power read: "idle ms ago" at its end
        let dated = power_ms <= MAX_POWER_READ_MS;
        let touched_at = s.idle_ms.map(|idle| power_to.saturating_sub(idle));
        let allowance = power_ms + OWN_SLACK_MS;
        let last = self.ours.get(&key).copied();
        // nobody but the agent has touched the phone since its last action
        let untouched = match (last, touched_at) {
            (Some(l), Some(at)) => dated && at <= l.saturating_add(allowance),
            _ => false,
        };
        let extra: Vec<String> = CALL_APPS.iter().map(|a| a.to_string()).collect();
        let left_over = untouched && s.idle_ms.is_some_and(|i| i >= self.idle_limit_ms);
        let verdict = if left_over {
            judge(&without_left_overs(&s), self.idle_limit_ms, &extra)
        } else {
            judge(&s, self.idle_limit_ms, &extra)
        };
        let mut reasons = match verdict {
            Use::Free(why) => {
                let why = if left_over {
                    format!("{why} (the app and keyboard left by the agent's own last action are not counted)")
                } else {
                    why
                };
                let v = Admitted::Free(why);
                self.verdicts.insert(key, (end, v.clone()));
                return Ok(v);
            }
            Use::InUse(r) => r,
        };
        if let Some(l) = last {
            let call_up = s.in_call != Some(false)
                || s.focus.as_deref().is_some_and(|f| CALL_APPS.contains(&f));
            if s.screen_on() == Some(true) && !call_up {
                if !dated {
                    reasons.push(format!(
                        "the phone took {power_ms} ms to give its last touch (over {MAX_POWER_READ_MS} ms), so the agent's own touches cannot be told from anyone else's"
                    ));
                } else if end < l.saturating_add(BURST_GAP_MS) && untouched {
                    self.verdicts.insert(key, (end, Admitted::Burst));
                    return Ok(Admitted::Burst);
                }
            }
        }
        Err(format!(
            "the phone is in use ({}); {ask}",
            reasons.join(", ")
        ))
    }

    /// The agent's own action on this phone has just ended (it was admitted and it ran).
    pub fn acted(&mut self, adb_serial: &str, hw_serial: &str) {
        self.acted_at(adb_serial, hw_serial, u64::MAX);
    }

    /// The agent's own action on this phone ended at `at` on the lock's monotonic clock (never
    /// later than now): work reported after it ended is dated by its end, not by the report.
    pub fn acted_at(&mut self, adb_serial: &str, hw_serial: &str, at: u64) {
        let at = at.min(self.mono.now());
        let key = phone_key(hw_serial, adb_serial);
        // what it did may have changed the phone: the next call reads it again
        self.verdicts.remove(&key);
        let e = self.ours.entry(key).or_insert(0);
        *e = (*e).max(at);
    }

    /// Work was let through for this phone (it may change it): drop the kept verdict, so the next
    /// call reads the phone again whether the work runs or not.
    pub fn forget(&mut self, adb_serial: &str, hw_serial: &str) {
        self.verdicts.remove(&phone_key(hw_serial, adb_serial));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::askscreen::{save_memory, Memory};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
            in_call: Some(false),
        }
    }

    fn asleep() -> Signals {
        Signals {
            wakefulness: Some("Asleep".into()),
            in_call: Some(false),
            ..Signals::default()
        }
    }

    /// The wall clock and the monotonic clock move together unless a test steps one alone.
    struct Rig {
        lock: ScreenLock,
        wall: Arc<AtomicU64>,
        mono: Arc<AtomicU64>,
        phone: Arc<Mutex<Option<Signals>>>,
        reads: Arc<AtomicUsize>,
        dir: PathBuf,
    }

    fn rig(name: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!(
            "agentroid-screenlock-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let wall = Arc::new(AtomicU64::new(T0));
        let mono = Arc::new(AtomicU64::new(T0));
        let phone = Arc::new(Mutex::new(None));
        let reads = Arc::new(AtomicUsize::new(0));
        let (w, m, p, n) = (wall.clone(), mono.clone(), phone.clone(), reads.clone());
        let lock = ScreenLock::new(
            dir.clone(),
            Box::new(move |_, clock| {
                n.fetch_add(1, Ordering::SeqCst);
                let at = clock();
                p.lock().unwrap().clone().map(|s| Reading {
                    signals: s,
                    power_from: at,
                    power_to: at,
                })
            }),
            Box::new(move || w.load(Ordering::SeqCst)),
            Box::new(move || m.load(Ordering::SeqCst)),
        );
        Rig {
            lock,
            wall,
            mono,
            phone,
            reads,
            dir,
        }
    }

    impl Rig {
        fn set(&self, s: Option<Signals>) {
            *self.phone.lock().unwrap() = s;
        }
        /// Both clocks to `t` (the monotonic one counts from T0 too, for readable tests).
        fn at(&self, t: u64) {
            self.wall.store(t, Ordering::SeqCst);
            self.mono.store(t, Ordering::SeqCst);
        }
        fn acted_at(&mut self, t: u64) {
            self.at(t);
            self.lock.acted("s1", "HW1");
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
            in_call: Some(false),
            ..Signals::default()
        }));
        assert!(matches!(r.lock.admit("s1", "HW1"), Ok(Admitted::Free(_))));
        r.at(T0 + 2 * VERDICT_TTL_MS);
        r.set(Some(awake("com.example.notes", false, 10 * 60_000)));
        assert!(matches!(r.lock.admit("s1", "HW1"), Ok(Admitted::Free(_))));
    }

    #[test]
    fn a_call_is_never_free_and_an_unreadable_call_state_is_not_free_either() {
        let mut r = rig("call");
        // screen off, in a call (at an ear): in use
        r.set(Some(Signals {
            in_call: Some(true),
            ..asleep()
        }));
        let e = r.lock.admit("s1", "HW1").unwrap_err();
        assert!(e.contains("a call is in progress"), "{e}");
        // screen off, the call state could not be read: in use
        r.set(Some(Signals {
            in_call: None,
            ..asleep()
        }));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // screen on, untouched for 10 minutes, the dialer up but the call state reads idle: the
        // call screen alone is in use
        r.set(Some(awake("com.google.android.dialer", false, 10 * 60_000)));
        let e = r.lock.admit("s1", "HW1").unwrap_err();
        assert!(e.contains("com.google.android.dialer is open"), "{e}");
        // and with no call at all it is free
        r.set(Some(asleep()));
        assert!(matches!(r.lock.admit("s1", "HW1"), Ok(Admitted::Free(_))));
    }

    #[test]
    fn a_burst_survives_the_agents_own_touches_but_not_anyone_elses() {
        let mut r = rig("burst");
        r.set(Some(awake("com.example.notes", false, 10 * 60_000)));
        assert!(matches!(r.lock.admit("s1", "HW1"), Ok(Admitted::Free(_))));
        // the agent's tap ends at T0+500; the phone dates its last touch then
        r.acted_at(T0 + 500);
        r.at(T0 + 4_000);
        // its own tap opened the app and the keyboard: in use, but only by the agent
        r.set(Some(awake("com.whatsapp", true, 3_500)));
        assert_eq!(r.lock.admit("s1", "HW1"), Ok(Admitted::Burst));
        // a touch 1 s ago, after the agent's last action ended: someone else
        r.at(T0 + 4_000 + VERDICT_TTL_MS + 1);
        r.set(Some(awake("com.whatsapp", true, 1_000)));
        let e = r.lock.admit("s1", "HW1").unwrap_err();
        assert!(e.contains("touched 1 s ago"), "{e}");
        // a burst is per phone: another phone's burst does not carry over
        r.set(Some(awake("com.whatsapp", true, 3_500)));
        assert!(r.lock.admit("s2", "HW2").is_err());
    }

    #[test]
    fn a_burst_ends_after_the_gap_and_on_a_call() {
        let mut r = rig("gap");
        r.acted_at(T0);
        r.at(T0 + BURST_GAP_MS + 1);
        // held on (so the agent's left-overs rule does not apply): the gap alone ends the burst
        let mut held = awake("com.whatsapp", false, BURST_GAP_MS + 1);
        held.screen_kept_on = Some(true);
        r.set(Some(held));
        assert!(r.lock.admit("s1", "HW1").is_err());
        r.at(T0 + 2_000);
        r.set(Some(awake("com.android.incallui", false, 2_000)));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // a call the registry reports, with an ordinary app in front
        let mut ringing = awake("com.whatsapp", false, 2_000);
        ringing.in_call = Some(true);
        r.set(Some(ringing));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // ... and the call state unknown
        let mut unknown = awake("com.whatsapp", false, 2_000);
        unknown.in_call = None;
        r.set(Some(unknown));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // ... and with the same signals but no call, it would go on
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
            Ok(Admitted::Lease(90_000))
        );
        // keyed by the adb serial (ask-screen given --device ip:port, serial unreadable)
        save_memory(&r.dir, "192.0.2.10:5555", &m).unwrap();
        assert_eq!(
            r.lock.admit("192.0.2.10:5555", "HW9"),
            Ok(Admitted::Lease(90_000))
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
        r.set(Some(asleep()));
        assert!(r
            .lock
            .admit("192.0.2.9:5555", "HW1")
            .unwrap_err()
            .contains("said No"));
    }

    #[test]
    fn a_junk_hardware_serial_is_never_a_key_for_leases_or_bursts() {
        let mut r = rig("junk");
        r.set(Some(awake("com.anthropic.claude", true, 1_000)));
        // a lease saved under the junk serial does not open another phone that shares it
        let mut m = Memory::default();
        m.leases.insert("0123456789abcdef".into(), T0 + 90_000);
        save_memory(&r.dir, "0123456789ABCDEF", &m).unwrap();
        assert!(r.lock.admit("192.0.2.9:5555", "0123456789ABCDEF").is_err());
        // and a burst on one phone with the junk serial does not carry over to another
        r.lock.acted("192.0.2.9:5555", "0123456789ABCDEF");
        r.at(T0 + 2_000);
        r.set(Some(awake("com.whatsapp", true, 2_000)));
        assert!(r.lock.admit("192.0.2.11:5555", "0123456789ABCDEF").is_err());
        assert_eq!(
            r.lock.admit("192.0.2.9:5555", "0123456789ABCDEF"),
            Ok(Admitted::Burst)
        );
    }

    /// A lock whose reader takes `before` ms, then a power read of `power` ms during which the
    /// phone states its idle time at the start of the power read.
    fn slow(dir: &str, before: u64, power: u64) -> (ScreenLock, Arc<AtomicU64>, Arc<AtomicU64>) {
        let dir =
            std::env::temp_dir().join(format!("agentroid-screenlock-{dir}-{}", std::process::id()));
        let clock = Arc::new(AtomicU64::new(T0));
        let touch = Arc::new(AtomicU64::new(T0));
        let (c1, c2, c3, tt) = (clock.clone(), clock.clone(), clock.clone(), touch.clone());
        let lock = ScreenLock::new(
            dir,
            Box::new(move |_, mono| {
                c1.fetch_add(before, Ordering::SeqCst);
                let from = mono();
                let idle = c1.load(Ordering::SeqCst) - tt.load(Ordering::SeqCst);
                c1.fetch_add(power, Ordering::SeqCst);
                let to = mono();
                Some(Reading {
                    signals: awake("com.whatsapp", true, idle),
                    power_from: from,
                    power_to: to,
                })
            }),
            Box::new(move || c2.load(Ordering::SeqCst)),
            Box::new(move || c3.load(Ordering::SeqCst)),
        );
        (lock, clock, touch)
    }

    #[test]
    fn the_touch_is_dated_from_the_end_of_the_power_read_and_its_length_is_allowed_for() {
        // a power read that takes 800 ms; the phone states "idle" at the start of it
        let (mut lock, clock, touch) = slow("slow", 0, 800);
        // the agent's own action ended at T0 + 1000 and its touch was at T0 + 900
        clock.store(T0 + 1_000, Ordering::SeqCst);
        lock.acted("s1", "HW1");
        touch.store(T0 + 900, Ordering::SeqCst);
        clock.store(T0 + 5_000, Ordering::SeqCst);
        assert_eq!(lock.admit("s1", "HW1"), Ok(Admitted::Burst));
        // someone else at T0 + 1500, 500 ms after the agent's action ended: refused despite the
        // 800 ms read (dated T0 + 2300 at the latest; allowed up to T0 + 1000 + 800 + 300)
        touch.store(T0 + 1_500, Ordering::SeqCst);
        clock.store(T0 + 9_000, Ordering::SeqCst);
        assert!(lock.admit("s1", "HW1").is_err());
    }

    #[test]
    fn time_spent_before_the_power_read_is_not_allowed_for() {
        // 3 s go before the power read (a slow adb connection); the power read itself is quick
        let (mut lock, clock, touch) = slow("before", 3_000, 10);
        clock.store(T0 + 1_000, Ordering::SeqCst);
        lock.acted("s1", "HW1");
        // someone touches 1 s after the agent's action ended: with the whole read allowed for,
        // that would be inside a 3.3 s allowance; with only the power read, it is not
        touch.store(T0 + 2_000, Ordering::SeqCst);
        clock.store(T0 + 5_000, Ordering::SeqCst);
        assert!(lock.admit("s1", "HW1").is_err());
        // the agent's own touch is still told apart
        touch.store(T0 + 950, Ordering::SeqCst);
        clock.store(T0 + 20_000, Ordering::SeqCst);
        assert_eq!(lock.admit("s1", "HW1"), Ok(Admitted::Burst));
    }

    #[test]
    fn a_slow_power_read_cannot_continue_a_burst_and_a_slow_read_is_refused() {
        let (mut lock, clock, touch) = slow("slowpower", 0, MAX_POWER_READ_MS + 500);
        clock.store(T0 + 1_000, Ordering::SeqCst);
        lock.acted("s1", "HW1");
        touch.store(T0 + 900, Ordering::SeqCst);
        clock.store(T0 + 5_000, Ordering::SeqCst);
        let e = lock.admit("s1", "HW1").unwrap_err();
        assert!(e.contains("cannot be told"), "{e}");
        // the whole read over its budget: refused, whatever it says
        let (mut lock, clock, touch) = slow("slowread", LOCK_READ_BUDGET_MS, 10);
        clock.store(T0 + 1_000, Ordering::SeqCst);
        lock.acted("s1", "HW1");
        touch.store(T0 + 900, Ordering::SeqCst);
        clock.store(T0 + 5_000, Ordering::SeqCst);
        let e = lock.admit("s1", "HW1").unwrap_err();
        assert!(e.contains("to read"), "{e}");
    }

    #[test]
    fn a_wall_clock_step_back_does_not_widen_the_burst() {
        let mut r = rig("wallstep");
        r.acted_at(T0);
        // the wall clock is set back 10 minutes while the monotonic clock moves past the gap
        r.wall.store(T0 - 10 * 60_000, Ordering::SeqCst);
        r.mono.store(T0 + BURST_GAP_MS + 1_000, Ordering::SeqCst);
        // held on, untouched since the agent's action: only the burst could let it through
        let mut held = awake("com.whatsapp", false, BURST_GAP_MS + 2_000);
        held.screen_kept_on = Some(true);
        r.set(Some(held));
        assert!(r.lock.admit("s1", "HW1").is_err());
    }

    #[test]
    fn an_allowed_verdict_is_kept_for_a_second_and_cleared_when_the_agent_acts() {
        let mut r = rig("cache");
        r.set(Some(asleep()));
        assert!(matches!(r.lock.admit("s1", "HW1"), Ok(Admitted::Free(_))));
        // a tap with require and verify checks three times: one read
        r.at(T0 + 300);
        assert!(r.lock.admit("s1", "HW1").is_ok());
        assert!(r.lock.admit("s1", "HW1").is_ok());
        assert_eq!(r.reads.load(Ordering::SeqCst), 1);
        // after a second it reads again
        r.at(T0 + VERDICT_TTL_MS + 1);
        assert!(r.lock.admit("s1", "HW1").is_ok());
        assert_eq!(r.reads.load(Ordering::SeqCst), 2);
        // once the agent has acted, the next check reads again: someone else touched it after
        // the agent's action, so it is refused, not given the cached Free
        r.lock.acted("s1", "HW1");
        r.at(T0 + VERDICT_TTL_MS + 500);
        r.set(Some(awake("com.anthropic.claude", true, 100)));
        assert!(r.lock.admit("s1", "HW1").is_err());
        assert_eq!(r.reads.load(Ordering::SeqCst), 3);
        // a refusal is never kept
        assert!(r.lock.admit("s1", "HW1").is_err());
        assert_eq!(r.reads.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn after_the_gap_the_agents_own_left_over_app_and_keyboard_do_not_lock_it_out() {
        let mut r = rig("leftover");
        r.acted_at(T0);
        // the agent left WhatsApp open with the keyboard up; nobody has touched it since
        r.at(T0 + BURST_GAP_MS + 60_000);
        r.set(Some(awake("com.whatsapp", true, BURST_GAP_MS + 60_000)));
        match r.lock.admit("s1", "HW1") {
            Ok(Admitted::Free(why)) => assert!(why.contains("left by the agent"), "{why}"),
            other => panic!("{other:?}"),
        }
        // a held screen still counts
        r.at(T0 + BURST_GAP_MS + 62_000);
        let mut held = awake("com.whatsapp", true, BURST_GAP_MS + 62_000);
        held.screen_kept_on = Some(true);
        r.set(Some(held));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // so does a call
        r.at(T0 + BURST_GAP_MS + 64_000);
        let mut call = awake("com.whatsapp", true, BURST_GAP_MS + 64_000);
        call.in_call = Some(true);
        r.set(Some(call));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // and someone who touched it since the agent's action (then left it for 5 minutes) has
        // their app and keyboard counted
        r.at(T0 + 2 * BURST_GAP_MS + 66_000);
        r.set(Some(awake("com.whatsapp", true, BURST_GAP_MS + 1_000)));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // a phone the agent never acted on: its app counts
        r.set(Some(awake("com.whatsapp", false, 10 * BURST_GAP_MS)));
        assert!(r.lock.admit("s2", "HW2").is_err());
    }

    #[test]
    fn emulators_are_not_locked_but_a_real_phone_behind_a_forwarded_port_is() {
        let mut r = rig("emu");
        r.set(Some(awake("com.anthropic.claude", true, 0)));
        // an emulator's own hardware serial
        assert_eq!(
            r.lock.admit("emulator-5554", "EMULATOR35X1X11X0"),
            Ok(Admitted::Emulator)
        );
        // a phone whose hardware serial merely looks like an emulator's is locked
        assert!(r.lock.admit("192.0.2.9:5555", "emulator-5554").is_err());
        // a real phone forwarded to a local port shows up as emulator-5554: the probe cannot be
        // read (default) -> locked; it says no -> locked; asked once per phone
        assert!(r.lock.admit("emulator-5554", "R5CX12345AB").is_err());
        assert!(r.lock.admit("emulator-5554", "emulator-5554").is_err());
        let asked = Arc::new(AtomicUsize::new(0));
        let (a, answer) = (asked.clone(), Arc::new(Mutex::new(Some(false))));
        let ans = answer.clone();
        let mut lock = std::mem::replace(
            &mut r.lock,
            ScreenLock::new(
                PathBuf::new(),
                Box::new(|_, _| None),
                Box::new(|| 0),
                Box::new(|| 0),
            ),
        )
        .with_emulator_probe(Box::new(move |_| {
            a.fetch_add(1, Ordering::SeqCst);
            *ans.lock().unwrap()
        }));
        assert!(lock.admit("emulator-5554", "R5CX12345AB").is_err());
        assert!(lock.admit("emulator-5554", "R5CX12345AB").is_err());
        assert_eq!(asked.load(Ordering::SeqCst), 1, "read once and kept");
        // a real emulator (qemu) is not locked
        *answer.lock().unwrap() = Some(true);
        assert_eq!(
            lock.admit("emulator-5556", "ABC123"),
            Ok(Admitted::Emulator)
        );
        // an answer that could not be read is not kept, and locks
        *answer.lock().unwrap() = None;
        assert!(lock.admit("emulator-5558", "R5CX9").is_err());
        assert!(lock.admit("emulator-5558", "R5CX9").is_err());
        assert_eq!(asked.load(Ordering::SeqCst), 4);
    }

    /// Put an emulator probe on the rig's lock that answers `answer` and counts its questions.
    fn probed(r: &mut Rig, answer: &Arc<Mutex<Option<bool>>>) -> Arc<AtomicUsize> {
        let asked = Arc::new(AtomicUsize::new(0));
        let (a, ans) = (asked.clone(), answer.clone());
        let lock = std::mem::replace(
            &mut r.lock,
            ScreenLock::new(
                PathBuf::new(),
                Box::new(|_, _| None),
                Box::new(|| 0),
                Box::new(|| 0),
            ),
        );
        r.lock = lock.with_emulator_probe(Box::new(move |_| {
            a.fetch_add(1, Ordering::SeqCst);
            *ans.lock().unwrap()
        }));
        asked
    }

    #[test]
    fn only_an_emulator_adb_name_can_be_exempt_whatever_the_device_says() {
        let mut r = rig("emuname");
        r.set(Some(awake("com.anthropic.claude", true, 0)));
        let answer = Arc::new(Mutex::new(Some(true)));
        let asked = probed(&mut r, &answer);
        // an emulator's hardware serial, or qemu=1, behind a name that is not `emulator-<port>`:
        // locked, and the device is not even asked
        assert!(r.lock.admit("192.0.2.9:5555", "EMULATOR35X1X11X0").is_err());
        assert!(r.lock.admit("R5CX12345AB", "R5CX12345AB").is_err());
        assert!(r
            .lock
            .admit("adb-R5CX1-x._adb-tls-connect._tcp", "R5CX1")
            .is_err());
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        // the positive control: the same answers behind an emulator name exempt it
        assert_eq!(
            r.lock.admit("emulator-5554", "EMULATOR35X1X11X0"),
            Ok(Admitted::Emulator)
        );
        assert_eq!(
            r.lock.admit("emulator-5556", "R5CX12345AB"),
            Ok(Admitted::Emulator)
        );
    }

    #[test]
    fn a_probe_answer_is_not_kept_for_a_device_whose_hardware_serial_was_not_read() {
        let mut r = rig("emucache");
        r.set(Some(awake("com.anthropic.claude", true, 0)));
        let answer = Arc::new(Mutex::new(Some(true)));
        let asked = probed(&mut r, &answer);
        // the daemon passes the adb serial (or nothing) when it could not read ro.serialno: an
        // emulator now, maybe a forwarded real phone on the same port later
        for hw in ["emulator-5554", "EMULATOR-5554", "", "0123456789ABCDEF"] {
            *answer.lock().unwrap() = Some(true);
            assert_eq!(
                r.lock.admit("emulator-5554", hw),
                Ok(Admitted::Emulator),
                "{hw:?}"
            );
            *answer.lock().unwrap() = Some(false);
            assert!(r.lock.admit("emulator-5554", hw).is_err(), "{hw:?}");
        }
        assert_eq!(asked.load(Ordering::SeqCst), 8);
        // the positive control: a hardware serial that was read keeps its answer
        *answer.lock().unwrap() = Some(true);
        assert_eq!(
            r.lock.admit("emulator-5554", "ABC123"),
            Ok(Admitted::Emulator)
        );
        *answer.lock().unwrap() = Some(false);
        assert_eq!(
            r.lock.admit("emulator-5554", "ABC123"),
            Ok(Admitted::Emulator)
        );
        assert_eq!(asked.load(Ordering::SeqCst), 9);
    }

    #[test]
    fn a_lease_ends_the_burst_it_interrupts() {
        // the positive control first: without a lease the agent's own touch carries the burst on
        let mut r = rig("leaseburst0");
        r.acted_at(T0);
        r.at(T0 + 120_000);
        r.set(Some(awake("com.whatsapp", true, 120_000)));
        assert_eq!(r.lock.admit("s1", "HW1"), Ok(Admitted::Burst));
        // the review's probe: a burst at T0, then a lease to T0 + 60 s used at T0 + 59 s; at
        // T0 + 120 s, with an app open, the keyboard up and the agent's touch the last one, the
        // lease is over and so is the burst
        let mut r = rig("leaseburst");
        r.acted_at(T0);
        let mut m = Memory::default();
        m.leases.insert("0123456789abcdef".into(), T0 + 60_000);
        save_memory(&r.dir, "hw1", &m).unwrap();
        r.at(T0 + 59_000);
        r.set(Some(awake("com.whatsapp", true, 59_000)));
        assert_eq!(r.lock.admit("s1", "HW1"), Ok(Admitted::Lease(1_000)));
        r.at(T0 + 120_000);
        r.set(Some(awake("com.whatsapp", true, 120_000)));
        let e = r.lock.admit("s1", "HW1").unwrap_err();
        assert!(e.contains("in use"), "{e}");
    }

    #[test]
    fn a_slow_power_read_cannot_strip_the_agents_left_overs_either() {
        // the positive control: a quick power read after the gap, untouched since the agent's
        // action: its left-over app and keyboard are not counted
        let (mut lock, clock, touch) = slow("leftquick", 0, 10);
        clock.store(T0 + 1_000, Ordering::SeqCst);
        lock.acted("s1", "HW1");
        touch.store(T0 + 900, Ordering::SeqCst);
        clock.store(T0 + BURST_GAP_MS + 60_000, Ordering::SeqCst);
        match lock.admit("s1", "HW1") {
            Ok(Admitted::Free(why)) => assert!(why.contains("left by the agent"), "{why}"),
            other => panic!("{other:?}"),
        }
        // the same with a power read too slow to date the touch: counted, so in use
        let (mut lock, clock, touch) = slow("leftslow", 0, MAX_POWER_READ_MS + 500);
        clock.store(T0 + 1_000, Ordering::SeqCst);
        lock.acted("s1", "HW1");
        touch.store(T0 + 900, Ordering::SeqCst);
        clock.store(T0 + BURST_GAP_MS + 60_000, Ordering::SeqCst);
        let e = lock.admit("s1", "HW1").unwrap_err();
        assert!(e.contains("in use"), "{e}");
    }

    #[test]
    fn a_no_under_a_junk_adb_serial_stands_for_the_usb_phone_with_that_serial() {
        // the review's probe: a USB phone whose adb serial and hardware serial are both the junk
        // one; ask-screen kept its state under --device, the adb serial
        let mut r = rig("junkno");
        let no = Memory {
            denied_until_ms: T0 + 200_000,
            ..Memory::default()
        };
        save_memory(&r.dir, "0123456789ABCDEF", &no).unwrap();
        r.set(Some(asleep()));
        let e = r
            .lock
            .admit("0123456789ABCDEF", "0123456789ABCDEF")
            .unwrap_err();
        assert!(e.contains("said No"), "{e}");
        // the positive control: a phone reached under another name that shares only the junk
        // hardware serial is not held to it
        assert!(matches!(
            r.lock.admit("192.0.2.9:5555", "0123456789ABCDEF"),
            Ok(Admitted::Free(_))
        ));
        // and a lease kept under the junk adb serial lets that phone through
        let mut m = Memory::default();
        m.leases.insert("0123456789abcdef".into(), T0 + 90_000);
        save_memory(&r.dir, "0123456789ABCDEF", &m).unwrap();
        r.set(Some(awake("com.anthropic.claude", true, 1_000)));
        assert_eq!(
            r.lock.admit("0123456789ABCDEF", "0123456789ABCDEF"),
            Ok(Admitted::Lease(90_000))
        );
    }

    #[test]
    fn a_late_report_is_dated_by_the_end_of_the_work_and_never_ahead_of_now() {
        let mut r = rig("actedat");
        // the work ended at T0 + 2 s and was reported at T0 + 20 s; a touch at T0 + 10 s is not
        // the agent's
        r.at(T0 + 20_000);
        r.lock.acted_at("s1", "HW1", T0 + 2_000);
        r.at(T0 + 21_000);
        r.set(Some(awake("com.whatsapp", true, 11_000)));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // a stamp ahead of the clock counts as now: a touch 1 s from now is not covered
        let mut r = rig("actedat2");
        r.lock.acted_at("s1", "HW1", T0 + 60_000);
        r.at(T0 + 2_000);
        r.set(Some(awake("com.whatsapp", true, 1_000)));
        assert!(r.lock.admit("s1", "HW1").is_err());
        // the positive control: the agent's own touch at the end of the work goes on as a burst
        let mut r = rig("actedat3");
        r.at(T0 + 20_000);
        r.lock.acted_at("s1", "HW1", T0 + 2_000);
        r.at(T0 + 21_000);
        r.set(Some(awake("com.whatsapp", true, 19_000)));
        assert_eq!(r.lock.admit("s1", "HW1"), Ok(Admitted::Burst));
    }

    #[test]
    fn the_probe_reads_the_qemu_properties() {
        use agentroid_adb_link::runner::{RunError, RunOutput};
        struct P(&'static str, &'static str);
        impl Runner for P {
            fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
                let v = match a.last().map(String::as_str) {
                    Some("getprop ro.kernel.qemu") => self.0,
                    Some("getprop ro.boot.qemu") => self.1,
                    _ => return Err(RunError::Refused("no".into())),
                };
                Ok(RunOutput {
                    code: Some(0),
                    stdout: format!("{v}\n"),
                    stderr: String::new(),
                })
            }
        }
        assert_eq!(probe_emulator(&P("1", ""), "emulator-5554"), Some(true));
        assert_eq!(probe_emulator(&P("", "1"), "emulator-5554"), Some(true));
        assert_eq!(probe_emulator(&P("", ""), "emulator-5554"), Some(false));
        assert_eq!(probe_emulator(&P("0", "0"), "emulator-5554"), Some(false));
    }
}
