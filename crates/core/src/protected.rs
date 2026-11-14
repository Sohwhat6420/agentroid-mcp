//! The protected-device lease (`docs/design/protected-device-lease.md`). Off unless the daemon is
//! started with `--protected-lease`.
//!
//! Without it the protected phone ([`agentroid_adb_link::guard::S24_SERIAL`]) answers only the
//! ruled reads. With it, a call that names the protected phone runs only under an open lease, and
//! one lease covers a burst of work (30 min by default):
//!
//! The gate is the owner's approval, not the screen's content (his ruling of 14 Nov 2026: nothing
//! on the protected phone is blocked from being seen; a notification asks for his approval before
//! it is taken over). What stays refused under any lease is engineering safety: digits on a PIN
//! prompt, `unlock` (it would type his PIN), raw shell lines, scripts, link segments, code pushes
//! and every adb line outside the guard's exact shapes.
//!
//! * **One Allow.** The first call with no lease sends the owner one push (Allow / Deny, answered
//!   within the ask window, 10 min by default) and is refused as `lease:pending`; the agent retries
//!   once the owner has tapped Allow. The Allow is followed by one quiet notice ("open until
//!   HH:MM SGT") with a Deny button. An Allowed lease also satisfies the screen lock's in-use check,
//!   so one Allow is never asked for twice; an `ask-screen` No still refuses. An Allow dated after
//!   the ask window has closed opens nothing; one dated inside it opens however late the agent
//!   calls again.
//! * **One ask per phone.** At most one ask is out at a time, whoever it is for; another holder is
//!   refused without a push, and no new ask goes out within the ask window of the last one.
//! * **Overnight.** When a presence file signed with the owner's key (HMAC-SHA256), written in the
//!   last `--presence-ttl-min` minutes and not dated in the future, says `asleep`, the lease opens
//!   without an ask, and a notice with a Deny button goes out (no notice sent: no lease). Any
//!   problem with the file turns the overnight rule off, so the call asks instead. The screen
//!   lock's own check still applies: the owner picking the phone up stops the agent.
//! * **Bound to its holder.** A lease, and a pending ask, belong to one daemon lease (the holder's
//!   name and the registry epoch) of the caller's own connection; another holder never inherits
//!   either, and the daemon closes the lease when its holder's connection goes.
//! * **Deny ends it.** The open lease's Deny channel is polled every 10 s; a Deny closes the lease
//!   and holds, for every holder: after an overnight Deny, until a trusted presence file says the
//!   owner is awake; after a daytime Deny (or no answer), for the ask window. The hold is kept in
//!   the holds file (`--state-dir`'s `protected-holds.json`), so a restart does not lift it; a file
//!   that cannot be read holds until a trusted presence state changes. The denied holder may lift
//!   it with a fresh `allow <id>` within the ask window of the Deny; an ask nobody answered cannot
//!   be lifted that way. The Deny events already seen are kept with the holds, so an old Deny
//!   never closes a lease allowed after a restart. A channel that cannot be read for 2 min closes
//!   the lease. The pushes whose Deny may still be in the owner's shade (every overnight notice and
//!   every Allowed ask, for a day, at most 64) are kept too and polled with the lease's own: a Deny
//!   on any of them holds (an overnight one until the owner is awake), lease open or not, one
//!   posted while the daemon was down included, and no overnight lease opens before they are read.
//!   The pending ask and the last ask's time are kept as well: after a restart the ask is nobody's,
//!   its Deny holds, and no new ask goes out within its window.
//! * **Logged first.** Every lease is written to the audit log before it opens (a failed write
//!   opens nothing), and every close is logged with its reason. A close that cannot be written
//!   stays queued, and every call on the protected phone is refused until it is written.

use crate::approval::{Approver, Outcome as Answer, Request, Style};
use crate::audit::{AuditLog, Entry};
use crate::gate::{Call, Refusal};
use crate::transport::DeviceInfo;
use agentroid_adb_link::lease::{self as leasebook, Granted, Holder, Lease, LeaseBook};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The default ask window (`--protected-ask-window-min`).
pub const DEFAULT_ASK_WINDOW_MIN: u64 = 10;
/// The longest ask window (`--protected-ask-window-min` is clamped to `1..=MAX_ASK_WINDOW_MIN`).
pub const MAX_ASK_WINDOW_MIN: u64 = 60;
/// How old a presence file may be (`--presence-ttl-min`, clamped to `1..=MAX_PRESENCE_TTL_MIN`).
pub const DEFAULT_PRESENCE_TTL_MIN: u64 = 15;
pub const MAX_PRESENCE_TTL_MIN: u64 = 60;
/// A presence file dated further ahead than this is refused (a clock set forward to keep it fresh).
pub const PRESENCE_MAX_FUTURE_MS: u64 = 120_000;
/// The shortest presence key.
pub const MIN_KEY_BYTES: usize = 16;
/// What the presence MAC covers: `"{PRESENCE_DOMAIN}\n{state}\n{written_at}"`.
pub const PRESENCE_DOMAIN: &str = "agentroid-presence-v1";
/// How often the open lease's Deny channel is polled.
pub const DENY_POLL_MS: u64 = 10_000;
/// How long the Deny channel may stay unreadable before the lease is closed.
pub const DENY_POLL_FAIL_MS: u64 = 120_000;
/// Clamp an ask window to `1..=MAX_ASK_WINDOW_MIN` minutes.
pub fn clamp_ask_window_min(m: u64) -> u64 {
    m.clamp(1, MAX_ASK_WINDOW_MIN)
}

/// Clamp a presence time-to-live to `1..=MAX_PRESENCE_TTL_MIN` minutes.
pub fn clamp_presence_ttl_min(m: u64) -> u64 {
    m.clamp(1, MAX_PRESENCE_TTL_MIN)
}

// ---------------------------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------------------------

/// Where the presence file and its key are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceConfig {
    pub file: PathBuf,
    /// The HMAC key. Any process that can read it can forge the file, so it belongs in a folder
    /// agents are told never to open.
    pub key_file: PathBuf,
    pub ttl_ms: u64,
}

impl PresenceConfig {
    pub fn new(file: impl Into<PathBuf>, key_file: impl Into<PathBuf>, ttl_min: u64) -> Self {
        PresenceConfig {
            file: file.into(),
            key_file: key_file.into(),
            ttl_ms: clamp_presence_ttl_min(ttl_min) * 60_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedConfig {
    pub lease_ms: u64,
    pub ask_window_ms: u64,
    /// `None`: the overnight rule is off.
    pub presence: Option<PresenceConfig>,
    pub deny_poll_ms: u64,
    pub deny_poll_fail_ms: u64,
    /// Where a hold after a Deny is kept across restarts (`None`: in memory only).
    pub holds_file: Option<PathBuf>,
}

impl ProtectedConfig {
    /// Lease and ask window in minutes, both clamped.
    pub fn new(lease_min: u64, ask_window_min: u64, presence: Option<PresenceConfig>) -> Self {
        ProtectedConfig {
            lease_ms: leasebook::clamp_minutes(lease_min) * 60_000,
            ask_window_ms: clamp_ask_window_min(ask_window_min) * 60_000,
            presence,
            deny_poll_ms: DENY_POLL_MS,
            deny_poll_fail_ms: DENY_POLL_FAIL_MS,
            holds_file: None,
        }
    }

    /// Keep holds in `path` (the daemon: `protected-holds.json` in its state directory).
    pub fn with_holds_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.holds_file = Some(path.into());
        self
    }
}

// ---------------------------------------------------------------------------------------------
// presence
// ---------------------------------------------------------------------------------------------

/// What the presence file says, once checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Presence {
    /// Signed, fresh, and `asleep`.
    Asleep,
    /// Signed and fresh, with another state (`present`, `away`...): the owner is not asleep.
    Other(String),
    /// Not to be trusted (no file, no key, a bad MAC, stale, dated ahead); why, with no secret.
    Untrusted(String),
}

impl Presence {
    pub fn asleep(&self) -> bool {
        matches!(self, Presence::Asleep)
    }
}

impl std::fmt::Display for Presence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Presence::Asleep => f.write_str("asleep"),
            Presence::Other(s) => write!(f, "{s}"),
            Presence::Untrusted(why) => write!(f, "untrusted: {why}"),
        }
    }
}

/// HMAC-SHA256 (RFC 2104) over `sha2`, so no new crate joins the workspace.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let ih = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(ih);
    outer.finalize().into()
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The MAC of a presence reading, as hex.
pub fn presence_mac(key: &[u8], state: &str, written_at: u64) -> String {
    hex(&hmac_sha256(
        key,
        format!("{PRESENCE_DOMAIN}\n{state}\n{written_at}").as_bytes(),
    ))
}

/// A signed presence file's text (what the owner's presence writer puts in the file).
pub fn sign_presence(key: &[u8], state: &str, written_at: u64) -> String {
    serde_json::json!({
        "state": state,
        "written_at": written_at,
        "mac": presence_mac(key, state, written_at),
    })
    .to_string()
}

/// Read the presence key: at least [`MIN_KEY_BYTES`] bytes once surrounding whitespace is
/// trimmed. The key is never put in an error.
pub fn read_key(path: &Path) -> Result<Vec<u8>, String> {
    let raw = std::fs::read(path)
        .map_err(|e| format!("presence key {}: {}", path.display(), e.kind()))?;
    let k = raw.trim_ascii();
    if k.len() < MIN_KEY_BYTES {
        return Err(format!(
            "presence key {} is shorter than {MIN_KEY_BYTES} bytes",
            path.display()
        ));
    }
    Ok(k.to_vec())
}

