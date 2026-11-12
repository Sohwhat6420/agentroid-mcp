//! Session stay-awake (12 Nov 2026, issue draft 05): while a client session is working a phone, the
//! screen does not time out; when the last session leaves, the phone's own values come back.
//!
//! * **When.** A session's first `device.call` on a `full` device leases two settings through
//!   care's [`LeaseManager`], scope `session`: `system screen_off_timeout` (the working timeout, so it
//!   holds on battery too) and `global stay_on_while_plugged_in` = 7 (what `svc power stayon true`
//!   sets: on while on AC, USB or wireless power).
//! * **Restore.** When the last session that touched the phone disconnects, has been idle for
//!   `idle_ms`, or the daemon stops. The restore is care's compare-and-swap: a value the user changed
//!   meanwhile is left alone. A later call from a session puts the lease back.
//! * **Crash-safe.** The record is in `leases.json` before the phone is touched. A daemon that died
//!   leaves records whose owner pid is dead; the first touch of that phone by the next daemon
//!   restores them (`on_reconnect`) before anything new is leased.
//! * **Off the call path.** A hold or restore talks to the phone (about 18 adb round trips for the
//!   first hold; 8.7 s on the protected phone over Wi-Fi), so the daemon queues them on one worker thread
//!   ([`SessionAwake::start_worker`]) and the call goes on at once. One queue keeps them in order:
//!   a session's hold is always handled before its own restore.
//! * **The protected phone (its one exception, its owner's choice on 12 Nov 2026).** Keep its screen on while a
//!   session works, screen timeout and stay-on only, put back after each session. The protected phone stays `read_only`; only these two keys may
//!   be leased and restored on it, through a runner built with
//!   `GuardedRunner::with_s24_screen_awake` and a lease manager with `allow_s24_screen_awake`.
//!   Every other command to it is refused as before. `AGENTROID_NO_S24_AWAKE=1` turns this part off.
//! * **Never.** Other `read_only` devices and `helper` devices (care refuses every lease scope
//!   there); lock, PIN, password or keyguard settings (only the two keys above are ever written);
//!   emulators (so benchmark runs stay as measured). `AGENTROID_NO_STAY_AWAKE=1` turns it all off.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use agentroid_adb_link::guard::GuardedRunner;
use agentroid_core::gate::{is_s24, DeviceMode};
use care::clock::CareClock;
use care::leases::LeaseStore;
use care::phone::AdbPhone;
use care::pid::PidProbe;
use care::settings::{LeaseManager, Owner, RestoreStatus, ScopeReq};
use care::thresholds::THRESHOLDS;

use crate::jobcare::CareBuffer;
use crate::jobs::DynRunner;

/// The `job_id` of every stay-awake lease (they belong to no job).
pub const AWAKE_JOB: &str = "session-awake";
/// A phone no session has called for this long gets its own values back.
pub const DEFAULT_IDLE_MS: u64 = 15 * 60 * 1000;

/// The settings leased while a session works a phone, and nothing else.
pub fn bundle() -> [(&'static str, &'static str, String); 2] {
    [
        (
            "system",
            "screen_off_timeout",
            THRESHOLDS.working_screen_timeout_ms.to_string(),
        ),
        ("global", "stay_on_while_plugged_in", "7".to_string()),
    ]
}

/// What stay-awake needs: adb (put behind the command guard here) and the shared lease store.
pub struct AwakeConfig {
    pub adb: DynRunner,
    /// The protected phone exception: the protected phone gets the two keys too (and nothing else).
    pub s24: bool,
    pub leases: PathBuf,
    pub clock: Arc<dyn CareClock>,
    pub pids: Arc<dyn PidProbe>,
    pub idle_ms: u64,
}

struct Dev {
    serial: String,
    mode: DeviceMode,
    /// connection id -> last call (unix ms).
    conns: HashMap<u64, u64>,
    held: bool,
}

