//! `agentroid ask-screen`: ask the phone's owner before taking over the screen (INTERIM).
//!
//! Any session that is about to drive a phone (through Agentroid's tools or raw adb) runs this
//! first. It reads whether the phone is in use and, if it is, asks the owner with a Yes / No push.
//!
//! ```text
//! agentroid ask-screen --device <serial|ip:port> --minutes N --why "TEXT"
//!     [--route FILE] [--adb PATH] [--name LABEL] [--idle-min M] [--wait-s S] [--state-dir DIR]
//! ```
//!
//! * **In use** means the screen is on and any of these holds: the focused app is one the owner
//!   is likely typing in (the Claude app, WhatsApp, Gmail, plus `--in-use-app`), the keyboard is
//!   open, or the last touch was under `--idle-min` minutes ago (default 5). Anything that cannot
//!   be read counts as in use, so a phone that answers only `dumpsys power` (a protected phone)
//!   is in use whenever its screen is on.
//! * **Not in use** (screen off or dozing, or on but untouched, no keyboard, no listed app):
//!   allowed at once, with no push. Run the command again before each burst of actions.
//! * **In use:** one push with what the session wants and for how long. Yes gives a lease of
//!   `ceil(1.5 x N)` minutes (at most 60), and calls inside the lease are allowed without a push.
//!   No, or no answer within `--wait-s` (default 120), is a refusal, and every call in the next
//!   5 minutes is refused without a push (no spam; the owner's "No stops it, re-ask in 5 min").
//! * **INTERIM:** the Yes is a plain tap, not yet a fingerprint. The push title says so.
//!
//! Prints one line of JSON. Exit codes: 0 allowed, 1 refused, 2 usage, 3 the phone could not be
//! read at all.
//!
//! The route file names the push channels (one or more ntfy servers):
//!
//! ```json
//! {"routes": [{"base": "http://127.0.0.1:2586", "tap_base": "http://100.64.0.1:2586",
//!              "topic_file": "alerts.topic", "inbound_topic_file": "taps.topic",
//!              "token_file": "laptop.token", "tap_token_file": "tap.token"}]}
//! ```
//!
//! Every field but `topic_file` is optional (`base` defaults to `https://ntfy.sh`). Relative paths
//! are read from the route file's folder.

use crate::approval::{AnyChannel, Approver, Channel, CurlHttp, NtfyChannel, NtfyConfig, Outcome};
use agentroid_adb_link::runner::{AdbRunner, Runner};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The apps a person is likely typing in or reading: in use whatever the idle time.
pub const IN_USE_APPS: [&str; 3] = [
    "com.anthropic.claude",
    "com.whatsapp",
    "com.google.android.gm",
];
/// After a No (or no answer), every call is refused without a push for this long.
pub const REASK_AFTER_MS: u64 = 5 * 60_000;
/// The longest lease a Yes gives.
pub const MAX_LEASE_MIN: u64 = 60;
/// The title of the push. It says INTERIM until the fingerprint step ships.
pub const TITLE: &str =
    "INTERIM ask (plain Yes/No, no fingerprint yet): Agentroid wants your screen";

/// What could be read from the phone. `None` is "could not be read".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signals {
    /// `mWakefulness` from `dumpsys power`: Awake, Dozing, Asleep, Dreaming.
    pub wakefulness: Option<String>,
    /// Milliseconds since the last user activity (touch, key), on the phone's uptime clock.
    pub idle_ms: Option<u64>,
    /// The package of the focused window (`mCurrentFocus`), or `""` when nothing has focus.
    pub focus: Option<String>,
    /// Whether the keyboard is on screen (`mInputShown`).
    pub ime_shown: Option<bool>,
    /// Whether the lock screen is showing (reported only; it does not decide).
    pub keyguard: Option<bool>,
}

impl Signals {
    pub fn screen_on(&self) -> Option<bool> {
        self.wakefulness.as_deref().map(|w| w == "Awake")
    }
}

/// The verdict on whether the phone is in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Use {
    Free(String),
    InUse(Vec<String>),
}