fn valid_state(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

/// Judge a presence file's text against `key` at `now_ms`.
pub fn judge_presence(text: &str, key: &[u8], ttl_ms: u64, now_ms: u64) -> Presence {
    let bad = |why: &str| Presence::Untrusted(why.to_string());
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return bad("the presence file is not JSON");
    };
    let (Some(state), Some(at), Some(mac)) = (
        v.get("state").and_then(|s| s.as_str()),
        v.get("written_at").and_then(|t| t.as_u64()),
        v.get("mac").and_then(|m| m.as_str()),
    ) else {
        return bad("the presence file needs state, written_at (Unix ms) and mac");
    };
    if !valid_state(state) {
        return bad("the presence state is not 1-32 of [a-z_]");
    }
    let want = presence_mac(key, state, at);
    if !ct_eq(want.as_bytes(), mac.to_ascii_lowercase().as_bytes()) {
        return bad("the presence file's MAC does not match (not signed with the presence key)");
    }
    if at > now_ms.saturating_add(PRESENCE_MAX_FUTURE_MS) {
        return bad("the presence file is dated more than 2 min ahead");
    }
    if now_ms.saturating_sub(at) > ttl_ms {
        return bad("the presence file is stale");
    }
    if state == "asleep" {
        Presence::Asleep
    } else {
        Presence::Other(state.to_string())
    }
}

/// Read and judge the presence file.
pub fn read_presence(cfg: &PresenceConfig, now_ms: u64) -> Presence {
    let key = match read_key(&cfg.key_file) {
        Ok(k) => k,
        Err(why) => return Presence::Untrusted(why),
    };
    match std::fs::read_to_string(&cfg.file) {
        Ok(text) => judge_presence(&text, &key, cfg.ttl_ms, now_ms),
        Err(e) => Presence::Untrusted(format!(
            "presence file {}: {}",
            cfg.file.display(),
            e.kind()
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// the PIN rule (a credential, not content)
// ---------------------------------------------------------------------------------------------

/// Fragments of a PIN or passcode prompt.
const PIN_MARKS: [&str; 6] = [
    "pinentry",
    "pin_entry",
    "pin_view",
    "pinview",
    "passcode",
    "pin_pad",
];
const PIN_TEXTS: [&str; 4] = ["enter pin", "enter your pin", "pin code", "passcode"];

/// What the protected phone's screen shows that matters to the lease: a PIN prompt, and where its
/// digit keys sit. Nothing else on the screen is judged (the owner's ruling of 14 Nov 2026:
/// nothing is blocked from being seen; the gate is his Allow).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenFacts {
    pub pin_prompt: bool,
    /// Where a single-digit key sits on a PIN prompt.
    pub digit_bounds: Vec<(i32, i32, i32, i32)>,
}

/// Read the facts from a uiautomator dump. `Err`: the dump did not parse (the message never
/// quotes it).
pub fn screen_facts(xml: &str) -> Result<ScreenFacts, String> {
    let tree =
        crate::tree::parse(xml, false).map_err(|_| "the screen did not parse".to_string())?;
    let mut f = ScreenFacts::default();
    for n in &tree.nodes {
        let rid = n.rid.to_ascii_lowercase();
        let class = n.class.to_ascii_lowercase();
        let label = format!("{} {}", n.text, n.desc).to_lowercase();
        if PIN_MARKS
            .iter()
            .any(|m| rid.contains(m) || class.contains(m))
            || n.flags.password
            || PIN_TEXTS.iter().any(|t| label.contains(t))
        {
            f.pin_prompt = true;
        }
        if let Some(b) = n.bounds {
            let one_digit = |s: &str| s.len() == 1 && s.chars().all(|c| c.is_ascii_digit());
            if one_digit(n.text.trim()) || one_digit(n.desc.trim()) {
                f.digit_bounds.push((b.l, b.t, b.r, b.b));
            }
        }
    }
    if !f.pin_prompt {
        f.digit_bounds.clear();
    }
    Ok(f)
}

/// The keys that only leave a screen: back, home, escape.
pub fn exit_key(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    let n = n.strip_prefix("keycode_").unwrap_or(&n);
    matches!(n, "back" | "home" | "escape")
}

/// A digit key (`5`, `KEYCODE_5`, `KEYCODE_NUMPAD_5`).
pub fn digit_key(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    let n = n.strip_prefix("keycode_").unwrap_or(&n);
    let n = n.strip_prefix("numpad_").unwrap_or(n);
    n.len() == 1 && n.chars().all(|c| c.is_ascii_digit())
}

/// The paste key (`paste`, `KEYCODE_PASTE`): it could enter digits from the clipboard.
pub fn paste_key(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    n.strip_prefix("keycode_").unwrap_or(&n) == "paste"
}

/// Does `call` need the PIN check before it is sent (it could enter a digit)? Typing, a digit key,
/// the paste key, a tap, a long press and a swipe; reads, launches and every other key (`wake`,
/// `back`, `enter`...) do not, so a dark screen never stops a wake key (review round 4, item 2).
pub fn needs_pin_check(call: &Call) -> bool {
    match call {
        Call::TypeText { .. } | Call::Tap { .. } | Call::LongPress { .. } | Call::Swipe { .. } => {
            true
        }
        Call::Key { name } => digit_key(name) || paste_key(name),
        _ => false,
    }
}

/// The PIN check for `call` before it is sent: digits on a PIN prompt (typed, pressed or tapped,
/// one at a time included) are refused. Exit keys always go. `at` is a touch's device point.
pub fn pin_refusal(
    f: &ScreenFacts,
    call: &Call,
    at: Option<(i32, i32)>,
) -> Option<(&'static str, String)> {
    if !f.pin_prompt {
        return None;
    }
    if let Call::Key { name } = call {
        if exit_key(name) {
            return None;
        }
    }
    let digits = match call {
        Call::TypeText { text, .. } => text.chars().any(|c| c.is_ascii_digit()),
        // a paste may carry digits: never on a PIN prompt (review round 4, item 2)
        Call::Key { name } => digit_key(name) || paste_key(name),
        _ => false,
    };
    let on_digit = at.is_some_and(|(x, y)| {
        f.digit_bounds
            .iter()
            .any(|b| x >= b.0 && x < b.2 && y >= b.1 && y < b.3)
    });
    (digits || on_digit).then(|| {
        (
            "pin",
            "a PIN prompt is showing, and digits are never entered there".to_string(),
        )
    })
}

/// The holder's name as the owner's push shows it (review round 3, item 13): words with a URL in
/// them dropped (a scheme, `www.`, or a dot and a slash), control characters and runs of spaces
/// taken out, at most 24 characters, after "Agent session:" so it never reads as the daemon's own
/// words.
pub fn push_holder(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| {
            let l = w.to_ascii_lowercase();
            !w.is_empty()
                && !l.contains("://")
                && !l.starts_with("www.")
                && !(l.contains('.') && l.contains('/'))
        })
        .collect();
    let clean: String = words
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(24)
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        "Agent session: (no name)".into()
    } else {
        format!("Agent session: {clean}")
    }
}

// ---------------------------------------------------------------------------------------------
// the lease
// ---------------------------------------------------------------------------------------------

/// Who holds the protected phone in the daemon for this call: the caller's own daemon lease
/// (`None`: the caller holds none on it, or it is no longer the current grant).
pub type HolderFn = Box<dyn Fn(&DeviceInfo) -> Option<Holder> + Send>;

#[derive(Debug, Clone)]
struct Pending {
    req: Request,
    holder: Holder,
    /// Read back from the holds file after a restart: it is nobody's now (daemon lease epochs
    /// start again), so its Allow opens nothing, while its Deny still holds (review round 4,
    /// item 4).
    restored: bool,
}

/// A push whose Deny still counts once its lease has ended: an overnight notice, or an Allowed
/// ask (its lease-open notice has the same id). Kept for [`NOTICES_KEPT_MS`], at most
/// [`NOTICES_KEPT`], in the holds file, so a Deny on one still in the owner's shade is seen, even
/// one posted while the daemon was down (review round 4, item 1).
#[derive(Debug, Clone)]
struct Kept {
    req: Request,
    /// An overnight notice: a Deny on it holds until the owner is awake.
    overnight: bool,
}

/// How long a push's Deny is looked for after the push was made.
pub const NOTICES_KEPT_MS: u64 = 24 * 60 * 60 * 1000;
/// How many pushes' Denies are looked for (the newest).
pub const NOTICES_KEPT: usize = 64;

/// What a hold lasts until.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Until {
    /// A daytime Deny, or an ask nobody answered: until this wall-clock time (the ask window).
    Window(u64),
    /// Denied while the owner was asleep: until a trusted presence file says otherwise.
    Awake,
    /// The holds file could not be read at start-up, so what it held is unknown: until a trusted
    /// presence file's state changes from the first one seen (`None`: none seen yet).
    Unknown(Option<String>),
}

#[derive(Debug, Clone)]
struct Held {
    req: Request,
    /// The daemon lease whose ask (or lease) was denied: only it may re-allow.
    holder: Option<Holder>,
    at_ms: u64,
    until: Until,
    /// May the denied holder lift the hold with a fresh `allow <id>`? Only after a Deny: an ask
    /// nobody answered is never re-allowed (review round 3, item 9).
    reallow: bool,
}

#[derive(Debug, Clone)]
struct Running {
    req: Request,
    last_poll_ms: u64,
    fail_since: Option<u64>,
}

/// The holds file's format version (2 adds `reallow` and the Deny events seen; 3 adds the pushes
/// whose Denies are still looked for, the pending ask and when the last ask went out). Versions 1
/// and 2 are still read: a version-1 hold is not re-allowable, and neither keeps pushes or asks.
pub const HOLDS_VERSION: u64 = 3;
/// How many Deny event ids the holds file keeps (the newest).
pub const SEEN_DENIES_KEPT: usize = 64;

