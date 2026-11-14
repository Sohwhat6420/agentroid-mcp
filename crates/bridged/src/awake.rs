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
//! * **Alerts when a value stays behind (14 Nov 2026).** A phone kept awake after nobody works it
//!   costs battery, so each of these raises an alert, at most once a day each (remembered across
//!   restarts in `awake-alerts.json` in the state folder): a `stay-awake ALERT` line in
//!   `bridged.log`, and a push when a channel is set ([`SessionAwake::set_notifier`]; the daemon uses
//!   the MCP server's ntfy topic file, `%USERPROFILE%\.agentroid\ntfy-topic.txt`, when it exists).
//!   Pushes go out after the locks are dropped, so a slow push service holds nothing up.
//!   (1) A session's lease finds a key already keeping the screen on ([`stays_awake`]: any
//!   `stay_on_while_plugged_in` bit, or a `screen_off_timeout` longer than any menu choice), whether
//!   set by something else or left by an earlier Agentroid session whose record was lost.
//!   (2) A restore leaves such a value in place: changed while held (compare-and-swap leaves it), or
//!   not put back (phone unreachable). (3) At start, records of a daemon that stopped without putting
//!   its values back ([`SessionAwake::leftover_check`]). An alert never changes a value: what was
//!   found is put back as found, and turning it off is the owner's call. Alerts name the protected
//!   phone by its role and any other phone by the last four characters of its serial, never a
//!   whole serial. `AGENTROID_AWAKE_ALERTS=0` keeps them to the log.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use agentroid_adb_link::guard::GuardedRunner;
use agentroid_core::gate::{is_s24, DeviceMode};
use care::clock::CareClock;
use care::heat::Notifier;
use care::leases::{LeaseEntry, LeaseStore};
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

/// The longest screen timeout a phone's own menu offers: 10 min on One UI, 30 min on stock Android
/// (Pixel). Agentroid's own working timeout is also 30 min, so a found 30 min is not counted (it may
/// be the owner's menu choice); a longer one was set by a tool.
pub const LONG_TIMEOUT_MS: u64 = 30 * 60 * 1000;

/// How long one alert stays quiet before it may be raised again, across daemon restarts too.
pub const ALERT_QUIET_MS: u64 = 24 * 60 * 60 * 1000;

/// Whether this value of one of the two keys keeps a screen on: any `stay_on_while_plugged_in` bit
/// (1 AC, 2 USB, 4 wireless, 8 dock), or a `screen_off_timeout` above [`LONG_TIMEOUT_MS`]. Any other
/// key, and a value that is not a number, is not counted.
pub fn stays_awake(ns: &str, key: &str, value: &str) -> bool {
    let n = value.trim().parse::<u64>();
    match (ns, key) {
        ("global", "stay_on_while_plugged_in") => n.is_ok_and(|v| v != 0),
        ("system", "screen_off_timeout") => n.is_ok_and(|v| v > LONG_TIMEOUT_MS),
        _ => false,
    }
}

/// One of the two stay-awake keys.
fn is_awake_key(ns: &str, key: &str) -> bool {
    matches!(
        (ns, key),
        ("global", "stay_on_while_plugged_in") | ("system", "screen_off_timeout")
    )
}

/// How an alert names a phone. Alerts go to a push service, so no serial goes in whole: the
/// protected phone by role, any other by the last four characters of its serial.
fn who(key: &str) -> String {
    if is_s24(key) {
        return "the protected phone".to_string();
    }
    let tail: String = {
        let c: Vec<char> = key.chars().collect();
        c[c.len().saturating_sub(4)..].iter().collect()
    };
    format!("the phone ending {tail}")
}

/// What the value does, for an alert.
fn effect(key: &str) -> &'static str {
    if key == "stay_on_while_plugged_in" {
        "keeps the screen on while charging"
    } else {
        "keeps the screen on far longer than any menu choice"
    }
}

/// The alert push's title.
pub const ALERT_TITLE: &str = "Agentroid: a phone screen is kept awake";