/// One per daemon.
pub struct SessionAwake {
    cfg: AwakeConfig,
    runner: GuardedRunner<DynRunner>,
    mgr: Mutex<LeaseManager<CareBuffer>>,
    buffer: CareBuffer,
    devs: Mutex<HashMap<String, Dev>>,
    /// Phones whose leftover leases (dead owners) this daemon has already restored.
    swept: Mutex<HashSet<String>>,
    /// What it did, newest last (bridged.log takes it; tests read it).
    notes: Mutex<Vec<String>>,
    /// The worker's queue and thread, once [`Self::start_worker`] ran.
    queue: Mutex<Option<mpsc::Sender<Op>>>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// One queued piece of work for the worker, in arrival order.
enum Op {
    Touch(u64, String, String, DeviceMode),
    Leave(u64),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The owner session of a phone's stay-awake leases: one per phone, however many sessions share it.
fn session_of(key: &str) -> String {
    format!("awake:{key}")
}

impl SessionAwake {
    pub fn new(cfg: AwakeConfig) -> SessionAwake {
        let buffer = CareBuffer::default();
        let mut mgr = LeaseManager::new(
            LeaseStore::new(&cfg.leases),
            buffer.clone(),
            cfg.clock.clone(),
            cfg.pids.clone(),
        );
        // the exception needs both: the guard's exact lines and care's two-key exemption
        let runner = if cfg.s24 {
            mgr.allow_s24_screen_awake();
            GuardedRunner::with_s24_screen_awake(cfg.adb.clone())
        } else {
            GuardedRunner::new(cfg.adb.clone())
        };
        SessionAwake {
            cfg,
            runner,
            mgr: Mutex::new(mgr),
            buffer,
            devs: Mutex::new(HashMap::new()),
            swept: Mutex::new(HashSet::new()),
            notes: Mutex::new(Vec::new()),
            queue: Mutex::new(None),
            worker: Mutex::new(None),
        }
    }

    /// Start the one worker thread that runs queued holds and restores in order. It holds only a
    /// weak reference, so it ends when the daemon drops its `SessionAwake` or calls [`Self::shutdown`].
    pub fn start_worker(self: &Arc<Self>) {
        let (tx, rx) = mpsc::channel::<Op>();
        let me = Arc::downgrade(self);
        let h = std::thread::spawn(move || {
            for op in rx {
                let Some(a) = me.upgrade() else { return };
                match op {
                    Op::Touch(conn, key, serial, mode) => a.touch(conn, &key, &serial, mode),
                    Op::Leave(conn) => a.leave(conn),
                }
            }
        });
        *lock(&self.queue) = Some(tx);
        *lock(&self.worker) = Some(h);
    }

    /// [`Self::touch`] on the worker when it runs (the daemon), else here (tests).
    pub fn touch_later(&self, conn: u64, key: &str, serial: &str, mode: DeviceMode) {
        if let Some(tx) = lock(&self.queue).as_ref() {
            if tx
                .send(Op::Touch(conn, key.into(), serial.into(), mode))
                .is_ok()
            {
                return;
            }
        }
        self.touch(conn, key, serial, mode);
    }

    /// [`Self::leave`] on the worker when it runs, else here.
    pub fn leave_later(&self, conn: u64) {
        if let Some(tx) = lock(&self.queue).as_ref() {
            if tx.send(Op::Leave(conn)).is_ok() {
                return;
            }
        }
        self.leave(conn);
    }

    /// Let the worker finish what is queued, then stop it.
    fn stop_worker(&self) {
        lock(&self.queue).take();
        let h = lock(&self.worker).take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }

    pub fn leases_path(&self) -> &std::path::Path {
        &self.cfg.leases
    }

    fn note(&self, s: String) {
        let mut n = lock(&self.notes);
        n.push(s);
        let len = n.len();
        if len > 200 {
            n.drain(..len - 200);
        }
    }

    /// Notes since the last call.
    pub fn take_notes(&self) -> Vec<String> {
        let _ = self.buffer.drain(); // care's journal events have no reader outside a job
        std::mem::take(&mut *lock(&self.notes))
    }

    /// True when the protected phone exception is on and this is the protected phone (by hardware serial or adb serial).
    pub fn covers_s24(&self, key: &str, serial: &str) -> bool {
        self.cfg.s24 && (is_s24(key) || is_s24(serial))
    }

