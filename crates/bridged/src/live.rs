//! The live adb [`Transport`]: the "adb server socket" half, wired at last
//! (until this module existed, `bridged` could only run `--fake` or `NoDevices`).
//!
//! Every command is one `adb -s <serial> ...` through a [`Runner`], so the caller decides what
//! stands in front: `main` always passes a `GuardedRunner`, which refuses the protected phone behind any
//! spelling and checks the target's real hardware serial before each command. Nothing here talks to
//! adb any other way and no command here is `start-server` or `kill-server`; but any adb call starts
//! a server that is not running, and a client of a different build than the running server restarts
//! it (dropping wireless connections), so `main` points `--adb auto` at the scrcpy adb first.
//!
//! Known limits of this first live version (each one is checked on a device at step 9/11):
//! * no helper app: `Backend::Helper` and non-ASCII typing answer "not installed";
//! * screenshots are the phone's PNG at full size: no resampling, so `max_width` is advisory, and
//!   the bytes sit in `Screenshot::jpeg` although they are PNG;
//! * screen rotation is read from `dumpsys input` (SurfaceOrientation of the first input device, as
//!   phone-hands does); portrait is checked on the POCO, landscape only on paper;
//! * `health_hint` carries only what replies already asked for said: battery, power and temperature
//!   after a `dumpsys battery` read, the screen after a wakefulness read or an unlock, the lock after an
//!   unlock. Nothing is probed for it, so a phone nobody has read the battery of still shows unknown.

use agentroid_adb_link::parse::parse_devices;
use agentroid_adb_link::runner::{RunError, RunOutput, Runner};
use agentroid_core::frame::Shot;
use agentroid_core::screenlock::{Reading, ScreenLock};
use agentroid_core::transport::{
    Backend, DeviceInfo, HealthHint, Input, Screenshot, Transport, TransportError,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

const SHELL_MS: u64 = 30_000;
const DUMP_MS: u64 = 45_000;
const SHOT_MS: u64 = 30_000;
const PROBE_MS: u64 = 15_000;
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// The fallback screen read for a UI that never goes idle (`assets/idle-free-dump/Dump.java`, built by
/// its `build.sh`). `uiautomator dump` gives up with "could not get idle state" on a screen that keeps
/// changing (a running stopwatch, 11 Nov 2026), and the screen guard then refuses every touch. This
/// makes the same platform calls without the idle requirement, so the XML is the platform's own.
const IDLE_FREE_DEX: &[u8] = include_bytes!("../assets/idle-free-dump/agentroid-dump.dex");
const IDLE_FREE_REMOTE: &str = agentroid_adb_link::guard::DUMP_HELPER_REMOTE;
const IDLE_FREE_CMD: &str = agentroid_adb_link::guard::IDLE_FREE_READ;
const NOT_IDLE: &str = "could not get idle state";

pub struct AdbTransport<R: Runner> {
    runner: R,
    /// adb serial -> `ro.serialno`, so `devices()` (which `Core` calls before every tool) does not
    /// spawn a `getprop` per phone per call. Dropped when the device leaves the list. The guard
    /// still reads the real serial before every command, so a stale entry cannot reach the protected phone.
    hw: HashMap<String, String>,
    /// adb serials this process has pushed the fallback reader to.
    dex_on: HashSet<String>,
    /// Off only for the benchmark's "guard as run 2 had it" configuration (`AGENTROID_NO_IDLE_FALLBACK=1`).
    idle_fallback: bool,
    /// adb serial -> what this transport's own replies said about the phone (the `health_hint` source).
    seen: HashMap<String, Seen>,
    /// The dump helper's own runner and staged copy
    /// ([`GuardedRunner::with_s24_helper`](agentroid_adb_link::guard::GuardedRunner::with_s24_helper)):
    /// the protected phone's helper is checked and put there through it alone. `None`: not wired,
    /// so the helper is never run on the protected phone.
    helper: Option<DumpHelper>,
    /// Protected phone adb serial -> when its helper last matched the daemon's copy by hash.
    helper_checked: HashMap<String, Instant>,
}

/// See [`AdbTransport::with_dump_helper`].
struct DumpHelper {
    runner: Box<dyn Runner + Send>,
    staged: std::path::PathBuf,
}

/// How long a helper hash check on the protected phone holds: the helper is checked again at each
/// process start and after this long.
pub const HELPER_RECHECK: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The embedded helper's sha256 (hex), as `sha256sum` prints it.
fn embedded_helper_hash() -> &'static str {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        let mut s = crate::sha256::Sha256::new();
        s.update(IDLE_FREE_DEX);
        s.finish_hex()
    })
}

/// The hash `sha256sum` printed for the helper: the first word of the first line when it is 64
/// lower-case hex digits and names the helper's path, else `None` (no file, an error, anything else).
pub fn parse_helper_hash(stdout: &str) -> Option<String> {
    let line = stdout.lines().next()?.trim();
    let mut w = line.split_whitespace();
    let (h, path) = (w.next()?, w.next()?);
    let hex = h.len() == 64
        && h.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (hex && path == IDLE_FREE_REMOTE && w.next().is_none()).then(|| h.to_string())
}

/// Write the helper to `path` unless it already holds exactly those bytes (tmp, then rename).
fn stage_helper(path: &Path) -> Result<(), TransportError> {
    if std::fs::read(path).ok().as_deref() == Some(IDLE_FREE_DEX) {
        return Ok(());
    }
    let tmp = path.with_extension("dex.tmp");
    std::fs::write(&tmp, IDLE_FREE_DEX)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| TransportError(format!("could not stage the fallback reader: {e}")))
}

/// One staging at a time in this process (two transports never write the staged copy together).
static STAGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What the replies served so far said about one phone, each with when it was read (field notes,
/// 11 Nov 2026: the POCO's header showed battery, power and temperature as unknown on every call, and
/// the screen as off after an unlock that had woken it). Filled only from output already asked for:
/// `dumpsys battery` (a `doctor` read or a job's care check), the wakefulness read, the unlock script.
#[derive(Debug, Default, Clone)]
struct Seen {
    battery: Option<(u8, Instant)>,
    powered: Option<(bool, Instant)>,
    temp_c: Option<(f64, Instant)>,
    screen: Option<(String, Instant)>,
    lock: Option<(String, Instant)>,
}

impl Seen {
    /// `Awake` is on; `Asleep`, `Dozing` and `Dreaming` are off (as `Core::health_of` maps it).
    fn note_wake(&mut self, wake: &str, at: Instant) {
        let s = if wake == "Awake" { "on" } else { "off" };
        self.screen = Some((s.to_string(), at));
    }

    fn note_reply(&mut self, line: &str, text: &str, at: Instant) {
        if line.trim() == "dumpsys battery" {
            self.note_battery(text, at);
        }
        // the unlock script's last state line: `after:mWakefulness=Awake isKeyguardShowing=false`
        if let Some(after) = text.lines().find_map(|l| l.trim().strip_prefix("after:")) {
            if let Some(w) = after
                .strip_prefix("mWakefulness=")
                .and_then(|r| r.split_whitespace().next())
            {
                self.note_wake(w, at);
            }
            if after.contains("isKeyguardShowing=true") || after.contains("mShowingLockscreen=true")
            {
                self.lock = Some(("locked".into(), at));
            } else if after.contains("isKeyguardShowing=false")
                || after.contains("mShowingLockscreen=false")
            {
                self.lock = Some(("unlocked".into(), at));
            }
        }
    }

    /// `dumpsys battery`: `level` over `scale` as a percent, `temperature` in tenths of a degree, and
    /// powered when any of the AC, USB, wireless or dock lines says true.
    fn note_battery(&mut self, text: &str, at: Instant) {
        let (mut level, mut scale) = (None::<u32>, 100u32);
        let (mut any_true, mut any_false) = (false, false);
        for l in text.lines() {
            let Some((k, v)) = l.trim().split_once(':') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "level" => level = v.parse().ok(),
                "scale" => scale = v.parse().ok().filter(|s| *s > 0).unwrap_or(100),
                "temperature" => {
                    if let Ok(t) = v.parse::<i32>() {
                        self.temp_c = Some((f64::from(t) / 10.0, at));
                    }
                }
                "AC powered" | "USB powered" | "Wireless powered" | "Dock powered" => match v {
                    "true" => any_true = true,
                    "false" => any_false = true,
                    _ => {}
                },
                _ => {}
            }
        }
        if let Some(n) = level {
            let pct = (n.saturating_mul(100) / scale).min(100);
            self.battery = Some((u8::try_from(pct).unwrap_or(100), at));
        }
        if any_true || any_false {
            self.powered = Some((any_true, at));
        }
    }

    fn hint(&self, now: Instant) -> HealthHint {
        let mut oldest = 0.0f64;
        let mut age = |at: &Instant| {
            oldest = oldest.max(now.saturating_duration_since(*at).as_secs_f64());
        };
        let mut h = HealthHint::default();
        if let Some((v, at)) = &self.battery {
            h.battery = Some(*v);
            age(at);
        }
        if let Some((v, at)) = &self.powered {
            h.powered = Some(*v);
            age(at);
        }
        if let Some((v, at)) = &self.temp_c {
            h.temp_c = Some(*v);
            age(at);
        }
        if let Some((v, at)) = &self.screen {
            h.screen = Some(v.clone());
            age(at);
        }
        if let Some((v, at)) = &self.lock {
            h.lock = Some(v.clone());
            age(at);
        }
        h.age_s = oldest;
        h
    }
}