/// The push channel the MCP server's job pushes use: `POST <base>/<topic>` with a Title header (the
/// daemon passes `https://ntfy.sh`, as the MCP server does; `--ntfy-base` is not used here). The
/// topic is read from its key file at push time, held to the MCP server's rule (16 to 64 of
/// `A-Z a-z 0-9 _ -`) and never logged; curl gets the whole request on stdin (`--config -`, after
/// `-q` so no `.curlrc` adds options), so the topic is not on its command line either. Windows'
/// own `curl.exe` is used when present. No console window.
pub struct NtfyNotifier {
    pub topic_file: PathBuf,
    /// `https://ntfy.sh` or a self-hosted server.
    pub base: String,
}

/// One value inside a double-quoted curl config string.
fn curl_quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if c.is_control() => o.push(' '),
            c => o.push(c),
        }
    }
    o
}

impl NtfyNotifier {
    /// The topic in the key file, if it holds exactly one plausible ntfy topic.
    fn topic(&self) -> Result<String, String> {
        let t = std::fs::read_to_string(&self.topic_file)
            .map_err(|e| format!("the topic file could not be read ({:?})", e.kind()))?;
        let t = t.trim();
        let ok = (16..=64).contains(&t.len())
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if ok {
            Ok(t.to_string())
        } else {
            Err("the topic file does not hold one ntfy topic".to_string())
        }
    }

    /// The curl config for one push (the topic is inside; never log it).
    fn request(&self, topic: &str, title: &str, body: &str) -> String {
        format!(
            "url = \"{}/{}\"\nheader = \"Title: {}\"\nheader = \"Tags: battery\"\ndata-raw = \"{}\"\nsilent\nshow-error\nfail\nmax-time = 15\n",
            self.base.trim_end_matches('/'),
            topic,
            curl_quote(title),
            curl_quote(body)
        )
    }
}

impl Notifier for NtfyNotifier {
    fn push(&self, _phone: &str, title: &str, body: &str) -> Result<(), String> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let topic = self.topic()?;
        let req = self.request(&topic, title, body);
        let mut cmd = Command::new(curl_exe());
        cmd.args(["-q", "--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("curl did not start ({:?})", e.kind()))?;
        let wrote = match child.stdin.take() {
            Some(mut s) => s.write_all(req.as_bytes()).is_ok(),
            None => false,
        }; // stdin is closed here, so curl reads the whole config
        let st = child
            .wait()
            .map_err(|e| format!("curl did not finish ({:?})", e.kind()))?;
        match (wrote, st.success()) {
            (true, true) => Ok(()),
            (false, _) => Err("the request could not be handed to curl".to_string()),
            (true, false) => Err(format!(
                "the push was not accepted (curl exit {:?})",
                st.code()
            )),
        }
    }
}

/// `%SystemRoot%\System32\curl.exe` when it exists (a program search would look in the daemon's
/// own folder first), else `curl` from the path.
fn curl_exe() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("curl.exe"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("curl"))
}

