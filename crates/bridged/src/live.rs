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
//! * `health_hint` says nothing (health rides on replies, and no reply carries it yet).

use agentroid_adb_link::parse::parse_devices;
use agentroid_adb_link::runner::{RunError, RunOutput, Runner};
use agentroid_core::frame::Shot;
use agentroid_core::transport::{
    Backend, DeviceInfo, Input, Screenshot, Transport, TransportError,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;

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
const IDLE_FREE_REMOTE: &str = "/data/local/tmp/agentroid-dump.dex";
const IDLE_FREE_CMD: &str = "CLASSPATH=/data/local/tmp/agentroid-dump.dex:/system/framework/uiautomator.jar app_process /system/bin dev.agentroid.dump.Dump /dev/tty";
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
}

impl<R: Runner> AdbTransport<R> {
    pub fn new(runner: R) -> Self {
        let off = std::env::var_os("AGENTROID_NO_IDLE_FALLBACK").is_some_and(|v| v == "1");
        AdbTransport {
            runner,
            hw: HashMap::new(),
            dex_on: HashSet::new(),
            idle_fallback: !off,
        }
    }

    /// The same transport without the idle-free fallback (what `AGENTROID_NO_IDLE_FALLBACK=1` gives).
    pub fn without_idle_fallback(mut self) -> Self {
        self.idle_fallback = false;
        self
    }

    /// The fallback read (see [`IDLE_FREE_DEX`]): pushes the reader once per device, then runs it.
    fn idle_free_xml(&mut self, serial: &str) -> Result<String, TransportError> {
        if !self.dex_on.contains(serial) {
            let local =
                std::env::temp_dir().join(format!("agentroid-dump-{}.dex", std::process::id()));
            std::fs::write(&local, IDLE_FREE_DEX)
                .map_err(|e| TransportError(format!("could not stage the fallback reader: {e}")))?;
            let pushed = self.push(serial, &local, IDLE_FREE_REMOTE);
            let _ = std::fs::remove_file(&local);
            pushed?;
            self.dex_on.insert(serial.to_string());
        }
        let out = self.run(&["-s", serial, "exec-out", IDLE_FREE_CMD], DUMP_MS)?;
        if let Some(why) = adb_level_failure(&out) {
            return Err(TransportError(why));
        }
        match xml_of(&out.stdout) {
            Some(x) => Ok(x.to_string()),
            None => {
                // a phone that lost the file (rebooted, /data/local/tmp cleaned) is pushed again next time
                self.dex_on.remove(serial);
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
        text.trim()
            .strip_prefix("mWakefulness=")
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                TransportError(format!(
                    "cannot read the wakefulness from {:?}",
                    text.trim()
                ))
            })
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
        self.sh(serial, line, SHELL_MS)
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
            t.shell(ip, "dumpsys power").is_err(),
            "an unapproved read is refused"
        );
        assert!(
            t.shell(ip, "dumpsys battery").is_ok(),
            "one of the 10 approved reads still passes"
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
}