/// Parse `dumpsys power`: the wakefulness and the idle time. The phone's "now" on the uptime
/// clock is any `name=<ms> (<n> ms ago)` line's sum; the last activity is
/// `mLastUserActivityTime(excludingAttention)=<ms>` (or `mLastUserActivityTime=<ms>`).
pub fn parse_power(text: &str) -> (Option<String>, Option<u64>) {
    let mut wake = None;
    let mut now: Option<u64> = None;
    let mut last: Option<u64> = None;
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("mWakefulness=") {
            if wake.is_none() && !v.is_empty() && v.chars().all(|c| c.is_ascii_alphabetic()) {
                wake = Some(v.to_string());
            }
            continue;
        }
        for key in [
            "mLastUserActivityTime(excludingAttention)=",
            "mLastUserActivityTime=",
        ] {
            if let Some(v) = l.strip_prefix(key) {
                let n: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = n.parse::<u64>() {
                    last = Some(last.map_or(n, |o: u64| o.max(n)));
                }
            }
        }
        // "mLastWakeTime=170339786 (870480 ms ago)"
        if now.is_none() {
            if let Some((name_val, rest)) = l.split_once(" (") {
                if let (Some((_, val)), Some(ago)) =
                    (name_val.split_once('='), rest.strip_suffix(" ms ago)"))
                {
                    if let (Ok(v), Ok(a)) = (val.parse::<u64>(), ago.parse::<u64>()) {
                        now = v.checked_add(a);
                    }
                }
            }
        }
    }
    let idle = match (now, last) {
        (Some(n), Some(l)) => Some(n.saturating_sub(l)),
        _ => None,
    };
    (wake, idle)
}

/// Parse `dumpsys window` (or its `mCurrentFocus=` / `isKeyguardShowing=` lines): the focused
/// package and whether the lock screen shows.
pub fn parse_window(text: &str) -> (Option<String>, Option<bool>) {
    let mut focus = None;
    let mut keyguard = None;
    for line in text.lines() {
        let l = line.trim();
        if focus.is_none() {
            if let Some(v) = l.strip_prefix("mCurrentFocus=") {
                focus = Some(if v == "null" {
                    String::new()
                } else {
                    // Window{1a2b3c u0 com.example.app/com.example.app.Main}
                    let inner = v.trim_start_matches("Window{").trim_end_matches('}');
                    let last = inner.split_whitespace().last().unwrap_or("");
                    last.split('/').next().unwrap_or("").to_string()
                });
            }
        }
        if keyguard.is_none() {
            for word in l.split_whitespace() {
                if let Some(v) = word.strip_prefix("isKeyguardShowing=") {
                    keyguard = match v {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    };
                }
            }
        }
    }
    (focus, keyguard)
}

/// Parse `dumpsys input_method`: whether the keyboard is shown (`mInputShown`, the field that
/// turns true the moment a text box is tapped; `mIsInputViewShown` stays true on some phones).
pub fn parse_ime(text: &str) -> Option<bool> {
    for line in text.lines() {
        for word in line.split_whitespace() {
            if let Some(v) = word.strip_prefix("mInputShown=") {
                return match v {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                };
            }
        }
    }
    None
}

/// Decide whether the phone is in use. Anything unknown while the screen is on counts as in use.
pub fn judge(s: &Signals, idle_limit_ms: u64, extra_apps: &[String]) -> Use {
    match s.screen_on() {
        Some(false) => {
            return Use::Free(format!(
                "screen not on ({})",
                s.wakefulness.as_deref().unwrap_or("")
            ))
        }
        None => return Use::InUse(vec!["screen state unknown".into()]),
        Some(true) => {}
    }
    let mut why = vec![];
    match s.focus.as_deref() {
        None => why.push("focused app unknown".to_string()),
        Some(p) if IN_USE_APPS.contains(&p) || extra_apps.iter().any(|a| a == p) => {
            why.push(format!("{p} is open"))
        }
        Some(_) => {}
    }
    match s.ime_shown {
        None => why.push("keyboard state unknown".into()),
        Some(true) => why.push("the keyboard is open".into()),
        Some(false) => {}
    }
    match s.idle_ms {
        None => why.push("last touch unknown".into()),
        Some(ms) if ms < idle_limit_ms => why.push(format!("touched {} s ago", ms / 1000)),
        Some(_) => {}
    }
    if why.is_empty() {
        Use::Free(format!(
            "screen on but untouched for {} s, no keyboard, no listed app",
            s.idle_ms.unwrap_or(0) / 1000
        ))
    } else {
        Use::InUse(why)
    }
}

fn shell(r: &dyn Runner, device: &str, cmd: &str) -> Option<String> {
    let a = vec![
        "-s".to_string(),
        device.to_string(),
        "shell".to_string(),
        cmd.to_string(),
    ];
    match r.run(&a, 15_000) {
        Ok(o) if o.ok() => Some(o.stdout),
        _ => None,
    }
}