    /// A session's call reached a phone. `key` is the device key (hardware serial), `serial` the adb serial.
    pub fn touch(&self, conn: u64, key: &str, serial: &str, mode: DeviceMode) {
        let wanted = match mode {
            DeviceMode::Full => true,
            DeviceMode::ReadOnly => self.covers_s24(key, serial),
            DeviceMode::Helper => false,
        };
        if !wanted || serial.starts_with("emulator-") {
            return;
        }
        let now = self.cfg.clock.unix_ms();
        let mut devs = lock(&self.devs);
        let d = devs.entry(key.to_string()).or_insert_with(|| Dev {
            serial: serial.to_string(),
            mode,
            conns: HashMap::new(),
            held: false,
        });
        d.serial = serial.to_string();
        d.mode = mode;
        d.conns.insert(conn, now);
        if d.held {
            return;
        }
        d.held = true; // one attempt per hold, success or not: a refusal is not retried on every call
        let phone = AdbPhone::new(&self.runner, serial.to_string());
        let mut mgr = lock(&self.mgr);
        if lock(&self.swept).insert(key.to_string()) {
            match mgr.on_reconnect(&phone, mode) {
                Ok(r) if !r.restored.is_empty() => self.note(format!(
                    "stay-awake: restored {} leftover lease(s) on {key} from a daemon that stopped",
                    r.restored.len()
                )),
                Ok(_) => {}
                Err(e) => self.note(format!(
                    "stay-awake: leftover-lease check on {key} failed: {e}"
                )),
            }
        }
        let owner = Owner::here(AWAKE_JOB, session_of(key));
        let mut done = Vec::new();
        for (ns, k, v) in bundle() {
            match mgr.apply(&phone, mode, &owner, ns, k, &v, ScopeReq::Session) {
                Ok(c) => done.push(format!("{ns}/{k} {} -> {}", c.before, c.applied)),
                Err(e) => self.note(format!("stay-awake: {ns}/{k} on {key} not leased: {e}")),
            }
        }
        if !done.is_empty() {
            self.note(format!("stay-awake on {key}: {}", done.join(", ")));
        }
    }

    fn restore(&self, key: &str, d: &Dev, why: &str) {
        let phone = AdbPhone::new(&self.runner, d.serial.clone());
        match lock(&self.mgr).restore_session(&phone, d.mode, &session_of(key)) {
            Ok(rs) => {
                let what: Vec<String> = rs
                    .iter()
                    .map(|(e, st)| {
                        let how = match st {
                            RestoreStatus::Restored => "restored".to_string(),
                            other => format!("{other:?}").to_lowercase(),
                        };
                        format!("{}/{} {how}", e.namespace, e.key)
                    })
                    .collect();
                self.note(format!(
                    "stay-awake off on {key} ({why}): {}",
                    what.join(", ")
                ));
            }
            Err(e) => self.note(format!("stay-awake: restore on {key} ({why}) failed: {e}")),
        }
    }

    /// A connection ended: phones no other session is using get their values back.
    pub fn leave(&self, conn: u64) {
        let mut devs = lock(&self.devs);
        for (key, d) in devs.iter_mut() {
            if d.conns.remove(&conn).is_some() && d.conns.is_empty() && d.held {
                self.restore(key, d, "session ended");
                d.held = false;
            }
        }
    }

    /// Drop sessions idle for `idle_ms`; phones left with none get their values back.
    pub fn sweep_idle(&self) {
        let now = self.cfg.clock.unix_ms();
        let idle = self.cfg.idle_ms;
        let mut devs = lock(&self.devs);
        for (key, d) in devs.iter_mut() {
            d.conns.retain(|_, last| now.saturating_sub(*last) < idle);
            if d.conns.is_empty() && d.held {
                self.restore(key, d, "idle");
                d.held = false;
            }
        }
    }

    /// The daemon is stopping: what is queued runs first, then every phone gets its values back.
    pub fn shutdown(&self) {
        self.stop_worker();
        let mut devs = lock(&self.devs);
        for (key, d) in devs.iter_mut() {
            d.conns.clear();
            if d.held {
                self.restore(key, d, "daemon stopping");
                d.held = false;
            }
        }
    }

