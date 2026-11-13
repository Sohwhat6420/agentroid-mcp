//! `agentroid ask-screen`: ask the phone's owner before taking over the screen (INTERIM).
//!
//! Any session that is about to drive a phone (through Agentroid's tools or raw adb) runs this
//! first. It reads whether the phone is in use and, if it is, asks the owner with a Yes / No push.
//!
//! ```text
//! agentroid ask-screen --device <serial|ip:port> --minutes N --why "TEXT"
//!     [--session LABEL] [--route FILE] [--adb PATH] [--name LABEL] [--idle-min M]
//!     [--wait-s S] [--in-use-app PKG]... [--state-dir DIR]
//! ```
//!
//! * **In use** means the screen is on and any of these holds: the focused app is one the owner
//!   is likely typing in (the Claude app, WhatsApp, Gmail, plus `--in-use-app`), the keyboard is
//!   open, the last touch was under `--idle-min` minutes ago (default 5), or an app is holding the
//!   screen on (a video, a reader with keep-screen-on). Anything that cannot be read counts as in
//!   use, so a phone that answers only `dumpsys power` (a protected phone) is in use whenever its
//!   screen is on.
//! * **A call** that is ringing or live (`mCallState` from `dumpsys telephony.registry`, read
//!   whatever the screen shows) is in use, screen on or off. A dump that ran and has no
//!   `mCallState` line at all (a device with no telephony) means no carrier call. A call state
//!   that cannot be read (the read failed or was refused) counts as in use, so a protected phone
//!   (which answers only `dumpsys power`) is always asked.
//! * **Not in use** (screen off or dozing; or on, untouched, nothing holding it on, no keyboard,
//!   no listed app): allowed at once, with no push. Run the command again before each burst.
//! * **In use:** one push with what the session wants and for how long. Yes gives a lease of
//!   `ceil(1.5 x N)` minutes (at most 60) to that session and purpose only (`--session`, default
//!   the `--why` text). No, or no answer within `--wait-s` (default 90, so the command ends inside
//!   a two-minute tool timeout), is a refusal, and every call for that phone in the next 5 minutes
//!   is refused without a push (the owner's "No stops it, re-ask in 5 min").
//! * **One ask at a time per phone:** while an ask is waiting on the owner, other calls for that
//!   phone are refused without a second push.
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
//!              "token_file": "laptop.token", "tap_token_file": "tap.token",
//!              "login_topics": true}]}
//! ```
//!
//! Every field but `topic_file` is optional (`base` defaults to `https://ntfy.sh`). Relative paths
//! are read from the route file's folder. `login_topics` says the server refuses anonymous access,
//! so short topic names are safe; it needs `token_file` and is refused for ntfy.sh. A token is
//! sent only to an https or loopback `base`.

use crate::approval::{AnyChannel, Approver, Channel, CurlHttp, NtfyChannel, NtfyConfig, Outcome};
use agentroid_adb_link::runner::{AdbRunner, Runner};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The apps a person is likely typing in or reading: in use whatever the idle time.
pub const IN_USE_APPS: [&str; 3] = [
    "com.anthropic.claude",
    "com.whatsapp",
    "com.google.android.gm",
];
/// After a No (or no answer), every call for that phone is refused without a push for this long.
pub const REASK_AFTER_MS: u64 = 5 * 60_000;
/// The longest lease a Yes gives.
pub const MAX_LEASE_MIN: u64 = 60;
/// The longest `--wait-s`.
pub const MAX_WAIT_S: u64 = 300;
/// The default `--wait-s`: short enough that the whole command ends inside a 2-minute timeout.
pub const DEFAULT_WAIT_S: u64 = 90;
/// Room for sending the push and the last poll, on top of the wait, in the pending marker.
const PENDING_SLACK_MS: u64 = 15_000;
/// The title of the push. It says INTERIM until the fingerprint step ships.
pub const TITLE: &str =
    "INTERIM ask (plain Yes/No, no fingerprint yet): Agentroid wants your screen";

/// What could be read from the phone. `None` is "could not be read".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signals {
    /// `mWakefulness` from `dumpsys power`: Awake, Dozing, Asleep, Dreaming.
    pub wakefulness: Option<String>,
    /// Milliseconds since the last user activity (touch, key).
    pub idle_ms: Option<u64>,
    /// Whether an app holds a screen wake lock (a video, keep-screen-on) or a call holds the
    /// proximity sensor (the phone is at someone's ear): someone is using it.
    pub screen_kept_on: Option<bool>,
    /// The package of the focused window (`mCurrentFocus`), or `""` when nothing has focus.
    pub focus: Option<String>,
    /// Whether the keyboard is on screen (`mInputShown`).
    pub ime_shown: Option<bool>,
    /// Whether the lock screen is showing (reported only; it does not decide).
    pub keyguard: Option<bool>,
    /// Whether a phone call is ringing or live (an `mCallState` other than 0 in
    /// `dumpsys telephony.registry`), read whatever the screen state.
    pub in_call: Option<bool>,
}

impl Signals {
    /// Awake is on; Asleep, Dozing and Dreaming are off; any other value is unknown.
    pub fn screen_on(&self) -> Option<bool> {
        match self.wakefulness.as_deref()? {
            "Awake" => Some(true),
            "Asleep" | "Dozing" | "Dreaming" => Some(false),
            _ => None,
        }
    }
}

/// The verdict on whether the phone is in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Use {
    Free(String),
    InUse(Vec<String>),
}

/// What `dumpsys power` gives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Power {
    pub wakefulness: Option<String>,
    pub idle_ms: Option<u64>,
    pub screen_kept_on: Option<bool>,
}

/// `key<value> (<n> ms ago)`, with `key` ending in `=`: (value, n).
fn ms_ago(line: &str, key: &str) -> Option<(u64, u64)> {
    let (val, ago) = line.strip_prefix(key)?.split_once(" (")?;
    let ago = ago.strip_suffix(" ms ago)")?;
    Some((val.parse().ok()?, ago.parse().ok()?))
}

/// The `mWakeLockSummary` bits that mean someone is using the screen: SCREEN_BRIGHT (0x2),
/// SCREEN_DIM (0x4) and PROXIMITY_SCREEN_OFF (0x10, held through a call while the phone is at an
/// ear; the screen stays Awake with no touches).
const SCREEN_LOCK_BITS: u64 = 0x2 | 0x4 | 0x10;
const SCREEN_LOCK_TYPES: [&str; 4] = [
    "SCREEN_BRIGHT_WAKE_LOCK",
    "SCREEN_DIM_WAKE_LOCK",
    "FULL_WAKE_LOCK",
    "PROXIMITY_SCREEN_OFF_WAKE_LOCK",
];

