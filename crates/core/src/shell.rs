//! Pure helpers ported from phone-hands' `server.py`: shell quoting, `input text` escaping, key
//! names, intent arguments, `am start` output, the scroll geometry and the unlock script. Nothing
//! here runs anything; the live transport feeds these strings to adb.

pub const XIAOMI_HINT: &str = "The phone refused input. On Xiaomi/POCO/Redmi (HyperOS/MIUI) turn on Developer options -> 'USB debugging (Security settings)' (it needs a SIM in the phone while you flip it, and a Mi account).";

/// Quote one argument for the phone's `/system/bin/sh` (adb shell joins its arguments into one line).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Escape ASCII text for `adb shell input text`: shell metacharacters get a backslash, spaces become `%s`.
pub fn escape_input_text(chunk: &str) -> String {
    const SPECIAL: &str = "\\'\"`$&|;<>()*?!#~{}[]";
    let mut out = String::with_capacity(chunk.len() + 8);
    for c in chunk.chars() {
        if SPECIAL.contains(c) {
            out.push('\\');
            out.push(c);
        } else if c == ' ' {
            out.push_str("%s");
        } else {
            out.push(c);
        }
    }
    out
}

/// phone-hands routes anything above ASCII 126 through the helper (`input text` only types ASCII).
pub fn needs_unicode_route(chunk: &str) -> bool {
    chunk.chars().any(|c| (c as u32) > 126)
}

/// The key names tools accept, and their keycodes.
pub const KEYS: [(&str, &str); 18] = [
    ("back", "KEYCODE_BACK"),
    ("home", "KEYCODE_HOME"),
    ("recents", "KEYCODE_APP_SWITCH"),
    ("enter", "KEYCODE_ENTER"),
    ("delete", "KEYCODE_DEL"),
    ("tab", "KEYCODE_TAB"),
    ("escape", "KEYCODE_ESCAPE"),
    ("search", "KEYCODE_SEARCH"),
    ("menu", "KEYCODE_MENU"),
    ("wake", "KEYCODE_WAKEUP"),
    ("sleep", "KEYCODE_SLEEP"),
    ("power", "KEYCODE_POWER"),
    ("volume_up", "KEYCODE_VOLUME_UP"),
    ("volume_down", "KEYCODE_VOLUME_DOWN"),
    ("mute", "KEYCODE_VOLUME_MUTE"),
    ("play_pause", "KEYCODE_MEDIA_PLAY_PAUSE"),
    ("camera", "KEYCODE_CAMERA"),
    ("paste", "KEYCODE_PASTE"),
];

/// A key name or `KEYCODE_*` to a keycode. Only `[A-Z0-9_]` passes the raw form, so nothing odd
/// reaches the shell.
pub fn keycode(name: &str) -> Result<String, String> {
    let lower = name.to_ascii_lowercase();
    if let Some((_, code)) = KEYS.iter().find(|(n, _)| *n == lower) {
        return Ok((*code).to_string());
    }
    if let Some(rest) = name.strip_prefix("KEYCODE_") {
        if !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            return Ok(name.to_string());
        }
    }
    let known: Vec<&str> = KEYS.iter().map(|(n, _)| *n).collect();
    Err(format!("Unknown key {name:?}. Known: {}", known.join(", ")))
}

/// `open_url` needs a scheme (`https:`, `market:`, ...).
pub fn check_uri(uri: &str) -> Result<(), String> {
    let mut chars = uri.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    if ok {
        for c in chars {
            if c == ':' {
                return Ok(());
            }
            if !(c.is_ascii_alphanumeric() || "+.-".contains(c)) {
                break;
            }
        }
    }
    Err("uri needs a scheme, such as https:// or market://".into())
}

/// Arguments for `am start -W` to open a link.
pub fn open_url_args(uri: &str, package: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = ["-a", "android.intent.action.VIEW", "-d", uri]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(p) = package {
        a.extend(["-p".to_string(), p.to_string()]);
    }
    a
}

/// Arguments for `am start -W` to start a component.
pub fn start_activity_args(
    component: &str,
    action: Option<&str>,
    data: Option<&str>,
) -> Result<Vec<String>, String> {
    if !component.contains('/') {
        return Err("component is package/activity, e.g. com.android.settings/.Settings".into());
    }
    let mut a = vec!["-n".to_string(), component.to_string()];
    if let Some(x) = action {
        a.extend(["-a".to_string(), x.to_string()]);
    }
    if let Some(d) = data {
        a.extend(["-d".to_string(), d.to_string()]);
    }
    Ok(a)
}