/// Read the signals through `r` (production: the guarded adb runner, so a protected phone answers
/// `dumpsys power` and nothing else). `None` when not even `dumpsys power` could be read.
pub fn read_signals(r: &dyn Runner, device: &str) -> Option<Signals> {
    let power = shell(r, device, "dumpsys power")?;
    let (wakefulness, idle_ms) = parse_power(&power);
    let mut s = Signals {
        wakefulness,
        idle_ms,
        ..Signals::default()
    };
    if s.screen_on() == Some(true) {
        if let Some(w) = shell(
            r,
            device,
            "dumpsys window | grep -E 'mCurrentFocus=|isKeyguardShowing='",
        ) {
            (s.focus, s.keyguard) = parse_window(&w);
        }
        if let Some(i) = shell(r, device, "dumpsys input_method | grep -E 'mInputShown='") {
            s.ime_shown = parse_ime(&i);
        }
    }
    Some(s)
}

/// What the state file remembers per device: a Yes lease and a No cool-down (Unix ms).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Memory {
    pub allowed_until_ms: u64,
    pub denied_until_ms: u64,
}

fn device_key(device: &str) -> String {
    let d = Sha256::digest(device.to_ascii_lowercase().as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn memory_path(dir: &Path, device: &str) -> PathBuf {
    dir.join(format!("ask-screen-{}.json", device_key(device)))
}

pub fn load_memory(dir: &Path, device: &str) -> Memory {
    let Ok(t) = std::fs::read_to_string(memory_path(dir, device)) else {
        return Memory::default();
    };
    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
    Memory {
        allowed_until_ms: v["allowed_until_ms"].as_u64().unwrap_or(0),
        denied_until_ms: v["denied_until_ms"].as_u64().unwrap_or(0),
    }
}

pub fn save_memory(dir: &Path, device: &str, m: Memory) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("state dir {}: {e}", dir.display()))?;
    let body = serde_json::json!({
        "allowed_until_ms": m.allowed_until_ms,
        "denied_until_ms": m.denied_until_ms,
    });
    std::fs::write(memory_path(dir, device), body.to_string())
        .map_err(|e| format!("state file: {e}"))
}

/// The request as the command line gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub device: String,
    pub minutes: u64,
    pub why: String,
    pub name: String,
    pub idle_limit_ms: u64,
    pub wait_ms: u64,
    pub extra_apps: Vec<String>,
}

/// The result, printed as one JSON line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub why: String,
    /// Minutes the session may act without asking again (a Yes lease), 0 otherwise.
    pub lease_minutes: u64,
    /// After a refusal: seconds before asking again makes sense.
    pub retry_after_s: u64,
    pub asked: bool,
}

/// The decision, given the signals, the memory and a channel. `now` and `sleep` are Unix-ms
/// clock functions (tests pass fakes). Updates `mem`.
pub fn decide(
    ask: &Ask,
    signals: &Signals,
    mem: &mut Memory,
    channel: Option<Box<dyn Channel + Send>>,
    now: &dyn Fn() -> u64,
    sleep: &dyn Fn(u64),
) -> Decision {
    let t = now();
    if t < mem.allowed_until_ms {
        return Decision {
            allowed: true,
            why: "inside the lease from an earlier Yes".into(),
            lease_minutes: (mem.allowed_until_ms - t).div_ceil(60_000),
            retry_after_s: 0,
            asked: false,
        };
    }
    // a No (or no answer) stops everything for 5 minutes, even if the phone is put down meanwhile
    if t < mem.denied_until_ms {
        return Decision {
            allowed: false,
            why: "the last ask was refused or unanswered: not asking again yet".into(),
            lease_minutes: 0,
            retry_after_s: (mem.denied_until_ms - t).div_ceil(1000),
            asked: false,
        };
    }
    let reasons = match judge(signals, ask.idle_limit_ms, &ask.extra_apps) {
        Use::Free(why) => {
            return Decision {
                allowed: true,
                why: format!("not in use: {why}"),
                lease_minutes: 0,
                retry_after_s: 0,
                asked: false,
            }
        }
        Use::InUse(r) => r,
    };
    let in_use = reasons.join(", ");
    let Some(channel) = channel else {
        return Decision {
            allowed: false,
            why: format!("in use ({in_use}), and no push channel is configured (--route): refused"),
            lease_minutes: 0,
            retry_after_s: REASK_AFTER_MS / 1000,
            asked: false,
        };
    };
    let mut approver = Approver::new(channel).with_expiry_ms(ask.wait_ms);
    let reason = format!(
        "wants to use your screen for about {} min: {} (in use: {in_use})",
        ask.minutes, ask.why
    );
    let req = approver.request(t, &ask.name, "ask-screen", "take-over", &reason, "");
    let outcome = approver.ask(&req, now, sleep);
    let after = now();
    match outcome {
        Outcome::Approved { .. } => {
            let lease = (ask.minutes * 3).div_ceil(2).clamp(1, MAX_LEASE_MIN);
            mem.allowed_until_ms = after + lease * 60_000;
            mem.denied_until_ms = 0;
            Decision {
                allowed: true,
                why: "the owner said Yes (INTERIM: a plain tap, no fingerprint yet)".into(),
                lease_minutes: lease,
                retry_after_s: 0,
                asked: true,
            }
        }
        o => {
            mem.denied_until_ms = after + REASK_AFTER_MS;
            Decision {
                allowed: false,
                why: format!("in use ({in_use}): {o}"),
                lease_minutes: 0,
                retry_after_s: REASK_AFTER_MS / 1000,
                asked: !matches!(o, Outcome::Unavailable(_)),
            }
        }
    }
}