/// The lease's state in core.
pub struct Protected {
    pub cfg: ProtectedConfig,
    book: Arc<LeaseBook>,
    holder_of: Option<HolderFn>,
    jobs: HashMap<u64, String>,
    /// At most one ask per phone is out at a time, whoever it is for.
    pending: Option<Pending>,
    held: Option<Held>,
    running: Option<Running>,
    /// When the last ask went out: no other goes out within the ask window of it.
    last_ask_ms: Option<u64>,
    /// The Deny events already applied (newest last), kept in the holds file so an old Deny never
    /// closes a lease re-allowed after a restart (review round 3, item 8).
    seen_denies: Vec<String>,
    /// `seen_denies` has been handed to the approver since start-up.
    seen_fed: bool,
    /// The pushes whose Denies are still looked for (oldest first).
    notices: Vec<Kept>,
    /// When `notices` were last read for a Deny without an error (`None`: not since start-up).
    notices_polled_ms: Option<u64>,
    /// The next ask goes at low priority: an overnight lease ended because the presence file
    /// could not be trusted, and it has not been trusted since (review round 4, item 5; the
    /// owner may reverse this default). Not kept across a restart.
    quiet_ask: bool,
}

impl std::fmt::Debug for Protected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Protected")
            .field("cfg", &self.cfg)
            .field("book", &self.book)
            .field("pending", &self.pending.is_some())
            .field("held", &self.held.is_some())
            .finish()
    }
}

/// The short, non-secret handle on an ask the agent is shown.
pub fn reference(id: &str) -> String {
    id.chars().take(6).collect()
}

fn device_name(d: &DeviceInfo) -> String {
    if d.hw_serial.is_empty() {
        d.serial.clone()
    } else {
        d.hw_serial.clone()
    }
}

fn clean(s: &str, n: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(n)
        .collect::<String>()
}

/// Singapore's offset from UTC (no daylight saving).
const SGT_OFFSET_MIN: u64 = 8 * 60;

/// `HH:MM SGT` for Unix ms (the push says when the lease ends, in the owner's own time zone).
pub fn sgt_hhmm(ms: u64) -> String {
    let m = (ms / 60_000 + SGT_OFFSET_MIN) % (24 * 60);
    format!("{:02}:{:02} SGT", m / 60, m % 60)
}

/// A request read back from the holds file: only its id and times are kept.
fn bare_request(id: &str, created_ms: u64, expires_ms: u64, style: Style) -> Request {
    Request {
        id: id.to_string(),
        device: String::new(),
        tool: String::new(),
        kind: "lease".into(),
        reason: String::new(),
        target: String::new(),
        created_ms,
        expires_ms,
        style,
        quiet: false,
    }
}

fn holder_json(h: &Holder) -> serde_json::Value {
    serde_json::json!({"name": h.name, "epoch": h.epoch})
}

/// What a holds file says.
#[derive(Debug, Default)]
struct Holds {
    held: Option<Held>,
    /// The Deny events seen.
    seen: Vec<String>,
    notices: Vec<Kept>,
    pending: Option<Pending>,
    last_ask_ms: Option<u64>,
}

/// The holds file's text for `h`.
fn holds_json(h: &Holds) -> String {
    let hold = h.held.as_ref().map(|h| {
        let (until, until_ms, state) = match &h.until {
            Until::Window(t) => ("window", Some(*t), None),
            Until::Awake => ("awake", None, None),
            Until::Unknown(s) => ("presence_change", None, s.clone()),
        };
        serde_json::json!({
            "request_id": h.req.id,
            "created_ms": h.req.created_ms,
            "expires_ms": h.req.expires_ms,
            "holder": h.holder.as_ref().map(holder_json),
            "denied_at_ms": h.at_ms,
            "until": until,
            "until_ms": until_ms,
            "presence_state": state,
            "reallow": h.reallow,
        })
    });
    let notices: Vec<serde_json::Value> = h
        .notices
        .iter()
        .map(|k| {
            serde_json::json!({
                "request_id": k.req.id,
                "created_ms": k.req.created_ms,
                "overnight": k.overnight,
            })
        })
        .collect();
    let pending = h.pending.as_ref().map(|p| {
        serde_json::json!({
            "request_id": p.req.id,
            "created_ms": p.req.created_ms,
            "expires_ms": p.req.expires_ms,
            "holder": holder_json(&p.holder),
        })
    });
    serde_json::json!({
        "version": HOLDS_VERSION,
        "hold": hold,
        "seen_denies": h.seen,
        "notices": notices,
        "pending": pending,
        "last_ask_ms": h.last_ask_ms,
    })
    .to_string()
}

fn parse_holder(x: &serde_json::Value) -> Option<Holder> {
    Some(Holder {
        name: x.get("name")?.as_str()?.to_string(),
        epoch: x.get("epoch")?.as_u64()?,
    })
}

/// Read a holds file's text. `Err`: it could not be read as one (the caller holds until the
/// presence state changes).
fn parse_holds(text: &str) -> Result<Holds, String> {
    let v: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|_| "the holds file is not JSON".to_string())?;
    let version = match v.get("version").and_then(serde_json::Value::as_u64) {
        Some(n @ 1..=HOLDS_VERSION) => n,
        _ => return Err("the holds file has another version".into()),
    };
    let mut out = Holds::default();
    out.seen = match v.get("seen_denies") {
        None if version == 1 => vec![],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| "the holds file's seen Deny events are malformed".to_string())?,
        _ => return Err("the holds file's seen Deny events are malformed".into()),
    };
    if version >= 3 {
        let bad = || "the holds file's pushes or pending ask are malformed".to_string();
        let Some(serde_json::Value::Array(ns)) = v.get("notices") else {
            return Err(bad());
        };
        for n in ns {
            let id = n
                .get("request_id")
                .and_then(|x| x.as_str())
                .ok_or_else(bad)?;
            let created = n
                .get("created_ms")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(bad)?;
            let overnight = n
                .get("overnight")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(bad)?;
            out.notices.push(Kept {
                req: bare_request(id, created, created, Style::Notice),
                overnight,
            });
        }
        out.pending = match v.get("pending") {
            Some(serde_json::Value::Null) => None,
            Some(p) => {
                let id = p
                    .get("request_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(bad)?;
                let num = |k: &str| p.get(k).and_then(serde_json::Value::as_u64).ok_or_else(bad);
                let holder = p.get("holder").and_then(parse_holder).ok_or_else(bad)?;
                Some(Pending {
                    req: bare_request(id, num("created_ms")?, num("expires_ms")?, Style::Lease),
                    holder,
                    restored: true,
                })
            }
            None => return Err(bad()),
        };
        out.last_ask_ms = match v.get("last_ask_ms") {
            Some(serde_json::Value::Null) => None,
            Some(x) => Some(x.as_u64().ok_or_else(bad)?),
            None => return Err(bad()),
        };
    }
    let h = match v.get("hold") {
        Some(serde_json::Value::Null) => return Ok(out),
        Some(h) => h,
        None => return Err("the holds file has no hold".into()),
    };
    let bad = || "the holds file's hold is malformed".to_string();
    let id = h
        .get("request_id")
        .and_then(|x| x.as_str())
        .ok_or_else(bad)?;
    let num = |k: &str| h.get(k).and_then(serde_json::Value::as_u64).ok_or_else(bad);
    let (created, expires, at) = (num("created_ms")?, num("expires_ms")?, num("denied_at_ms")?);
    let holder = match h.get("holder") {
        Some(serde_json::Value::Null) | None => None,
        Some(x) => Some(parse_holder(x).ok_or_else(bad)?),
    };
    let until = match h.get("until").and_then(|x| x.as_str()) {
        Some("window") => Until::Window(num("until_ms")?),
        Some("awake") => Until::Awake,
        Some("presence_change") => Until::Unknown(
            h.get("presence_state")
                .and_then(|x| x.as_str())
                .map(str::to_string),
        ),
        _ => return Err(bad()),
    };
    // version 1 kept no `reallow`: its hold is not lifted by an Allow
    let reallow = match h.get("reallow") {
        Some(serde_json::Value::Bool(b)) => *b,
        None if version == 1 => false,
        _ => return Err(bad()),
    };
    out.held = Some(Held {
        req: bare_request(id, created, expires, Style::Lease),
        holder,
        at_ms: at,
        until,
        reallow,
    });
    Ok(out)
}

/// Write `text` to `path` atomically: a temporary file beside it, then a rename over it.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

impl Protected {
    /// A new lease state. With a holds file configured, the hold it records is loaded; a file
    /// that cannot be read as one holds until a trusted presence state change.
    pub fn new(cfg: ProtectedConfig, book: Arc<LeaseBook>) -> Self {
        let mut p = Protected {
            cfg,
            book,
            holder_of: None,
            jobs: HashMap::new(),
            pending: None,
            held: None,
            running: None,
            last_ask_ms: None,
            seen_denies: vec![],
            seen_fed: false,
            notices: vec![],
            notices_polled_ms: None,
            quiet_ask: false,
        };
        let h = p.load_holds();
        p.held = h.held;
        p.seen_denies = h.seen;
        p.notices = h.notices;
        p.pending = h.pending;
        p.last_ask_ms = h.last_ask_ms;
        p
    }

    fn load_holds(&self) -> Holds {
        let Some(path) = self.cfg.holds_file.as_ref() else {
            return Holds::default();
        };
        let unknown = |why: String| {
            let now = self.book.now_ms().unwrap_or(0);
            let state = match self.presence(now) {
                Presence::Untrusted(_) => None,
                p => Some(p.to_string()),
            };
            Holds {
                held: Some(Held {
                    req: bare_request(&format!("unknown hold ({why})"), 0, 0, Style::Lease),
                    holder: None,
                    at_ms: now,
                    until: Until::Unknown(state),
                    reallow: false,
                }),
                ..Holds::default()
            }
        };
        match std::fs::read_to_string(path) {
            Ok(text) => parse_holds(&text).unwrap_or_else(unknown),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Holds::default(),
            Err(e) => unknown(format!("the holds file could not be read: {}", e.kind())),
        }
    }