    /// Whether a phone is held awake now (tests, `devices.list`).
    pub fn is_held(&self, key: &str) -> bool {
        lock(&self.devs).get(key).is_some_and(|d| d.held)
    }
}

/// The idle sweep, every `every` until the daemon drops its `SessionAwake`.
pub fn spawn_sweeper(awake: Weak<SessionAwake>, every: Duration) {
    std::thread::spawn(move || loop {
        std::thread::sleep(every);
        match awake.upgrade() {
            Some(a) => a.sweep_idle(),
            None => return,
        }
    });
}

/// `AGENTROID_NO_S24_AWAKE=1` leaves the protected phone out of stay-awake (the exception off).
pub fn s24_from_env() -> bool {
    !std::env::var_os("AGENTROID_NO_S24_AWAKE").is_some_and(|v| v == "1")
}

/// `AGENTROID_NO_STAY_AWAKE=1` turns stay-awake off; `AGENTROID_AWAKE_IDLE_S` sets the idle time.
pub fn from_env() -> (bool, u64) {
    let off = std::env::var_os("AGENTROID_NO_STAY_AWAKE").is_some_and(|v| v == "1");
    let idle = std::env::var("AGENTROID_AWAKE_IDLE_S")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|s| *s >= 60)
        .map_or(DEFAULT_IDLE_MS, |s| s * 1000);
    (!off, idle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use care::clock::{FakeClock, TEST_EPOCH_MS};
    use care::fake::FakePhone;
    use care::pid::FakePids;

    const PHONE: &str = "AWAKETEST01";
    const ADB: &str = "192.0.2.9:5555";

    struct Rig {
        phone: FakePhone,
        clock: Arc<FakeClock>,
        pids: Arc<FakePids>,
        leases: PathBuf,
    }

    fn rig(name: &str) -> Rig {
        rig_of(name, PHONE)
    }

    fn rig_of(name: &str, hw: &str) -> Rig {
        let dir =
            std::env::temp_dir().join(format!("agentroid-awake-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let phone = FakePhone::new(hw);
        phone.state().tokens.push(ADB.to_string());
        phone.set("system", "screen_off_timeout", "30000");
        phone.set("global", "stay_on_while_plugged_in", "0");
        Rig {
            phone,
            clock: Arc::new(FakeClock::new(TEST_EPOCH_MS)),
            pids: Arc::new(FakePids::default()),
            leases: dir.join("leases.json"),
        }
    }

    fn awake(r: &Rig) -> SessionAwake {
        awake_s24(r, true)
    }

    fn awake_s24(r: &Rig, s24: bool) -> SessionAwake {
        SessionAwake::new(AwakeConfig {
            adb: DynRunner(Arc::new(r.phone.clone())),
            s24,
            leases: r.leases.clone(),
            clock: r.clock.clone(),
            pids: r.pids.clone(),
            idle_ms: 60_000,
        })
    }

    fn values(r: &Rig) -> (Option<String>, Option<String>) {
        (
            r.phone.get("system", "screen_off_timeout"),
            r.phone.get("global", "stay_on_while_plugged_in"),
        )
    }

    fn held_values() -> (Option<String>, Option<String>) {
        (
            Some(THRESHOLDS.working_screen_timeout_ms.to_string()),
            Some("7".to_string()),
        )
    }

    fn original() -> (Option<String>, Option<String>) {
        (Some("30000".into()), Some("0".into()))
    }

    fn records(r: &Rig) -> usize {
        LeaseStore::new(&r.leases).entries().unwrap().len()
    }

    #[test]
    fn first_call_holds_the_screen_awake_and_the_last_session_leaving_restores_it() {
        let r = rig("leave");
        let a = awake(&r);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        assert_eq!(values(&r), held_values());
        assert_eq!(records(&r), 2, "both leases are on disk while held");
        assert!(a.is_held(PHONE));
        // a second session on the same phone: nothing new is written
        let before = r.phone.commands().len();
        a.touch(2, PHONE, ADB, DeviceMode::Full);
        assert_eq!(r.phone.commands().len(), before);
        // the first one leaves: the second still works the phone
        a.leave(1);
        assert_eq!(values(&r), held_values());
        a.leave(2);
        assert_eq!(values(&r), original());
        assert_eq!(records(&r), 0);
        assert!(!a.is_held(PHONE));
        // the next call holds it again
        a.touch(3, PHONE, ADB, DeviceMode::Full);
        assert_eq!(values(&r), held_values());
    }

    #[test]
    fn an_idle_phone_gets_its_values_back() {
        let r = rig("idle");
        let a = awake(&r);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        r.clock.advance(59_000);
        a.sweep_idle();
        assert_eq!(values(&r), held_values(), "not idle yet");
        r.clock.advance(2_000);
        a.sweep_idle();
        assert_eq!(values(&r), original());
        assert_eq!(records(&r), 0);
    }

    #[test]
    fn a_daemon_stopping_restores_every_phone() {
        let r = rig("shutdown");
        let a = awake(&r);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        a.shutdown();
        assert_eq!(values(&r), original());
        assert_eq!(records(&r), 0);
    }

    #[test]
    fn after_a_crash_the_next_daemon_restores_first_and_keeps_the_true_original() {
        let r = rig("crash");
        let a = awake(&r);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        drop(a); // no leave, no shutdown: the daemon died
        r.pids.kill(std::process::id());
        assert_eq!(
            values(&r),
            held_values(),
            "a crash leaves the change on the phone"
        );
        let b = awake(&r);
        b.touch(7, PHONE, ADB, DeviceMode::Full);
        let n = b.take_notes().join("\n");
        assert!(n.contains("restored 2 leftover lease(s)"), "{n}");
        // the new leases record the phone's own values, not the crashed daemon's
        let before: Vec<String> = LeaseStore::new(&r.leases)
            .entries()
            .unwrap()
            .iter()
            .map(|e| e.before.clone())
            .collect();
        assert_eq!(before.len(), 2);
        assert!(
            before.contains(&"30000".to_string()) && before.contains(&"0".to_string()),
            "{before:?}"
        );
    }

    #[test]
    fn a_value_changed_by_hand_is_left_alone() {
        let r = rig("cas");
        let a = awake(&r);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        r.phone.set("system", "screen_off_timeout", "120000"); // the owner changed it meanwhile
        a.leave(1);
        assert_eq!(
            r.phone.get("system", "screen_off_timeout").as_deref(),
            Some("120000")
        );
        assert_eq!(
            r.phone.get("global", "stay_on_while_plugged_in").as_deref(),
            Some("0")
        );
    }

    #[test]
    fn read_only_helper_and_emulators_are_never_touched() {
        let r = rig("modes");
        let a = awake(&r);
        a.touch(1, PHONE, ADB, DeviceMode::ReadOnly);
        a.touch(1, PHONE, ADB, DeviceMode::Helper);
        a.touch(1, "EMU1", "emulator-5554", DeviceMode::Full);
        assert!(r.phone.commands().is_empty(), "{:?}", r.phone.commands());
        assert_eq!(values(&r), original());
        assert_eq!(records(&r), 0);
    }

    #[test]
    fn only_the_two_screen_keys_are_ever_written() {
        let r = rig("keys");
        let a = awake(&r);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        a.leave(1);
        for c in r.phone.commands() {
            if c.starts_with("settings put") || c.starts_with("settings delete") {
                assert!(
                    c.contains("system screen_off_timeout")
                        || c.contains("global stay_on_while_plugged_in"),
                    "unexpected write: {c}"
                );
            }
            let low = c.to_lowercase();
            assert!(
                !low.contains("lock") && !low.contains("password") && !low.contains("pin "),
                "{c}"
            );
        }
    }

    // ---- the protected phone exception (12 Nov 2026): the two screen keys, nothing else ----

    const S24: &str = agentroid_adb_link::guard::S24_SERIAL;

    /// Every line sent to the phone is a get or put of one of the two keys.
    fn only_screen_key_lines(r: &Rig) {
        for c in r.phone.commands() {
            let w: Vec<&str> = c.split(' ').collect();
            let two = matches!(
                w.get(1..4),
                Some(["get" | "put" | "delete", "system", "screen_off_timeout"])
                    | Some([
                        "get" | "put" | "delete",
                        "global",
                        "stay_on_while_plugged_in"
                    ])
            );
            assert!(
                w.first() == Some(&"settings") && two,
                "unexpected S24 command: {c}"
            );
        }
    }

    #[test]
    fn the_s24_keeps_its_screen_on_and_gets_its_values_back_each_way() {
        // reached by its address, as it is on the LAN (the mDNS name comes and goes)
        let r = rig_of("s24", S24);
        let a = awake(&r);
        assert!(a.covers_s24(S24, ADB));
        a.touch(1, S24, ADB, DeviceMode::ReadOnly);
        assert_eq!(values(&r), held_values());
        assert_eq!(records(&r), 2);
        a.leave(1);
        assert_eq!(values(&r), original(), "session end");
        assert_eq!(records(&r), 0);
        // idle
        a.touch(2, S24, ADB, DeviceMode::ReadOnly);
        assert_eq!(values(&r), held_values());
        r.clock.advance(61_000);
        a.sweep_idle();
        assert_eq!(values(&r), original(), "idle");
        // daemon stopping
        a.touch(3, S24, ADB, DeviceMode::ReadOnly);
        a.shutdown();
        assert_eq!(values(&r), original(), "shutdown");
        assert_eq!(records(&r), 0);
        only_screen_key_lines(&r);
    }

    #[test]
    fn the_s24_after_a_crash_gets_the_true_original_back() {
        let r = rig_of("s24-crash", S24);
        let a = awake(&r);
        a.touch(1, S24, ADB, DeviceMode::ReadOnly);
        drop(a); // the daemon died: its leases stay, their owner pid is dead
        r.pids.kill(std::process::id());
        assert_eq!(values(&r), held_values());
        let b = awake(&r);
        b.touch(1, S24, ADB, DeviceMode::ReadOnly);
        let n = b.take_notes().join(
            "
",
        );
        assert!(n.contains("restored 2 leftover lease(s)"), "{n}");
        let before: Vec<String> = LeaseStore::new(&r.leases)
            .entries()
            .unwrap()
            .iter()
            .map(|e| e.before.clone())
            .collect();
        assert!(
            before.contains(&"30000".to_string()) && before.contains(&"0".to_string()),
            "{before:?}"
        );
        b.leave(1);
        assert_eq!(values(&r), original());
        assert_eq!(records(&r), 0);
        only_screen_key_lines(&r);
    }

    #[test]
    fn with_the_exception_off_the_s24_is_never_touched() {
        let r = rig_of("s24-off", S24);
        let a = awake_s24(&r, false);
        assert!(!a.covers_s24(S24, ADB));
        a.touch(1, S24, ADB, DeviceMode::ReadOnly);
        a.leave(1);
        assert!(r.phone.commands().is_empty(), "{:?}", r.phone.commands());
        assert_eq!(values(&r), original());
        assert_eq!(records(&r), 0);
    }

    #[test]
    fn the_exception_covers_the_s24_and_no_other_read_only_phone() {
        let r = rig("other-ro");
        let a = awake(&r);
        assert!(!a.covers_s24(PHONE, ADB));
        a.touch(1, PHONE, ADB, DeviceMode::ReadOnly);
        assert!(r.phone.commands().is_empty(), "{:?}", r.phone.commands());
        assert_eq!(values(&r), original());
        // and the protected phone in helper mode (never configured so, but still) is left alone
        let s = rig_of("s24-helper", S24);
        awake(&s).touch(1, S24, ADB, DeviceMode::Helper);
        assert!(s.phone.commands().is_empty());
    }

    #[test]
    fn the_worker_keeps_holds_and_restores_in_order() {
        let r = rig("worker");
        let a = Arc::new(awake(&r));
        a.start_worker();
        // a session, its end, and a new session, queued back to back
        a.touch_later(1, PHONE, ADB, DeviceMode::Full);
        a.leave_later(1);
        a.touch_later(2, PHONE, ADB, DeviceMode::Full);
        for _ in 0..500 {
            if a.is_held(PHONE) && values(&r) == held_values() && records(&r) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(values(&r), held_values(), "the second session holds");
        // the second hold leased from the phone's own values, not the first hold's
        let before: Vec<String> = LeaseStore::new(&r.leases)
            .entries()
            .unwrap()
            .iter()
            .map(|e| e.before.clone())
            .collect();
        assert!(
            before.contains(&"30000".to_string()) && before.contains(&"0".to_string()),
            "{before:?}"
        );
        // shutdown runs what is queued, then restores
        a.touch_later(3, PHONE, ADB, DeviceMode::Full);
        a.shutdown();
        assert_eq!(values(&r), original());
        assert_eq!(records(&r), 0);
        // after shutdown the queue is gone: calls run here and still work
        a.touch_later(4, PHONE, ADB, DeviceMode::Full);
        assert_eq!(values(&r), held_values());
        a.leave_later(4);
        assert_eq!(values(&r), original());
    }
}