/// The alert channel's key file: the MCP server's `%USERPROFILE%\.agentroid\ntfy-topic.txt`, when it
/// exists and `AGENTROID_AWAKE_ALERTS` is not `0`.
pub fn alert_topic_file() -> Option<PathBuf> {
    if std::env::var_os("AGENTROID_AWAKE_ALERTS").is_some_and(|v| v == "0") {
        return None;
    }
    let home = std::env::var_os("USERPROFILE")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("HOME"))?;
    let p = PathBuf::from(home)
        .join(".agentroid")
        .join("ntfy-topic.txt");
    p.is_file().then_some(p)
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
    /// Alert -> when it was last raised (unix ms), so each stays quiet for [`ALERT_QUIET_MS`].
    alerted: Mutex<HashMap<String, u64>>,
    /// Where `alerted` is kept across restarts ([`Self::set_alert_file`]); memory only without it.
    alert_file: Mutex<Option<PathBuf>>,
    /// Where alerts are pushed, besides `bridged.log`.
    notifier: Mutex<Option<Arc<dyn Notifier + Send + Sync>>>,
    /// Pushes waiting until no lock is held (a push can take seconds): (phone, text).
    outbox: Mutex<Vec<(String, String)>>,
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
            alerted: Mutex::new(HashMap::new()),
            alert_file: Mutex::new(None),
            notifier: Mutex::new(None),
            outbox: Mutex::new(Vec::new()),
        }
    }

    /// Push alerts here too (the daemon: [`NtfyNotifier`]; tests: a recorder).
    pub fn set_notifier(&self, n: Arc<dyn Notifier + Send + Sync>) {
        *lock(&self.notifier) = Some(n);
    }

    /// Keep the alerts raised in the last [`ALERT_QUIET_MS`] in this file (JSON, alert -> unix ms),
    /// so a daemon restart does not raise them again. An unreadable file counts as empty.
    pub fn set_alert_file(&self, p: PathBuf) {
        let now = self.cfg.clock.unix_ms();
        let kept: HashMap<String, u64> = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let mut a = lock(&self.alerted);
        for (k, t) in kept {
            if now.saturating_sub(t) < ALERT_QUIET_MS {
                let e = a.entry(k).or_insert(t);
                *e = (*e).max(t);
            }
        }
        *lock(&self.alert_file) = Some(p);
    }

    /// Write `alerted` to the alert file, if one is set (temporary file, then rename).
    fn save_alerts(&self, a: &HashMap<String, u64>) {
        let Some(p) = lock(&self.alert_file).clone() else {
            return;
        };
        let tmp = p.with_extension("json.tmp");
        let r = serde_json::to_string(a)
            .map_err(|e| e.to_string())
            .and_then(|s| std::fs::write(&tmp, s).map_err(|e| e.to_string()))
            .and_then(|()| std::fs::rename(&tmp, &p).map_err(|e| e.to_string()));
        if let Err(e) = r {
            self.note(format!("stay-awake: the alert file was not saved: {e}"));
        }
    }

    /// One alert per finding per [`ALERT_QUIET_MS`]: a note now, and a push once no lock is held
    /// ([`Self::flush_pushes`]). It never changes a value on the phone.
    fn alert(&self, key: &str, text: String) {
        let now = self.cfg.clock.unix_ms();
        {
            let mut a = lock(&self.alerted);
            let id = text.clone(); // names the phone by who(), never by its whole serial
            if a.get(&id)
                .is_some_and(|t| now.saturating_sub(*t) < ALERT_QUIET_MS)
            {
                return;
            }
            a.insert(id, now);
            a.retain(|_, t| now.saturating_sub(*t) < ALERT_QUIET_MS);
            self.save_alerts(&a);
        }
        self.note(format!("stay-awake ALERT: {text}"));
        lock(&self.outbox).push((who(key), text));
    }

    /// Send the waiting pushes. Called at the end of each public entry point, after its locks are
    /// dropped, so a slow push service never holds up `is_held`, the sweeper or a stop.
    fn flush_pushes(&self) {
        let items = std::mem::take(&mut *lock(&self.outbox));
        if items.is_empty() {
            return;
        }
        let Some(n) = lock(&self.notifier).clone() else {
            return;
        };
        for (phone, text) in items {
            if let Err(e) = n.push(&phone, ALERT_TITLE, &text) {
                self.note(format!("stay-awake: the alert push failed: {e}"));
            }
        }
    }

    /// A lease found a key already keeping the screen on.
    fn found_on(&self, key: &str, ns: &str, k: &str, before: &str, applied: &str) {
        if !stays_awake(ns, k, before) {
            return;
        }
        let source = if before == applied {
            "the value Agentroid itself writes, so an earlier Agentroid session probably left it \
             behind without a record"
                .to_string()
        } else {
            format!(
                "not a value Agentroid writes (it writes {applied}), so something else set it, such \
                 as the phone's Developer options > Stay awake or another adb tool"
            )
        };
        self.alert(
            key,
            format!(
                "{ns}/{k} on {} was already {before} before this session's lease: {source}. It {}, \
                 which costs battery. Agentroid puts back what it found and does not change it; \
                 turning it off is the owner's call.",
                who(key),
                effect(k)
            ),
        );
    }

    /// Restores that left a screen-keeping value in place.
    fn check_restores(
        &self,
        key: &str,
        phone: &AdbPhone<'_, DynRunner>,
        rs: &[(LeaseEntry, RestoreStatus)],
        why: &str,
    ) {
        for (e, st) in rs {
            let (ns, k) = (e.namespace.as_str(), e.key.as_str());
            if !is_awake_key(ns, k) {
                continue;
            }
            match st {
                RestoreStatus::Restored | RestoreStatus::Deleted | RestoreStatus::Reset => {}
                RestoreStatus::Left => {
                    if let Ok(Some(now)) = phone.settings_get(ns, k) {
                        if stays_awake(ns, k, &now) {
                            self.alert(
                                key,
                                format!(
                                    "{ns}/{k} on {} is {now} after the session ({why}): it was \
                                     changed while Agentroid held it, so compare-and-swap left it \
                                     rather than putting {} back. It {}, which costs battery; \
                                     turning it off is the owner's call.",
                                    who(key),
                                    e.before,
                                    effect(k)
                                ),
                            );
                        }
                    }
                }
                other => {
                    // the value is Agentroid's own lease: worth an alert whatever it is
                    let how = format!("{other:?}").to_lowercase();
                    self.alert(
                        key,
                        format!(
                            "{ns}/{k} on {} may still be {} ({why}): Agentroid could not put {} \
                             back ({}). The lease record stays, and the next contact with the \
                             phone tries again.",
                            who(key),
                            e.applied,
                            e.before,
                            how
                        ),
                    );
                }
            }
        }
    }

    /// Daemon start: stay-awake records of a daemon that stopped without putting its values back
    /// (killed, crashed, power cut). They are restored on the phone's next contact; until then the
    /// phone may still be kept awake, so each is an alert.
    pub fn leftover_check(&self) {
        let entries = match lock(&self.mgr).store.entries() {
            Ok(e) => e,
            Err(e) => {
                self.note(format!(
                    "stay-awake: the leftover check could not read the lease store: {e}"
                ));
                return;
            }
        };
        for e in entries {
            if e.job_id != AWAKE_JOB
                || self.cfg.pids.alive(e.owner_pid)
                || !is_awake_key(&e.namespace, &e.key)
            {
                continue;
            }
            let key = e.ident().to_string();
            let next = if is_s24(&key) && !self.cfg.s24 {
                "This daemon has the protected phone's stay-awake off (AGENTROID_NO_S24_AWAKE=1), \
                 so it will not put it back; turning it off is the owner's call."
            } else {
                "It is put back the next time a session reaches that phone."
            };
            self.alert(
                &key,
                format!(
                    "{}/{} on {} was left at {} by an Agentroid daemon that stopped without putting \
                     {} back. {next}",
                    e.namespace,
                    e.key,
                    who(&key),
                    e.applied,
                    e.before
                ),
            );
        }
        self.flush_pushes();
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
        self.touch_locked(conn, key, serial, mode);
        self.flush_pushes();
    }

    fn touch_locked(&self, conn: u64, key: &str, serial: &str, mode: DeviceMode) {
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
                Ok(r) if !r.restored.is_empty() => {
                    self.note(format!(
                        "stay-awake: restored {} leftover lease(s) on {key} from a daemon that stopped",
                        r.restored.len()
                    ));
                    self.check_restores(
                        key,
                        &phone,
                        &r.restored,
                        "leftover from a daemon that stopped",
                    );
                }
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
                Ok(c) => {
                    self.found_on(key, ns, k, &c.before, &c.applied);
                    done.push(format!("{ns}/{k} {} -> {}", c.before, c.applied));
                }
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
                self.check_restores(key, &phone, &rs, why);
            }
            Err(e) => self.note(format!("stay-awake: restore on {key} ({why}) failed: {e}")),
        }
    }

    /// A connection ended: phones no other session is using get their values back.
    pub fn leave(&self, conn: u64) {
        {
            let mut devs = lock(&self.devs);
            for (key, d) in devs.iter_mut() {
                if d.conns.remove(&conn).is_some() && d.conns.is_empty() && d.held {
                    self.restore(key, d, "session ended");
                    d.held = false;
                }
            }
        }
        self.flush_pushes();
    }

    /// Drop sessions idle for `idle_ms`; phones left with none get their values back.
    pub fn sweep_idle(&self) {
        let now = self.cfg.clock.unix_ms();
        let idle = self.cfg.idle_ms;
        {
            let mut devs = lock(&self.devs);
            for (key, d) in devs.iter_mut() {
                d.conns.retain(|_, last| now.saturating_sub(*last) < idle);
                if d.conns.is_empty() && d.held {
                    self.restore(key, d, "idle");
                    d.held = false;
                }
            }
        }
        self.flush_pushes();
    }

    /// The daemon is stopping: what is queued runs first, then every phone gets its values back.
    pub fn shutdown(&self) {
        self.stop_worker();
        {
            let mut devs = lock(&self.devs);
            for (key, d) in devs.iter_mut() {
                d.conns.clear();
                if d.held {
                    self.restore(key, d, "daemon stopping");
                    d.held = false;
                }
            }
        }
        self.flush_pushes();
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

    // ---- alerts when a screen-keeping value stays behind (14 Nov 2026) ----

    /// Records pushes instead of sending them.
    #[derive(Default)]
    struct Pushes(Mutex<Vec<(String, String, String)>>);

    impl Notifier for Pushes {
        fn push(&self, phone: &str, title: &str, body: &str) -> Result<(), String> {
            lock(&self.0).push((phone.into(), title.into(), body.into()));
            Ok(())
        }
    }

    fn with_pushes(a: &SessionAwake) -> Arc<Pushes> {
        let p = Arc::new(Pushes::default());
        a.set_notifier(p.clone());
        p
    }

    fn alerts(notes: &[String]) -> Vec<&String> {
        notes
            .iter()
            .filter(|n| n.starts_with("stay-awake ALERT"))
            .collect()
    }

    #[test]
    fn stays_awake_counts_only_screen_keeping_values_of_the_two_keys() {
        for v in ["1", "2", "3", "7", "8", "15"] {
            assert!(stays_awake("global", "stay_on_while_plugged_in", v), "{v}");
        }
        for v in ["0", " 0 ", "", "null", "-1", "on"] {
            assert!(
                !stays_awake("global", "stay_on_while_plugged_in", v),
                "{v:?}"
            );
        }
        assert!(!stays_awake("system", "screen_off_timeout", "30000"));
        assert!(!stays_awake("system", "screen_off_timeout", "600000"));
        let working = THRESHOLDS.working_screen_timeout_ms.to_string();
        assert!(
            !stays_awake("system", "screen_off_timeout", &working),
            "30 min is a menu choice on stock Android"
        );
        assert!(!stays_awake("system", "screen_off_timeout", "1800000"));
        assert!(stays_awake("system", "screen_off_timeout", "1800001"));
        assert!(stays_awake("system", "screen_off_timeout", "2147483647"));
        // the namespace must match too, and no other key counts
        assert!(!stays_awake("system", "stay_on_while_plugged_in", "15"));
        assert!(!stays_awake("global", "screen_off_timeout", &working));
        assert!(!stays_awake("system", "screen_brightness", "255"));
    }

    #[test]
    fn a_value_found_on_before_the_lease_alerts_once_and_is_put_back_as_found() {
        let r = rig("found-on");
        r.phone.set("global", "stay_on_while_plugged_in", "15");
        let a = awake(&r);
        let p = with_pushes(&a);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        a.leave(1);
        assert_eq!(
            r.phone.get("global", "stay_on_while_plugged_in").as_deref(),
            Some("15"),
            "put back as found, never turned off by the alert"
        );
        let notes = a.take_notes();
        let al = alerts(&notes);
        assert_eq!(al.len(), 1, "{notes:?}");
        assert!(al[0].contains("was already 15") && al[0].contains("not a value Agentroid writes"));
        let pushed = lock(&p.0).clone();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].0, "the phone ending ST01", "never a whole serial");
        assert!(!pushed[0].2.contains(PHONE), "{}", pushed[0].2);
        assert_eq!(pushed[0].1, ALERT_TITLE);
        assert!(pushed[0].2.contains("while charging"), "{}", pushed[0].2);
        // the same finding again: no second alert this run
        a.touch(2, PHONE, ADB, DeviceMode::Full);
        a.leave(2);
        assert!(alerts(&a.take_notes()).is_empty());
        assert_eq!(lock(&p.0).len(), 1);
    }

    #[test]
    fn our_own_value_found_before_the_lease_is_called_ours() {
        let r = rig("found-ours");
        r.phone.set("global", "stay_on_while_plugged_in", "7");
        let a = awake(&r);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        let notes = a.take_notes();
        let al = alerts(&notes);
        assert_eq!(al.len(), 1, "{notes:?}");
        assert!(al[0].contains("Agentroid itself writes"), "{}", al[0]);
    }

    #[test]
    fn ordinary_values_raise_no_alert() {
        let r = rig("quiet");
        let a = awake(&r);
        let p = with_pushes(&a);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        a.leave(1);
        a.touch(2, PHONE, ADB, DeviceMode::Full);
        r.clock.advance(61_000);
        a.sweep_idle();
        a.leftover_check();
        assert!(alerts(&a.take_notes()).is_empty());
        assert!(lock(&p.0).is_empty());
        assert_eq!(values(&r), original());
    }

    #[test]
    fn a_value_changed_while_held_is_left_and_alerted() {
        let r = rig("left");
        let a = awake(&r);
        let p = with_pushes(&a);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        r.phone.set("global", "stay_on_while_plugged_in", "15"); // someone else, mid-session
        a.leave(1);
        assert_eq!(
            r.phone.get("global", "stay_on_while_plugged_in").as_deref(),
            Some("15"),
            "compare-and-swap leaves it"
        );
        let notes = a.take_notes();
        let al = alerts(&notes);
        assert_eq!(al.len(), 1, "{notes:?}");
        assert!(
            al[0].contains("is 15 after the session (session ended)"),
            "{}",
            al[0]
        );
        assert_eq!(lock(&p.0).len(), 1);
    }

    #[test]
    fn records_of_a_daemon_that_stopped_alert_at_start() {
        let r = rig("leftover");
        let a = awake(&r);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        drop(a); // killed: its records stay, their owner pid is dead
        r.pids.kill(std::process::id());
        let b = awake(&r);
        let p = with_pushes(&b);
        b.leftover_check();
        let notes = b.take_notes();
        let al = alerts(&notes);
        assert_eq!(al.len(), 2, "both keys: {notes:?}");
        assert!(al.iter().all(|n| n.contains("stopped without putting")));
        assert_eq!(lock(&p.0).len(), 2);
        // once per run, and nothing on the phone changed because of it
        b.leftover_check();
        assert_eq!(lock(&p.0).len(), 2);
        assert_eq!(values(&r), held_values());
    }

    #[test]
    fn the_protected_phone_is_named_not_numbered_and_only_its_two_keys_are_read() {
        let r = rig_of("s24-alert", S24);
        let a = awake(&r);
        let p = with_pushes(&a);
        a.touch(1, S24, ADB, DeviceMode::ReadOnly);
        r.phone.set("global", "stay_on_while_plugged_in", "15");
        a.leave(1);
        let pushed = lock(&p.0).clone();
        assert_eq!(pushed.len(), 1, "{pushed:?}");
        let (phone, _, body) = &pushed[0];
        assert_eq!(phone, "the protected phone");
        assert!(body.contains("the protected phone"), "{body}");
        assert!(!body.contains(S24) && !phone.contains(S24));
        for n in alerts(&a.take_notes()) {
            assert!(!n.contains(S24), "{n}");
        }
        only_screen_key_lines(&r);
    }

    #[test]
    fn the_ntfy_request_keeps_its_strings_inside_their_quotes() {
        let n = NtfyNotifier {
            topic_file: PathBuf::from("unused"),
            base: "https://ntfy.example/".into(),
        };
        let req = n.request(
            "topic-1",
            ALERT_TITLE,
            "a \"b\"\\c\nurl = \"https://evil.example\"",
        );
        let lines: Vec<&str> = req.lines().collect();
        assert_eq!(lines[0], "url = \"https://ntfy.example/topic-1\"");
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("url")).count(),
            1,
            "a body cannot add a url line: {req}"
        );
        assert!(
            req.contains("data-raw = \"a \\\"b\\\"\\\\c\\nurl = \\\"https://evil.example\\\"\""),
            "{req}"
        );
    }

    #[test]
    fn an_unusable_topic_file_sends_nothing() {
        let dir =
            std::env::temp_dir().join(format!("agentroid-awake-topic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing = NtfyNotifier {
            topic_file: dir.join("none.txt"),
            base: "https://ntfy.example".into(),
        };
        assert!(missing.topic().is_err());
        let bad = dir.join("bad.txt");
        std::fs::write(&bad, "two words\n").unwrap();
        let n = NtfyNotifier {
            topic_file: bad,
            base: "https://ntfy.example".into(),
        };
        let e = n.push("p", "t", "b").unwrap_err();
        assert!(!e.contains("two words"), "the topic is never echoed: {e}");
        let good = dir.join("good.txt");
        std::fs::write(&good, " abc_DEF-123-xyz_456 \r\n").unwrap();
        let g = NtfyNotifier {
            topic_file: good,
            base: "https://ntfy.example".into(),
        };
        assert_eq!(g.topic().unwrap(), "abc_DEF-123-xyz_456");
        // the MCP server's rule: 16 to 64 characters, so a short guessable topic is refused
        let short = dir.join("short.txt");
        std::fs::write(&short, "abcDEF123").unwrap();
        let s = NtfyNotifier {
            topic_file: short,
            base: "https://ntfy.example".into(),
        };
        assert!(s.topic().is_err());
        let long = dir.join("long.txt");
        std::fs::write(&long, "a".repeat(65)).unwrap();
        let l = NtfyNotifier {
            topic_file: long,
            base: "https://ntfy.example".into(),
        };
        assert!(l.topic().is_err());
    }

    #[test]
    fn alerts_name_phones_without_a_whole_serial() {
        assert_eq!(who("AWAKETEST01"), "the phone ending ST01");
        assert_eq!(who("ab"), "the phone ending ab");
        assert_eq!(who(S24), "the protected phone");
        assert_eq!(
            who(&format!("{S24}._adb-tls-connect._tcp")),
            "the protected phone"
        );
    }

    #[test]
    fn an_alert_stays_quiet_for_a_day_across_restarts_then_comes_back() {
        let r = rig("quiet-day");
        r.phone.set("global", "stay_on_while_plugged_in", "15");
        let file = r.leases.with_file_name("awake-alerts.json");
        let a = awake(&r);
        a.set_alert_file(file.clone());
        let p = with_pushes(&a);
        a.touch(1, PHONE, ADB, DeviceMode::Full);
        a.leave(1);
        assert_eq!(lock(&p.0).len(), 1);
        assert!(file.is_file(), "the alert is remembered on disk");
        // a restarted daemon an hour later: quiet
        drop(a);
        r.clock.advance(60 * 60 * 1000);
        let b = awake(&r);
        b.set_alert_file(file.clone());
        let q = with_pushes(&b);
        b.touch(1, PHONE, ADB, DeviceMode::Full);
        b.leave(1);
        assert!(lock(&q.0).is_empty(), "within the day");
        assert!(alerts(&b.take_notes()).is_empty());
        // a day after the first: it comes back once
        r.clock.advance(ALERT_QUIET_MS);
        b.touch(2, PHONE, ADB, DeviceMode::Full);
        b.leave(2);
        assert_eq!(lock(&q.0).len(), 1);
        // an unreadable file counts as empty, and nothing panics
        std::fs::write(&file, "not json").unwrap();
        let c = awake(&r);
        c.set_alert_file(file);
        let s = with_pushes(&c);
        c.touch(1, PHONE, ADB, DeviceMode::Full);
        assert_eq!(lock(&s.0).len(), 1);
    }

    #[test]
    fn a_push_never_runs_while_a_lock_is_held() {
        /// Fails the test if `is_held` (the devices lock) or the manager lock cannot be taken
        /// while a push runs.
        struct Probe(std::sync::Weak<SessionAwake>, Mutex<usize>);
        impl Notifier for Probe {
            fn push(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
                let a = self.0.upgrade().expect("alive");
                assert!(a.devs.try_lock().is_ok(), "devices lock held during a push");
                assert!(a.mgr.try_lock().is_ok(), "manager lock held during a push");
                *lock(&self.1) += 1;
                Ok(())
            }
        }
        let r = rig("no-lock-push");
        r.phone.set("global", "stay_on_while_plugged_in", "15");
        let a = Arc::new(awake(&r));
        let probe = Arc::new(Probe(Arc::downgrade(&a), Mutex::new(0)));
        a.set_notifier(probe.clone());
        a.touch(1, PHONE, ADB, DeviceMode::Full); // found on
        r.phone.set("system", "screen_off_timeout", "3600000"); // changed while held
        a.leave(1);
        assert_eq!(*lock(&probe.1), 2);
    }
}