/// Build the channels a route file names.
pub fn load_route(path: &Path) -> Result<Box<dyn Channel + Send>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("route file {}: {e}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("route file {}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let file = |r: &serde_json::Value, k: &str| -> Option<PathBuf> {
        r.get(k).and_then(|x| x.as_str()).map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                dir.join(p)
            }
        })
    };
    let url = |r: &serde_json::Value, k: &str| -> Result<Option<String>, String> {
        match r.get(k).and_then(|x| x.as_str()) {
            None => Ok(None),
            Some(u) if u.starts_with("http://") || u.starts_with("https://") => {
                Ok(Some(u.trim_end_matches('/').to_string()))
            }
            Some(_) => Err(format!("route {k} must be an http(s) URL")),
        }
    };
    let routes = v["routes"]
        .as_array()
        .ok_or("route file needs a \"routes\" list")?;
    let mut chans: Vec<Box<dyn Channel + Send>> = vec![];
    for r in routes {
        let topic = file(r, "topic_file").ok_or("every route needs a topic_file")?;
        let mut cfg = NtfyConfig::new(topic);
        if let Some(b) = url(r, "base")? {
            cfg.base = b;
        }
        cfg.tap_base = url(r, "tap_base")?;
        cfg.inbound_topic_file = file(r, "inbound_topic_file");
        cfg.token_file = file(r, "token_file");
        cfg.tap_token_file = file(r, "tap_token_file");
        cfg.title = TITLE.into();
        chans.push(Box::new(NtfyChannel::new(cfg, CurlHttp::default())));
    }
    if chans.is_empty() {
        return Err("route file names no routes".into());
    }
    Ok(Box::new(AnyChannel::new(chans)))
}

fn default_state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AGENTROID_STATE_DIR") {
        return PathBuf::from(d).join("ask-screen");
    }
    if let Some(d) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(d).join("agentroid").join("ask-screen");
    }
    if let Some(d) = std::env::var_os("HOME") {
        return PathBuf::from(d).join(".agentroid").join("ask-screen");
    }
    PathBuf::from(".agentroid-ask-screen")
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub const USAGE: &str = "usage: agentroid ask-screen --device <serial|ip:port> --minutes N --why \"TEXT\" [--route FILE] [--adb PATH] [--name LABEL] [--idle-min M] [--wait-s S] [--in-use-app PKG]... [--state-dir DIR]";

