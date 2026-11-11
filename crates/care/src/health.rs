//! Pure parsers for `dumpsys battery`, `dumpsys thermalservice`, `dumpsys power`, `dumpsys window`
//! and `cmd thermalservice headroom`. A port of phone-guard `health.py` (no regex crate offline, so
//! the scanning is by hand; the accepted shapes are the same).

/// `dumpsys battery`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BatteryInfo {
    pub level: Option<i32>,
    /// Tenths of a degree C.
    pub temp_dc: Option<i32>,
    /// `BatteryManager.BATTERY_STATUS_*` (2 charging, 3 discharging, 4 not charging, 5 full).
    /// **Never** used for the floor: a phone held at the 80 % cap is powered but "not charging".
    pub status: Option<i32>,
    pub ac: bool,
    pub usb: bool,
    pub wireless: bool,
    pub dock: bool,
    /// Any of AC / USB / Wireless / Dock `powered: true`.
    pub powered: bool,
    /// `UPDATES STOPPED`: a `dumpsys battery set` override is in force; level and temperature are fake.
    pub frozen: bool,
}

/// First `^\s*<name>: (-?\d+)` in the text.
fn field_int(text: &str, name: &str) -> Option<i32> {
    for line in text.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix(name).and_then(|r| r.strip_prefix(": ")) {
            let t: String = rest
                .chars()
                .enumerate()
                .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && *c == '-'))
                .map(|(_, c)| c)
                .collect();
            if let Ok(v) = t.parse() {
                return Some(v);
            }
        }
    }
    None
}

pub fn parse_battery(text: &str) -> BatteryInfo {
    let flag = |k: &str| text.contains(&format!("{k} powered: true"));
    let (ac, usb, wireless, dock) = (flag("AC"), flag("USB"), flag("Wireless"), flag("Dock"));
    BatteryInfo {
        level: field_int(text, "level"),
        temp_dc: field_int(text, "temperature"),
        status: field_int(text, "status"),
        ac,
        usb,
        wireless,
        dock,
        powered: ac || usb || wireless || dock,
        frozen: text.contains("UPDATES STOPPED"),
    }
}

/// One `Temperature{mValue=V, mType=T, mName=N` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ThermalSample {
    pub value_c: f64,
    pub kind: i32,
    pub name: String,
}

fn samples(text: &str) -> Vec<ThermalSample> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("Temperature{mValue=") {
        rest = &rest[i + "Temperature{mValue=".len()..];
        let v: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        let after = &rest[v.len()..];
        let Some(after) = after.strip_prefix(", mType=") else {
            continue;
        };
        let t: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let after = &after[t.len()..];
        let Some(after) = after.strip_prefix(", mName=") else {
            continue;
        };
        let name: String = after
            .chars()
            .take_while(|c| *c != ',' && *c != '}')
            .collect();
        if let (Ok(value_c), Ok(kind)) = (v.parse::<f64>(), t.parse::<i32>()) {
            out.push(ThermalSample {
                value_c,
                kind,
                name: name.trim().to_ascii_lowercase(),
            });
        }
    }
    out
}