impl<R: Runner> AdbTransport<R> {
    pub fn new(runner: R) -> Self {
        let off = std::env::var_os("AGENTROID_NO_IDLE_FALLBACK").is_some_and(|v| v == "1");
        AdbTransport {
            runner,
            hw: HashMap::new(),
            dex_on: HashSet::new(),
            idle_fallback: !off,
            seen: HashMap::new(),
            helper: None,
            helper_checked: HashMap::new(),
        }
    }

    /// Wire the dump helper's integrity: `staged` is the daemon's own copy (in its state
    /// directory), and `runner` the one runner that may check the helper on the protected phone and
    /// push `staged` there (built with `GuardedRunner::with_s24_helper` for the same path). Other
    /// phones get `staged` through the main runner.
    pub fn with_dump_helper(
        mut self,
        runner: Box<dyn Runner + Send>,
        staged: std::path::PathBuf,
    ) -> Self {
        self.helper = Some(DumpHelper { runner, staged });
        self
    }

    /// Is `serial` the protected phone (by its hardware serial, or by its name)?
    fn is_protected_serial(&mut self, serial: &str) -> bool {
        agentroid_core::gate::is_s24(serial)
            || agentroid_core::gate::is_s24(&self.hardware_serial(serial))
    }

    /// The helper's hash on the protected phone, read through the helper's own runner.
    fn helper_hash(&self, serial: &str) -> Result<Option<String>, TransportError> {
        let h = self.helper.as_ref().ok_or_else(no_helper)?;
        let argv: Vec<String> = [
            "-s",
            serial,
            "shell",
            agentroid_adb_link::guard::DUMP_HELPER_HASH_READ,
        ]
        .iter()
        .map(|a| (*a).to_string())
        .collect();
        let out = h.runner.run(&argv, PROBE_MS).map_err(run_err)?;
        if let Some(why) = adb_level_failure(&out) {
            return Err(TransportError(why));
        }
        Ok(parse_helper_hash(&out.stdout))
    }

    /// Review round 3, item 2: run the helper on the protected phone only while it matches the
    /// daemon's copy. One hash read per process start and per [`HELPER_RECHECK`]; a mismatch (or no
    /// file) pushes the staged copy through the helper's runner and reads again; a second mismatch
    /// refuses.
    fn ensure_dex_protected(&mut self, serial: &str) -> Result<(), TransportError> {
        if self
            .helper_checked
            .get(serial)
            .is_some_and(|t| t.elapsed() < HELPER_RECHECK)
        {
            return Ok(());
        }
        let want = embedded_helper_hash();
        if self.helper_hash(serial)?.as_deref() == Some(want) {
            self.helper_checked
                .insert(serial.to_string(), Instant::now());
            return Ok(());
        }
        {
            let h = self.helper.as_ref().ok_or_else(no_helper)?;
            let _one = STAGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            stage_helper(&h.staged)?;
            let l = h.staged.to_string_lossy().into_owned();
            let argv: Vec<String> = ["-s", serial, "push", &l, IDLE_FREE_REMOTE]
                .iter()
                .map(|a| (*a).to_string())
                .collect();
            let out = h.runner.run(&argv, 120_000).map_err(run_err)?;
            if !out.ok() {
                return Err(TransportError(format!(
                    "adb push of the fallback reader failed: {}",
                    out.stderr.trim()
                )));
            }
        }
        if self.helper_hash(serial)?.as_deref() == Some(want) {
            self.helper_checked
                .insert(serial.to_string(), Instant::now());
            return Ok(());
        }
        Err(TransportError(
            "the fallback reader on the protected phone does not match the daemon's copy, even after a fresh push, so it is not run there".into(),
        ))
    }

    /// Put the dump helper on the phone once per process (the fallback read runs it). On the
    /// protected phone it is checked by hash first ([`Self::ensure_dex_protected`]).
    fn ensure_dex(&mut self, serial: &str) -> Result<(), TransportError> {
        if self.is_protected_serial(serial) {
            if self.helper.is_none() {
                return Err(no_helper());
            }
            return self.ensure_dex_protected(serial);
        }
        if self.dex_on.contains(serial) {
            return Ok(());
        }
        if let Some(staged) = self.helper.as_ref().map(|h| h.staged.clone()) {
            let _one = STAGE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            stage_helper(&staged)?;
            self.push(serial, &staged, IDLE_FREE_REMOTE)?;
            self.dex_on.insert(serial.to_string());
            return Ok(());
        }
        // one staging file per push: two transports in one process never write or remove each
        // other's (on Windows a file another thread has open cannot be written)
        static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let local =
            std::env::temp_dir().join(format!("agentroid-dump-{}-{n}.dex", std::process::id()));
        std::fs::write(&local, IDLE_FREE_DEX)
            .map_err(|e| TransportError(format!("could not stage the fallback reader: {e}")))?;
        let pushed = self.push(serial, &local, IDLE_FREE_REMOTE);
        let _ = std::fs::remove_file(&local);
        pushed?;
        self.dex_on.insert(serial.to_string());
        Ok(())
    }

    /// The same transport without the idle-free fallback (what `AGENTROID_NO_IDLE_FALLBACK=1` gives).
    pub fn without_idle_fallback(mut self) -> Self {
        self.idle_fallback = false;
        self
    }

    /// The fallback read (see [`IDLE_FREE_DEX`]): pushes the reader once per device, then runs it.
    fn idle_free_xml(&mut self, serial: &str) -> Result<String, TransportError> {
        self.ensure_dex(serial)?;
        let out = self.run(&["-s", serial, "exec-out", IDLE_FREE_CMD], DUMP_MS)?;
        if let Some(why) = adb_level_failure(&out) {
            return Err(TransportError(why));
        }
        match xml_of(&out.stdout) {
            Some(x) => Ok(x.to_string()),
            None => {
                // a phone that lost the file (rebooted, /data/local/tmp cleaned) is pushed again next time
                self.dex_on.remove(serial);
                self.helper_checked.remove(serial);
                Err(TransportError(
                    format!("{}{}", out.stdout.trim(), out.stderr.trim())
                        .chars()
                        .take(300)
                        .collect(),
                ))
            }
        }
    }

    pub fn runner(&self) -> &R {
        &self.runner
    }

    fn run(&self, argv: &[&str], ms: u64) -> Result<RunOutput, TransportError> {
        let v: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
        self.runner.run(&v, ms).map_err(run_err)
    }

    /// `adb -s <serial> shell <line>`. A non-zero exit from the phone's own command is not an
    /// error here (`grep` finding nothing exits 1); adb failing to reach the phone is.
    fn sh(&self, serial: &str, line: &str, ms: u64) -> Result<String, TransportError> {
        let out = self.run(&["-s", serial, "shell", line], ms)?;
        if let Some(why) = adb_level_failure(&out) {
            return Err(TransportError(why));
        }
        let mut text = out.stdout;
        if !out.stderr.trim().is_empty() {
            text.push_str(&out.stderr);
        }
        Ok(text)
    }

    fn hardware_serial(&mut self, serial: &str) -> String {
        if let Some(h) = self.hw.get(serial) {
            return h.clone();
        }
        let got = self
            .run(&["-s", serial, "shell", "getprop ro.serialno"], PROBE_MS)
            .ok()
            .filter(RunOutput::ok)
            .map(|o| o.stdout.trim().to_string())
            .filter(|s| !s.is_empty());
        match got {
            Some(h) => {
                self.hw.insert(serial.to_string(), h.clone());
                h
            }
            // unreadable now: say so with the adb serial, and do not cache it
            None => serial.to_string(),
        }
    }
}

fn no_helper() -> TransportError {
    TransportError(
        "the dump helper's own runner is not wired in this daemon, so the fallback reader is not run on the protected phone".into(),
    )
}

fn run_err(e: RunError) -> TransportError {
    TransportError(e.to_string())
}