struct Parsed {
    ask: Ask,
    route: Option<PathBuf>,
    adb: PathBuf,
    state_dir: PathBuf,
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut device = None;
    let mut minutes = None;
    let mut why = None;
    let mut route = None;
    let mut adb = PathBuf::from("adb");
    let mut name = "your phone".to_string();
    let mut idle_min = 5u64;
    let mut wait_s = 120u64;
    let mut extra = vec![];
    let mut state_dir = None;
    let mut i = 0;
    while i < args.len() {
        let v = args
            .get(i + 1)
            .cloned()
            .ok_or(format!("{} needs a value", args[i]))?;
        let num = |lo: u64, hi: u64| -> Result<u64, String> {
            v.parse::<u64>()
                .ok()
                .filter(|n| (lo..=hi).contains(n))
                .ok_or(format!(
                    "{} must be a whole number from {lo} to {hi}",
                    args[i]
                ))
        };
        match args[i].as_str() {
            "--device" => device = Some(v.clone()),
            "--minutes" => minutes = Some(num(1, MAX_LEASE_MIN)?),
            "--why" => why = Some(v.clone()),
            "--route" => route = Some(PathBuf::from(&v)),
            "--adb" => adb = PathBuf::from(&v),
            "--name" => name = v.clone(),
            "--idle-min" => idle_min = num(1, 60)?,
            "--wait-s" => wait_s = num(30, 300)?,
            "--in-use-app" => extra.push(v.clone()),
            "--state-dir" => state_dir = Some(PathBuf::from(&v)),
            other => return Err(format!("unknown option {other}")),
        }
        i += 2;
    }
    let device = device.ok_or("--device is required")?;
    let plain = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
    };
    if !plain(&device) {
        return Err("--device must be a serial, ip:port or adb mDNS name".into());
    }
    let why = why.ok_or("--why is required")?;
    if why.trim().is_empty() || why.chars().count() > 200 || why.chars().any(char::is_control) {
        return Err("--why must be 1-200 characters on one line".into());
    }
    if name.chars().count() > 40 || name.chars().any(char::is_control) {
        return Err("--name must be at most 40 characters on one line".into());
    }
    Ok(Parsed {
        ask: Ask {
            device,
            minutes: minutes.ok_or("--minutes is required")?,
            why,
            name,
            idle_limit_ms: idle_min * 60_000,
            wait_ms: wait_s * 1000,
            extra_apps: extra,
        },
        route,
        adb,
        state_dir: state_dir.unwrap_or_else(default_state_dir),
    })
}

fn signals_json(s: &Signals) -> serde_json::Value {
    serde_json::json!({
        "screen": s.wakefulness,
        "focus": s.focus,
        "keyboard": s.ime_shown,
        "idle_s": s.idle_ms.map(|m| m / 1000),
        "locked": s.keyguard,
    })
}