/// The one shell line for `am start -W ...`, every argument quoted.
pub fn am_start_line(args: &[String]) -> String {
    let q: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    format!("am start -W {}", q.join(" "))
}

/// Read `am start -W` output: `Ok("status=ok activity=...")` or the error text.
pub fn parse_am_start(out: &str) -> Result<String, String> {
    let bad = out.lines().any(|l| {
        l.starts_with("Error") || l.contains("Exception") || l.contains("unable to resolve")
    });
    if bad {
        return Err(out.trim().chars().take(400).collect());
    }
    let find = |prefix: &str| {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(prefix).map(|r| r.trim().to_string()))
    };
    let mut s = format!("status={}", find("Status:").unwrap_or_else(|| "?".into()));
    if let Some(a) = find("Activity:") {
        s += &format!(" activity={a}");
    }
    if let Some(w) = out.lines().find(|l| l.starts_with("Warning: ")) {
        s += &format!(" ({w})");
    }
    Ok(s)
}

/// Did injecting input fail because the phone refuses it (Xiaomi's extra switch)?
pub fn check_input(out: &str) -> Result<(), String> {
    if out.contains("INJECT_EVENTS") || out.contains("SecurityException") {
        Err(XIAOMI_HINT.to_string())
    } else {
        Ok(())
    }
}

/// Swipe for `scroll(direction)` on a `w` x `h` screen: `(x1, y1, x2, y2)`.
pub fn scroll_swipe(direction: &str, w: i32, h: i32) -> Result<(i32, i32, i32, i32), String> {
    let (cx, cy) = (w / 2, h / 2);
    let f = |v: i32, k: f64| (f64::from(v) * k) as i32;
    match direction {
        "down" => Ok((cx, f(h, 0.75), cx, f(h, 0.3))),
        "up" => Ok((cx, f(h, 0.3), cx, f(h, 0.75))),
        "right" => Ok((f(w, 0.8), cy, f(w, 0.2), cy)),
        "left" => Ok((f(w, 0.2), cy, f(w, 0.8), cy)),
        _ => Err("direction must be up, down, left or right".into()),
    }
}

const KEYGUARD: &str = "dumpsys window | grep -E -o 'isKeyguardShowing=(true|false)|mShowingLockscreen=(true|false)' | head -1";
const WAKE: &str = "dumpsys power | grep -o 'mWakefulness=[A-Za-z]*'";

/// The one-shell-call unlock script (wake, swipe the lock screen away if showing), so the lock
/// screen's short sleep timer cannot win between steps.
pub fn unlock_script(w: i32, h: i32) -> String {
    let (x, y1, y2) = (
        w / 2,
        (f64::from(h) * 0.88) as i32,
        (f64::from(h) * 0.25) as i32,
    );
    format!(
        "echo before:$({WAKE}); input keyevent KEYCODE_WAKEUP; sleep 0.4; if {KEYGUARD} | grep -q true; then input swipe {x} {y1} {x} {y2} 350; echo swiped; sleep 0.8; fi; echo after:$({WAKE}) $({KEYGUARD}); echo disabled:$(cmd lock_settings get-disabled 2>&1)"
    )
}

