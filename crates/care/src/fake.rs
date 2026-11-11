//! A fake phone: an adb-link [`Runner`] that answers `adb -s <token> shell <line>` from an
//! in-memory device. Tests wrap it in adb-link's real `GuardedRunner`, so every command still
//! passes the guard. It never starts a process.
//!
//! The line is split the way the phone's `sh` would (single and double quotes, backslashes), so
//! wrong quoting shows up as a wrong value, as it would on a phone.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use agentroid_adb_link::runner::{RunError, RunOutput, Runner};

#[derive(Debug, Clone)]
pub struct FakeState {
    pub hw_serial: String,
    /// adb tokens that reach this phone (USB serial, `ip:port` ...).
    pub tokens: Vec<String>,
    /// `"<ns>/<key>"` → value. Survives a reboot.
    pub settings: BTreeMap<String, String>,
    pub level: i32,
    pub battery_dc: i32,
    pub status: i32,
    pub ac: bool,
    pub usb: bool,
    pub wireless: bool,
    /// `dumpsys battery set temp` in force.
    pub temp_override: Option<i32>,
    pub level_override: Option<i32>,
    pub skin_dc: Option<i32>,
    pub headroom: Option<f32>,
    pub status_override: Option<i32>,
    /// Injected thermal samples: name → (type, value °C).
    pub injected: BTreeMap<String, (String, f32)>,
    pub awake: bool,
    pub keyguard_showing: bool,
    pub keyguard_secure: bool,
    pub offline: bool,
    /// Lines starting with one of these fail (exit 1, `error: injected failure`).
    pub fail_prefixes: Vec<String>,
    /// The next N `dumpsys battery reset` calls fail.
    pub reset_failures: u32,
    /// Every shell line, in order.
    pub log: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FakePhone {
    state: Arc<Mutex<FakeState>>,
}

impl FakePhone {
    pub fn new(hw_serial: &str) -> Self {
        FakePhone {
            state: Arc::new(Mutex::new(FakeState {
                hw_serial: hw_serial.into(),
                tokens: vec![hw_serial.into()],
                settings: BTreeMap::new(),
                level: 80,
                battery_dc: 300,
                status: 4,
                ac: false,
                usb: true,
                wireless: false,
                temp_override: None,
                level_override: None,
                skin_dc: Some(320),
                headroom: Some(0.3),
                status_override: None,
                injected: BTreeMap::new(),
                awake: true,
                keyguard_showing: false,
                keyguard_secure: false,
                offline: false,
                fail_prefixes: vec![],
                reset_failures: 0,
                log: vec![],
            })),
        }
    }

    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set(&self, ns: &str, key: &str, value: &str) {
        self.state()
            .settings
            .insert(format!("{ns}/{key}"), value.into());
    }
    pub fn get(&self, ns: &str, key: &str) -> Option<String> {
        self.state().settings.get(&format!("{ns}/{key}")).cloned()
    }

    /// Shell lines sent, without the guard's identity probes.
    pub fn commands(&self) -> Vec<String> {
        self.state()
            .log
            .iter()
            .filter(|l| *l != "getprop ro.serialno")
            .cloned()
            .collect()
    }

    /// A reboot: overrides and injected samples are gone, settings stay, the screen is on and locked.
    pub fn reboot(&self) {
        let mut s = self.state();
        s.temp_override = None;
        s.level_override = None;
        s.status_override = None;
        s.injected.clear();
        s.awake = true;
        s.keyguard_showing = true;
    }

    fn battery_text(s: &FakeState) -> String {
        let mut t = String::from("Current Battery Service state:\n");
        if s.temp_override.is_some() || s.level_override.is_some() {
            t.push_str("  (UPDATES STOPPED -- use 'reset' to restart)\n");
        }
        t.push_str(&format!(
            "  AC powered: {}\n  USB powered: {}\n  Wireless powered: {}\n  Dock powered: false\n  Max charging current: 0\n  status: {}\n  health: 2\n  present: true\n  level: {}\n  scale: 100\n  voltage: 4200\n  temperature: {}\n  technology: Li-ion\n",
            s.ac,
            s.usb,
            s.wireless,
            s.status,
            s.level_override.unwrap_or(s.level),
            s.temp_override.unwrap_or(s.battery_dc)
        ));
        t
    }

