//! The only road from this crate to a phone: [`AdbPhone`] wraps adb-link's [`GuardedRunner`]
//! (concretely, not a `dyn Runner`), so every command passes the deny-by-default guard and the
//! S24 identity check. Every command line is built here from validated parts; nothing a script or
//! a model wrote reaches the phone's shell through this crate.

use agentroid_adb_link::guard::GuardedRunner;
use agentroid_adb_link::runner::{RunError, RunOutput, Runner};

use crate::health::{self, BatteryInfo, LockState};
use crate::heat::Screen;
use crate::CareError;

/// Per-command timeout.
pub const ADB_TIMEOUT_MS: u64 = 15_000;
/// The settings namespaces a lease may touch (phone-guard `NAMESPACES`).
pub const NAMESPACES: [&str; 3] = ["system", "secure", "global"];
/// `before` of a key that did not exist (phone-guard's sentinel, shared format).
pub const ABSENT: &str = "ABSENT";

/// phone-guard `KEY_RE` `^[A-Za-z0-9_.:-]+$`, plus: starts alphanumeric (never read as an option), ≤ 128.
pub fn valid_key(key: &str) -> bool {
    (1..=128).contains(&key.len())
        && key.starts_with(|c: char| c.is_ascii_alphanumeric())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// Namespace whitelist and key pattern: nothing else reaches the phone's shell as a name (S3).
pub fn check_target(ns: &str, key: &str) -> Result<(), CareError> {
    if !NAMESPACES.contains(&ns) {
        return Err(CareError::Invalid(format!(
            "settings namespace must be one of {NAMESPACES:?}, got {ns:?}"
        )));
    }
    if !valid_key(key) {
        return Err(CareError::Invalid(format!(
            "settings key {key:?} is not [A-Za-z0-9_.:-]+"
        )));
    }
    Ok(())
}

/// POSIX shell quoting, as Python's `shlex.quote` (phone-guard `_q`).
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// What the guard reads, in one go.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// Battery temperature, deci-°C: the one number that pauses and resumes.
    pub battery_dc: i32,
    /// Hottest skin sensor, deci-°C (HAL block only): paces work early.
    pub skin_dc: Option<i32>,
    /// Thermal headroom (unverified source; `None` when unreadable): paces work early.
    pub headroom: Option<f32>,
    pub level: i32,
    pub powered: bool,
    /// A `dumpsys battery set` override is in force.
    pub frozen: bool,
}

impl Reading {
    /// A plain reading for tests and callers that have only a battery temperature.
    pub fn battery(battery_dc: i32) -> Self {
        Reading {
            battery_dc,
            skin_dc: None,
            headroom: None,
            level: 80,
            powered: true,
            frozen: false,
        }
    }
}

/// One phone, reached through the guarded runner.
pub struct AdbPhone<'a, R: Runner> {
    runner: &'a GuardedRunner<R>,
    serial: String,
}

impl<'a, R: Runner> AdbPhone<'a, R> {
    pub fn new(runner: &'a GuardedRunner<R>, serial: impl Into<String>) -> Self {
        AdbPhone {
            runner,
            serial: serial.into(),
        }
    }

    /// The adb token (USB serial or `ip:port`) this handle talks to.
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// `adb -s <serial> shell <line>` through the guard.
    pub fn shell(&self, line: &str) -> Result<RunOutput, CareError> {
        let argv = vec![
            "-s".to_string(),
            self.serial.clone(),
            "shell".to_string(),
            line.to_string(),
        ];
        self.runner.run(&argv, ADB_TIMEOUT_MS).map_err(|e| match e {
            RunError::Refused(m) => CareError::refused(m),
            RunError::Io(m) => CareError::Unreachable(m),
        })
    }

    /// As [`Self::shell`], but a non-zero exit or adb error text is an error. Returns stdout.
    pub fn shell_ok(&self, line: &str) -> Result<String, CareError> {
        let o = self.shell(line)?;
        if !o.ok() || health::has_adb_error(&o.stdout, &o.stderr) {
            let why = if o.stderr.trim().is_empty() {
                o.stdout.trim()
            } else {
                o.stderr.trim()
            };
            return Err(CareError::Adb(format!(
                "`{line}` failed (exit {:?}): {why}",
                o.code
            )));
        }
        Ok(o.stdout)
    }