/// Parse `dumpsys power`.
///
/// * The idle time comes from the display group's `lastUserActivityTime=<ms> (<n> ms ago)`
///   (the most recent group), which states it directly. Without that line, the phone's "now" on
///   the uptime clock is `mLastWakeTime` (or `mLastSleepTime`) plus its "ms ago", and the idle
///   time is that minus `mLastUserActivityTime(excludingAttention)`. Other "ms ago" lines are on
///   other clocks (`mLastGlobalWakeTimeRealtime` is on the realtime clock) and are never used. If
///   the wake and sleep lines disagree on "now" by more than a second, the idle time is unknown.
/// * The screen is kept on when `mWakeLockSummary` has a screen bit or the wake-lock list names
///   a screen lock.
pub fn parse_power(text: &str) -> Power {
    let mut wake = None;
    let mut group_idle: Option<u64> = None;
    let mut wake_now: Option<u64> = None;
    let mut sleep_now: Option<u64> = None;
    let mut last: Option<u64> = None;
    let mut summary: Option<u64> = None;
    let mut screen_lock_line = false;
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("mWakefulness=") {
            if wake.is_none() && !v.is_empty() && v.chars().all(|c| c.is_ascii_alphabetic()) {
                wake = Some(v.to_string());
            }
            continue;
        }
        if let Some((_, ago)) = ms_ago(l, "lastUserActivityTime=") {
            group_idle = Some(group_idle.map_or(ago, |g| g.min(ago)));
            continue;
        }
        if let Some((v, a)) = ms_ago(l, "mLastWakeTime=") {
            wake_now = wake_now.or(v.checked_add(a));
            continue;
        }
        if let Some((v, a)) = ms_ago(l, "mLastSleepTime=") {
            sleep_now = sleep_now.or(v.checked_add(a));
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
        if let Some(v) = l.strip_prefix("mWakeLockSummary=0x") {
            if summary.is_none() {
                summary = u64::from_str_radix(v.trim(), 16).ok();
            }
        }
        if SCREEN_LOCK_TYPES.contains(&l.split_whitespace().next().unwrap_or(""))
            || l == "mProximityPositive=true"
        {
            screen_lock_line = true;
        }
    }
    let idle_ms = group_idle.or_else(|| {
        let now = match (wake_now, sleep_now) {
            (Some(a), Some(b)) if a.abs_diff(b) > 1000 => return None,
            (Some(a), _) | (None, Some(a)) => a,
            (None, None) => return None,
        };
        Some(now.saturating_sub(last?))
    });
    let screen_kept_on = if screen_lock_line {
        Some(true)
    } else {
        summary.map(|s| s & SCREEN_LOCK_BITS != 0)
    };
    Power {
        wakefulness: wake,
        idle_ms,
        screen_kept_on,
    }
}

fn is_user_token(t: &str) -> bool {
    t.len() > 1 && t.starts_with('u') && t[1..].chars().all(|c| c.is_ascii_digit())
}

/// The package in `Window{<hash> u0 [<STATE>] com.example.app/com.example.app.Main [EXITING]}`:
/// the token with a `/`; else one that looks like a package; else the first plain name. Window
/// states such as `EXITING` are never taken. `None` when no token fits.
fn focus_package(v: &str) -> Option<String> {
    let inner = v.trim_start_matches("Window{").trim_end_matches('}');
    let toks: Vec<&str> = inner.split_whitespace().skip(1).collect();
    let state = |t: &&&str| t.chars().all(|c| c.is_ascii_uppercase() || c == '_');
    let pick = toks
        .iter()
        .find(|t| t.contains('/'))
        .or_else(|| toks.iter().find(|t| t.contains('.') && !state(t)))
        .or_else(|| toks.iter().find(|t| !is_user_token(t) && !state(t)))?;
    Some(pick.split('/').next().unwrap_or("").to_string())
}