/// Summarise the unlock script's output as phone-hands does.
pub fn parse_unlock(out: &str) -> String {
    let after_wake = |tag: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(tag))
            .and_then(|r| r.strip_prefix("mWakefulness="))
            .map(|r| r.split_whitespace().next().unwrap_or("").to_string())
    };
    let before = after_wake("before:");
    let after = after_wake("after:");
    let still_locked = out.lines().any(|l| {
        l.starts_with("after:")
            && (l.contains("isKeyguardShowing=true") || l.contains("mShowingLockscreen=true"))
    });
    let disabled = out
        .lines()
        .find_map(|l| l.strip_prefix("disabled:"))
        .map(|r| r.split_whitespace().next().unwrap_or("?").to_string());
    let b = before.unwrap_or_else(|| "?".into());
    let mut parts = vec![format!(
        "screen was {b}{}",
        if b == "Dozing" {
            " (DOZING: it was asleep)"
        } else {
            ""
        }
    )];
    parts.push(if out.contains("swiped") {
        "lock screen swiped away".into()
    } else {
        "no lock screen was showing".into()
    });
    parts.push(format!("now {}", after.unwrap_or_else(|| "?".into())));
    if still_locked {
        parts.push("STILL LOCKED: a PIN, pattern or password is probably set; that is for the phone's owner to enter".into());
    }
    parts.push(format!(
        "lock_settings disabled={}",
        disabled.unwrap_or_else(|| "?".into())
    ));
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_keeps_ampersand_and_apostrophe() {
        assert_eq!(
            sh_quote("market://search?q=it's&c=apps"),
            "'market://search?q=it'\\''s&c=apps'"
        );
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn input_text_escaping() {
        assert_eq!(escape_input_text("a b"), "a%sb");
        assert_eq!(
            escape_input_text("it's $5 & (more)"),
            "it\\'s%s\\$5%s\\&%s\\(more\\)"
        );
        assert!(needs_unicode_route("café 東京"));
        assert!(!needs_unicode_route("plain ~ text"));
        assert!(needs_unicode_route("\u{7f}")); // above 126
    }

    #[test]
    fn keys() {
        assert_eq!(keycode("back").unwrap(), "KEYCODE_BACK");
        assert_eq!(keycode("HOME").unwrap(), "KEYCODE_HOME");
        assert_eq!(keycode("KEYCODE_F5").unwrap(), "KEYCODE_F5");
        assert!(keycode("KEYCODE_BACK; reboot").is_err());
        assert!(keycode("KEYCODE_").is_err());
        assert!(keycode("nope").unwrap_err().contains("Known:"));
    }

    #[test]
    fn uri_scheme() {
        assert!(check_uri("https://example.com").is_ok());
        assert!(check_uri("market://details?id=x").is_ok());
        assert!(check_uri("no-scheme-here").is_err());
        assert!(check_uri("://x").is_err());
        assert!(check_uri("").is_err());
        assert!(check_uri("1http://x").is_err());
    }

    #[test]
    fn intents() {
        assert_eq!(
            open_url_args("u:x", Some("com.p")),
            vec![
                "-a",
                "android.intent.action.VIEW",
                "-d",
                "u:x",
                "-p",
                "com.p"
            ]
        );
        assert!(start_activity_args("nope", None, None).is_err());
        let a = start_activity_args("a.b/.C$D", Some("act"), Some("d:1")).unwrap();
        assert_eq!(
            am_start_line(&a),
            "am start -W '-n' 'a.b/.C$D' '-a' 'act' '-d' 'd:1'"
        );
    }

    #[test]
    fn am_start_output() {
        let ok = "Starting: Intent { cmp=com.android.settings/.Settings }\nStatus: ok\nLaunchState: COLD\nActivity: com.android.settings/.Settings\nTotalTime: 300\n";
        assert_eq!(
            parse_am_start(ok).unwrap(),
            "status=ok activity=com.android.settings/.Settings"
        );
        assert!(parse_am_start("Error: Activity class {x/y} does not exist.").is_err());
        assert!(parse_am_start("Status: ok\nWarning: Activity not started, its current task has been brought to the front\n").unwrap().contains("Warning"));
    }

    #[test]
    fn input_refusal_is_explained() {
        assert!(check_input(
            "java.lang.SecurityException: Injecting to another application requires INJECT_EVENTS"
        )
        .unwrap_err()
        .contains("Security settings"));
        assert!(check_input("").is_ok());
    }

    #[test]
    fn scroll_geometry() {
        assert_eq!(
            scroll_swipe("down", 1220, 2712).unwrap(),
            (610, 2034, 610, 813)
        );
        assert_eq!(
            scroll_swipe("left", 1000, 2000).unwrap(),
            (200, 1000, 800, 1000)
        );
        assert!(scroll_swipe("sideways", 1, 1).is_err());
    }

    #[test]
    fn unlock_summary() {
        assert!(unlock_script(1220, 2712).contains("input swipe 610 2386 610 678 350"));
        let out = "before:mWakefulness=Dozing\nswiped\nafter:mWakefulness=Awake isKeyguardShowing=false\ndisabled:false\n";
        let s = parse_unlock(out);
        assert!(
            s.contains("DOZING")
                && s.contains("lock screen swiped away")
                && s.contains("now Awake")
                && !s.contains("STILL LOCKED"),
            "{s}"
        );
        let locked = "before:mWakefulness=Awake\nafter:mWakefulness=Awake isKeyguardShowing=true\ndisabled:false\n";
        assert!(parse_unlock(locked).contains("STILL LOCKED"));
    }
}