    /// The phone's real `ro.serialno`: the identity of a lease (phone-guard S9).
    pub fn ro_serial(&self) -> Result<String, CareError> {
        let out = self.shell_ok("getprop ro.serialno")?;
        let v = out.trim();
        if v.is_empty()
            || v.len() > 64
            || !v
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '_' | '-'))
        {
            return Err(CareError::Unreachable(format!(
                "cannot identify {} (ro.serialno {v:?})",
                self.serial
            )));
        }
        Ok(v.to_string())
    }

    pub fn battery(&self) -> Result<BatteryInfo, CareError> {
        let b = health::parse_battery(&self.shell_ok("dumpsys battery")?);
        if b.level.is_none() || b.temp_dc.is_none() {
            return Err(CareError::Adb(
                "could not read level/temperature from dumpsys battery".into(),
            ));
        }
        Ok(b)
    }

    /// Battery (required), skin and headroom (best effort; a failure leaves them `None`).
    pub fn reading(&self) -> Result<Reading, CareError> {
        let b = self.battery()?;
        let skin_dc = self
            .shell_ok("dumpsys thermalservice")
            .ok()
            .and_then(|t| health::parse_skin(&t));
        let headroom = self
            .shell_ok("cmd thermalservice headroom 10")
            .ok()
            .and_then(|t| health::parse_headroom(&t));
        Ok(Reading {
            battery_dc: b.temp_dc.unwrap_or_default(),
            skin_dc,
            headroom,
            level: b.level.unwrap_or_default(),
            powered: b.powered,
            frozen: b.frozen,
        })
    }

    /// `settings get`. `None` when the key is absent (`null` or empty, as phone-guard reads it).
    /// Only the line ending adb adds is cut, so the value comes back byte for byte.
    pub fn settings_get(&self, ns: &str, key: &str) -> Result<Option<String>, CareError> {
        check_target(ns, key)?;
        let out = self.shell_ok(&format!("settings get {ns} {}", sh_quote(key)))?;
        let v = out
            .strip_suffix('\n')
            .map(|s| s.strip_suffix('\r').unwrap_or(s))
            .unwrap_or(&out);
        Ok(if v.is_empty() || v == "null" {
            None
        } else {
            Some(v.to_string())
        })
    }

    pub fn settings_put(&self, ns: &str, key: &str, value: &str) -> Result<(), CareError> {
        check_target(ns, key)?;
        self.shell_ok(&format!(
            "settings put {ns} {} {}",
            sh_quote(key),
            sh_quote(value)
        ))
        .map(|_| ())
    }

    pub fn settings_delete(&self, ns: &str, key: &str) -> Result<(), CareError> {
        check_target(ns, key)?;
        self.shell_ok(&format!("settings delete {ns} {}", sh_quote(key)))
            .map(|_| ())
    }

    fn verified(&self, cmd: &str) -> Option<String> {
        let o = self.shell(cmd).ok()?;
        (o.ok() && !health::has_adb_error_broad(&o.stdout, &o.stderr)).then_some(o.stdout)
    }

    /// `dumpsys battery reset`, proven: exit 0, no adb error text, and a follow-up `dumpsys battery`
    /// without `UPDATES STOPPED`. Two attempts (phone-guard `battery_reset_verified`).
    pub fn battery_reset_verified(&self) -> bool {
        (0..2).any(|_| {
            self.verified("dumpsys battery reset").is_some()
                && self
                    .verified("dumpsys battery")
                    .is_some_and(|t| !health::parse_battery(&t).frozen)
        })
    }

    /// Undo injected heat, proven: a cool `NONE` sample under the same name (best effort), then
    /// `cmd thermalservice reset`, then `IsStatusOverride: false` (phone-guard `thermal_reset_verified`).
    pub fn thermal_reset_verified(&self, name: &str, sensor_type: &str) -> bool {
        if !crate::fake_heat::thermal_name_ok(name)
            || !crate::fake_heat::thermal_name_ok(sensor_type)
        {
            return false;
        }
        (0..2).any(|_| {
            let _ = self.shell(&format!(
                "cmd thermalservice inject-temperature {sensor_type} NONE {name} 25.0"
            ));
            self.verified("cmd thermalservice reset").is_some()
                && self
                    .verified("dumpsys thermalservice")
                    .is_some_and(|t| health::parse_status_override(&t) == Some(false))
        })
    }

    pub fn lock_state(&self) -> Result<LockState, CareError> {
        Ok(health::parse_lock(&self.shell_ok("dumpsys window")?))
    }
}

impl<R: Runner> Screen for AdbPhone<'_, R> {
    fn sleep(&self) -> Result<(), CareError> {
        self.shell_ok("input keyevent KEYCODE_SLEEP").map(|_| ())
    }
    fn wake(&self) -> Result<bool, CareError> {
        self.shell_ok("input keyevent KEYCODE_WAKEUP")?;
        let lock = self.lock_state()?;
        // Unlock only when no PIN, pattern or password is set (phone-guard `thermal_guard`).
        if lock.showing == Some(true) && lock.secure == Some(false) {
            self.shell_ok("wm dismiss-keyguard")?;
            return Ok(false);
        }
        Ok(lock.showing != Some(false))
    }
    fn awake(&self) -> Option<bool> {
        self.shell_ok("dumpsys power")
            .ok()
            .and_then(|t| health::parse_wakefulness(&t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_matches_shlex() {
        assert_eq!(sh_quote("screen_brightness"), "screen_brightness");
        assert_eq!(sh_quote("60.0"), "60.0");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(sh_quote("$HOME;x"), "'$HOME;x'");
    }

    #[test]
    fn targets_are_whitelisted() {
        assert!(check_target("system", "screen_brightness").is_ok());
        assert!(check_target("secure", "miui_refresh_rate").is_ok());
        assert!(check_target("vendor", "x").is_err());
        assert!(check_target("system", "a;reboot").is_err());
        assert!(check_target("system", "-x").is_err());
        assert!(check_target("system", "").is_err());
    }
}