/// Parse `dumpsys window` (or its `mCurrentFocus=` / `isKeyguardShowing=` lines): the focused
/// package and whether the lock screen shows.
pub fn parse_window(text: &str) -> (Option<String>, Option<bool>) {
    let mut focus = None;
    let mut seen_focus = false;
    let mut keyguard = None;
    for line in text.lines() {
        let l = line.trim();
        if !seen_focus {
            if let Some(v) = l.strip_prefix("mCurrentFocus=") {
                seen_focus = true;
                focus = if v == "null" {
                    Some(String::new())
                } else {
                    focus_package(v)
                };
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

/// Parse the output of a `dumpsys telephony.registry` that ran (exit 0): `Some(true)` when any
/// `mCallState=` line (one per SIM) is ringing (1) or off the hook (2), or not a number;
/// `Some(false)` when every such line is idle (0), or when there is no such line at all (no
/// telephony, so no carrier call); `None` when the dump is a refusal (`Permission Denial`). A
/// read that failed or was refused never gets here: the caller keeps it unknown.
pub fn parse_call_state(text: &str) -> Option<bool> {
    let mut seen = None;
    for word in text.split_whitespace() {
        if let Some(v) = word.strip_prefix("mCallState=") {
            let busy = v.parse::<u32>() != Ok(0);
            seen = Some(seen.unwrap_or(false) || busy);
        }
    }
    match seen {
        Some(v) => Some(v),
        None if text.contains("Permission Denial") => None,
        None => Some(false),
    }
}

/// The hardware serial `ask-screen` keeps its state under, from what `getprop ro.serialno` gave
/// (`None`: it could not be read). `None` too for anything that is not a plain serial, and for a
/// junk serial many phones share: then the state is kept under `--device`.
pub(crate) fn state_serial(read: Option<&str>) -> Option<String> {
    read.map(|s| s.trim().to_string())
        .filter(|s| (1..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric()))
        // a junk serial shared by many phones would make them share leases: use --device then
        .filter(|s| !is_generic_serial(s))
}

/// Decide whether the phone is in use. Anything unknown while the screen is on counts as in use,
/// and so does a call (ringing, live, or a call state that cannot be read) whatever the screen
/// shows.
pub fn judge(s: &Signals, idle_limit_ms: u64, extra_apps: &[String]) -> Use {
    let call = match s.in_call {
        Some(false) => None,
        Some(true) => Some("a call is in progress"),
        None => Some("whether a call is in progress is unknown"),
    };
    match s.screen_on() {
        Some(false) => {
            if let Some(c) = call {
                return Use::InUse(vec![c.to_string()]);
            }
            return Use::Free(format!(
                "screen not on ({})",
                s.wakefulness.as_deref().unwrap_or("")
            ));
        }
        None => return Use::InUse(vec!["screen state unknown".into()]),
        Some(true) => {}
    }
    let mut why: Vec<String> = call.map(str::to_string).into_iter().collect();
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
    match s.screen_kept_on {
        None => why.push("whether an app holds the screen on is unknown".into()),
        Some(true) => {
            why.push("the screen is held on (a video, a reader, or a call at an ear)".into())
        }
        Some(false) => {}
    }
    if why.is_empty() {
        Use::Free(format!(
            "screen on but untouched for {} s, nothing holding it on, no keyboard, no listed app",
            s.idle_ms.unwrap_or(0) / 1000
        ))
    } else {
        Use::InUse(why)
    }
}

pub(crate) fn shell(r: &dyn Runner, device: &str, cmd: &str) -> Option<String> {
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

/// [`shell`], with `mark` called just before the command itself is sent (after the guard's own
/// checks): see [`Runner::run_marked`].
fn shell_marked(r: &dyn Runner, device: &str, cmd: &str, mark: &mut dyn FnMut()) -> Option<String> {
    let a = vec![
        "-s".to_string(),
        device.to_string(),
        "shell".to_string(),
        cmd.to_string(),
    ];
    match r.run_marked(&a, 15_000, mark) {
        Ok(o) if o.ok() => Some(o.stdout),
        _ => None,
    }
}

/// Read the signals through `r` (production: the guarded adb runner, so a protected phone answers
/// `dumpsys power` and nothing else). `None` when not even `dumpsys power` could be read.
pub fn read_signals(r: &dyn Runner, device: &str) -> Option<Signals> {
    read_signals_timed(r, device, &|| 0).map(|(s, _, _)| s)
}

/// [`read_signals`], with the times on `clock` just before and just after the `dumpsys power`
/// read (the read that states the last touch as "so many ms ago"). The screen lock uses them.
/// "Just before" is after the guarded runner's own serial read, so only the power read is timed.
pub fn read_signals_timed(
    r: &dyn Runner,
    device: &str,
    clock: &dyn Fn() -> u64,
) -> Option<(Signals, u64, u64)> {
    let mut sent = None;
    let power = shell_marked(r, device, "dumpsys power", &mut || sent = Some(clock()));
    let to = clock();
    let from = sent.unwrap_or(to);
    let power = parse_power(&power?);
    let to = to.max(from);
    let mut s = Signals {
        wakefulness: power.wakefulness,
        idle_ms: power.idle_ms,
        screen_kept_on: power.screen_kept_on,
        ..Signals::default()
    };
    // a call is in use whatever the screen shows (held at an ear, the screen may be off)
    // the whole dump, not a grep: a grep that finds nothing exits 1, which would make a device
    // with no telephony read as unknown; a dump that failed or was refused stays unknown
    s.in_call = shell(r, device, "dumpsys telephony.registry").and_then(|t| parse_call_state(&t));
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
    Some((s, from, to))
}

/// A hardware serial too generic to tell phones apart (cheap phones and some boards all report
/// `0123456789ABCDEF`): never used as a state or lock key.
pub fn is_generic_serial(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    let mut c = l.chars();
    let first = c.next();
    matches!(l.as_str(), "0123456789abcdef" | "0123456789" | "unknown")
        || first.is_some_and(|f| c.all(|x| x == f))
}

/// What the state file remembers per phone (Unix ms): the Yes leases, one per session and
/// purpose; the No cool-down; and the end of an ask that is still waiting on the owner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Memory {
    pub leases: BTreeMap<String, u64>,
    pub denied_until_ms: u64,
    pub asking_until_ms: u64,
}

fn short_hash(s: &str) -> String {
    let d = Sha256::digest(s.as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn device_key(device: &str) -> String {
    short_hash(&device.to_ascii_lowercase())
}

/// A lease covers one session and one purpose.
pub fn lease_key(session: &str, why: &str) -> String {
    short_hash(&format!("{session}\n{why}"))
}

fn memory_path(dir: &Path, device: &str) -> PathBuf {
    dir.join(format!("ask-screen-{}.json", device_key(device)))
}

/// Load the memory for a phone. Values further ahead than this program ever writes (a lease past
/// 60 minutes, a cool-down past 5, a pending ask past the longest wait) are ignored, as are
/// leases that have run out. So a damaged file, or one left by an older run, can only make it ask.
/// The file is not signed: anyone who can write the state folder can forge a lease of up to 60
/// minutes, so it belongs in the user's own profile.
pub fn load_memory(dir: &Path, device: &str, now: u64) -> Memory {
    let Ok(t) = std::fs::read_to_string(memory_path(dir, device)) else {
        return Memory::default();
    };
    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or_default();
    let mut m = Memory::default();
    if let Some(obj) = v["leases"].as_object() {
        for (k, x) in obj {
            let ok_key = k.len() == 16 && k.chars().all(|c| c.is_ascii_hexdigit());
            if let Some(u) = x.as_u64() {
                if ok_key && u > now && u <= now + MAX_LEASE_MIN * 60_000 {
                    m.leases.insert(k.clone(), u);
                }
            }
        }
    }
    let d = v["denied_until_ms"].as_u64().unwrap_or(0);
    if d <= now + REASK_AFTER_MS {
        m.denied_until_ms = d;
    }
    let a = v["asking_until_ms"].as_u64().unwrap_or(0);
    if a <= now + MAX_WAIT_S * 1000 + PENDING_SLACK_MS {
        m.asking_until_ms = a;
    }
    m
}

/// Write the memory for a phone: a temporary file, then a rename, so a reader never sees half.
pub fn save_memory(dir: &Path, device: &str, m: &Memory) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("state dir {}: {e}", dir.display()))?;
    let body = serde_json::json!({
        "leases": m.leases,
        "denied_until_ms": m.denied_until_ms,
        "asking_until_ms": m.asking_until_ms,
    });
    let path = memory_path(dir, device);
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, body.to_string()).map_err(|e| format!("state file: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("state file: {e}")
    })
}

/// Write only what changed between `before` and `after` onto the file as it is now, so another
/// session's lease or cool-down written in the meantime is kept.
pub fn save_changes(
    dir: &Path,
    device: &str,
    before: &Memory,
    after: &Memory,
    now: u64,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    let mut cur = load_memory(dir, device, now);
    for (k, v) in &after.leases {
        if before.leases.get(k) != Some(v) {
            cur.leases.insert(k.clone(), *v);
        }
    }
    if after.denied_until_ms != before.denied_until_ms {
        cur.denied_until_ms = after.denied_until_ms;
    }
    if after.asking_until_ms != before.asking_until_ms {
        cur.asking_until_ms = after.asking_until_ms;
    }
    save_memory(dir, device, &cur)
}

/// The request as the command line gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub device: String,
    pub minutes: u64,
    pub why: String,
    /// Who is asking; a lease covers this session and this `why` only.
    pub session: String,
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

fn refused(why: String, retry_after_s: u64, asked: bool) -> Decision {
    Decision {
        allowed: false,
        why,
        lease_minutes: 0,
        retry_after_s,
        asked,
    }
}

/// The decision, given the signals, the memory and a channel. `now` and `sleep` are Unix-ms
/// clock functions (tests pass fakes). Updates `mem`, and calls `persist` with it just before a
/// push goes out, so other sessions see that an ask is waiting even if this one is killed. If
/// `persist` returns false (the marker could not be saved), nothing is sent and the call is refused.
pub fn decide(
    ask: &Ask,
    signals: &Signals,
    mem: &mut Memory,
    channel: Option<Box<dyn Channel + Send>>,
    now: &dyn Fn() -> u64,
    sleep: &dyn Fn(u64),
    persist: &mut dyn FnMut(&Memory) -> bool,
) -> Decision {
    let t = now();
    let key = lease_key(&ask.session, &ask.why);
    if let Some(&until) = mem.leases.get(&key) {
        if t < until {
            return Decision {
                allowed: true,
                why: "inside the lease from an earlier Yes for this session and purpose".into(),
                lease_minutes: (until - t).div_ceil(60_000),
                retry_after_s: 0,
                asked: false,
            };
        }
    }
    // a No (or no answer) stops everything for 5 minutes, even if the phone is put down meanwhile
    if t < mem.denied_until_ms {
        return refused(
            "the last ask was refused or unanswered: not asking again yet".into(),
            (mem.denied_until_ms - t).div_ceil(1000),
            false,
        );
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
    if t < mem.asking_until_ms {
        return refused(
            format!("in use ({in_use}), and another ask is already waiting on the owner"),
            (mem.asking_until_ms - t).div_ceil(1000),
            false,
        );
    }
    let Some(channel) = channel else {
        return refused(
            format!("in use ({in_use}), and no push channel is configured (--route): refused"),
            REASK_AFTER_MS / 1000,
            false,
        );
    };
    mem.asking_until_ms = t + ask.wait_ms + PENDING_SLACK_MS;
    if !persist(mem) {
        mem.asking_until_ms = 0;
        return refused(
            format!("in use ({in_use}), and the pending ask could not be saved (--state-dir), or another ask started meanwhile: refused without a push"),
            60,
            false,
        );
    }
    let mut approver = Approver::new(channel).with_expiry_ms(ask.wait_ms);
    let reason = format!(
        "wants to use your screen for about {} min: {} (in use: {in_use})",
        ask.minutes, ask.why
    );
    let req = approver.request(t, &ask.name, "ask-screen", "take-over", &reason, "");
    let outcome = approver.ask(&req, now, sleep);
    let after = now();
    mem.asking_until_ms = 0;
    match outcome {
        Outcome::Approved { .. } => {
            let lease = (ask.minutes * 3).div_ceil(2).clamp(1, MAX_LEASE_MIN);
            mem.leases.insert(key, after + lease * 60_000);
            Decision {
                allowed: true,
                why: "the owner said Yes (INTERIM: a plain tap, no fingerprint yet)".into(),
                lease_minutes: lease,
                retry_after_s: 0,
                asked: true,
            }
        }
        // the push never went out: refused, but no cool-down (the owner said nothing)
        o @ Outcome::Unavailable(_) => refused(format!("in use ({in_use}): {o}"), 60, false),
        o => {
            mem.denied_until_ms = after + REASK_AFTER_MS;
            refused(
                format!("in use ({in_use}): {o}"),
                REASK_AFTER_MS / 1000,
                true,
            )
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
        cfg.login_topics = match r.get("login_topics") {
            None => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(_) => return Err("route login_topics must be true or false".into()),
        };
        cfg.title = TITLE.into();
        chans.push(Box::new(NtfyChannel::new(cfg, CurlHttp::default())));
    }
    if chans.is_empty() {
        return Err("route file names no routes".into());
    }
    Ok(Box::new(AnyChannel::new(chans)))
}

/// Where the leases and the log are kept unless `--state-dir` says otherwise. The daemon's screen
/// lock reads the leases from here too.
pub fn default_state_dir() -> PathBuf {
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

pub const USAGE: &str = "usage: agentroid ask-screen --device <serial|ip:port> --minutes N --why \"TEXT\" [--session LABEL] [--route FILE] [--adb PATH] [--name LABEL] [--idle-min M] [--wait-s S] [--in-use-app PKG]... [--state-dir DIR]";

struct Parsed {
    ask: Ask,
    route: Option<PathBuf>,
    adb: PathBuf,
    state_dir: PathBuf,
}

fn one_line(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control)
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut device = None;
    let mut minutes = None;
    let mut why = None;
    let mut session = None;
    let mut route = None;
    let mut adb = PathBuf::from("adb");
    let mut name = "your phone".to_string();
    let mut idle_min = 5u64;
    let mut wait_s = DEFAULT_WAIT_S;
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
            "--session" => session = Some(v.clone()),
            "--route" => route = Some(PathBuf::from(&v)),
            "--adb" => adb = PathBuf::from(&v),
            "--name" => name = v.clone(),
            "--idle-min" => idle_min = num(1, 60)?,
            "--wait-s" => wait_s = num(30, MAX_WAIT_S)?,
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
    if !one_line(&why, 200) {
        return Err("--why must be 1-200 characters on one line".into());
    }
    let session = session.unwrap_or_else(|| why.clone());
    if !one_line(&session, 80) && session != why {
        return Err("--session must be 1-80 characters on one line".into());
    }
    if name.chars().count() > 40 || name.chars().any(char::is_control) {
        return Err("--name must be at most 40 characters on one line".into());
    }
    Ok(Parsed {
        ask: Ask {
            device,
            minutes: minutes.ok_or("--minutes is required")?,
            why,
            session,
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
        "screen_kept_on": s.screen_kept_on,
        "locked": s.keyguard,
        "in_call": s.in_call,
    })
}

/// `agentroid ask-screen ...`: (the JSON line, the exit code).
pub fn command(args: &[String]) -> (String, i32) {
    let p = match parse_args(args) {
        Ok(p) => p,
        Err(e) => return (format!("{e}\n{USAGE}"), 2),
    };
    let channel = match &p.route {
        Some(r) => match load_route(r) {
            Ok(c) => Some(c),
            Err(e) => return (format!("{e}\n{USAGE}"), 2),
        },
        None => None,
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
    // keep the state under the phone's hardware serial when it can be read, so the daemon's screen
    // lock finds the lease whatever name adb gives the phone (serial, ip:port, mDNS name)
    let hw = state_serial(shell(&runner, &p.ask.device, "getprop ro.serialno").as_deref());
    let dir = &p.state_dir;
    let device: &str = hw.as_deref().unwrap_or(&p.ask.device);
    let loaded = load_memory(dir, device, unix_ms());
    let mut mem = loaded.clone();
    let written = std::cell::RefCell::new(loaded);
    let mut persist = |m: &Memory| {
        let mut w = written.borrow_mut();
        // another session may have started its ask after this one loaded the file: look again
        // just before writing, so two asks do not both go out (narrows the race; not a file lock)
        let t = unix_ms();
        if load_memory(dir, device, t).asking_until_ms > t.max(w.asking_until_ms) {
            return false;
        }
        let ok = save_changes(dir, device, &w, m, t).is_ok();
        if ok {
            *w = m.clone();
        }
        ok
    };
    let d = decide(
        &p.ask,
        &signals,
        &mut mem,
        channel,
        &unix_ms,
        &|ms| std::thread::sleep(std::time::Duration::from_millis(ms)),
        &mut persist,
    );
    let saved = save_changes(dir, device, &written.borrow(), &mem, unix_ms());
    let out = serde_json::json!({
        "decision": if d.allowed { "allowed" } else { "denied" },
        "why": d.why,
        "lease_minutes": d.lease_minutes,
        "recheck": if d.allowed && d.lease_minutes == 0 { "run ask-screen again before each burst of actions" } else { "" },
        "retry_after_s": d.retry_after_s,
        "asked": d.asked,
        "interim": true,
        "session": p.ask.session,
        "state_key": if hw.is_some() { "hardware serial" } else { "--device" },
        "signals": signals_json(&signals),
        "state_saved": saved.is_ok(),
    });
    let line = out.to_string();
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ask-screen.log.jsonl"))
        .and_then(|mut f| {
            use std::io::Write;
            let entry = serde_json::json!({
                "t_ms": unix_ms(), "device": device_key(device), "minutes": p.ask.minutes,
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

    // The POCO's own layout. The realtime-clock line comes first and must never set "now".
    const POWER_AWAKE: &str = "POWER MANAGER (dumpsys power)\n\nPower Manager State:\n  mDirty=0x0\n  mWakefulness=Awake\n  mWakefulnessChanging=false\n  mIsPowered=true\n  mWakeLockSummary=0x1\n  mLastGlobalWakeTimeRealtime=901234567 (5 ms ago)\n  mLastWakeTime=170339786 (870480 ms ago)\n  mLastSleepTime=170506409 (703857 ms ago)\n mLastUserActivityTime(excludingAttention)=171200266\n\nWake Locks: size=1\n  PARTIAL_WAKE_LOCK              'termux:service' ACQ=-1h2m (uid=10234 pid=1234)\n\nDisplay Group 0:\n  lastUserActivityTime=171200266 (10000 ms ago)\n";
    const POWER_DOZING: &str = "  mWakefulness=Dozing\n  mWakeLockSummary=0x41\n  mLastWakeTime=170339786 (870480 ms ago)\n mLastUserActivityTime(excludingAttention)=170386406\nWake Locks: size=2\n  PARTIAL_WAKE_LOCK 'termux:service'\n  DOZE_WAKE_LOCK 'dream:doze'\n";

    fn without(text: &str, prefix: &str) -> String {
        text.lines()
            .filter(|l| !l.trim().starts_with(prefix))
            .map(|l| format!("{l}\n"))
            .collect()
    }

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

    #[test]
    fn power_takes_the_idle_time_from_the_display_group_line() {
        let p = parse_power(POWER_AWAKE);
        assert_eq!(p.wakefulness.as_deref(), Some("Awake"));
        assert_eq!(p.idle_ms, Some(10_000));
        // the POCO's real line, read on 13 Nov
        let real = "  mWakefulness=Awake\n  lastUserActivityTime=172182878 (563524 ms ago)\n";
        assert_eq!(parse_power(real).idle_ms, Some(563_524));
        // two groups: the most recent touch wins
        let two =
            "  lastUserActivityTime=5 (9000 ms ago)\n  lastUserActivityTime=6 (2000 ms ago)\n";
        assert_eq!(parse_power(two).idle_ms, Some(2_000));
    }

    #[test]
    fn without_the_group_line_now_comes_from_the_wake_time_never_the_realtime_clock() {
        // regression: the realtime line is the first "ms ago" line; taking it read a 10 s idle
        // phone as idle for days, so free
        let no_group = without(POWER_AWAKE, "lastUserActivityTime=");
        assert_eq!(parse_power(&no_group).idle_ms, Some(10_000));
        let only_sleep = without(&no_group, "mLastWakeTime=");
        assert_eq!(parse_power(&only_sleep).idle_ms, Some(10_000));
        // wake and sleep disagree on "now" by 5 s: unknown, never guessed
        let skew = no_group.replace("(703857 ms ago)", "(708857 ms ago)");
        assert_eq!(parse_power(&skew).idle_ms, None);
        // no "ms ago" line at all: unknown
        assert_eq!(
            parse_power("mWakefulness=Awake\nmLastUserActivityTime=5\n").idle_ms,
            None
        );
        assert_eq!(parse_power(""), Power::default());
    }

    #[test]
    fn power_says_whether_an_app_holds_the_screen_on() {
        assert_eq!(parse_power(POWER_AWAKE).screen_kept_on, Some(false));
        assert_eq!(parse_power(POWER_DOZING).screen_kept_on, Some(false));
        let video = POWER_AWAKE.replace("mWakeLockSummary=0x1", "mWakeLockSummary=0x3");
        assert_eq!(parse_power(&video).screen_kept_on, Some(true));
        let dim = POWER_AWAKE.replace("mWakeLockSummary=0x1", "mWakeLockSummary=0x5");
        assert_eq!(parse_power(&dim).screen_kept_on, Some(true));
        // the list names a screen lock even if the summary does not
        let listed = POWER_AWAKE.replace(
            "  PARTIAL_WAKE_LOCK ",
            "  SCREEN_BRIGHT_WAKE_LOCK        'WindowManager' ON_AFTER_RELEASE\n  PARTIAL_WAKE_LOCK ",
        );
        assert_eq!(parse_power(&listed).screen_kept_on, Some(true));
        // no summary: unknown
        let none = without(POWER_AWAKE, "mWakeLockSummary=");
        assert_eq!(parse_power(&none).screen_kept_on, None);
    }

    #[test]
    fn a_call_at_an_ear_counts_as_a_held_screen() {
        // the proximity lock: Awake, untouched for minutes, no keyboard, the dialer focused
        let call = POWER_AWAKE.replace("mWakeLockSummary=0x1", "mWakeLockSummary=0x11");
        assert_eq!(parse_power(&call).screen_kept_on, Some(true));
        let listed = POWER_AWAKE.replace(
            "  PARTIAL_WAKE_LOCK ",
            "  PROXIMITY_SCREEN_OFF_WAKE_LOCK 'incall' ACQ=-6m\n  PARTIAL_WAKE_LOCK ",
        );
        assert_eq!(parse_power(&listed).screen_kept_on, Some(true));
        let near = format!("{POWER_AWAKE}  mProximityPositive=true\n");
        assert_eq!(parse_power(&near).screen_kept_on, Some(true));
        let far = format!("{POWER_AWAKE}  mProximityPositive=false\n");
        assert_eq!(parse_power(&far).screen_kept_on, Some(false));
        let mut s = awake("com.google.android.dialer", false, 600_000);
        s.screen_kept_on = parse_power(&call).screen_kept_on;
        assert!(matches!(judge(&s, 5 * 60_000, &[]), Use::InUse(_)));
    }

    #[test]
    fn an_unknown_wakefulness_is_never_screen_off() {
        for w in ["awake", "On", "Unknown", ""] {
            let mut s = awake("com.example.reader", false, 3_600_000);
            s.wakefulness = Some(w.into());
            assert_eq!(s.screen_on(), None, "{w:?}");
            assert!(matches!(judge(&s, 60_000, &[]), Use::InUse(_)), "{w:?}");
        }
        for w in ["Asleep", "Dozing", "Dreaming"] {
            let s = Signals {
                wakefulness: Some(w.into()),
                in_call: Some(false),
                ..Signals::default()
            };
            assert!(matches!(judge(&s, 60_000, &[]), Use::Free(_)), "{w}");
        }
    }

    #[test]
    fn a_pending_marker_that_cannot_be_saved_sends_nothing() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let (ch, seen) = answering("allow");
        let d = decide(
            &ask(),
            &awake("com.anthropic.claude", true, 1_000),
            &mut m,
            Some(ch),
            &|| t.get(),
            &|ms| t.set(t.get() + ms),
            &mut |_| false,
        );
        assert!(!d.allowed && !d.asked && d.retry_after_s == 60, "{d:?}");
        assert!(seen.lock().unwrap().is_empty(), "no push went out");
        assert_eq!(m, Memory::default(), "nothing held against the next ask");
    }

    #[test]
    fn window_gives_the_focused_package_and_the_lock_screen() {
        let w = "  mCurrentFocus=Window{535f00b u0 com.anthropic.claude/com.anthropic.claude.mainactivity.MainActivity}\n    isKeyguardShowing=false\n";
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
    fn a_window_state_such_as_exiting_is_never_the_focused_app() {
        let f = |v: &str| parse_window(&format!("  mCurrentFocus={v}\n")).0;
        assert_eq!(
            f("Window{abc u0 com.whatsapp/com.whatsapp.Main EXITING}").as_deref(),
            Some("com.whatsapp")
        );
        assert_eq!(
            f("Window{abc u0 Splash Screen com.whatsapp EXITING}").as_deref(),
            Some("com.whatsapp")
        );
        assert_eq!(
            f("Window{abc u0 NotificationShade}").as_deref(),
            Some("NotificationShade")
        );
        // nothing but a state: unknown, so in use
        assert_eq!(f("Window{abc u0 EXITING}"), None);
    }

    #[test]
    fn the_keyboard_is_input_shown_not_input_view_shown() {
        let poco = "  mInputShown=false\n  mIsInputViewShown=true mStatusIcon=0\n";
        assert_eq!(parse_ime(poco), Some(false));
        assert_eq!(parse_ime("  mInputShown=true\n"), Some(true));
        assert_eq!(parse_ime("nothing"), None);
    }

    #[test]
    fn in_use_means_a_listed_app_the_keyboard_a_recent_touch_or_a_held_screen() {
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
        // a video: untouched for an hour, but an app holds the screen on
        let mut video = awake("com.example.player", false, 3_600_000);
        video.screen_kept_on = Some(true);
        assert!(matches!(judge(&video, lim, &[]), Use::InUse(_)));
        // and with none of them, an awake screen is free
        assert!(matches!(
            judge(&awake("com.example.reader", false, 3_600_000), lim, &[]),
            Use::Free(_)
        ));
        // screen off is free
        let off = Signals {
            wakefulness: Some("Dozing".into()),
            in_call: Some(false),
            ..Signals::default()
        };
        assert!(matches!(judge(&off, lim, &[]), Use::Free(_)));
    }

    #[test]
    fn a_call_is_in_use_screen_on_or_off_and_an_unreadable_call_state_is_never_free() {
        let lim = 5 * 60_000;
        let off = |c: Option<bool>| Signals {
            wakefulness: Some("Asleep".into()),
            in_call: c,
            ..Signals::default()
        };
        assert!(matches!(judge(&off(Some(false)), lim, &[]), Use::Free(_)));
        match judge(&off(Some(true)), lim, &[]) {
            Use::InUse(r) => assert_eq!(r, vec!["a call is in progress".to_string()]),
            other => panic!("{other:?}"),
        }
        assert!(matches!(judge(&off(None), lim, &[]), Use::InUse(_)));
        // screen on, untouched for an hour, nothing else: the call alone makes it in use
        let mut on = awake("com.example.reader", false, 3_600_000);
        assert!(matches!(judge(&on, lim, &[]), Use::Free(_)));
        on.in_call = Some(true);
        assert!(matches!(judge(&on, lim, &[]), Use::InUse(_)));
        on.in_call = None;
        assert!(matches!(judge(&on, lim, &[]), Use::InUse(_)));
        // the registry's lines, one per SIM: any ringing (1) or off-hook (2) line is a call
        assert_eq!(
            parse_call_state("  mCallState=0\n  mCallState=0\n"),
            Some(false)
        );
        assert_eq!(
            parse_call_state("  mCallState=0\n  mCallState=2\n"),
            Some(true)
        );
        assert_eq!(parse_call_state("  mCallState=1\n"), Some(true));
        assert_eq!(parse_call_state("  mCallState=x\n"), Some(true));
        // a dump that ran with no such line: no telephony, so no carrier call
        assert_eq!(parse_call_state("nothing here"), Some(false));
        assert_eq!(
            parse_call_state("Can't find service: telephony.registry\n"),
            Some(false)
        );
        assert_eq!(parse_call_state(""), Some(false));
        // a refusal printed by dumpsys itself stays unknown
        assert_eq!(
            parse_call_state("Permission Denial: can't dump telephony.registry from pid=1\n"),
            None
        );
    }

    #[test]
    fn the_state_is_kept_under_a_real_hardware_serial_and_never_a_junk_one() {
        assert_eq!(
            state_serial(Some("R5CX12345AB\n")).as_deref(),
            Some("R5CX12345AB")
        );
        // junk serials many phones share: under --device instead
        for junk in [
            "0123456789ABCDEF",
            "0123456789abcdef",
            "unknown",
            "0000000000",
        ] {
            assert_eq!(state_serial(Some(junk)), None, "{junk}");
        }
        // unreadable, empty, too long, not plain
        assert_eq!(state_serial(None), None);
        assert_eq!(state_serial(Some("  \n")), None);
        assert_eq!(state_serial(Some(&"A1".repeat(33))), None);
        assert_eq!(state_serial(Some("R5CX-12")), None);
    }

    /// Answers `dumpsys power` (dozing) and the call-state read with `call`; `None` refuses it.
    struct CallPhone {
        call: Option<(i32, &'static str)>,
    }
    impl Runner for CallPhone {
        fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
            let cmd = a.last().cloned().unwrap_or_default();
            let (code, out) = if cmd == "dumpsys power" {
                (0, POWER_DOZING)
            } else if cmd == "dumpsys telephony.registry" {
                match self.call {
                    Some(c) => c,
                    None => return Err(RunError::Refused("not on the list".into())),
                }
            } else {
                return Err(RunError::Refused("not on the list".into()));
            };
            Ok(RunOutput {
                code: Some(code),
                stdout: out.into(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn a_device_with_no_telephony_has_no_call_but_a_failed_call_read_stays_unknown() {
        // the dump ran and has no mCallState line (a tablet, an emulator without a radio)
        for out in [
            "Can't find service: telephony.registry\n",
            "  mServiceState=...\n",
        ] {
            let s = read_signals(
                &CallPhone {
                    call: Some((0, out)),
                },
                "X",
            )
            .unwrap();
            assert_eq!(s.in_call, Some(false), "{out:?}");
            assert!(matches!(judge(&s, 60_000, &[]), Use::Free(_)));
        }
        // the read failed (non-zero exit) or was refused (a protected phone): unknown, in use
        for p in [
            CallPhone {
                call: Some((1, "")),
            },
            CallPhone { call: None },
        ] {
            let s = read_signals(&p, "X").unwrap();
            assert_eq!(s.in_call, None);
            assert!(matches!(judge(&s, 60_000, &[]), Use::InUse(_)));
        }
        // the positive control: a call the registry reports
        let s = read_signals(
            &CallPhone {
                call: Some((0, "  mCallState=2\n")),
            },
            "X",
        )
        .unwrap();
        assert_eq!(s.in_call, Some(true));
    }

    #[test]
    fn only_the_power_read_itself_is_timed() {
        use std::cell::Cell;
        // a runner that spends 900 ms on its own checks before it sends each command (the guard's
        // serial read), then 40 ms on the command
        struct Checked<'a>(&'a Cell<u64>);
        impl Runner for Checked<'_> {
            fn run(&self, a: &[String], t: u64) -> Result<RunOutput, RunError> {
                self.run_marked(a, t, &mut || {})
            }
            fn run_marked(
                &self,
                a: &[String],
                _t: u64,
                mark: &mut dyn FnMut(),
            ) -> Result<RunOutput, RunError> {
                self.0.set(self.0.get() + 900);
                mark();
                self.0.set(self.0.get() + 40);
                let out = if a.last().map(String::as_str) == Some("dumpsys power") {
                    POWER_DOZING
                } else {
                    NO_CALL
                };
                Ok(RunOutput {
                    code: Some(0),
                    stdout: out.into(),
                    stderr: String::new(),
                })
            }
        }
        let clock = Cell::new(1_000);
        let (_, from, to) = read_signals_timed(&Checked(&clock), "X", &|| clock.get()).unwrap();
        assert_eq!((from, to), (1_900, 1_940));
    }

    #[test]
    fn generic_serials_are_never_keys() {
        for s in [
            "0123456789ABCDEF",
            "0123456789abcdef",
            "unknown",
            "0000000000000000",
            "1",
        ] {
            assert!(is_generic_serial(s), "{s}");
        }
        for s in ["R5CX12345AB", "a1b2c3d4", "EMULATOR35X1X11X0"] {
            assert!(!is_generic_serial(s), "{s}");
        }
    }

    #[test]
    fn anything_unknown_while_the_screen_is_on_counts_as_in_use() {
        // a protected phone: only dumpsys power answers
        let only_power = Signals {
            wakefulness: Some("Awake".into()),
            idle_ms: Some(3_600_000),
            screen_kept_on: Some(false),
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
        let mut s = awake("com.example.reader", false, 3_600_000);
        s.screen_kept_on = None;
        assert!(matches!(judge(&s, 60_000, &[]), Use::InUse(_)));
    }

    struct Phone {
        power: &'static str,
        window: Option<&'static str>,
        ime: Option<&'static str>,
        sent: Mutex<Vec<String>>,
    }
    /// The call-state line a phone in no call gives.
    const NO_CALL: &str = "  mCallState=0\n";
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
            } else if cmd.starts_with("dumpsys telephony.registry") {
                // like the window and keyboard reads: answered only when the phone answers them
                self.window.map(|_| NO_CALL)
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
    fn a_dozing_phone_is_read_with_two_commands_power_and_the_call_state() {
        let p = Phone {
            power: POWER_DOZING,
            window: Some(""),
            ime: None,
            sent: Mutex::new(vec![]),
        };
        let s = read_signals(&p, "X").unwrap();
        assert_eq!((s.screen_on(), s.in_call), (Some(false), Some(false)));
        assert!(matches!(judge(&s, 60_000, &[]), Use::Free(_)));
        let sent = p.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2, "{sent:?}");
        // a phone that does not give its call state is in use even with the screen off
        let p = Phone {
            power: POWER_DOZING,
            window: None,
            ime: None,
            sent: Mutex::new(vec![]),
        };
        let s = read_signals(&p, "X").unwrap();
        assert_eq!(s.in_call, None);
        assert!(matches!(judge(&s, 60_000, &[]), Use::InUse(_)));
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
        struct Hw(Arc<Mutex<Vec<String>>>);
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
        let log = Arc::new(Mutex::new(vec![]));
        let g = GuardedRunner::new(Hw(log.clone()));
        let s = read_signals(&g, "192.0.2.10:5555").unwrap();
        assert_eq!(s.wakefulness.as_deref(), Some("Awake"));
        assert_eq!((s.focus.clone(), s.ime_shown), (None, None));
        assert!(matches!(judge(&s, 60_000, &[]), Use::InUse(_)));
        // what reached the phone: the guard's serial check and dumpsys power, nothing else
        let sent = log.lock().unwrap().clone();
        assert!(sent.iter().any(|c| c == "dumpsys power"), "{sent:?}");
        assert!(
            sent.iter()
                .all(|c| c == "getprop ro.serialno" || c == "dumpsys power"),
            "{sent:?}"
        );
    }

    fn ask() -> Ask {
        Ask {
            device: "192.0.2.10:5555".into(),
            minutes: 10,
            why: "run the settings check".into(),
            session: "session A".into(),
            name: "your phone".into(),
            idle_limit_ms: 5 * 60_000,
            wait_ms: 90_000,
            extra_apps: vec![],
        }
    }

    fn run(
        a: &Ask,
        s: &Signals,
        m: &mut Memory,
        ch: Option<Box<dyn Channel + Send>>,
        t: &Cell<u64>,
    ) -> Decision {
        decide(
            a,
            s,
            m,
            ch,
            &|| t.get(),
            &|ms| t.set(t.get() + ms),
            &mut |_| true,
        )
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
        let d = run(
            &ask(),
            &awake("com.example.reader", false, 3_600_000),
            &mut m,
            None,
            &t,
        );
        assert!(d.allowed && !d.asked && d.lease_minutes == 0);
    }

    #[test]
    fn yes_gives_a_lease_and_calls_inside_it_are_not_asked_again() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let (ch, seen) = answering("allow");
        let busy = awake("com.anthropic.claude", true, 1_000);
        let d = run(&ask(), &busy, &mut m, Some(ch), &t);
        assert!(d.allowed && d.asked);
        assert_eq!(d.lease_minutes, 15, "1.5 x 10 min");
        assert!(seen.lock().unwrap()[0].contains("about 10 min: run the settings check"));
        // inside the lease: allowed, no channel needed
        let d2 = run(&ask(), &busy, &mut m, None, &t);
        assert!(d2.allowed && !d2.asked);
        // after it: asked again (here: no channel, so refused)
        t.set(t.get() + 16 * 60_000);
        assert!(!run(&ask(), &busy, &mut m, None, &t).allowed);
    }

    #[test]
    fn a_lease_covers_only_the_session_and_purpose_it_was_given_to() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let busy = awake("com.anthropic.claude", true, 1_000);
        let (ch, _) = answering("allow");
        assert!(run(&ask(), &busy, &mut m, Some(ch), &t).allowed);
        let mut other = ask();
        other.session = "session B".into();
        assert!(
            !run(&other, &busy, &mut m, None, &t).allowed,
            "another session"
        );
        let mut other_why = ask();
        other_why.why = "something else".into();
        assert!(
            !run(&other_why, &busy, &mut m, None, &t).allowed,
            "another purpose"
        );
        assert!(run(&ask(), &busy, &mut m, None, &t).allowed);
    }

    #[test]
    fn no_refuses_and_stops_asking_for_five_minutes() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let (ch, _) = answering("deny");
        let busy = awake("com.whatsapp", false, 1_000);
        let d = run(&ask(), &busy, &mut m, Some(ch), &t);
        assert!(!d.allowed && d.asked && d.retry_after_s == 300);
        // a second call within 5 min, from any session: refused without a push
        let (ch2, seen2) = answering("allow");
        t.set(t.get() + 60_000);
        let mut other = ask();
        other.session = "session B".into();
        let d2 = run(&other, &busy, &mut m, Some(ch2), &t);
        assert!(!d2.allowed && !d2.asked);
        assert!(
            seen2.lock().unwrap().is_empty(),
            "no push inside the cool-down"
        );
        assert_eq!(d2.retry_after_s, 240);
        // after it: asked again
        t.set(t.get() + 5 * 60_000);
        let (ch3, _) = answering("allow");
        assert!(run(&ask(), &busy, &mut m, Some(ch3), &t).allowed);
    }

    #[test]
    fn after_a_no_even_a_free_phone_waits_out_the_five_minutes() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let (ch, _) = answering("deny");
        run(
            &ask(),
            &awake("com.whatsapp", false, 1_000),
            &mut m,
            Some(ch),
            &t,
        );
        let off = Signals {
            wakefulness: Some("Asleep".into()),
            in_call: Some(false),
            ..Signals::default()
        };
        t.set(t.get() + 60_000);
        assert!(!run(&ask(), &off, &mut m, None, &t).allowed);
        t.set(t.get() + 5 * 60_000);
        assert!(run(&ask(), &off, &mut m, None, &t).allowed);
    }

    #[test]
    fn the_pending_ask_is_saved_before_the_push_and_blocks_a_second_push() {
        let t = Cell::new(1_000_000u64);
        let order = Arc::new(Mutex::new(vec![]));
        let o2 = order.clone();
        let ch = FakeChannel {
            answer_with: Some(Box::new(move |r, n| {
                o2.lock().unwrap().push("poll".to_string());
                if n == 2 {
                    vec![Inbound {
                        event_id: "e".into(),
                        time_ms: r.created_ms + 1000,
                        message: format!("allow {}", r.id),
                    }]
                } else {
                    vec![]
                }
            })),
            ..FakeChannel::default()
        };
        let mut m = Memory::default();
        let mut snaps = vec![];
        let busy = awake("com.anthropic.claude", true, 1_000);
        let d = decide(
            &ask(),
            &busy,
            &mut m,
            Some(Box::new(ch)),
            &|| t.get(),
            &|ms| t.set(t.get() + ms),
            &mut |mm: &Memory| {
                order.lock().unwrap().push("persist".to_string());
                snaps.push(mm.clone());
                true
            },
        );
        assert!(d.allowed);
        assert_eq!(order.lock().unwrap()[0], "persist", "saved before any poll");
        assert_eq!(snaps[0].asking_until_ms, 1_000_000 + 90_000 + 15_000);
        assert_eq!(m.asking_until_ms, 0, "cleared once answered");
        // another session while an ask is waiting: refused, no second push
        let mut waiting = Memory {
            asking_until_ms: t.get() + 30_000,
            ..Memory::default()
        };
        let (ch2, seen2) = answering("allow");
        let mut other = ask();
        other.session = "session B".into();
        let d2 = run(&other, &busy, &mut waiting, Some(ch2), &t);
        assert!(!d2.allowed && !d2.asked && d2.retry_after_s == 30);
        assert!(seen2.lock().unwrap().is_empty());
        // a free phone is still allowed while someone else waits
        let free = awake("com.example.reader", false, 3_600_000);
        assert!(run(&other, &free, &mut waiting, None, &t).allowed);
    }

    #[test]
    fn no_answer_is_a_refusal() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let d = run(
            &ask(),
            &awake("com.anthropic.claude", false, 1_000),
            &mut m,
            Some(Box::new(FakeChannel::default())),
            &t,
        );
        assert!(!d.allowed && d.asked);
        assert!(d.why.contains("no human answer"), "{}", d.why);
        assert!(m.denied_until_ms > 0, "no answer starts the cool-down");
        assert_eq!(m.asking_until_ms, 0);
    }

    #[test]
    fn a_push_that_never_went_out_refuses_without_a_cool_down() {
        let t = Cell::new(1_000_000u64);
        let mut m = Memory::default();
        let down = FakeChannel {
            send_fails: Some("network down".into()),
            ..FakeChannel::default()
        };
        let busy = awake("com.anthropic.claude", false, 1_000);
        let d = run(&ask(), &busy, &mut m, Some(Box::new(down)), &t);
        assert!(!d.allowed && !d.asked && d.retry_after_s == 60);
        assert_eq!(
            m.denied_until_ms, 0,
            "the owner said nothing, so nothing is held against the next ask"
        );
        assert_eq!(m.asking_until_ms, 0);
        let (ch, _) = answering("allow");
        assert!(run(&ask(), &busy, &mut m, Some(ch), &t).allowed);
    }

    #[test]
    fn the_push_title_says_interim() {
        assert!(TITLE.starts_with("INTERIM"));
    }

    #[test]
    fn bad_arguments_are_usage_errors() {
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let ok = a(&["--device", "X", "--minutes", "5", "--why", "x"]);
        let p = parse_args(&ok).unwrap();
        assert_eq!(p.ask.wait_ms, 90_000, "the default wait ends inside 2 min");
        assert_eq!(p.ask.session, "x", "the session defaults to the why");
        for bad in [
            a(&["--minutes", "5", "--why", "x"]),
            a(&["--device", "X", "--why", "x"]),
            a(&["--device", "X", "--minutes", "0", "--why", "x"]),
            a(&["--device", "X", "--minutes", "61", "--why", "x"]),
            a(&["--device", "X;reboot", "--minutes", "5", "--why", "x"]),
            a(&["--device", "X", "--minutes", "5", "--why", "line\nbreak"]),
            [ok.clone(), a(&["--wait-s", "5"])].concat(),
            [ok.clone(), a(&["--wait-s", "301"])].concat(),
            [ok.clone(), a(&["--session", "two\nlines"])].concat(),
            [ok.clone(), a(&["--session", " "])].concat(),
            [ok.clone(), a(&["--bogus", "1"])].concat(),
            [ok.clone(), a(&["--session"])].concat(),
        ] {
            assert!(parse_args(&bad).is_err(), "{bad:?}");
        }
        // and command turns them into exit code 2 before touching any phone
        assert_eq!(command(&a(&["--device", "X;reboot"])).1, 2);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("agentroid-askscreen-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn memory_round_trips_per_device_and_leaves_no_temporary_file() {
        let d = temp_dir("rt");
        let now = 1_000_000;
        let mut m = Memory {
            denied_until_ms: now + 60_000,
            asking_until_ms: now + 30_000,
            ..Memory::default()
        };
        m.leases.insert(lease_key("s", "w"), now + 10 * 60_000);
        save_memory(&d, "A", &m).unwrap();
        assert_eq!(load_memory(&d, "a", now), m, "the key ignores case");
        assert_eq!(load_memory(&d, "B", now), Memory::default());
        let names: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn values_further_ahead_than_ever_written_are_ignored() {
        let d = temp_dir("far");
        let now = 1_000_000;
        let body = serde_json::json!({
            "leases": {
                lease_key("s", "w"): now + 61 * 60_000,
                lease_key("s", "ok"): now + 60 * 60_000,
                lease_key("s", "old"): now - 1,
                "not-a-key": now + 60_000,
            },
            "denied_until_ms": now + REASK_AFTER_MS + 1,
            "asking_until_ms": now + MAX_WAIT_S * 1000 + PENDING_SLACK_MS + 1,
        });
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(memory_path(&d, "A"), body.to_string()).unwrap();
        let m = load_memory(&d, "A", now);
        assert_eq!(
            m.leases.keys().cloned().collect::<Vec<_>>(),
            vec![lease_key("s", "ok")]
        );
        assert_eq!((m.denied_until_ms, m.asking_until_ms), (0, 0));
        // a broken file is an empty memory
        std::fs::write(memory_path(&d, "A"), "{not json").unwrap();
        assert_eq!(load_memory(&d, "A", now), Memory::default());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saving_keeps_what_another_session_wrote_in_the_meantime() {
        let d = temp_dir("merge");
        let now = 1_000_000;
        let before = load_memory(&d, "A", now);
        // another session gets a Yes while this one is deciding
        let mut theirs = Memory::default();
        theirs.leases.insert(lease_key("B", "w"), now + 60_000);
        save_memory(&d, "A", &theirs).unwrap();
        // this one gets a No
        let mut ours = before.clone();
        ours.denied_until_ms = now + REASK_AFTER_MS;
        save_changes(&d, "A", &before, &ours, now).unwrap();
        let m = load_memory(&d, "A", now);
        assert!(m.leases.contains_key(&lease_key("B", "w")), "{m:?}");
        assert_eq!(m.denied_until_ms, now + REASK_AFTER_MS);
        // clearing the pending marker is written even though the lease map did not change
        let mut pending = m.clone();
        pending.asking_until_ms = now + 10_000;
        save_changes(&d, "A", &m, &pending, now).unwrap();
        let mut cleared = pending.clone();
        cleared.asking_until_ms = 0;
        save_changes(&d, "A", &pending, &cleared, now).unwrap();
        assert_eq!(load_memory(&d, "A", now).asking_until_ms, 0);
        let _ = std::fs::remove_dir_all(&d);
    }
}