/// `agentroid ask-screen ...`: (the JSON line, the exit code).
pub fn command(args: &[String]) -> (String, i32) {
    let p = match parse_args(args) {
        Ok(p) => p,
        Err(e) => return (format!("{e}\n{USAGE}"), 2),
    };
    let runner = agentroid_adb_link::guard::GuardedRunner::new(AdbRunner::new(&p.adb));
    let Some(signals) = read_signals(&runner, &p.ask.device) else {
        let out = serde_json::json!({
            "decision": "denied", "interim": true,
            "why": "the phone could not be read (not connected, or adb refused)",
            "retry_after_s": 60,
        });
        return (out.to_string(), 3);
    };
    let mut mem = load_memory(&p.state_dir, &p.ask.device);
    let channel = match &p.route {
        Some(r) => match load_route(r) {
            Ok(c) => Some(c),
            Err(e) => return (format!("{e}\n{USAGE}"), 2),
        },
        None => None,
    };
    let d = decide(&p.ask, &signals, &mut mem, channel, &unix_ms, &|ms| {
        std::thread::sleep(std::time::Duration::from_millis(ms))
    });
    let saved = save_memory(&p.state_dir, &p.ask.device, mem);
    let out = serde_json::json!({
        "decision": if d.allowed { "allowed" } else { "denied" },
        "why": d.why,
        "lease_minutes": d.lease_minutes,
        "recheck": if d.allowed && d.lease_minutes == 0 { "run ask-screen again before each burst of actions" } else { "" },
        "retry_after_s": d.retry_after_s,
        "asked": d.asked,
        "interim": true,
        "signals": signals_json(&signals),
        "state_saved": saved.is_ok(),
    });
    let line = out.to_string();
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p.state_dir.join("ask-screen.log.jsonl"))
        .and_then(|mut f| {
            use std::io::Write;
            let entry = serde_json::json!({
                "t_ms": unix_ms(), "device": device_key(&p.ask.device), "minutes": p.ask.minutes,
                "why": p.ask.why, "result": out,
            });
            writeln!(f, "{entry}")
        });
    (line, if d.allowed { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{FakeChannel, Inbound};
    use agentroid_adb_link::runner::{RunError, RunOutput};
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

    const POWER_AWAKE: &str = "POWER MANAGER (dumpsys power)\n  mWakefulness=Awake\n  mWakefulnessChanging=false\n  mLastWakeTime=170339786 (870480 ms ago)\n  mLastSleepTime=170506409 (703857 ms ago)\n mLastUserActivityTime(excludingAttention)=171200266\n";
    const POWER_DOZING: &str = "  mWakefulness=Dozing\n  mLastWakeTime=170339786 (870480 ms ago)\n mLastUserActivityTime(excludingAttention)=170386406\n";

    fn awake(focus: &str, ime: bool, idle_ms: u64) -> Signals {
        Signals {
            wakefulness: Some("Awake".into()),
            idle_ms: Some(idle_ms),
            focus: Some(focus.into()),
            ime_shown: Some(ime),
            keyguard: Some(false),
        }
    }

    #[test]
    fn power_gives_the_wakefulness_and_the_idle_time_on_the_uptime_clock() {
        // now = 170339786 + 870480 = 171210266; last touch 171200266, so 10 s idle
        assert_eq!(
            parse_power(POWER_AWAKE),
            (Some("Awake".into()), Some(10_000))
        );
        assert_eq!(parse_power(POWER_DOZING).0.as_deref(), Some("Dozing"));
        assert_eq!(parse_power(""), (None, None));
        // no "ms ago" line: the idle time is unknown, never guessed
        assert_eq!(
            parse_power("mWakefulness=Awake\nmLastUserActivityTime=5\n").1,
            None
        );
    }

    #[test]
    fn window_gives_the_focused_package_and_the_lock_screen() {
        let w = "  mCurrentFocus=Window{1a2b u0 com.anthropic.claude/com.anthropic.claude.MainActivity}\n    isKeyguardShowing=false\n";
        assert_eq!(
            parse_window(w),
            (Some("com.anthropic.claude".into()), Some(false))
        );
        assert_eq!(
            parse_window("  mCurrentFocus=null\n").0.as_deref(),
            Some("")
        );
        assert_eq!(parse_window("").0, None);
    }

    #[test]
    fn the_keyboard_is_input_shown_not_input_view_shown() {
        let poco = "  mInputShown=false\n  mIsInputViewShown=true mStatusIcon=0\n";
        assert_eq!(parse_ime(poco), Some(false));
        assert_eq!(parse_ime("  mInputShown=true\n"), Some(true));
        assert_eq!(parse_ime("nothing"), None);
    }

    #[test]
    fn in_use_means_a_listed_app_the_keyboard_or_a_recent_touch() {
        let lim = 5 * 60_000;
        // positive controls: each signal alone makes it in use
        for app in IN_USE_APPS {
            assert!(
                matches!(
                    judge(&awake(app, false, 3_600_000), lim, &[]),
                    Use::InUse(_)
                ),
                "{app}"
            );
        }
        assert!(matches!(
            judge(&awake("com.example.reader", true, 3_600_000), lim, &[]),
            Use::InUse(_)
        ));
        assert!(matches!(
            judge(&awake("com.example.reader", false, 30_000), lim, &[]),
            Use::InUse(_)
        ));
        assert!(matches!(
            judge(
                &awake("com.example.reader", false, 3_600_000),
                lim,
                &["com.example.reader".into()]
            ),
            Use::InUse(_)
        ));
        // and with none of them, an awake screen is free
        assert!(matches!(
            judge(&awake("com.example.reader", false, 3_600_000), lim, &[]),
            Use::Free(_)
        ));
        // screen off is free
        let off = Signals {
            wakefulness: Some("Dozing".into()),
            ..Signals::default()
        };
        assert!(matches!(judge(&off, lim, &[]), Use::Free(_)));
    }

    #[test]
    fn anything_unknown_while_the_screen_is_on_counts_as_in_use() {
        // a protected phone: only dumpsys power answers
        let only_power = Signals {
            wakefulness: Some("Awake".into()),
            idle_ms: Some(3_600_000),
            ..Signals::default()
        };
        assert!(matches!(judge(&only_power, 60_000, &[]), Use::InUse(_)));
        assert!(matches!(
            judge(&Signals::default(), 60_000, &[]),
            Use::InUse(_)
        ));
        let mut s = awake("com.example.reader", false, 3_600_000);
        s.idle_ms = None;
        assert!(matches!(judge(&s, 60_000, &[]), Use::InUse(_)));
    }

    struct Phone {
        power: &'static str,
        window: Option<&'static str>,
        ime: Option<&'static str>,
        sent: Mutex<Vec<String>>,
    }
    impl Runner for Phone {
        fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
            let cmd = a.last().cloned().unwrap_or_default();
            self.sent.lock().unwrap().push(cmd.clone());
            let out = if cmd == "dumpsys power" {
                Some(self.power)
            } else if cmd.starts_with("dumpsys window") {
                self.window
            } else if cmd.starts_with("dumpsys input_method") {
                self.ime
            } else {
                None
            };
            match out {
                Some(o) => Ok(RunOutput {
                    code: Some(0),
                    stdout: o.into(),
                    stderr: String::new(),
                }),
                None => Err(RunError::Refused("not on the list".into())),
            }
        }
    }

    #[test]
    fn a_dozing_phone_is_read_with_one_command() {
        let p = Phone {
            power: POWER_DOZING,
            window: None,
            ime: None,
            sent: Mutex::new(vec![]),
        };
        let s = read_signals(&p, "X").unwrap();
        assert_eq!(s.screen_on(), Some(false));
        assert_eq!(p.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_phone_that_refuses_all_but_dumpsys_power_reads_as_in_use_when_on() {
        let p = Phone {
            power: POWER_AWAKE,
            window: None,
            ime: None,
            sent: Mutex::new(vec![]),
        };
        let s = read_signals(&p, "X").unwrap();
        assert_eq!(s.focus, None);
        assert!(matches!(judge(&s, 60_000, &[]), Use::InUse(_)));
    }

    #[test]
    fn the_guard_lets_only_dumpsys_power_reach_the_protected_phone() {
        use agentroid_adb_link::guard::{GuardedRunner, S24_SERIAL};
        struct Hw(Mutex<Vec<String>>);
        impl Runner for Hw {
            fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
                let cmd = a.last().cloned().unwrap_or_default();
                self.0.lock().unwrap().push(cmd.clone());
                let stdout = if cmd == "getprop ro.serialno" {
                    format!("{S24_SERIAL}\n")
                } else if cmd == "dumpsys power" {
                    POWER_AWAKE.to_string()
                } else {
                    "  mCurrentFocus=null\n  mInputShown=false\n".to_string()
                };
                Ok(RunOutput {
                    code: Some(0),
                    stdout,
                    stderr: String::new(),
                })
            }
        }
        let g = GuardedRunner::new(Hw(Mutex::new(vec![])));
        let s = read_signals(&g, "192.0.2.10:5555").unwrap();
        assert_eq!(s.wakefulness.as_deref(), Some("Awake"));
        assert_eq!(
            (s.focus.clone(), s.ime_shown),
            (None, None),
            "window and input_method never sent"
        );
        assert!(matches!(judge(&s, 60_000, &[]), Use::InUse(_)));
    }

    fn ask() -> Ask {
        Ask {
            device: "192.0.2.10:5555".into(),
            minutes: 10,
            why: "run the settings check".into(),
            name: "your phone".into(),
            idle_limit_ms: 5 * 60_000,
            wait_ms: 120_000,
            extra_apps: vec![],
        }
    }

    fn answering(verb: &'static str) -> (Box<dyn Channel + Send>, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(vec![]));
        let s2 = seen.clone();
        let ch = FakeChannel {
            answer_with: Some(Box::new(move |r, n| {
                s2.lock().unwrap().push(r.message());
                if n == 2 {
                    vec![Inbound {
                        event_id: format!("e{n}"),
                        time_ms: r.created_ms + 1000,
                        message: format!("{verb} {}", r.id),
                    }]
                } else {
                    vec![]
                }
            })),
            ..FakeChannel::default()
        };
        (Box::new(ch), seen)
    }

    #[test]
    fn free_is_allowed_without_a_push() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let d = decide(
            &ask(),
            &awake("com.example.reader", false, 3_600_000),
            &mut m,
            None,
            &|| t.get(),
            &|ms| t.set(t.get() + ms),
        );
        assert!(d.allowed && !d.asked && d.lease_minutes == 0);
    }

    #[test]
    fn yes_gives_a_lease_and_calls_inside_it_are_not_asked_again() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let (ch, seen) = answering("allow");
        let busy = awake("com.anthropic.claude", true, 1_000);
        let d = decide(&ask(), &busy, &mut m, Some(ch), &|| t.get(), &|ms| {
            t.set(t.get() + ms)
        });
        assert!(d.allowed && d.asked);
        assert_eq!(d.lease_minutes, 15, "1.5 x 10 min");
        assert!(seen.lock().unwrap()[0].contains("about 10 min: run the settings check"));
        // inside the lease: allowed, no channel needed
        let d2 = decide(&ask(), &busy, &mut m, None, &|| t.get(), &|ms| {
            t.set(t.get() + ms)
        });
        assert!(d2.allowed && !d2.asked);
        // after it: asked again (here: no channel, so refused)
        t.set(t.get() + 16 * 60_000);
        let d3 = decide(&ask(), &busy, &mut m, None, &|| t.get(), &|ms| {
            t.set(t.get() + ms)
        });
        assert!(!d3.allowed);
    }

    #[test]
    fn no_refuses_and_stops_asking_for_five_minutes() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let (ch, _) = answering("deny");
        let busy = awake("com.whatsapp", false, 1_000);
        let d = decide(&ask(), &busy, &mut m, Some(ch), &|| t.get(), &|ms| {
            t.set(t.get() + ms)
        });
        assert!(!d.allowed && d.asked && d.retry_after_s == 300);
        // a second call within 5 min: refused without a push
        let (ch2, seen2) = answering("allow");
        t.set(t.get() + 60_000);
        let d2 = decide(&ask(), &busy, &mut m, Some(ch2), &|| t.get(), &|ms| {
            t.set(t.get() + ms)
        });
        assert!(!d2.allowed && !d2.asked);
        assert!(
            seen2.lock().unwrap().is_empty(),
            "no push inside the cool-down"
        );
        assert_eq!(d2.retry_after_s, 240);
        // after it: asked again
        t.set(t.get() + 5 * 60_000);
        let (ch3, _) = answering("allow");
        assert!(
            decide(&ask(), &busy, &mut m, Some(ch3), &|| t.get(), &|ms| t
                .set(t.get() + ms))
            .allowed
        );
    }

    #[test]
    fn after_a_no_even_a_free_phone_waits_out_the_five_minutes() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let (ch, _) = answering("deny");
        decide(
            &ask(),
            &awake("com.whatsapp", false, 1_000),
            &mut m,
            Some(ch),
            &|| t.get(),
            &|ms| t.set(t.get() + ms),
        );
        let off = Signals {
            wakefulness: Some("Asleep".into()),
            ..Signals::default()
        };
        t.set(t.get() + 60_000);
        assert!(
            !decide(&ask(), &off, &mut m, None, &|| t.get(), &|ms| t
                .set(t.get() + ms))
            .allowed
        );
        t.set(t.get() + 5 * 60_000);
        assert!(
            decide(&ask(), &off, &mut m, None, &|| t.get(), &|ms| t
                .set(t.get() + ms))
            .allowed
        );
    }

    #[test]
    fn no_answer_is_a_refusal() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let d = decide(
            &ask(),
            &awake("com.anthropic.claude", false, 1_000),
            &mut m,
            Some(Box::new(FakeChannel::default())),
            &|| t.get(),
            &|ms| t.set(t.get() + ms),
        );
        assert!(!d.allowed && d.asked);
        assert!(d.why.contains("no human answer"), "{}", d.why);
    }

    #[test]
    fn the_push_title_says_interim() {
        assert!(TITLE.starts_with("INTERIM"));
    }

    #[test]
    fn bad_arguments_are_usage_errors() {
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        for bad in [
            a(&["--minutes", "5", "--why", "x"]),
            a(&["--device", "X", "--why", "x"]),
            a(&["--device", "X", "--minutes", "0", "--why", "x"]),
            a(&["--device", "X", "--minutes", "61", "--why", "x"]),
            a(&["--device", "X;reboot", "--minutes", "5", "--why", "x"]),
            a(&["--device", "X", "--minutes", "5", "--why", "line\nbreak"]),
            a(&[
                "--device",
                "X",
                "--minutes",
                "5",
                "--why",
                "x",
                "--wait-s",
                "5",
            ]),
            a(&[
                "--device",
                "X",
                "--minutes",
                "5",
                "--why",
                "x",
                "--bogus",
                "1",
            ]),
        ] {
            assert_eq!(command(&bad).1, 2, "{bad:?}");
        }
    }

    #[test]
    fn memory_round_trips_per_device() {
        let d = std::env::temp_dir().join(format!("agentroid-askscreen-{}", std::process::id()));
        let m = Memory {
            allowed_until_ms: 5,
            denied_until_ms: 7,
        };
        save_memory(&d, "A", m).unwrap();
        assert_eq!(load_memory(&d, "a"), m, "the key ignores case");
        assert_eq!(load_memory(&d, "B"), Memory::default());
        let _ = std::fs::remove_dir_all(&d);
    }
}