/// adb itself could not do the job (no device, offline, unauthorised), as opposed to the phone's
/// command returning a non-zero status.
fn adb_level_failure(out: &RunOutput) -> Option<String> {
    if out.code == Some(0) {
        return None;
    }
    let err = out.stderr.trim();
    let low = err.to_ascii_lowercase();
    // any line: adb prints "* daemon started successfully" before its own error when it had to start
    let adb_says = low
        .lines()
        .any(|l| l.trim_start().starts_with("error:") || l.trim_start().starts_with("adb:"))
        || low.contains("device offline")
        || low.contains("unauthorized")
        || low.contains("no devices/emulators");
    if out.code.is_none() || (adb_says && out.stdout.trim().is_empty()) {
        return Some(if err.is_empty() {
            "adb failed without a message".to_string()
        } else {
            err.to_string()
        });
    }
    None
}

/// `wm size` output to (width, height): an Override size wins over the Physical one.
pub fn parse_wm_size(text: &str) -> Option<(i32, i32)> {
    let pick = |label: &str| {
        text.lines().find_map(|l| {
            let rest = l.trim().strip_prefix(label)?;
            let (w, h) = rest.trim().split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
    };
    pick("Override size:").or_else(|| pick("Physical size:"))
}

/// True when the display is rotated a quarter turn (width and height swap).
pub fn parse_quarter_turn(text: &str) -> bool {
    // `dumpsys input | grep -m1 SurfaceOrientation` is what phone-hands reads (server.py:184), run on
    // the POCO since 18 Oct; the other spellings are what `dumpsys window` prints on other builds
    for l in text.lines() {
        for key in ["SurfaceOrientation:", "mCurrentRotation=", "mRotation="] {
            if let Some(i) = l.find(key) {
                let v: String = l[i + key.len()..]
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                let v = v.trim_start_matches("ROTATION_");
                return matches!(v, "1" | "3" | "90" | "270");
            }
        }
    }
    false
}

/// `exec-out` merges the phone's stderr into stdout, so a warning (Android 12+ prints "Multiple
/// displays were found" when there are several) can sit in front of the PNG: cut to its signature.
pub fn png_bytes(b: &[u8]) -> Option<&[u8]> {
    let at = b
        .windows(PNG_MAGIC.len())
        .take(1024)
        .position(|w| w == PNG_MAGIC)?;
    Some(&b[at..])
}

/// Width and height from a PNG's IHDR chunk.
pub fn png_size(b: &[u8]) -> Option<(i32, i32)> {
    if b.len() < 24 || b[..8] != PNG_MAGIC || &b[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| i32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let (w, h) = (be(16), be(20));
    (w > 0 && h > 0).then_some((w, h))
}

fn xml_of(text: &str) -> Option<&str> {
    let start = text.find("<hierarchy")?;
    let end = text.rfind("</hierarchy>")? + "</hierarchy>".len();
    (start < end).then(|| &text[start..end])
}

fn input_line(input: &Input) -> Result<String, TransportError> {
    let d = |display: u32| {
        if display == 0 {
            String::new()
        } else {
            format!("-d {display} ")
        }
    };
    Ok(match input {
        Input::Tap { x, y, display } => format!("input {}tap {x} {y}", d(*display)),
        Input::LongPress { x, y, ms, display } => {
            format!("input {}swipe {x} {y} {x} {y} {ms}", d(*display))
        }
        Input::Swipe {
            x1,
            y1,
            x2,
            y2,
            ms,
            display,
        } => format!("input {}swipe {x1} {y1} {x2} {y2} {ms}", d(*display)),
        Input::Key(k) => {
            // the name has been through core's keycode(): [A-Z0-9_] only, but never trust it into a shell
            if k.is_empty()
                || !k
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                return Err(TransportError(format!("refusing key name {k:?}")));
            }
            format!("input keyevent {k}")
        }
        // already escaped by core::shell::escape_input_text
        Input::Text(t) => format!("input text {t}"),
        Input::UnicodeText(_) => {
            return Err(TransportError(
                "non-ASCII text needs the helper app, which this live transport does not have yet"
                    .into(),
            ))
        }
    })
}

impl<R: Runner> Transport for AdbTransport<R> {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        let out = self.run(&["devices", "-l"], PROBE_MS)?;
        if !out.ok() {
            return Err(TransportError(format!(
                "adb devices failed: {}",
                out.stderr.trim()
            )));
        }
        let listed: Vec<_> = parse_devices(&out.stdout)
            .into_iter()
            .filter(|l| l.state == "device")
            .collect();
        self.hw.retain(|k, _| listed.iter().any(|l| &l.serial == k));
        let mut v: Vec<DeviceInfo> = Vec::with_capacity(listed.len());
        for l in listed {
            let hw_serial = self.hardware_serial(&l.serial);
            let info = DeviceInfo {
                model: l.model.unwrap_or_else(|| "unknown".into()),
                serial: l.serial,
                hw_serial,
            };
            // one phone can be listed twice (wireless debugging shows an mDNS name and a "(2)" copy,
            // or USB plus Wi-Fi): core would call that ambiguous. Keep one per hardware serial; a
            // plain name beats a "(2)" duplicate, otherwise the first listed wins. An unreadable
            // serial falls back to the adb serial, which is unique anyway.
            match v.iter().position(|d| d.hw_serial == info.hw_serial) {
                None => v.push(info),
                Some(i) if v[i].serial.contains(" (") && !info.serial.contains(" (") => v[i] = info,
                Some(_) => {}
            }
        }
        Ok(v)
    }

    fn screen_size(&mut self, serial: &str) -> Result<(i32, i32), TransportError> {
        let text = self.sh(
            serial,
            "wm size; dumpsys input | grep -m1 SurfaceOrientation",
            SHELL_MS,
        )?;
        let (w, h) = parse_wm_size(&text).ok_or_else(|| {
            TransportError(format!(
                "cannot read the screen size from {:?}",
                text.trim()
            ))
        })?;
        Ok(if parse_quarter_turn(&text) {
            (h, w)
        } else {
            (w, h)
        })
    }

    fn screen_state(&mut self, serial: &str) -> Result<String, TransportError> {
        let text = self.sh(
            serial,
            "dumpsys power | grep -o 'mWakefulness=[A-Za-z]*' | head -1",
            SHELL_MS,
        )?;
        let wake = text
            .trim()
            .strip_prefix("mWakefulness=")
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                TransportError(format!(
                    "cannot read the wakefulness from {:?}",
                    text.trim()
                ))
            })?;
        self.seen
            .entry(serial.to_string())
            .or_default()
            .note_wake(&wake, Instant::now());
        Ok(wake)
    }

    fn screenshot(&mut self, serial: &str, _max_width: i32) -> Result<Screenshot, TransportError> {
        let v: Vec<String> = ["-s", serial, "exec-out", "screencap", "-p"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let out = self.runner.run_bytes(&v, SHOT_MS).map_err(run_err)?;
        if !out.ok() {
            return Err(TransportError(format!(
                "screencap failed: {}",
                out.stderr.trim()
            )));
        }
        let not_png = || {
            TransportError(format!(
                "screencap did not return a PNG ({} bytes)",
                out.stdout.len()
            ))
        };
        let png = png_bytes(&out.stdout).ok_or_else(not_png)?;
        let (w, h) = png_size(png).ok_or_else(not_png)?;
        Ok(Screenshot {
            jpeg: png.to_vec(),
            shot: Shot {
                dev_w: w,
                dev_h: h,
                img_w: w,
                img_h: h,
            },
        })
    }

    fn tree_xml(&mut self, serial: &str, backend: Backend) -> Result<String, TransportError> {
        if backend == Backend::Helper {
            return Err(TransportError(
                "the helper is not installed (this live transport has no helper link yet)".into(),
            ));
        }
        // exec-out, as phone-hands does: a plain `adb shell` has no tty, and on the POCO (HyperOS) the
        // dump to /dev/tty then prints only its one-line message and loses the XML (measured 26 Oct)
        let out = self.run(
            &["-s", serial, "exec-out", "uiautomator", "dump", "/dev/tty"],
            DUMP_MS,
        )?;
        if let Some(why) = adb_level_failure(&out) {
            return Err(TransportError(why));
        }
        match xml_of(&out.stdout) {
            Some(x) => Ok(x.to_string()),
            None => {
                let first: String = format!("{}{}", out.stdout.trim(), out.stderr.trim())
                    .chars()
                    .take(300)
                    .collect();
                if !self.idle_fallback || !first.contains(NOT_IDLE) {
                    return Err(TransportError(first));
                }
                self.idle_free_xml(serial).map_err(|e| {
                    TransportError(format!(
                        "{first}; the idle-free fallback read failed too: {}",
                        e.0
                    ))
                })
            }
        }
    }

    fn input(&mut self, serial: &str, input: &Input) -> Result<(), TransportError> {
        let line = input_line(input)?;
        let text = self.sh(serial, &line, SHELL_MS)?;
        let low = text.to_ascii_lowercase();
        if low.contains("exception") || low.contains("error:") {
            return Err(TransportError(text.trim().to_string()));
        }
        Ok(())
    }

    fn packages(&mut self, serial: &str) -> Result<Vec<String>, TransportError> {
        let text = self.sh(serial, "pm list packages", SHELL_MS)?;
        Ok(text
            .lines()
            .filter_map(|l| l.trim().strip_prefix("package:"))
            .map(str::to_string)
            .collect())
    }

    fn shell(&mut self, serial: &str, line: &str) -> Result<String, TransportError> {
        let text = self.sh(serial, line, SHELL_MS)?;
        // health rides on replies already asked for (a `doctor` battery read, the unlock script); never a probe
        self.seen
            .entry(serial.to_string())
            .or_default()
            .note_reply(line, &text, Instant::now());
        Ok(text)
    }

    fn health_hint(&mut self, serial: &str) -> HealthHint {
        self.seen
            .get(serial)
            .map_or_else(HealthHint::default, |s| s.hint(Instant::now()))
    }

    fn helper_status(&mut self, _serial: &str) -> Result<(), String> {
        Err("no helper app on this live transport yet".into())
    }

    fn push(&mut self, serial: &str, local: &Path, remote: &str) -> Result<(), TransportError> {
        let l = local.to_string_lossy();
        let out = self.run(&["-s", serial, "push", &l, remote], 120_000)?;
        if out.ok() {
            Ok(())
        } else {
            Err(TransportError(format!(
                "adb push failed: {}",
                out.stderr.trim()
            )))
        }
    }

    fn pull(&mut self, serial: &str, remote: &str, local: &Path) -> Result<(), TransportError> {
        let l = local.to_string_lossy();
        let out = self.run(&["-s", serial, "pull", remote, &l], 120_000)?;
        if out.ok() {
            Ok(())
        } else {
            Err(TransportError(format!(
                "adb pull failed: {}",
                out.stderr.trim()
            )))
        }
    }
}