    fn thermal_text(s: &FakeState) -> String {
        let mut t = format!(
            "IsStatusOverride: {}\nThermal Status: {}\nCached temperatures:\n",
            s.status_override.is_some(),
            s.status_override.unwrap_or(0)
        );
        for (name, (ty, v)) in &s.injected {
            let code = if ty == "SKIN" { 3 } else { 0 };
            t.push_str(&format!(
                "\tTemperature{{mValue={v:.1}, mType={code}, mName={name}, mStatus=3}}\n"
            ));
        }
        t.push_str("HAL Ready: true\nCurrent temperatures from HAL:\n");
        if let Some(sk) = s.skin_dc {
            t.push_str(&format!(
                "\tTemperature{{mValue={:.1}, mType=3, mName=skin, mStatus=0}}\n",
                sk as f64 / 10.0
            ));
        }
        t.push_str(&format!(
            "\tTemperature{{mValue={:.1}, mType=2, mName=battery, mStatus=0}}\n",
            s.battery_dc as f64 / 10.0
        ));
        t.push_str("Current cooling devices from HAL:\n");
        t
    }

    fn answer(s: &mut FakeState, line: &str) -> RunOutput {
        let ok = |out: String| RunOutput {
            code: Some(0),
            stdout: out,
            stderr: String::new(),
        };
        let fail = |err: &str| RunOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: err.to_string(),
        };
        if s.fail_prefixes.iter().any(|p| line.starts_with(p.as_str())) {
            return fail("error: injected failure");
        }
        let Some(w) = sh_split(line) else {
            return fail("sh: syntax error: unterminated quoted string");
        };
        let w: Vec<&str> = w.iter().map(String::as_str).collect();
        match w.as_slice() {
            ["getprop", "ro.serialno"] => ok(format!("{}\n", s.hw_serial)),
            ["settings", "get", ns, key] => ok(format!(
                "{}\n",
                s.settings
                    .get(&format!("{ns}/{key}"))
                    .map_or("null", String::as_str)
            )),
            // like the real `settings put NS KEY VALUE [TAG] [default]`: only the 5th word is the value
            ["settings", "put", ns, key, value, ..] => {
                s.settings.insert(format!("{ns}/{key}"), value.to_string());
                ok(String::new())
            }
            ["settings", "delete", ns, key] => {
                let n = usize::from(s.settings.remove(&format!("{ns}/{key}")).is_some());
                ok(format!("Deleted {n} rows\n"))
            }
            ["dumpsys", "battery"] => ok(Self::battery_text(s)),
            ["dumpsys", "battery", "set", "temp", v] => match v.parse() {
                Ok(n) => {
                    s.temp_override = Some(n);
                    ok(String::new())
                }
                Err(_) => fail("Bad value"),
            },
            ["dumpsys", "battery", "set", "level", v] => match v.parse() {
                Ok(n) => {
                    s.level_override = Some(n);
                    ok(String::new())
                }
                Err(_) => fail("Bad value"),
            },
            ["dumpsys", "battery", "reset"] => {
                if s.reset_failures > 0 {
                    s.reset_failures -= 1;
                    return fail("error: reset refused (injected)");
                }
                s.temp_override = None;
                s.level_override = None;
                ok(String::new())
            }
            ["dumpsys", "thermalservice"] => ok(Self::thermal_text(s)),
            ["cmd", "thermalservice", "override-status", n] => match n.parse() {
                Ok(v) => {
                    s.status_override = Some(v);
                    ok(String::new())
                }
                Err(_) => fail("Error: invalid status"),
            },
            ["cmd", "thermalservice", "inject-temperature", ty, _status, name, v] => {
                match v.parse() {
                    Ok(val) => {
                        s.injected.insert(name.to_string(), (ty.to_string(), val));
                        ok(String::new())
                    }
                    Err(_) => fail("Error: invalid value"),
                }
            }
            ["cmd", "thermalservice", "reset"] => {
                s.status_override = None;
                s.injected.clear();
                ok(String::new())
            }
            ["cmd", "thermalservice", "headroom", _] => match s.headroom {
                Some(h) => ok(format!("Headroom in 10 seconds: {h}\n")),
                None => fail("Unknown command: headroom"),
            },
            ["input", "keyevent", "KEYCODE_SLEEP"] => {
                s.awake = false;
                s.keyguard_showing = true;
                ok(String::new())
            }
            ["input", "keyevent", "KEYCODE_WAKEUP"] => {
                s.awake = true;
                ok(String::new())
            }
            ["dumpsys", "power"] => ok(format!(
                "  mWakefulness={}\n",
                if s.awake { "Awake" } else { "Asleep" }
            )),
            ["dumpsys", "window"] => ok(format!(
                "    isKeyguardShowing={} isKeyguardSecure={}\n",
                s.keyguard_showing, s.keyguard_secure
            )),
            ["wm", "dismiss-keyguard"] => {
                s.keyguard_showing = false;
                ok(String::new())
            }
            _ => RunOutput {
                code: Some(127),
                stdout: String::new(),
                stderr: format!("/system/bin/sh: {line}: inaccessible or not found"),
            },
        }
    }
}