    /// Write the hold, the Deny events seen, the kept pushes, the pending ask and when the last
    /// ask went out to the holds file, when one is configured. `Err`: it could not be written
    /// (the state stands in memory either way).
    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.cfg.holds_file else {
            return Ok(());
        };
        let h = Holds {
            held: self.held.clone(),
            seen: self.seen_denies.clone(),
            notices: self.notices.clone(),
            pending: self.pending.clone(),
            last_ask_ms: self.last_ask_ms,
        };
        write_atomic(path, &holds_json(&h))
            .map_err(|e| format!("the holds file could not be written: {}", e.kind()))
    }

    /// Set (or clear) the hold, and write it to the holds file ([`Self::save`]).
    fn set_held(&mut self, h: Option<Held>) -> Result<(), String> {
        self.held = h;
        self.save()
    }

    /// Keep a Deny event as applied (written with the next [`Self::save`]).
    fn note_deny(&mut self, event_id: &str) {
        if event_id.is_empty() || self.seen_denies.iter().any(|e| e == event_id) {
            return;
        }
        self.seen_denies.push(event_id.to_string());
        if self.seen_denies.len() > SEEN_DENIES_KEPT {
            let extra = self.seen_denies.len() - SEEN_DENIES_KEPT;
            self.seen_denies.drain(..extra);
        }
    }

    /// Drop the kept pushes made more than [`NOTICES_KEPT_MS`] before `now`.
    fn prune_notices(&mut self, now: u64) {
        self.notices
            .retain(|k| k.req.created_ms.saturating_add(NOTICES_KEPT_MS) > now);
    }

    /// Keep `req`'s Deny looked for (an overnight notice, or an Allowed ask); written with the
    /// next [`Self::save`].
    fn keep_notice(&mut self, req: &Request, overnight: bool, now: u64) {
        self.prune_notices(now);
        match self.notices.iter_mut().find(|k| k.req.id == req.id) {
            Some(k) => k.overnight |= overnight,
            None => {
                self.notices.push(Kept {
                    req: bare_request(&req.id, req.created_ms, req.expires_ms, Style::Notice),
                    overnight,
                });
                if self.notices.len() > NOTICES_KEPT {
                    let extra = self.notices.len() - NOTICES_KEPT;
                    self.notices.drain(..extra);
                }
            }
        }
    }

    /// Read the channel once for a Deny on any kept push or on the open lease's own ask.
    /// `Ok(Some((push, event id)))` on one.
    fn poll_kept(&mut self, a: &mut Approver, now: u64) -> Result<Option<(Kept, String)>, String> {
        self.prune_notices(now);
        let mut kept = self.notices.clone();
        if let Some(run) = &self.running {
            if !kept.iter().any(|k| k.req.id == run.req.id) {
                kept.push(Kept {
                    req: run.req.clone(),
                    overnight: false,
                });
            }
        }
        let reqs: Vec<Request> = kept.iter().map(|k| k.req.clone()).collect();
        let hit = a.poll_denies(&reqs)?;
        self.notices_polled_ms = Some(now);
        Ok(hit.map(|(i, ev)| (kept[i].clone(), ev)))
    }

    /// Close the open lease through the book's close queue, so the close is written to the audit
    /// log like every other one and never dropped (review round 4, item 6). `Err`: it could not
    /// be written (it stays queued, and the protected phone is refused until it is).
    fn close_lease(
        &mut self,
        why: &'static str,
        audit: Option<&mut AuditLog>,
        tool: &'static str,
    ) -> Result<(), Refusal> {
        self.book.close_for(why);
        self.running = None;
        self.audit_closes(audit, tool)
    }

    /// The owner tapped Deny (`event_id`) on kept push `k`: the open lease `lease`, if any, is
    /// closed through the queue, and the phone is held for every holder. The hold lasts until a
    /// trusted presence file says the owner is awake when the push was an overnight notice, the
    /// lease was an overnight one, or the presence file says asleep; else for the ask window. A
    /// stronger hold already standing is kept. Only a Deny on an open lease may be re-allowed, by
    /// its holder. The first `Err` is the audit log's; the inner one says the holds file could not
    /// be written.
    fn apply_deny(
        &mut self,
        k: &Kept,
        event_id: &str,
        lease: Option<&Lease>,
        now: u64,
        audit: Option<&mut AuditLog>,
        tool: &'static str,
    ) -> Result<Result<(), String>, Refusal> {
        self.note_deny(event_id);
        let closed = match lease {
            Some(_) => self.close_lease("denied", audit, tool),
            None => Ok(()),
        };
        let asleep = k.overnight
            || lease.is_some_and(|l| l.granted == Granted::Overnight)
            || self.presence(now).asleep();
        let until = if asleep {
            Until::Awake
        } else {
            Until::Window(now.saturating_add(self.cfg.ask_window_ms))
        };
        let saved = match (&self.held, &until) {
            // an overnight hold (or one from an unreadable holds file) is never cut short
            (Some(h), Until::Window(_)) if !matches!(h.until, Until::Window(_)) => self.save(),
            (Some(h), Until::Window(t)) if matches!(h.until, Until::Window(old) if old > *t) => {
                self.save()
            }
            _ => self.set_held(Some(Held {
                req: k.req.clone(),
                holder: lease.map(|l| l.holder.clone()),
                at_ms: now,
                until,
                reallow: lease.is_some(),
            })),
        };
        closed.map(|()| saved)
    }

    /// How core learns who holds the protected phone (the daemon's registry, for the caller's own
    /// connection). Without it no lease opens.
    pub fn set_holders(&mut self, f: HolderFn) {
        self.holder_of = Some(f);
    }

    /// The job now running under daemon lease `epoch` (`None`: it ended), named on the next ask
    /// and on the open lease.
    pub fn set_job(&mut self, epoch: u64, job: Option<String>) {
        match &job {
            Some(j) => {
                self.jobs.insert(epoch, clean(j, 64));
            }
            None => {
                self.jobs.remove(&epoch);
            }
        }
        self.book.set_job(epoch, job.map(|j| clean(&j, 64)));
    }

    pub fn book(&self) -> &Arc<LeaseBook> {
        &self.book
    }

    /// Close the lease of daemon lease `epoch` (its holder released the phone). The close is
    /// written to the audit log on the next call ([`Protected::audit_closes`]).
    pub fn close_epoch(&mut self, epoch: u64) -> Option<Lease> {
        let closed = self.book.close_epoch(epoch, "released");
        if closed.is_some() {
            self.running = None;
        }
        self.jobs.remove(&epoch);
        closed
    }

    /// Write every lease the book closed without core (it ran out, its holder released the phone
    /// or went) to the audit log, oldest first. `Err`: a write failed; that close and every later
    /// one go back in the queue (review round 3, item 10), and the caller refuses until they are
    /// written.
    pub fn audit_closes(
        &mut self,
        audit: Option<&mut AuditLog>,
        tool: &'static str,
    ) -> Result<(), Refusal> {
        // (reading the book first drops, and queues, a lease that has just run out)
        let open = self.book.active().is_some();
        let ended = self.book.take_ended();
        if ended.is_empty() {
            return Ok(());
        }
        // a run-out or released lease's Deny poll ends with it
        if !open {
            self.running = None;
        }
        let Some(a) = audit else {
            return Ok(());
        };
        let mut rest = ended.into_iter();
        while let Some((l, why)) = rest.next() {
            let written = a.append(&Entry {
                device: String::new(),
                tool: tool.to_string(),
                decision: "blocked".into(),
                rule: "lease:closed".into(),
                reason: format!(
                    "lease {} for {} (epoch {}) closed: {why}",
                    reference(&l.id),
                    clean(&l.holder.name, 40),
                    l.holder.epoch
                ),
                target: String::new(),
                screen: String::new(),
            });
            if let Err(e) = written {
                let mut unwritten = vec![(l, why)];
                unwritten.extend(rest);
                self.book.requeue_ended(unwritten);
                return Err(Refusal::Audit {
                    tool,
                    why: format!(
                        "a lease close could not be written to the audit log ({e}); it is kept, and the protected phone is refused until it is written"
                    ),
                });
            }
        }
        Ok(())
    }

    /// What the presence file says now (no file configured: untrusted, so the overnight rule is off).
    pub fn presence(&self, now_ms: u64) -> Presence {
        match &self.cfg.presence {
            Some(p) => read_presence(p, now_ms),
            None => Presence::Untrusted("no presence file is configured".into()),
        }
    }

    fn open(&mut self, now: u64, holder: &Holder, granted: Granted, req: Request) -> Lease {
        let lease = self.book.open(Lease {
            id: req.id.clone(),
            holder: holder.clone(),
            job: self.jobs.get(&holder.epoch).cloned(),
            opened_at_ms: now,
            expires_at_ms: now.saturating_add(self.cfg.lease_ms),
            granted,
            bound: vec![],
        });
        self.running = Some(Running {
            req,
            last_poll_ms: now,
            fail_since: None,
        });
        lease
    }

    /// After an Allow: one quiet notice, with the same request id, saying until when the lease is
    /// open, with a Deny button (the Allow button cleared the ask and its Deny). `Err`: not sent.
    fn send_open_notice(
        &self,
        a: &mut Approver,
        ask: &Request,
        now: u64,
        tool: &str,
    ) -> Result<(), String> {
        let mut n = ask.clone();
        n.style = Style::Notice;
        n.tool = tool.to_string();
        n.created_ms = now;
        n.expires_ms = now.saturating_add(self.cfg.lease_ms);
        n.reason = format!(
            "Lease on the protected phone open until {} ({} min).",
            sgt_hhmm(n.expires_ms),
            self.cfg.lease_ms / 60_000
        );
        a.send(&n)
    }

    /// Admit a call for the protected phone `d` under a lease: the open one when it is this
    /// holder's, or a new one (overnight), or an ask (refused as pending). Binds `d.serial` for the
    /// adb guard on success. Every lease is written to the audit log before it opens; a failed
    /// write opens nothing.
    pub fn admit(
        &mut self,
        d: &DeviceInfo,
        tool: &'static str,
        approver: Option<&mut Approver>,
        audit: Option<&mut AuditLog>,
    ) -> Result<Lease, Refusal> {
        let mut approver = approver;
        let mut audit = audit;
        let device = device_name(d);
        let refused = |why: String| Refusal::LeaseRefused { tool, why };
        self.audit_closes(audit.as_deref_mut(), tool)?;
        if !self.seen_fed {
            if let Some(a) = approver.as_deref_mut() {
                a.mark_seen(&self.seen_denies);
                self.seen_fed = true;
            }
        }
        let Some(now) = self.book.now_ms() else {
            return Err(refused("the wall clock cannot be read".into()));
        };
        let Some(holder) = self.holder_of.as_ref().and_then(|f| f(d)) else {
            return Err(refused(
                "no daemon lease on the protected phone is held by this caller: take the device lease first".into(),
            ));
        };
        let log = |audit: Option<&mut AuditLog>,
                   decision: &str,
                   rule: &str,
                   reason: String|
         -> Result<(), Refusal> {
            match audit {
                Some(a) => a
                    .append(&Entry {
                        device: device.clone(),
                        tool: tool.to_string(),
                        decision: decision.into(),
                        rule: rule.into(),
                        reason,
                        target: String::new(),
                        screen: String::new(),
                    })
                    .map(|_| ())
                    .map_err(|e| Refusal::Audit {
                        tool,
                        why: e.to_string(),
                    }),
                None => Ok(()),
            }
        };

        // 1. the open lease
        if let Some(lease) = self.book.active() {
            if lease.holder != holder {
                self.close_lease("another holder called", audit.as_deref_mut(), tool)?;
            } else {
                // a Deny (on this lease's push or on any kept one), or a Deny channel down too
                // long, closes it; a clock that went back polls at once (review round 4, item 7)
                let due = self.running.as_ref().is_some_and(|run| {
                    now < run.last_poll_ms
                        || now.saturating_sub(run.last_poll_ms) >= self.cfg.deny_poll_ms
                });
                if due {
                    if let Some(run) = self.running.as_mut() {
                        run.last_poll_ms = now;
                    }
                    let polled = match approver.as_deref_mut() {
                        Some(a) => self.poll_kept(a, now),
                        None => Err("no approval channel".into()),
                    };
                    match polled {
                        Ok(None) => {
                            if let Some(run) = self.running.as_mut() {
                                run.fail_since = None;
                            }
                        }
                        Ok(Some((k, event_id))) => {
                            let saved = self.apply_deny(
                                &k,
                                &event_id,
                                Some(&lease),
                                now,
                                audit.as_deref_mut(),
                                tool,
                            )?;
                            log(
                                audit.as_deref_mut(),
                                "blocked",
                                "lease:denied",
                                format!(
                                    "the owner tapped Deny on push {}: lease {} closed and held{}",
                                    reference(&k.req.id),
                                    reference(&lease.id),
                                    saved.err().map_or(String::new(), |e| format!(" ({e})"))
                                ),
                            )?;
                            return Err(refused(
                                "the owner tapped Deny, so the lease was closed".into(),
                            ));
                        }
                        Err(e) => {
                            let since = self
                                .running
                                .as_mut()
                                .map_or(now, |run| *run.fail_since.get_or_insert(now));
                            if now.saturating_sub(since) >= self.cfg.deny_poll_fail_ms {
                                self.close_lease(
                                    "deny channel unreadable",
                                    audit.as_deref_mut(),
                                    tool,
                                )?;
                                log(
                                    audit.as_deref_mut(),
                                    "blocked",
                                    "lease:deny_unreadable",
                                    format!(
                                        "the Deny channel could not be read for {} s ({e}): lease {} closed",
                                        self.cfg.deny_poll_fail_ms / 1000,
                                        reference(&lease.id)
                                    ),
                                )?;
                                return Err(refused(format!(
                                    "the owner's Deny could not be checked for {} s, so the lease was closed",
                                    self.cfg.deny_poll_fail_ms / 1000
                                )));
                            }
                            log(
                                audit.as_deref_mut(),
                                "blocked",
                                "lease:deny_poll",
                                format!(
                                    "the Deny channel could not be read ({e}); the lease closes if this lasts {} s",
                                    self.cfg.deny_poll_fail_ms / 1000
                                ),
                            )?;
                        }
                    }
                }
                if lease.granted != Granted::Overnight {
                    self.book.bind(&d.serial);
                    return Ok(lease);
                }
                // an overnight lease lasts only while a trusted presence file says asleep
                let p = self.presence(now);
                if p.asleep() {
                    self.book.bind(&d.serial);
                    return Ok(lease);
                }
                // it was asleep at the last trusted reading: an unreadable presence file is a
                // hiccup at night, so the ask that follows goes quietly (review round 4, item 5)
                self.quiet_ask = matches!(p, Presence::Untrusted(_));
                self.close_lease("presence ended", audit.as_deref_mut(), tool)?;
                log(
                    audit.as_deref_mut(),
                    "blocked",
                    "lease:overnight_ended",
                    format!(
                        "the overnight lease {} ended: the presence file says {p}",
                        reference(&lease.id)
                    ),
                )?;
            }
        }

        // 1b. no lease open: a Deny on a kept push (an older overnight notice, an earlier lease's
        // notice) still holds the phone, one posted while the daemon was down included: the
        // first call after start-up looks at once (review round 4, item 1)
        if let Some(a) = approver.as_deref_mut() {
            let due = self
                .notices_polled_ms
                .is_none_or(|t| now < t || now.saturating_sub(t) >= self.cfg.deny_poll_ms);
            if due && !self.notices.is_empty() {
                if let Ok(Some((k, event_id))) = self.poll_kept(a, now) {
                    let saved =
                        self.apply_deny(&k, &event_id, None, now, audit.as_deref_mut(), tool)?;
                    log(
                        audit.as_deref_mut(),
                        "blocked",
                        "lease:denied",
                        format!(
                            "the owner tapped Deny on push {} of an earlier lease: held{}",
                            reference(&k.req.id),
                            saved.err().map_or(String::new(), |e| format!(" ({e})"))
                        ),
                    )?;
                    return Err(refused(
                        "the owner tapped Deny on an earlier lease's push, so the phone is held"
                            .into(),
                    ));
                }
            }
        }

        // 2. a hold after a Deny (or no answer), for every holder: a new holder inherits it
        if let Some(h) = self.held.clone() {
            // only the denied holder, within the ask window after the Deny, may re-allow; an ask
            // nobody answered is never re-allowed
            let own = h.reallow && h.holder.as_ref() == Some(&holder);
            let until = h.at_ms.saturating_add(self.cfg.ask_window_ms);
            if let Some(ev) = approver
                .as_deref_mut()
                .filter(|_| own)
                .and_then(|a| a.reallowed(&h.req, h.at_ms, until))
            {
                self.keep_notice(&h.req, false, now);
                let saved = self.set_held(None);
                if let Err(e) = log(
                    audit.as_deref_mut(),
                    "allowed",
                    "lease:reallowed",
                    format!(
                        "the owner re-allowed ask {} (answer {ev}): lease open for {} min for {} (epoch {}){}",
                        reference(&h.req.id),
                        self.cfg.lease_ms / 60_000,
                        holder.name,
                        holder.epoch,
                        saved.err().map_or(String::new(), |e| format!(" ({e})"))
                    ),
                ) {
                    // nothing opens unlogged, and the hold stands
                    let _ = self.set_held(Some(h));
                    return Err(e);
                }
                let notice = match approver.as_deref_mut() {
                    Some(a) => self.send_open_notice(a, &h.req, now, tool),
                    None => Err("no approval channel".into()),
                };
                let lease = self.open(
                    now,
                    &holder,
                    Granted::Approved {
                        request_id: h.req.id.clone(),
                    },
                    h.req.clone(),
                );
                if let Err(e) = notice {
                    log(
                        audit.as_deref_mut(),
                        "blocked",
                        "lease:notice_unsent",
                        format!("the lease-open notice could not be sent ({e}); the owner can still reply deny"),
                    )?;
                }
                self.book.bind(&d.serial);
                return Ok(lease);
            }
            let presence = self.presence(now);
            let (over, until_text) = match &h.until {
                Until::Window(t) => (
                    now >= *t,
                    format!("for {} min", self.cfg.ask_window_ms / 60_000),
                ),
                Until::Awake => (
                    matches!(presence, Presence::Other(_)),
                    "until the presence file says the owner is awake".to_string(),
                ),
                Until::Unknown(seen) => {
                    let state = match &presence {
                        Presence::Untrusted(_) => None,
                        p => Some(p.to_string()),
                    };
                    match (seen, state) {
                        (Some(a), Some(b)) => (*a != b, String::new()),
                        (None, Some(b)) => {
                            // the first trusted state seen: the hold lasts until it changes
                            let mut h2 = h.clone();
                            h2.until = Until::Unknown(Some(b));
                            let _ = self.set_held(Some(h2));
                            (false, String::new())
                        }
                        (_, None) => (false, String::new()),
                    }
                }
            };
            if !over {
                if matches!(h.until, Until::Unknown(_)) {
                    return Err(refused(
                        "held: the holds file could not be read at start-up, so an earlier Deny may stand; held until a trusted presence file's state changes".into(),
                    ));
                }
                return Err(refused(format!(
                    "held after the owner's Deny on ask {} ({until_text}); the owner can lift it by answering allow to that ask within {} min of the Deny",
                    reference(&h.req.id),
                    self.cfg.ask_window_ms / 60_000
                )));
            }
            let saved = self.set_held(None);
            log(
                audit.as_deref_mut(),
                "allowed",
                "lease:hold_ended",
                format!(
                    "the hold after ask {} ended (the presence file says {presence}){}",
                    reference(&h.req.id),
                    saved.err().map_or(String::new(), |e| format!(" ({e})"))
                ),
            )?;
        }

        // 3. the one pending ask on this phone, whoever it is for
        if let Some(p) = self.pending.take() {
            let Some(a) = approver.as_deref_mut() else {
                self.pending = Some(p);
                return Err(refused("no approval channel is configured".into()));
            };
            // an ask read back after a restart is nobody's (review round 4, item 4)
            let mine = !p.restored && p.holder == holder;
            match a.check(&p.req, now) {
                None if mine => {
                    let r = reference(&p.req.id);
                    self.pending = Some(p);
                    return Err(Refusal::LeasePending { tool, reference: r });
                }
                None => {
                    let r = reference(&p.req.id);
                    let who = clean(&p.holder.name, 40);
                    let epoch = p.holder.epoch;
                    self.pending = Some(p);
                    return Err(refused(format!(
                        "ask {r} for {who} (epoch {epoch}) is still open; one ask at a time goes to the owner, so try again once it has ended"
                    )));
                }
                Some(Answer::Approved { event_id }) if mine => {
                    // its lease-open notice has the ask's id: a Deny on it counts after the lease
                    self.keep_notice(&p.req, false, now);
                    let saved = self.save();
                    log(
                        audit.as_deref_mut(),
                        "allowed",
                        "lease:opened",
                        format!(
                            "the owner allowed ask {} (answer {event_id}): lease open for {} min for {} (epoch {}){}",
                            reference(&p.req.id),
                            self.cfg.lease_ms / 60_000,
                            holder.name,
                            holder.epoch,
                            saved.err().map_or(String::new(), |e| format!(" ({e})"))
                        ),
                    )?;
                    let notice = self.send_open_notice(a, &p.req, now, tool);
                    let lease = self.open(
                        now,
                        &holder,
                        Granted::Approved {
                            request_id: p.req.id.clone(),
                        },
                        p.req.clone(),
                    );
                    if let Err(e) = notice {
                        log(
                            audit.as_deref_mut(),
                            "blocked",
                            "lease:notice_unsent",
                            format!("the lease-open notice could not be sent ({e}); the owner can still reply deny"),
                        )?;
                    }
                    self.book.bind(&d.serial);
                    return Ok(lease);
                }
                Some(Answer::Approved { .. }) => {
                    let saved = self.save();
                    log(
                        audit.as_deref_mut(),
                        "blocked",
                        "lease:stale",
                        format!(
                            "ask {} was allowed for {} (epoch {}{}), which no longer holds the phone: nothing opened for {} (epoch {}){}",
                            reference(&p.req.id),
                            p.holder.name,
                            p.holder.epoch,
                            if p.restored { ", before a restart" } else { "" },
                            holder.name,
                            holder.epoch,
                            saved.err().map_or(String::new(), |e| format!(" ({e})"))
                        ),
                    )?;
                }
                Some(Answer::Denied { event_id }) => {
                    self.note_deny(&event_id);
                    let asleep = self.presence(now).asleep();
                    let saved = self.set_held(Some(Held {
                        req: p.req.clone(),
                        holder: Some(p.holder.clone()),
                        at_ms: now,
                        until: if asleep {
                            Until::Awake
                        } else {
                            Until::Window(now.saturating_add(self.cfg.ask_window_ms))
                        },
                        reallow: true,
                    }));
                    log(
                        audit.as_deref_mut(),
                        "blocked",
                        "lease:denied",
                        format!(
                            "the owner denied ask {}{}",
                            reference(&p.req.id),
                            saved.err().map_or(String::new(), |e| format!(" ({e})"))
                        ),
                    )?;
                    return Err(refused("the owner tapped Deny".into()));
                }
                Some(other) => {
                    let saved = self.set_held(Some(Held {
                        req: p.req.clone(),
                        holder: Some(p.holder.clone()),
                        at_ms: now,
                        until: Until::Window(now.saturating_add(self.cfg.ask_window_ms)),
                        reallow: false,
                    }));
                    log(
                        audit.as_deref_mut(),
                        "blocked",
                        "lease:expired",
                        format!(
                            "ask {}: {other}{}",
                            reference(&p.req.id),
                            saved.err().map_or(String::new(), |e| format!(" ({e})"))
                        ),
                    )?;
                    return Err(refused(format!("the ask ended unanswered ({other})")));
                }
            }
        }

        // 4. overnight, or 5. a new ask
        let Some(a) = approver else {
            return Err(refused("no approval channel is configured".into()));
        };
        let presence = self.presence(now);
        if !matches!(presence, Presence::Untrusted(_)) {
            self.quiet_ask = false;
        }
        let minutes = self.cfg.lease_ms / 60_000;
        let who = push_holder(&holder.name);
        let job = self
            .jobs
            .get(&holder.epoch)
            .map_or(String::new(), |j| format!(", job {j}"));
        if presence.asleep() {
            // the Denies on the earlier pushes are looked for first, every time: one found holds,
            // and an unreadable channel opens nothing (review round 4, item 1)
            match self.poll_kept(a, now) {
                Ok(None) => {}
                Ok(Some((k, event_id))) => {
                    let saved =
                        self.apply_deny(&k, &event_id, None, now, audit.as_deref_mut(), tool)?;
                    log(
                        audit.as_deref_mut(),
                        "blocked",
                        "lease:denied",
                        format!(
                            "the owner tapped Deny on push {} of an earlier lease: held{}",
                            reference(&k.req.id),
                            saved.err().map_or(String::new(), |e| format!(" ({e})"))
                        ),
                    )?;
                    return Err(refused(
                        "the owner tapped Deny on an earlier lease's push, so the phone is held"
                            .into(),
                    ));
                }
                Err(e) => {
                    return Err(refused(format!(
                        "the overnight rule opens a lease only once the Denies on the earlier pushes have been checked, and the channel could not be read ({e})"
                    )));
                }
            }
            let mut req = a.request(
                now,
                &device,
                tool,
                "lease",
                &format!(
                    "Overnight: {who}{job} took over the protected phone for {minutes} min (the presence file says asleep). Tap Deny to end it."
                ),
                "",
            );
            req.style = Style::Notice;
            req.expires_ms = now.saturating_add(self.cfg.lease_ms);
            if let Err(e) = a.send(&req) {
                return Err(refused(format!(
                    "the overnight rule opens a lease only once its notice is sent, and it could not be ({e})"
                )));
            }
            let id = req.id.clone();
            self.keep_notice(&req, true, now);
            let saved = self.save();
            log(
                audit.as_deref_mut(),
                "allowed",
                "lease:overnight",
                format!(
                    "a signed, fresh presence file says asleep: lease {} open for {minutes} min for {} (epoch {}), notice sent{}",
                    reference(&id),
                    holder.name,
                    holder.epoch,
                    saved.err().map_or(String::new(), |e| format!(" ({e})"))
                ),
            )?;
            let lease = self.open(now, &holder, Granted::Overnight, req);
            self.book.bind(&d.serial);
            return Ok(lease);
        }
        // one ask per ask window, whoever it was for
        if let Some(last) = self.last_ask_ms {
            let next = last.saturating_add(self.cfg.ask_window_ms);
            if now < next {
                return Err(refused(format!(
                    "an ask went to the owner {} min ago; the next may go in {} min",
                    now.saturating_sub(last) / 60_000,
                    (next - now).div_ceil(60_000)
                )));
            }
        }
        let mut req = a.request(
            now,
            &device,
            tool,
            "lease",
            &format!(
                "{who}{job} asks to take over the protected phone for {minutes} min (first call: {tool}). Allow covers every call for {minutes} min; Deny refuses."
            ),
            "",
        );
        req.style = Style::Lease;
        req.expires_ms = now.saturating_add(self.cfg.ask_window_ms);
        // at night, after an overnight lease ended on an unreadable presence file, the ask waits
        // quietly in the shade, answerable in the morning (review round 4, item 5)
        req.quiet = self.quiet_ask && matches!(presence, Presence::Untrusted(_));
        if let Err(e) = a.send(&req) {
            return Err(refused(format!("the ask could not be sent ({e})")));
        }
        self.last_ask_ms = Some(now);
        let r = reference(&req.id);
        let quiet = req.quiet;
        self.pending = Some(Pending {
            req,
            holder: holder.clone(),
            restored: false,
        });
        // the ask and the throttle survive a restart (review round 4, item 4)
        let saved = self.save();
        log(
            audit,
            "asked",
            "lease:asked",
            format!(
                "asked the owner for a {minutes}-minute lease for {} (epoch {}), ask {r}, {} min to answer{}; overnight rule: {presence}{}",
                holder.name,
                holder.epoch,
                self.cfg.ask_window_ms / 60_000,
                if quiet { ", sent quietly" } else { "" },
                saved.err().map_or(String::new(), |e| format!(" ({e})"))
            ),
        )?;
        Err(Refusal::LeasePending { tool, reference: r })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231_case_2_and_a_long_key() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 4231 case 6: a 131-byte key is hashed first
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    const KEY: &[u8] = b"0123456789abcdef-presence-key";
    const NOW: u64 = 1_700_000_000_000;
    const TTL: u64 = 15 * 60_000;

    #[test]
    fn only_a_signed_fresh_file_dated_now_says_asleep() {
        let ok = sign_presence(KEY, "asleep", NOW - 60_000);
        assert_eq!(judge_presence(&ok, KEY, TTL, NOW), Presence::Asleep);
        let awake = sign_presence(KEY, "present", NOW);
        assert_eq!(
            judge_presence(&awake, KEY, TTL, NOW),
            Presence::Other("present".into())
        );
        // another key, an edited state, stale, dated ahead, junk
        let forged = sign_presence(b"another-key-of-16-bytes", "asleep", NOW);
        assert!(!judge_presence(&forged, KEY, TTL, NOW).asleep());
        let edited = sign_presence(KEY, "present", NOW).replace("present", "asleep");
        assert!(matches!(
            judge_presence(&edited, KEY, TTL, NOW),
            Presence::Untrusted(_)
        ));
        let stale = sign_presence(KEY, "asleep", NOW - TTL - 1);
        assert!(matches!(
            judge_presence(&stale, KEY, TTL, NOW),
            Presence::Untrusted(w) if w.contains("stale")
        ));
        let ahead = sign_presence(KEY, "asleep", NOW + PRESENCE_MAX_FUTURE_MS + 1);
        assert!(matches!(
            judge_presence(&ahead, KEY, TTL, NOW),
            Presence::Untrusted(w) if w.contains("ahead")
        ));
        let near = sign_presence(KEY, "asleep", NOW + PRESENCE_MAX_FUTURE_MS);
        assert!(judge_presence(&near, KEY, TTL, NOW).asleep());
        assert!(!judge_presence("{\"state\":\"asleep\"}", KEY, TTL, NOW).asleep());
        assert!(!judge_presence("asleep", KEY, TTL, NOW).asleep());
    }

    #[test]
    fn no_key_a_short_key_or_no_file_turns_the_overnight_rule_off() {
        let d = std::env::temp_dir().join(format!("agentroid-presence-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (file, key) = (d.join("presence.json"), d.join("presence.key"));
        let cfg = PresenceConfig::new(&file, &key, 15);
        assert!(!read_presence(&cfg, NOW).asleep(), "no key file");
        std::fs::write(&key, "short").unwrap();
        std::fs::write(&file, sign_presence(b"short", "asleep", NOW)).unwrap();
        assert!(
            matches!(read_presence(&cfg, NOW), Presence::Untrusted(w) if w.contains("shorter"))
        );
        std::fs::write(&key, format!("{}\n", std::str::from_utf8(KEY).unwrap())).unwrap();
        std::fs::write(&file, sign_presence(KEY, "asleep", NOW)).unwrap();
        assert!(read_presence(&cfg, NOW).asleep());
        std::fs::remove_file(&file).unwrap();
        assert!(!read_presence(&cfg, NOW).asleep(), "no presence file");
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(PresenceConfig::new("a", "b", 0).ttl_ms, 60_000);
        assert_eq!(PresenceConfig::new("a", "b", 999).ttl_ms, 60 * 60_000);
    }

    #[test]
    fn the_windows_are_clamped() {
        let c = ProtectedConfig::new(0, 0, None);
        assert_eq!((c.lease_ms, c.ask_window_ms), (60_000, 60_000));
        let c = ProtectedConfig::new(10_000, 10_000, None);
        assert_eq!((c.lease_ms, c.ask_window_ms), (120 * 60_000, 60 * 60_000));
    }

    fn node(pkg: &str, rid: &str, text: &str, desc: &str, bounds: &str) -> String {
        format!(
            "<node index=\"0\" text=\"{text}\" resource-id=\"{rid}\" class=\"android.widget.TextView\" package=\"{pkg}\" content-desc=\"{desc}\" clickable=\"true\" bounds=\"{bounds}\" />"
        )
    }

    fn screen(nodes: &[String]) -> String {
        format!(
            "<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\">{}</hierarchy>",
            nodes.join("")
        )
    }

    fn tap() -> Call {
        Call::Tap {
            target: crate::gate::Target::Index(0),
        }
    }

    fn typed(t: &str) -> Call {
        Call::TypeText {
            text: t.into(),
            index: None,
            submit: false,
        }
    }

    /// The owner's ruling of 14 Nov 2026: nothing is blocked from being seen. The screens the
    /// earlier rounds refused (a protected app, the shade, the keyguard, Recents, notification
    /// history, a node naming a protected app) give no fact and no refusal now.
    #[test]
    fn no_screen_is_refused_for_what_it_shows() {
        for n in [
            node("com.whatsapp", "", "Dinner?", "", "[0,0][10,10]"),
            node("com.google.android.gm", "", "Inbox", "", "[0,0][10,10]"),
            node(
                "com.android.systemui",
                "com.android.systemui:id/expandableNotificationRow",
                "Hi",
                "",
                "[0,0][10,10]",
            ),
            node(
                "com.android.systemui",
                "com.android.systemui:id/keyguard_status_view",
                "",
                "",
                "[0,0][10,10]",
            ),
            node(
                "com.google.android.apps.nexuslauncher",
                "com.google.android.apps.nexuslauncher:id/task_thumbnail",
                "",
                "",
                "[0,0][10,10]",
            ),
            node(
                "com.android.settings",
                "com.android.settings:id/notification_history",
                "",
                "",
                "[0,0][10,10]",
            ),
            node(
                "com.android.systemui",
                "com.android.systemui:id/icon",
                "",
                "WhatsApp notification: Dinner?",
                "[0,0][10,10]",
            ),
        ] {
            let f = screen_facts(&screen(std::slice::from_ref(&n))).unwrap();
            assert_eq!(f, ScreenFacts::default(), "{n}");
            for c in [tap(), typed("hello 5"), Call::Key { name: "5".into() }] {
                assert_eq!(pin_refusal(&f, &c, Some((5, 5))), None, "{n} {c:?}");
            }
        }
        assert!(screen_facts("not xml").is_err());
    }

    #[test]
    fn digits_on_a_pin_prompt_are_refused_one_at_a_time_too() {
        let prompt = node(
            "com.example.bank",
            "com.example.bank:id/pinEntry",
            "",
            "",
            "[0,0][100,50]",
        );
        let two = node(
            "com.example.bank",
            "com.example.bank:id/key2",
            "2",
            "",
            "[0,100][50,150]",
        );
        let f = screen_facts(&screen(&[prompt.clone(), two.clone()])).unwrap();
        assert!(f.pin_prompt);
        assert_eq!(f.digit_bounds, vec![(0, 100, 50, 150)]);
        for call in [
            typed("7"),
            typed("ab3"),
            Call::Key {
                name: "KEYCODE_5".into(),
            },
            Call::Key {
                name: "KEYCODE_NUMPAD_0".into(),
            },
            Call::Key { name: "9".into() },
        ] {
            assert_eq!(
                pin_refusal(&f, &call, None).map(|r| r.0),
                Some("pin"),
                "{call:?}"
            );
        }
        assert_eq!(
            pin_refusal(&f, &tap(), Some((10, 120))).map(|r| r.0),
            Some("pin")
        );
        // the bounds are half-open: the right and bottom edges are outside
        assert_eq!(
            pin_refusal(&f, &tap(), Some((0, 100))).map(|r| r.0),
            Some("pin")
        );
        assert_eq!(
            pin_refusal(&f, &tap(), Some((49, 149))).map(|r| r.0),
            Some("pin")
        );
        assert_eq!(pin_refusal(&f, &tap(), Some((50, 120))), None);
        assert_eq!(pin_refusal(&f, &tap(), Some((10, 150))), None);
        assert_eq!(pin_refusal(&f, &tap(), Some((-1, 120))), None);
        assert_eq!(pin_refusal(&f, &tap(), Some((10, 99))), None);
        assert_eq!(pin_refusal(&f, &tap(), Some((10, 300))), None);
        assert_eq!(pin_refusal(&f, &tap(), None), None);
        assert_eq!(pin_refusal(&f, &typed("abc"), None), None);
        for exit in ["KEYCODE_BACK", "home", "Escape"] {
            let k = Call::Key { name: exit.into() };
            assert_eq!(pin_refusal(&f, &k, None), None, "{exit}");
        }
        // no prompt: digits go, and no digit key is kept
        let f = screen_facts(&screen(std::slice::from_ref(&two))).unwrap();
        assert!(!f.pin_prompt && f.digit_bounds.is_empty());
        assert_eq!(pin_refusal(&f, &typed("1234"), Some((10, 120))), None);
        // a prompt by its text, or by a password field
        for n in [
            node("com.example.bank", "", "Enter PIN", "", "[0,0][10,10]"),
            node("com.example.bank", "", "", "Passcode", "[0,0][10,10]"),
            node("com.example.bank", "x:id/PIN_PAD", "", "", "[0,0][10,10]"),
        ] {
            assert!(
                screen_facts(&screen(std::slice::from_ref(&n)))
                    .unwrap()
                    .pin_prompt,
                "{n}"
            );
        }
        let pw = "<node index=\"0\" text=\"\" resource-id=\"\" class=\"android.widget.EditText\" package=\"a\" content-desc=\"\" password=\"true\" bounds=\"[0,0][10,10]\" />";
        assert!(screen_facts(&screen(&[pw.to_string()])).unwrap().pin_prompt);
        // a two-digit label is not a key
        let f = screen_facts(&screen(&[
            prompt,
            node("a", "", "12", "", "[0,0][10,10]"),
            node("a", "", "", "3", "[20,0][30,10]"),
        ]))
        .unwrap();
        assert_eq!(f.digit_bounds, vec![(20, 0, 30, 10)]);
    }

    /// Review round 4, item 2: only a key that could enter a digit (a digit, or paste) is
    /// checked; `wake`, `enter` and the rest go without a screen read.
    #[test]
    fn only_calls_that_could_enter_a_digit_need_the_pin_check() {
        for c in [
            tap(),
            typed("a"),
            Call::Key { name: "5".into() },
            Call::Key {
                name: "KEYCODE_NUMPAD_5".into(),
            },
            Call::Key {
                name: "paste".into(),
            },
            Call::Key {
                name: "KEYCODE_PASTE".into(),
            },
            Call::LongPress {
                target: crate::gate::Target::Index(0),
                ms: 600,
            },
            Call::Swipe {
                x1: 1.0,
                y1: 2.0,
                x2: 3.0,
                y2: 4.0,
                ms: 300,
                space: None,
                image_width: None,
            },
        ] {
            assert!(needs_pin_check(&c), "{c:?}");
        }
        for c in [
            Call::Key {
                name: "KEYCODE_BACK".into(),
            },
            Call::Key {
                name: "home".into(),
            },
            Call::Key {
                name: "wake".into(),
            },
            Call::Key {
                name: "KEYCODE_WAKEUP".into(),
            },
            Call::Key {
                name: "KEYCODE_ENTER".into(),
            },
            Call::Key {
                name: "KEYCODE_PASTE_X".into(),
            },
            Call::OpenApp {
                package: "com.android.settings".into(),
            },
            Call::Screenshot { max_width: 540 },
        ] {
            assert!(!needs_pin_check(&c), "{c:?}");
        }
        assert!(paste_key(" Paste ") && paste_key("keycode_paste") && !paste_key("pastel"));
        assert!(digit_key("KEYCODE_NUMPAD_7") && digit_key(" 3 ") && !digit_key("12"));
        assert!(exit_key("KEYCODE_HOME") && !exit_key("KEYCODE_ENTER"));
    }

    /// Review round 3, item 13: the push names the holder as an agent session, briefly, with no
    /// link and no line break.
    #[test]
    fn the_push_names_the_holder_briefly_and_with_no_link() {
        assert_eq!(push_holder("lane-7"), "Agent session: lane-7");
        assert_eq!(
            push_holder("tap https://evil.example/x now"),
            "Agent session: tap now"
        );
        assert_eq!(push_holder("see www.evil.example"), "Agent session: see");
        assert_eq!(push_holder("go evil.example/login"), "Agent session: go");
        assert_eq!(push_holder("a\nb\r\tc\u{7}d"), "Agent session: a b c d");
        assert_eq!(push_holder("  a   b  "), "Agent session: a b");
        assert_eq!(
            push_holder(&"x".repeat(40)),
            format!("Agent session: {}", "x".repeat(24))
        );
        assert_eq!(push_holder("v1.2 lane"), "Agent session: v1.2 lane");
        assert_eq!(push_holder(""), "Agent session: (no name)");
        assert_eq!(push_holder("https://x.y"), "Agent session: (no name)");
        assert_eq!(push_holder("HTTP://X.Y ok"), "Agent session: ok");
        assert_eq!(push_holder("WWW.x ok"), "Agent session: ok");
    }

    /// The orchestrator's ruling (14 Nov 2026): the notice gives Singapore time, labelled SGT.
    #[test]
    fn the_notice_time_is_singapore_time() {
        // 1_700_000_000_000 ms is 22:13:20 UTC on 14 Nov 2023, 06:13 SGT on the 15th
        assert_eq!(sgt_hhmm(NOW), "06:13 SGT");
        assert_eq!(sgt_hhmm(0), "08:00 SGT");
        assert_eq!(sgt_hhmm(16 * 3_600_000 - 1), "23:59 SGT");
        assert_eq!(sgt_hhmm(16 * 3_600_000), "00:00 SGT");
        assert_eq!(sgt_hhmm(16 * 3_600_000 + 61_000), "00:01 SGT");
    }

    fn held(reallow: bool) -> Held {
        Held {
            req: bare_request("abcdefghijklmnopqrst", 1, 2, Style::Lease),
            holder: Some(Holder {
                name: "lane".into(),
                epoch: 4,
            }),
            at_ms: 3,
            until: Until::Window(9),
            reallow,
        }
    }

    /// Review round 3, items 8 and 9, and round 4, items 1 and 4: the holds file keeps whether
    /// the hold may be re-allowed, the Deny events seen, the pushes whose Denies are looked for,
    /// the pending ask (read back as nobody's) and when the last ask went out; versions 1 and 2
    /// are read without what they never kept.
    #[test]
    fn the_holds_file_keeps_reallow_the_denies_seen_the_pushes_and_the_ask() {
        let seen = vec!["e1".to_string(), "e2".to_string()];
        let holds = |held: Option<Held>| Holds {
            held,
            seen: seen.clone(),
            notices: vec![
                Kept {
                    req: bare_request("bbbbbbbbbbbbbbbbbbbb", 5, 6, Style::Notice),
                    overnight: true,
                },
                Kept {
                    req: bare_request("cccccccccccccccccccc", 7, 8, Style::Notice),
                    overnight: false,
                },
            ],
            pending: Some(Pending {
                req: bare_request("dddddddddddddddddddd", 10, 11, Style::Lease),
                holder: Holder {
                    name: "lane".into(),
                    epoch: 2,
                },
                restored: false,
            }),
            last_ask_ms: Some(10),
        };
        for r in [true, false] {
            let h = parse_holds(&holds_json(&holds(Some(held(r))))).unwrap();
            let held = h.held.unwrap();
            assert_eq!(
                (held.reallow, held.until, held.at_ms),
                (r, Until::Window(9), 3)
            );
            assert_eq!(h.seen, seen);
        }
        let h = parse_holds(&holds_json(&holds(None))).unwrap();
        assert!(h.held.is_none());
        assert_eq!(h.seen, seen);
        let pushes: Vec<(String, u64, bool)> = h
            .notices
            .iter()
            .map(|k| (k.req.id.clone(), k.req.created_ms, k.overnight))
            .collect();
        assert_eq!(
            pushes,
            vec![
                ("bbbbbbbbbbbbbbbbbbbb".to_string(), 5, true),
                ("cccccccccccccccccccc".to_string(), 7, false)
            ]
        );
        let p = h.pending.unwrap();
        assert_eq!(
            (
                p.req.id.as_str(),
                p.req.created_ms,
                p.req.expires_ms,
                p.holder.epoch
            ),
            ("dddddddddddddddddddd", 10, 11, 2)
        );
        assert!(p.restored, "an ask read back is nobody's");
        assert_eq!(h.last_ask_ms, Some(10));
        let empty = parse_holds(&holds_json(&Holds::default())).unwrap();
        assert!(empty.pending.is_none() && empty.last_ask_ms.is_none() && empty.notices.is_empty());
        // version 1 (round 2): no reallow, no seen list
        let v1 = r#"{"version":1,"hold":{"request_id":"abc","created_ms":1,"expires_ms":2,"holder":null,"denied_at_ms":3,"until":"window","until_ms":9}}"#;
        let h = parse_holds(v1).unwrap();
        assert!(!h.held.unwrap().reallow);
        assert!(h.seen.is_empty());
        assert!(parse_holds(r#"{"version":1,"hold":null}"#)
            .unwrap()
            .held
            .is_none());
        // version 2 (round 3): no pushes, no ask
        let v2 = parse_holds(r#"{"version":2,"hold":null,"seen_denies":["e1"]}"#).unwrap();
        assert!(v2.notices.is_empty() && v2.pending.is_none() && v2.last_ask_ms.is_none());
        // version 2 needs both; version 3 needs its three
        for bad in [
            r#"{"version":2,"hold":null}"#,
            r#"{"version":2,"hold":null,"seen_denies":"e1"}"#,
            r#"{"version":2,"hold":null,"seen_denies":[1]}"#,
            r#"{"version":2,"seen_denies":[]}"#,
            r#"{"version":4,"hold":null,"seen_denies":[]}"#,
            r#"{"version":0,"hold":null,"seen_denies":[]}"#,
            r#"{"version":3,"hold":null,"seen_denies":[]}"#,
            r#"{"version":3,"hold":null,"seen_denies":[],"notices":[],"pending":null}"#,
            r#"{"version":3,"hold":null,"seen_denies":[],"notices":[],"last_ask_ms":null}"#,
            r#"{"version":3,"hold":null,"seen_denies":[],"pending":null,"last_ask_ms":null}"#,
            r#"{"version":3,"hold":null,"seen_denies":[],"notices":[{"request_id":"a","created_ms":1}],"pending":null,"last_ask_ms":null}"#,
            r#"{"version":3,"hold":null,"seen_denies":[],"notices":[],"pending":{"request_id":"a","created_ms":1,"expires_ms":2},"last_ask_ms":null}"#,
            r#"{"version":3,"hold":null,"seen_denies":[],"notices":[],"pending":null,"last_ask_ms":"x"}"#,
            r#"{"version":2,"hold":{"request_id":"abc","created_ms":1,"expires_ms":2,"holder":null,"denied_at_ms":3,"until":"window","until_ms":9},"seen_denies":[]}"#,
            r#"{"version":2,"hold":{"request_id":"abc","created_ms":1,"expires_ms":2,"holder":null,"denied_at_ms":3,"until":"window","until_ms":9,"reallow":"yes"},"seen_denies":[]}"#,
        ] {
            assert!(parse_holds(bad).is_err(), "{bad}");
        }
    }

    /// Review round 4, item 1: at most [`NOTICES_KEPT`] pushes, for at most a day, each once.
    #[test]
    fn the_pushes_kept_are_bounded_by_count_and_age() {
        let book = Arc::new(LeaseBook::new(Arc::new(leasebook::ManualClock::new(NOW))));
        let mut p = Protected::new(ProtectedConfig::new(30, 10, None), book);
        let r = |i: usize, at: u64| bare_request(&format!("{i:020}"), at, at, Style::Notice);
        p.keep_notice(&r(0, NOW), false, NOW);
        p.keep_notice(&r(0, NOW), true, NOW);
        assert_eq!(p.notices.len(), 1, "once");
        assert!(p.notices[0].overnight, "an overnight mark is never lost");
        for i in 1..=NOTICES_KEPT + 3 {
            p.keep_notice(&r(i, NOW + i as u64), false, NOW + i as u64);
        }
        assert_eq!(p.notices.len(), NOTICES_KEPT);
        assert_eq!(p.notices[0].req.id, format!("{:020}", 4), "the oldest go");
        let later = NOW + NOTICES_KEPT_MS + 10;
        p.keep_notice(&r(999, later), false, later);
        let ids: Vec<&str> = p.notices.iter().map(|k| k.req.id.as_str()).collect();
        assert_eq!(
            ids.len(),
            NOTICES_KEPT - 6,
            "made a day before, gone: {ids:?}"
        );
    }

    #[test]
    fn the_denies_seen_are_kept_once_and_only_the_newest() {
        let book = Arc::new(LeaseBook::new(Arc::new(leasebook::ManualClock::new(NOW))));
        let mut p = Protected::new(ProtectedConfig::new(30, 10, None), book);
        p.note_deny("");
        p.note_deny("a");
        p.note_deny("a");
        assert_eq!(p.seen_denies, ["a"]);
        for i in 0..SEEN_DENIES_KEPT {
            p.note_deny(&format!("e{i}"));
        }
        assert_eq!(p.seen_denies.len(), SEEN_DENIES_KEPT);
        assert_eq!(p.seen_denies[0], "e0");
        assert_eq!(
            p.seen_denies.last().map(String::as_str),
            Some(format!("e{}", SEEN_DENIES_KEPT - 1).as_str())
        );
    }
}