/// The screen lock's phone reader over `inner`: the one runner that may read the protected
/// phone's call state, focused app and keyboard (adb-link's `S24_CALL_STATE_READ` and
/// `S24_FOCUS_READ`; the owner's ruling of 14 Nov 2026, "Allow the two reads (Recommended)"), and
/// it keeps only the parsed state the lock decides on ([`agentroid_core::askscreen::lock_signals`]).
pub fn lock_reader<R: Runner + Send + 'static>(
    inner: R,
) -> impl FnMut(&str, &dyn Fn() -> u64) -> Option<Reading> + Send {
    let read_runner = agentroid_adb_link::guard::GuardedRunner::with_s24_lock_reads(inner);
    move |serial, mono| {
        agentroid_core::askscreen::read_signals_timed(
            &read_runner,
            serial,
            mono,
            agentroid_core::screenlock::LOCK_READ_BUDGET_MS,
        )
        .map(|(signals, power_from, power_to)| Reading {
            signals: agentroid_core::askscreen::lock_signals(signals),
            power_from,
            power_to,
        })
    }
}

/// The screen lock for live adb (13 Nov 2026): no screen-changing call to a real phone unless
/// `ask-screen` gave a live lease or the phone is not in use at the time of the call. It reads the
/// phone through its own guarded runners and the leases from `lease_dir`. There is no switch to
/// turn it off; `bridged --adb` always installs it.
pub fn screen_lock_for_adb(lease_dir: std::path::PathBuf, adb: &Path) -> ScreenLock {
    use agentroid_adb_link::guard::GuardedRunner;
    use agentroid_adb_link::runner::AdbRunner;
    let probe_runner = GuardedRunner::new(AdbRunner::new(adb));
    let origin = Instant::now();
    ScreenLock::new(
        lease_dir,
        Box::new(lock_reader(AdbRunner::new(adb))),
        Box::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        }),
        Box::new(move || u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX)),
    )
    .with_emulator_probe(Box::new(move |serial| {
        agentroid_core::screenlock::probe_emulator(&probe_runner, serial)
    }))
}

/// Put [`screen_lock_for_adb`] on `core`.
pub fn install_screen_lock<T: Transport, C: agentroid_adb_link::runner::Clock>(
    core: &mut agentroid_core::transport::Core<T, C>,
    lease_dir: std::path::PathBuf,
    adb: &Path,
) {
    core.set_screen_lock(screen_lock_for_adb(lease_dir, adb));
}

#[cfg(test)]
mod lock_wiring_tests {
    use super::*;
    use agentroid_core::fake::{FakeClock, FakeDevice};
    use agentroid_core::transport::Core;

    #[test]
    fn the_adb_wiring_installs_the_screen_lock_with_the_lease_folder() {
        let mut core = Core::new(FakeDevice::new(), FakeClock::new());
        assert!(core.screen_lock().is_none());
        let dir = std::env::temp_dir().join("agentroid-lock-wiring-leases");
        // an adb path that does not exist: building the lock runs nothing
        install_screen_lock(
            &mut core,
            dir.clone(),
            Path::new("Z:/no/such/adb/for/this/test.exe"),
        );
        let lock = core.screen_lock().expect("the lock is installed");
        assert_eq!(lock.state_dir(), dir.as_path());
    }

    /// The protected phone as adb would answer the screen lock's reads, recording each line.
    struct ProtectedPhone(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    impl Runner for ProtectedPhone {
        fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
            use agentroid_adb_link::guard::{
                S24_CALL_STATE_READ, S24_FOCUS_READ, S24_KEYBOARD_READ, S24_SERIAL,
            };
            let cmd = a.last().cloned().unwrap_or_default();
            self.0.lock().unwrap().push(cmd.clone());
            let out = match cmd.as_str() {
                "getprop ro.serialno" => format!("{S24_SERIAL}\n"),
                "dumpsys power" => "  mWakefulness=Awake\n  mWakeLockSummary=0x1\n  mLastWakeTime=1 (900000 ms ago)\nDisplay Group 0:\n  lastUserActivityTime=1 (600000 ms ago)\n".into(),
                c if c == S24_CALL_STATE_READ => "  mCallState=0\n".into(),
                c if c == S24_FOCUS_READ => "  mCurrentFocus=Window{1 u0 com.example.notes/com.example.notes.Main}\n    isKeyguardShowing=false\n".into(),
                c if c == S24_KEYBOARD_READ => "  mInputShown=false\n".into(),
                _ => return Err(RunError::Refused("not on the list".into())),
            };
            Ok(RunOutput {
                code: Some(0),
                stdout: out,
                stderr: String::new(),
            })
        }
    }