impl Runner for FakePhone {
    fn run(&self, args: &[String], _timeout_ms: u64) -> Result<RunOutput, RunError> {
        let mut s = self.state();
        let [flag, token, sub, line] = args else {
            return Err(RunError::Io(format!(
                "fake phone: unexpected argv {args:?}"
            )));
        };
        if flag != "-s" || sub != "shell" {
            return Err(RunError::Io(format!(
                "fake phone: unexpected argv {args:?}"
            )));
        }
        if s.offline || !s.tokens.contains(token) {
            return Err(RunError::Io(format!("adb: device '{token}' not found")));
        }
        s.log.push(line.clone());
        Ok(Self::answer(&mut s, line))
    }
}

/// Split a command line as POSIX `sh` would (quotes and backslashes; no expansion).
/// `None` for an unterminated quote.
pub fn sh_split(line: &str) -> Option<Vec<String>> {
    let mut words = vec![];
    let mut cur = String::new();
    let mut in_word = false;
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match it.next()? {
                        '\'' => break,
                        x => cur.push(x),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' if matches!(it.peek(), Some('"' | '\\' | '$' | '`')) => {
                            cur.push(it.next()?)
                        }
                        x => cur.push(x),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(it.next()?);
            }
            c if c == ' ' || c == '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_sh() {
        assert_eq!(
            sh_split("settings put system k 'a b'").unwrap(),
            ["settings", "put", "system", "k", "a b"]
        );
        assert_eq!(sh_split("x 'it'\"'\"'s'").unwrap(), ["x", "it's"]);
        assert_eq!(sh_split("x ''").unwrap(), ["x", ""]);
        assert_eq!(sh_split(r#"x "a\"b" c\ d"#).unwrap(), ["x", "a\"b", "c d"]);
        assert!(sh_split("x 'open").is_none());
    }

    #[test]
    fn sh_quote_round_trips_through_the_fake_shell() {
        for v in [
            "128",
            "a b  c",
            "it's",
            "quote\"d",
            "ünïcödé",
            "$HOME",
            "semi;colon",
            "",
            "back\\slash",
            "'",
            "-1",
        ] {
            let line = format!("settings put system k {}", crate::phone::sh_quote(v));
            assert_eq!(sh_split(&line).unwrap()[4], v, "{v:?}");
        }
    }
}