/// Only the `Current temperatures from HAL:` block when the dump has one (measured on the POCO,
/// 20 Oct 2026: the cached block above it held a stale 44.6 °C skin while the HAL read 34.1 °C).
fn hal_block(text: &str) -> String {
    let mut lines = text.lines();
    let mut found = false;
    for l in lines.by_ref() {
        if l.trim() == "Current temperatures from HAL:" {
            found = true;
            break;
        }
    }
    if !found {
        return text.to_string();
    }
    lines
        .take_while(|l| l.trim().starts_with("Temperature{"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Highest skin **or** battery reading, deci-°C (phone-guard's `parse_thermal`).
pub fn parse_thermal(text: &str) -> Option<i32> {
    samples(&hal_block(text))
        .into_iter()
        .filter(|s| s.kind == 2 || s.kind == 3 || s.name == "skin" || s.name == "battery")
        .map(|s| (s.value_c * 10.0).round() as i32)
        .max()
}

/// Highest **skin** reading (type 3 or name `skin`), deci-°C: what paces the work.
pub fn parse_skin(text: &str) -> Option<i32> {
    samples(&hal_block(text))
        .into_iter()
        .filter(|s| s.kind == 3 || s.name == "skin")
        .map(|s| (s.value_c * 10.0).round() as i32)
        .max()
}

/// `IsStatusOverride: true|false` from `dumpsys thermalservice`.
pub fn parse_status_override(text: &str) -> Option<bool> {
    text.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("IsStatusOverride:")?.trim();
        match rest {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    })
}

/// The last number in the output of `cmd thermalservice headroom <s>`, if it is in 0.0 ..= 10.0.
/// UNVERIFIED on hardware: the output shape is not known; a NaN, a missing command or anything
/// unparsed gives `None`, and then only the skin temperature paces the work.
pub fn parse_headroom(text: &str) -> Option<f32> {
    let mut last = None;
    for tok in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
        if tok.contains(|c: char| c.is_ascii_digit()) {
            if let Ok(v) = tok.parse::<f32>() {
                last = Some(v);
            }
        }
    }
    last.filter(|v| v.is_finite() && (0.0..=10.0).contains(v))
}

/// True awake, false asleep or dozing, `None` unknown.
pub fn parse_wakefulness(text: &str) -> Option<bool> {
    let word_after = |key: &str| {
        text.find(key).map(|i| {
            text[i + key.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        })
    };
    if let Some(w) = word_after("mWakefulness=") {
        return Some(w.eq_ignore_ascii_case("awake"));
    }
    word_after("Display Power: state=").map(|w| w.eq_ignore_ascii_case("ON"))
}

/// Keyguard showing and secure (a PIN, pattern or password is set). `None` when the dump does not say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockState {
    pub showing: Option<bool>,
    pub secure: Option<bool>,
}

fn flag(text: &str, names: &[&str]) -> Option<bool> {
    for n in names {
        let key = format!("{n}=");
        if let Some(i) = text.find(&key) {
            let rest = &text[i + key.len()..];
            if rest.starts_with("true") {
                return Some(true);
            }
            if rest.starts_with("false") {
                return Some(false);
            }
        }
    }
    None
}

pub fn parse_lock(text: &str) -> LockState {
    LockState {
        showing: flag(
            text,
            &[
                "isKeyguardShowing",
                "mKeyguardShowing",
                "mShowingLockscreen",
            ],
        ),
        secure: flag(text, &["isKeyguardSecure", "mIsSecure", "keyguardSecure"]),
    }
}

/// adb's own error text in a command's output (phone-guard `adb.ADB_ERROR`: `error:` at a line
/// start, `device '…' not found`, no devices, offline, unauthorized, more than one device).
pub fn has_adb_error(out: &str, err: &str) -> bool {
    error_text(out, err, false)
}

/// The broader check phone-guard uses to verify a reset (`health._ADB_ERR`): any `not found`
/// also counts, so a doubtful reset keeps its lease.
pub fn has_adb_error_broad(out: &str, err: &str) -> bool {
    error_text(out, err, true)
}

fn error_text(out: &str, err: &str, broad: bool) -> bool {
    [out, err].iter().any(|t| {
        let low = t.to_ascii_lowercase();
        low.lines().any(|l| {
            let s = l.trim_start();
            s.starts_with("error:")
                || s.starts_with("adb: error:")
                || (l.contains("not found")
                    && (broad || l[..l.find("not found").unwrap_or(0)].contains("device")))
        }) || low.contains("no devices/emulators found")
            || low.contains("device offline")
            || low.contains("device unauthorized")
            || low.contains("more than one device")
            || low.contains("more than one emulator")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_fields_and_powered_flags() {
        let t = "Current Battery Service state:\n  AC powered: false\n  USB powered: true\n  Wireless powered: false\n  status: 4\n  level: 80\n  temperature: 431\n";
        let b = parse_battery(t);
        assert_eq!(
            (b.level, b.temp_dc, b.status, b.powered, b.usb, b.frozen),
            (Some(80), Some(431), Some(4), true, true, false)
        );
        let f = parse_battery("  (UPDATES STOPPED -- use 'reset' to restart)\n  AC powered: false\n  level: -1\n  temperature: -5\n");
        assert!(f.frozen && !f.powered);
        assert_eq!((f.level, f.temp_dc), (Some(-1), Some(-5)));
    }

    #[test]
    fn thermal_reads_only_the_hal_block() {
        let t = "IsStatusOverride: false\nCached temperatures:\n\tTemperature{mValue=44.6, mType=3, mName=skin, mStatus=0}\nHAL Ready: true\nCurrent temperatures from HAL:\n\tTemperature{mValue=34.1, mType=3, mName=skin, mStatus=0}\n\tTemperature{mValue=30.0, mType=2, mName=battery, mStatus=0}\n\tTemperature{mValue=50.0, mType=0, mName=cpu0, mStatus=0}\nCurrent cooling devices from HAL:\n";
        assert_eq!(parse_thermal(t), Some(341));
        assert_eq!(parse_skin(t), Some(341));
        assert_eq!(parse_status_override(t), Some(false));
        // without the header the whole text counts
        assert_eq!(
            parse_skin("Temperature{mValue=40.5, mType=3, mName=skin}"),
            Some(405)
        );
        assert_eq!(parse_thermal("nothing here"), None);
    }

    #[test]
    fn screen_lock_headroom_and_errors() {
        assert_eq!(parse_wakefulness("  mWakefulness=Asleep\n"), Some(false));
        assert_eq!(parse_wakefulness("Display Power: state=ON"), Some(true));
        assert_eq!(parse_wakefulness("??"), None);
        let l = parse_lock("isKeyguardShowing=true isKeyguardSecure=false");
        assert_eq!((l.showing, l.secure), (Some(true), Some(false)));
        assert_eq!(parse_headroom("Headroom in 10 seconds: 0.85\n"), Some(0.85));
        assert_eq!(parse_headroom("Unknown command: headroom"), None);
        assert!(has_adb_error("", "error: device offline"));
        assert!(has_adb_error("", "adb: device 'X' not found"));
        assert!(!has_adb_error("Success", ""));
        assert!(
            !has_adb_error("value not found here", ""),
            "a setting's text is not an adb error"
        );
        assert!(has_adb_error_broad("Service not found", ""));
    }
}