    /// Review round 4c, the owner's ruling (14 Nov 2026): "Allow the two reads (Recommended)".
    /// The daemon's screen lock reads the protected phone's focused app and keyboard through its
    /// own runner, and keeps the package and the keyboard's yes or no only.
    #[test]
    fn the_screen_lock_reads_the_protected_phones_focused_app_and_keyboard() {
        use agentroid_adb_link::guard::{S24_FOCUS_READ, S24_KEYBOARD_READ};
        let sent = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
        let mut read = lock_reader(ProtectedPhone(sent.clone()));
        let mono = || 0u64;
        let r = read("192.0.2.99:40000", &mono).expect("the protected phone is read");
        assert_eq!(r.signals.focus.as_deref(), Some("com.example.notes"));
        assert_eq!(r.signals.ime_shown, Some(false));
        assert_eq!(r.signals.keyguard, None);
        let sent = sent.lock().unwrap();
        assert!(sent.iter().any(|c| c == S24_FOCUS_READ), "{sent:?}");
        assert!(sent.iter().any(|c| c == S24_KEYBOARD_READ), "{sent:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentroid_adb_link::guard::{GuardedRunner, S24_SERIAL};
    use agentroid_adb_link::runner::BytesOutput;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Log = Rc<RefCell<Vec<Vec<String>>>>;

    /// Scripted adb: answers by the joined argv after `-s <serial>`, records every argv.
    struct Script {
        log: Log,
        hw: String,
        png: Vec<u8>,
        rotation: &'static str,
        fail_shell_with: Option<(i32, &'static str)>,
        devices: String,
        /// `uiautomator dump` answers "could not get idle state" (a running stopwatch, 11 Nov).
        never_idle: bool,
        /// The fallback reader answers without a tree (as if the push had been lost).
        fallback_broken: bool,
    }

    impl Script {
        fn new(log: &Log) -> Script {
            Script {
                log: log.clone(),
                hw: "EMU33X1".into(),
                png: png(1080, 2400),
                rotation: "",
                fail_shell_with: None,
                never_idle: false,
                fallback_broken: false,
                devices: "List of devices attached\nemulator-5554          device product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 device:emu64xa transport_id:1\n".into(),
            }
        }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = PNG_MAGIC.to_vec();
        v.extend([0, 0, 0, 13]);
        v.extend(b"IHDR");
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v.extend([8, 6, 0, 0, 0, 0xff, 0x00, 0x7f, 0x00, 0x80]); // bit depth etc. + bytes that are not UTF-8
        v
    }

    fn ok(stdout: &str) -> RunOutput {
        RunOutput {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    impl Runner for Script {
        fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
            self.log.borrow_mut().push(a.to_vec());
            if a == ["devices", "-l"] {
                return Ok(ok(&self.devices));
            }
            let line = a.last().map(String::as_str).unwrap_or("");
            if let Some((code, msg)) = self.fail_shell_with {
                if line != "getprop ro.serialno" {
                    return Ok(RunOutput {
                        code: Some(code),
                        stdout: String::new(),
                        stderr: msg.into(),
                    });
                }
            }
            Ok(if line == "getprop ro.serialno" {
                ok(&format!("{}\n", self.hw))
            } else if line.starts_with("wm size") {
                // the rotation line is the answer to the `dumpsys input` half of the command only
                let rot = if line.contains("dumpsys input | grep -m1 SurfaceOrientation") {
                    self.rotation
                } else {
                    ""
                };
                ok(&format!("Physical size: 1080x2400\n{rot}\n"))
            } else if line.starts_with("dumpsys power") {
                ok("mWakefulness=Awake\n")
            } else if a.iter().any(|x| x == "exec-out") && a.iter().any(|x| x == "uiautomator") {
                if self.never_idle {
                    ok("ERROR: could not get idle state.\n")
                } else {
                    ok("<?xml version='1.0'?><hierarchy rotation=\"0\"><node class=\"a\" bounds=\"[0,0][10,10]\"/></hierarchy>UI hierchary dumped to: /dev/tty")
                }
            } else if line.starts_with("CLASSPATH=") {
                if self.fallback_broken {
                    ok("Error: Could not find or load main class dev.agentroid.dump.Dump\n")
                } else {
                    ok("<?xml version='1.0'?><hierarchy rotation=\"0\"><node class=\"b\" content-desc=\"Pause\" bounds=\"[0,0][10,10]\"/></hierarchy>agentroid-dump: not-idle\n")
                }
            } else if line == "pm list packages" {
                ok("package:com.android.settings\r\npackage:org.tasks\r\n")
            } else if line == "dumpsys battery" {
                // the POCO's reply, 11 Nov 2026 (trimmed)
                ok("Current Battery Service state:\n  AC powered: false\n  USB powered: true\n  Wireless powered: false\n  Dock powered: false\n  level: 98\n  scale: 100\n  temperature: 315\n")
            } else if line.starts_with("echo before:") {
                ok("before:mWakefulness=Dozing\nafter:mWakefulness=Awake isKeyguardShowing=false\ndisabled:true\n")
            } else {
                ok("")
            })
        }
        fn run_bytes(&self, a: &[String], _t: u64) -> Result<BytesOutput, RunError> {
            self.log.borrow_mut().push(a.to_vec());
            Ok(BytesOutput {
                code: Some(0),
                stdout: self.png.clone(),
                stderr: String::new(),
            })
        }
    }

    fn live(s: Script) -> AdbTransport<Script> {
        AdbTransport::new(s)
    }

    fn lines(log: &Log) -> Vec<String> {
        log.borrow().iter().map(|a| a.join(" ")).collect()
    }

    #[test]
    fn devices_lists_online_phones_with_their_hardware_serial_and_caches_it() {
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.devices
            .push_str("0123456789ABCDEF       offline transport_id:2\n");
        let mut t = live(s);
        let d = t.devices().unwrap();
        assert_eq!(d.len(), 1, "an offline phone is not listed");
        assert_eq!(
            (
                d[0].serial.as_str(),
                d[0].hw_serial.as_str(),
                d[0].model.as_str()
            ),
            ("emulator-5554", "EMU33X1", "sdk gphone64 x86 64")
        );
        t.devices().unwrap();
        let probes = lines(&log)
            .iter()
            .filter(|l| l.ends_with("getprop ro.serialno"))
            .count();
        assert_eq!(
            probes,
            1,
            "the second listing reuses the cached serial: {:?}",
            lines(&log)
        );
    }

    #[test]
    fn one_phone_listed_twice_is_one_device() {
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.devices = "List of devices attached\nadb-ABC-x (2)._adb-tls-connect._tcp\tdevice\nadb-ABC-x._adb-tls-connect._tcp\tdevice\n".into();
        let d = live(s).devices().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].serial, "adb-ABC-x._adb-tls-connect._tcp",
            "the plain name beats the (2) copy"
        );
        // two different phones stay two
        let mut s = Script::new(&log);
        s.devices =
            "List of devices attached\nemulator-5554\tdevice\nemulator-5556\tdevice\n".into();
        s.hw = "SAME".into();
        assert_eq!(
            live(s).devices().unwrap().len(),
            1,
            "same hardware serial = same phone"
        );
        let mut t = live(Script::new(&log));
        t.runner.devices =
            "List of devices attached\nemulator-5554\tdevice\n0123456789\tdevice\n".into();
        // the scripted phone answers every getprop with the same serial, so make the second differ
        t.hw.insert("0123456789".into(), "OTHER".into());
        assert_eq!(t.devices().unwrap().len(), 2);
    }

    #[test]
    fn the_cache_forgets_a_phone_that_left_the_list() {
        let log: Log = Default::default();
        let mut t = live(Script::new(&log));
        t.devices().unwrap();
        t.runner.devices = "List of devices attached\n".into();
        assert!(t.devices().unwrap().is_empty());
        t.runner.devices = Script::new(&log).devices;
        t.devices().unwrap();
        assert_eq!(
            lines(&log)
                .iter()
                .filter(|l| l.ends_with("getprop ro.serialno"))
                .count(),
            2
        );
    }

    #[test]
    fn every_command_names_its_target_with_dash_s() {
        let log: Log = Default::default();
        let mut t = live(Script::new(&log));
        t.devices().unwrap();
        t.screen_size("emulator-5554").unwrap();
        t.screen_state("emulator-5554").unwrap();
        t.screenshot("emulator-5554", 720).unwrap();
        t.tree_xml("emulator-5554", Backend::Uiautomator).unwrap();
        t.input(
            "emulator-5554",
            &Input::Tap {
                x: 1,
                y: 2,
                display: 0,
            },
        )
        .unwrap();
        t.packages("emulator-5554").unwrap();
        t.shell("emulator-5554", "echo hi").unwrap();
        for a in log.borrow().iter().filter(|a| a[0] != "devices") {
            assert_eq!(&a[..2], ["-s", "emulator-5554"], "{a:?}");
        }
    }

    #[test]
    fn screen_size_prefers_override_and_swaps_on_a_quarter_turn() {
        assert_eq!(
            parse_wm_size("Physical size: 1080x2400\nOverride size: 720x1600\n"),
            Some((720, 1600))
        );
        assert_eq!(
            parse_wm_size("Physical size: 1080x2400\n"),
            Some((1080, 2400))
        );
        assert_eq!(parse_wm_size("garbage"), None);
        assert!(parse_quarter_turn("mCurrentRotation=ROTATION_90"));
        assert!(parse_quarter_turn("mCurrentRotation=ROTATION_270"));
        assert!(parse_quarter_turn("mRotation=1"));
        assert!(parse_quarter_turn("  SurfaceOrientation: 1"));
        assert!(parse_quarter_turn("  SurfaceOrientation: 3"));
        assert!(!parse_quarter_turn("  SurfaceOrientation: 0"));
        assert!(!parse_quarter_turn("  SurfaceOrientation: 2"));
        assert!(!parse_quarter_turn("mCurrentRotation=ROTATION_0"));
        assert!(!parse_quarter_turn("mCurrentRotation=ROTATION_180"));
        assert!(!parse_quarter_turn("nothing"));
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.rotation = "  SurfaceOrientation: 1";
        assert_eq!(live(s).screen_size("emulator-5554").unwrap(), (2400, 1080));
        assert_eq!(
            live(Script::new(&log))
                .screen_size("emulator-5554")
                .unwrap(),
            (1080, 2400)
        );
    }

    #[test]
    fn health_hint_is_empty_until_a_reply_carries_health() {
        let log: Log = Default::default();
        let mut t = live(Script::new(&log));
        assert!(t.health_hint("emulator-5554").is_empty());
        t.shell("emulator-5554", "getprop ro.product.model")
            .unwrap();
        assert!(t.health_hint("emulator-5554").is_empty());
        // reading the hint sends nothing
        let before = log.borrow().len();
        let _ = t.health_hint("emulator-5554");
        assert_eq!(log.borrow().len(), before);
    }

    #[test]
    fn a_battery_read_fills_battery_power_and_temperature() {
        let log: Log = Default::default();
        let mut t = live(Script::new(&log));
        t.shell("emulator-5554", "dumpsys battery").unwrap();
        let h = t.health_hint("emulator-5554");
        assert_eq!(h.battery, Some(98));
        assert_eq!(h.powered, Some(true));
        assert_eq!(h.temp_c, Some(31.5));
        assert!(h.age_s >= 0.0 && h.age_s < 5.0);
        // another phone's hint is untouched
        assert!(t.health_hint("emulator-5556").is_empty());
    }

    #[test]
    fn an_unlock_refreshes_screen_and_lock() {
        let log: Log = Default::default();
        let mut t = live(Script::new(&log));
        t.shell(
            "emulator-5554",
            &agentroid_core::shell::unlock_script(1080, 2400),
        )
        .unwrap();
        let h = t.health_hint("emulator-5554");
        assert_eq!(h.screen.as_deref(), Some("on"));
        assert_eq!(h.lock.as_deref(), Some("unlocked"));
    }

    #[test]
    fn battery_text_scales_and_reads_unpowered() {
        let mut s = Seen::default();
        let at = Instant::now();
        s.note_battery(
            "  AC powered: false\n  USB powered: false\n  level: 40\n  scale: 50\n  temperature: -12\n",
            at,
        );
        let h = s.hint(at);
        assert_eq!(h.battery, Some(80));
        assert_eq!(h.powered, Some(false));
        assert_eq!(h.temp_c, Some(-1.2));
        let mut w = Seen::default();
        w.note_wake("Dozing", at);
        assert_eq!(w.hint(at).screen.as_deref(), Some("off"));
    }

    #[test]
    fn screen_state_is_the_wakefulness_word() {
        let log: Log = Default::default();
        assert_eq!(
            live(Script::new(&log))
                .screen_state("emulator-5554")
                .unwrap(),
            "Awake"
        );
    }

    #[test]
    fn screenshots_are_binary_safe_and_sized_from_the_png_header() {
        let log: Log = Default::default();
        let mut t = live(Script::new(&log));
        let s = t.screenshot("emulator-5554", 540).unwrap();
        assert_eq!(
            (s.shot.dev_w, s.shot.dev_h, s.shot.img_w, s.shot.img_h),
            (1080, 2400, 1080, 2400)
        );
        assert_eq!(s.jpeg, png(1080, 2400), "bytes 0x00 and 0xff survive");
        assert!(lines(&log)
            .iter()
            .any(|l| l == "-s emulator-5554 exec-out screencap -p"));
        let mut bad = Script::new(&log);
        bad.png = b"Display not found".to_vec();
        assert!(live(bad)
            .screenshot("emulator-5554", 540)
            .unwrap_err()
            .0
            .contains("not return a PNG"));
        assert_eq!(png_size(&png(0, 5)), None);
    }

    #[test]
    fn a_warning_in_front_of_the_png_is_cut_off() {
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        let mut bytes =
            b"[Warning] Multiple displays were found, but no display id was specified!\n".to_vec();
        bytes.extend(png(1080, 2400));
        s.png = bytes;
        let shot = live(s).screenshot("emulator-5554", 540).unwrap();
        assert_eq!(shot.jpeg, png(1080, 2400));
        assert_eq!((shot.shot.dev_w, shot.shot.dev_h), (1080, 2400));
    }

    #[test]
    fn an_adb_error_after_a_daemon_started_line_is_still_an_error() {
        let out = |stderr: &str| RunOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.into(),
        };
        assert!(adb_level_failure(&out("* daemon not running; starting now at tcp:5037\n* daemon started successfully\nerror: device 'x' not found")).is_some());
        assert!(adb_level_failure(&out("error: device offline")).is_some());
        assert!(
            adb_level_failure(&out("")).is_none(),
            "a plain non-zero exit from the phone's command is not an adb failure"
        );
        assert!(adb_level_failure(&RunOutput {
            code: Some(0),
            stdout: String::new(),
            stderr: "error: x".into()
        })
        .is_none());
    }

    #[test]
    fn a_screen_that_never_goes_idle_is_read_by_the_fallback_pushed_once() {
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.never_idle = true;
        let mut t = live(s);
        let x = t.tree_xml("emulator-5554", Backend::Uiautomator).unwrap();
        assert!(
            x.contains("content-desc=\"Pause\"") && x.ends_with("</hierarchy>"),
            "{x}"
        );
        t.tree_xml("emulator-5554", Backend::Uiautomator).unwrap();
        let l = lines(&log);
        let pushes: Vec<_> = l.iter().filter(|x| x.contains(" push ")).collect();
        assert_eq!(pushes.len(), 1, "pushed once per device per process: {l:?}");
        assert!(
            pushes[0].ends_with("/data/local/tmp/agentroid-dump.dex"),
            "{}",
            pushes[0]
        );
        assert_eq!(
            l.iter()
                .filter(|x| x.contains("app_process /system/bin dev.agentroid.dump.Dump"))
                .count(),
            2
        );
    }

    #[test]
    fn the_fallback_runs_only_for_the_idle_failure_and_both_messages_are_kept() {
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.never_idle = true;
        s.fallback_broken = true;
        let e = live(s)
            .tree_xml("emulator-5554", Backend::Uiautomator)
            .unwrap_err()
            .0;
        assert!(
            e.contains("could not get idle state")
                && e.contains("fallback read failed too")
                && e.contains("Could not find"),
            "{e}"
        );
        // an ordinary dump never touches the fallback
        let log2: Log = Default::default();
        live(Script::new(&log2))
            .tree_xml("emulator-5554", Backend::Uiautomator)
            .unwrap();
        assert!(
            !lines(&log2)
                .iter()
                .any(|x| x.contains("push") || x.contains("app_process")),
            "{:?}",
            lines(&log2)
        );
    }

    #[test]
    fn with_the_fallback_off_a_busy_screen_fails_as_it_did_in_run_2() {
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.never_idle = true;
        let e = live(s)
            .without_idle_fallback()
            .tree_xml("emulator-5554", Backend::Uiautomator)
            .unwrap_err()
            .0;
        assert!(
            e.contains("could not get idle state") && !e.contains("fallback"),
            "{e}"
        );
        assert!(
            !lines(&log)
                .iter()
                .any(|x| x.contains("push") || x.contains("app_process")),
            "{:?}",
            lines(&log)
        );
    }

    #[test]
    fn the_embedded_fallback_reader_is_a_dex() {
        assert_eq!(
            &IDLE_FREE_DEX[..4],
            b"dex\n",
            "rebuild it with assets/idle-free-dump/build.sh"
        );
        // the round-2 heads-up mode is gone from the helper (review round 3)
        let needle = b"--heads-up";
        assert!(
            !IDLE_FREE_DEX.windows(needle.len()).any(|w| w == needle),
            "rebuild it with assets/idle-free-dump/build.sh"
        );
        assert_eq!(embedded_helper_hash().len(), 64);
    }

    /// The helper's runner (it must be `Send`): records its argv, answers each hash read with the
    /// next of `hashes` (then the last one), and every push with `push_ok`.
    struct HelperSpy {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        hashes: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        push_ok: bool,
    }

    impl Runner for HelperSpy {
        fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
            self.seen.lock().unwrap().push(a.to_vec());
            if a.iter().any(|x| x == "push") {
                return Ok(RunOutput {
                    code: Some(if self.push_ok { 0 } else { 1 }),
                    stdout: String::new(),
                    stderr: if self.push_ok { "" } else { "no room" }.into(),
                });
            }
            let mut h = self.hashes.lock().unwrap();
            let next = if h.len() > 1 {
                h.remove(0)
            } else {
                h[0].clone()
            };
            Ok(ok(&next))
        }
    }

    type HelperLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>;

    fn staged_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("agentroid-live-{}-{name}.dex", std::process::id()))
    }

    fn sum_line(h: &str) -> String {
        format!("{h}  {IDLE_FREE_REMOTE}\n")
    }

    /// A transport aimed at the protected phone (its `ro.serialno` is the protected serial), on a
    /// screen that never goes idle, with the helper wired to answer `hashes` in turn.
    fn protected_rig(
        name: &str,
        hashes: &[String],
        push_ok: bool,
    ) -> (AdbTransport<Script>, Log, HelperLog, std::path::PathBuf) {
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.never_idle = true;
        s.hw = S24_SERIAL.into();
        let seen: HelperLog = Default::default();
        let staged = staged_path(name);
        let _ = std::fs::remove_file(&staged);
        let t = live(s).with_dump_helper(
            Box::new(HelperSpy {
                seen: seen.clone(),
                hashes: std::sync::Arc::new(std::sync::Mutex::new(hashes.to_vec())),
                push_ok,
            }),
            staged.clone(),
        );
        (t, log, seen, staged)
    }

    fn hash_reads(seen: &HelperLog) -> usize {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|a| {
                a.last().map(String::as_str)
                    == Some(agentroid_adb_link::guard::DUMP_HELPER_HASH_READ)
            })
            .count()
    }

    fn helper_runs(log: &Log) -> usize {
        lines(log)
            .iter()
            .filter(|x| x.contains("app_process /system/bin dev.agentroid.dump.Dump"))
            .count()
    }

    #[test]
    fn the_helper_hash_is_read_strictly() {
        let h = "a".repeat(64);
        assert_eq!(parse_helper_hash(&sum_line(&h)), Some(h.clone()));
        assert_eq!(
            parse_helper_hash(&format!("{h} {IDLE_FREE_REMOTE}\r\nmore\n")),
            Some(h.clone())
        );
        for junk in [
            String::new(),
            format!("{}  {IDLE_FREE_REMOTE}\n", "A".repeat(64)),
            format!("{}  {IDLE_FREE_REMOTE}\n", "a".repeat(63)),
            format!("{}  {IDLE_FREE_REMOTE}\n", "g".repeat(64)),
            format!("{h}  /sdcard/other.dex\n"),
            format!("{h}\n"),
            format!("{h}  {IDLE_FREE_REMOTE} extra\n"),
            format!("sha256sum: {IDLE_FREE_REMOTE}: No such file or directory\n"),
            format!("\n{h}  {IDLE_FREE_REMOTE}\n"),
        ] {
            assert_eq!(parse_helper_hash(&junk), None, "{junk:?}");
        }
    }

    /// Review round 3, item 2: on the protected phone the helper runs only after its hash matched
    /// the daemon's copy, read through the helper's own runner; a match holds for
    /// [`HELPER_RECHECK`], and nothing goes through the main runner but the run itself.
    #[test]
    fn the_protected_phone_runs_the_helper_only_once_its_hash_matches() {
        let want = embedded_helper_hash().to_string();
        let (mut t, log, seen, staged) = protected_rig("match", &[sum_line(&want)], true);
        t.tree_xml("emulator-5554", Backend::Uiautomator).unwrap();
        assert_eq!(hash_reads(&seen), 1);
        assert_eq!(helper_runs(&log), 1);
        assert!(!lines(&log).iter().any(|x| x.contains(" push ")));
        assert!(!seen
            .lock()
            .unwrap()
            .iter()
            .any(|a| a.iter().any(|x| x == "push")));
        assert!(
            !staged.exists(),
            "nothing is staged while the phone's copy matches"
        );
        // within the window: no second read
        t.tree_xml("emulator-5554", Backend::Uiautomator).unwrap();
        assert_eq!(hash_reads(&seen), 1);
        assert_eq!(helper_runs(&log), 2);
        // past it: read again
        let old = Instant::now().checked_sub(HELPER_RECHECK).unwrap();
        t.helper_checked.insert("emulator-5554".into(), old);
        t.tree_xml("emulator-5554", Backend::Uiautomator).unwrap();
        assert_eq!(hash_reads(&seen), 2);
        assert_eq!(
            seen.lock().unwrap()[0],
            [
                "-s",
                "emulator-5554",
                "shell",
                agentroid_adb_link::guard::DUMP_HELPER_HASH_READ
            ]
        );
    }

    #[test]
    fn a_mismatched_helper_is_pushed_again_from_the_state_dir_and_checked_again() {
        let want = embedded_helper_hash().to_string();
        let other = sum_line(&"0".repeat(64));
        let (mut t, log, seen, staged) = protected_rig("mismatch", &[other, sum_line(&want)], true);
        t.tree_xml("emulator-5554", Backend::Uiautomator).unwrap();
        let s = seen.lock().unwrap().clone();
        assert_eq!(s.len(), 3, "{s:?}");
        assert_eq!(
            s[1],
            [
                "-s".to_string(),
                "emulator-5554".into(),
                "push".into(),
                staged.to_string_lossy().into_owned(),
                IDLE_FREE_REMOTE.into()
            ]
        );
        assert_eq!(std::fs::read(&staged).unwrap(), IDLE_FREE_DEX);
        assert_eq!(helper_runs(&log), 1);
        assert!(
            !lines(&log).iter().any(|x| x.contains(" push ")),
            "never through the main runner"
        );
        let _ = std::fs::remove_file(&staged);
    }

    #[test]
    fn a_helper_that_still_does_not_match_or_cannot_be_read_is_never_run() {
        let other = sum_line(&"0".repeat(64));
        // a second mismatch after the push
        let (mut t, log, seen, staged) = protected_rig("twice", std::slice::from_ref(&other), true);
        let e = t
            .tree_xml("emulator-5554", Backend::Uiautomator)
            .unwrap_err()
            .0;
        assert!(e.contains("does not match"), "{e}");
        assert_eq!(hash_reads(&seen), 2);
        assert_eq!(helper_runs(&log), 0);
        // and the next call checks again (no match was recorded)
        let _ = t.tree_xml("emulator-5554", Backend::Uiautomator);
        assert_eq!(hash_reads(&seen), 4);
        let _ = std::fs::remove_file(&staged);
        // a failed push
        let (mut t, log, seen, staged) = protected_rig("nopush", &[other], false);
        let e = t
            .tree_xml("emulator-5554", Backend::Uiautomator)
            .unwrap_err()
            .0;
        assert!(e.contains("push of the fallback reader failed"), "{e}");
        assert_eq!(hash_reads(&seen), 1);
        assert_eq!(helper_runs(&log), 0);
        let _ = std::fs::remove_file(&staged);
        // not wired: refused, nothing pushed or run
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.never_idle = true;
        s.hw = S24_SERIAL.into();
        let e = live(s)
            .tree_xml("emulator-5554", Backend::Uiautomator)
            .unwrap_err()
            .0;
        assert!(e.contains("not wired"), "{e}");
        assert!(!lines(&log)
            .iter()
            .any(|x| x.contains(" push ") || x.contains("app_process")));
    }

    #[test]
    fn an_ordinary_phone_gets_the_staged_copy_through_the_main_runner() {
        let (t, log, seen, staged) = protected_rig("poco", &[String::new()], true);
        let mut t = t;
        // the same rig, aimed at an ordinary phone
        t.runner.hw = "EMU33X1".into();
        t.tree_xml("emulator-5554", Backend::Uiautomator).unwrap();
        t.tree_xml("emulator-5554", Backend::Uiautomator).unwrap();
        let pushes: Vec<_> = lines(&log)
            .into_iter()
            .filter(|x| x.contains(" push "))
            .collect();
        assert_eq!(pushes.len(), 1, "{pushes:?}");
        assert!(
            pushes[0].contains(&*staged.to_string_lossy()),
            "{}",
            pushes[0]
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "the helper's runner is not used"
        );
        assert_eq!(helper_runs(&log), 2);
        let _ = std::fs::remove_file(&staged);
    }

    #[test]
    fn staging_writes_only_when_the_copy_differs() {
        let p = staged_path("stage");
        let _ = std::fs::remove_file(&p);
        stage_helper(&p).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), IDLE_FREE_DEX);
        std::fs::write(&p, b"tampered").unwrap();
        stage_helper(&p).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), IDLE_FREE_DEX);
        let before = std::fs::metadata(&p).unwrap().modified().unwrap();
        stage_helper(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), before);
        assert!(!p.with_extension("dex.tmp").exists());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_tree_is_cut_out_of_the_dump_message() {
        let log: Log = Default::default();
        let x = live(Script::new(&log))
            .tree_xml("emulator-5554", Backend::Uiautomator)
            .unwrap();
        assert!(
            x.starts_with("<hierarchy") && x.ends_with("</hierarchy>"),
            "{x}"
        );
        assert!(
            lines(&log)
                .iter()
                .any(|l| l == "-s emulator-5554 exec-out uiautomator dump /dev/tty"),
            "a plain `shell` has no tty and the POCO then loses the XML: {:?}",
            lines(&log)
        );
        assert!(live(Script::new(&log))
            .tree_xml("emulator-5554", Backend::Helper)
            .unwrap_err()
            .0
            .contains("helper"));
    }

    #[test]
    fn input_commands_have_the_exact_shapes_the_phone_expects() {
        let line = |i: Input| input_line(&i).unwrap();
        assert_eq!(
            line(Input::Tap {
                x: 10,
                y: 20,
                display: 0
            }),
            "input tap 10 20"
        );
        assert_eq!(
            line(Input::Tap {
                x: 10,
                y: 20,
                display: 3
            }),
            "input -d 3 tap 10 20"
        );
        assert_eq!(
            line(Input::LongPress {
                x: 5,
                y: 6,
                ms: 800,
                display: 0
            }),
            "input swipe 5 6 5 6 800"
        );
        assert_eq!(
            line(Input::Swipe {
                x1: 1,
                y1: 2,
                x2: 3,
                y2: 4,
                ms: 350,
                display: 0
            }),
            "input swipe 1 2 3 4 350"
        );
        assert_eq!(
            line(Input::Key("KEYCODE_BACK".into())),
            "input keyevent KEYCODE_BACK"
        );
        assert_eq!(line(Input::Text("a%sb".into())), "input text a%sb");
        assert!(input_line(&Input::Key("BACK; rm -rf /".into())).is_err());
        assert!(input_line(&Input::Key(String::new())).is_err());
        assert!(input_line(&Input::UnicodeText("東京".into())).is_err());
    }

    #[test]
    fn a_phone_side_refusal_comes_back_as_an_error_but_a_plain_nonzero_exit_does_not() {
        struct Refuse(Log);
        impl Runner for Refuse {
            fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
                self.0.borrow_mut().push(a.to_vec());
                Ok(ok(
                    "java.lang.SecurityException: Injecting input events requires INJECT_EVENTS\n",
                ))
            }
        }
        let log: Log = Default::default();
        let e = AdbTransport::new(Refuse(log.clone()))
            .input(
                "emulator-5554",
                &Input::Tap {
                    x: 1,
                    y: 1,
                    display: 0,
                },
            )
            .unwrap_err();
        assert!(e.0.contains("SecurityException"));
        // grep finding nothing: exit 1, empty output, no adb-level message: still Ok("")
        let mut s = Script::new(&log);
        s.fail_shell_with = Some((1, ""));
        assert_eq!(
            live(s).shell("emulator-5554", "echo x | grep y").unwrap(),
            ""
        );
        // adb cannot reach the phone: an error
        let mut s = Script::new(&log);
        s.fail_shell_with = Some((1, "error: device offline"));
        assert!(live(s)
            .shell("emulator-5554", "echo x")
            .unwrap_err()
            .0
            .contains("offline"));
        let mut s = Script::new(&log);
        s.fail_shell_with = Some((1, "adb: no devices/emulators found"));
        assert!(live(s).shell("emulator-5554", "echo x").is_err());
    }

    #[test]
    fn packages_strip_the_prefix_and_carriage_returns() {
        let log: Log = Default::default();
        assert_eq!(
            live(Script::new(&log)).packages("emulator-5554").unwrap(),
            ["com.android.settings", "org.tasks"]
        );
    }

    #[test]
    fn behind_the_guard_the_s24_is_never_sent_an_input_or_a_screenshot() {
        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.hw = S24_SERIAL.into();
        s.devices =
            "List of devices attached\n192.0.2.99:40000   device model:SM_S921B transport_id:1\n"
                .into();
        let mut t = AdbTransport::new(GuardedRunner::new(s));
        let d = t.devices().unwrap();
        assert_eq!(
            d[0].hw_serial, S24_SERIAL,
            "it is listed (so core can refuse it by name) ..."
        );
        let ip = "192.0.2.99:40000";
        assert!(t
            .input(
                ip,
                &Input::Tap {
                    x: 1,
                    y: 1,
                    display: 0
                }
            )
            .is_err());
        assert!(t.screenshot(ip, 540).is_err());
        assert!(t.tree_xml(ip, Backend::Uiautomator).is_err());
        assert!(
            t.shell(ip, "dumpsys window").is_err(),
            "an unapproved read is refused"
        );
        assert!(
            t.shell(ip, "dumpsys battery").is_ok(),
            "one of the 11 approved reads still passes"
        );
        // ... but nothing but the identity probe and the device list ever left
        for l in lines(&log) {
            assert!(
                l == "devices -l"
                    || l.ends_with("shell getprop ro.serialno")
                    || l.ends_with("shell dumpsys battery"),
                "sent to the protected phone: {l}"
            );
        }
    }

    /// The guard and core together: core's lease binds the adb name it checked, the guard lets
    /// that name (and only that name) through, with the protected-phone policy still on.
    #[test]
    fn behind_the_guard_one_allow_lets_core_act_on_the_protected_phone_and_nothing_else_rides_on_it(
    ) {
        use agentroid_adb_link::lease::{Holder, LeaseBook, ManualClock};
        use agentroid_core::approval::{Approver, Channel, Inbound, Request};
        use agentroid_core::fake::FakeClock;
        use agentroid_core::gate::{Call, Refusal, Target};
        use agentroid_core::protected::{Protected, ProtectedConfig};
        use agentroid_core::transport::{Core, CoreError};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Box2 {
            sent: Vec<Request>,
            events: Vec<Inbound>,
        }
        struct Ch(Arc<Mutex<Box2>>);
        impl Channel for Ch {
            fn name(&self) -> &'static str {
                "test"
            }
            fn send(&mut self, req: &Request) -> Result<(), String> {
                self.0.lock().unwrap().sent.push(req.clone());
                Ok(())
            }
            fn poll(&mut self, _: &Request) -> Result<Vec<Inbound>, String> {
                Ok(self.0.lock().unwrap().events.clone())
            }
        }

        let log: Log = Default::default();
        let mut s = Script::new(&log);
        s.hw = S24_SERIAL.into();
        s.devices = "List of devices attached
192.0.2.99:40000   device model:SM_S921B transport_id:1
"
        .into();
        let ip = "192.0.2.99:40000";
        let wall = Arc::new(ManualClock::new(1_800_000_000_000));
        let book = Arc::new(LeaseBook::new(wall.clone()));
        let mut prot = Protected::new(ProtectedConfig::new(30, 10, None), book.clone());
        prot.set_holders(Box::new(|_| {
            Some(Holder {
                name: "lane-a".into(),
                epoch: 1,
            })
        }));
        let inbox = Arc::new(Mutex::new(Box2::default()));
        let mut core = Core::new(
            AdbTransport::new(GuardedRunner::with_leases(s, book.clone())),
            FakeClock::new(),
        );
        core.set_approver(Approver::new(Box::new(Ch(inbox.clone()))));
        core.set_protected(prot);
        let tap = Call::Tap {
            target: Target::Point {
                x: 5.0,
                y: 5.0,
                space: Some("device".into()),
                image_width: None,
            },
        };
        let acted = |log: &Log| {
            lines(log)
                .into_iter()
                .filter(|l| l.contains("input tap"))
                .count()
        };
        // no lease: an ask, and nothing acts
        let r = core.call(Some(ip), &tap);
        assert!(
            matches!(r, Err(CoreError::Refused(Refusal::LeasePending { .. }))),
            "{r:?}"
        );
        assert_eq!(acted(&log), 0);
        // the owner allows: core binds the name it checked, the guard lets the tap through
        let id = inbox.lock().unwrap().sent[0].id.clone();
        inbox.lock().unwrap().events.push(Inbound {
            event_id: "ev1".into(),
            time_ms: 1_800_000_000_000,
            message: format!("allow {id}"),
        });
        core.call(Some(ip), &tap).expect("the lease admits the tap");
        assert_eq!(acted(&log), 1, "{:?}", lines(&log));
        assert!(book.admits(ip));
        // another name for the protected phone never rides on the lease, nor does a refused form
        let other = core
            .transport
            .run(&["-s", "192.0.2.98:40000", "shell", "input tap 1 1"], 1000);
        assert!(other.is_err(), "an unbound name is refused: {other:?}");
        let reboot = core.transport.run(&["-s", ip, "shell", "reboot"], 1000);
        assert!(
            reboot.is_err(),
            "the protected policy holds under the lease"
        );
        let usb = core.transport.run(&["-s", ip, "usb"], 1000);
        assert!(usb.is_err(), "only shell, exec-out, push and pull");
        // the lease ends: the guard refuses again, whatever core does
        book.close_epoch(1, "released");
        assert!(!book.admits(ip));
        let after = core
            .transport
            .run(&["-s", ip, "shell", "input tap 1 1"], 1000);
        assert!(after.is_err());
        assert_eq!(acted(&log), 1, "{:?}", lines(&log));
    }
}
