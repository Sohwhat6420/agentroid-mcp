//! The protected-device lease (`docs/design/protected-device-lease.md`). Off unless the daemon is
//! started with `--protected-lease`.
//!
//! Without it the protected phone ([`agentroid_adb_link::guard::S24_SERIAL`]) answers only the
//! ruled reads. With it, a call that names the protected phone runs only under an open lease, and
//! one lease covers a burst of work (30 min by default):
//!
//! * **One Allow.** The first call with no lease sends the owner one push (Allow / Deny, answered
//!   within the ask window, 10 min by default) and is refused as `lease:pending`; the agent retries
//!   once the owner has tapped Allow. The same lease also satisfies the screen lock, so one Allow
//!   is never asked for twice.
//! * **Overnight.** When a presence file signed with the owner's key (HMAC-SHA256), written in the
//!   last `--presence-ttl-min` minutes and not dated in the future, says `asleep`, the lease opens
//!   without an ask, and a notice with a Deny button goes out (no notice sent: no lease). Any
//!   problem with the file turns the overnight rule off, so the call asks instead.
//! * **Bound to its holder.** A lease, and a pending ask, belong to one daemon lease (the holder's
//!   name and the registry epoch); another holder never inherits either, and the daemon closes the
//!   lease when its holder's connection goes.
//! * **Deny ends it.** The open lease's Deny channel is polled every 10 s; a Deny closes the lease
//!   and holds: after an overnight Deny, until a trusted presence file says the owner is awake;
//!   after a daytime Deny (or no answer), for the ask window. A fresh `allow <id>` for the denied
//!   request lifts the hold. A channel that cannot be read for 2 min closes the lease.
//! * **Never under any lease.** Gmail (and Gmail Go) and WhatsApp (any package naming it), the
//!   keyguard, the notification shade, notification rows and Recents, a system bar naming a
//!   protected app, digits on a PIN prompt (one at a time included), `unlock`, paths naming the
//!   protected apps, raw shell lines and link segments. Reads are refused on those screens too, and
//!   a screenshot is checked again after it is taken.

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
/// The packages that are never driven, read or opened on the protected phone; any package whose
/// name contains `whatsapp` is one too.
pub const PROTECTED_APPS: [&str; 4] = [
    "com.google.android.gm",
    "com.google.android.gm.lite",
    "com.whatsapp",
    "com.whatsapp.w4b",
];

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
        }
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
// what is never done on the protected phone
// ---------------------------------------------------------------------------------------------

/// Gmail, Gmail Go, WhatsApp, WhatsApp Business, and any package naming WhatsApp.
pub fn is_protected_package(p: &str) -> bool {
    let p = p.trim().to_ascii_lowercase();
    PROTECTED_APPS.contains(&p.as_str())
        || p.starts_with("com.google.android.gm.")
        || p.contains("whatsapp")
}

/// Does a label or name mention a protected app?
pub fn names_protected(s: &str) -> bool {
    let s = s.to_lowercase();
    s.contains("whatsapp") || s.contains("gmail")
}

/// Resource-id and class fragments of the keyguard (lock screen and bouncer).
const KEYGUARD_MARKS: [&str; 6] = [
    "keyguard",
    "bouncer",
    "lock_icon",
    "lockscreen",
    "lock_screen",
    "notification_panel_lock",
];
/// Fragments of the notification shade, notification rows, heads-up notifications and quick
/// settings (the status bar's own icon row is not one of them).
const PREVIEW_MARKS: [&str; 14] = [
    "expandablenotificationrow",
    "notification_stack_scroller",
    "notificationstackscroll",
    "notification_panel",
    "notification_container",
    "notification_main_column",
    "notification_content",
    "notification_row",
    "heads_up",
    "headsup",
    "qs_frame",
    "qs_panel",
    "quick_settings",
    "status_bar_expanded",
];
/// Fragments of Recents (the overview's app thumbnails), in System UI or a launcher.
const RECENTS_MARKS: [&str; 6] = [
    "recents",
    "overview_panel",
    "task_thumbnail",
    "snapshot",
    "task_view",
    "taskview",
];
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

/// What the protected phone's screen shows that matters to the lease.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenFacts {
    /// A protected app's package on screen.
    pub protected_app: Option<String>,
    pub keyguard: bool,
    pub preview: bool,
    pub recents: bool,
    /// A System UI node (a status-bar icon's description, a toast) names a protected app.
    pub protected_mention: bool,
    pub pin_prompt: bool,
    /// Where a node naming a protected app sits (a launcher icon, a share target): no touch there.
    pub protected_bounds: Vec<(i32, i32, i32, i32)>,
    /// Where a single-digit key sits on a PIN prompt.
    pub digit_bounds: Vec<(i32, i32, i32, i32)>,
}

fn is_systemui(pkg: &str) -> bool {
    pkg == "com.android.systemui"
}

fn is_launcher(pkg: &str) -> bool {
    pkg.contains("launcher") || pkg.ends_with(".home") || pkg.contains("quickstep")
}

/// Read the facts from a uiautomator dump. `Err`: the dump did not parse (the message never
/// quotes it).
pub fn screen_facts(xml: &str) -> Result<ScreenFacts, String> {
    let tree =
        crate::tree::parse(xml, false).map_err(|_| "the screen did not parse".to_string())?;
    let mut f = ScreenFacts::default();
    for n in &tree.nodes {
        let pkg = n.package.to_ascii_lowercase();
        let rid = n.rid.to_ascii_lowercase();
        let class = n.class.to_ascii_lowercase();
        let marks = |list: &[&str]| list.iter().any(|m| rid.contains(m) || class.contains(m));
        if !pkg.is_empty() && is_protected_package(&pkg) && f.protected_app.is_none() {
            f.protected_app = Some(pkg.clone());
        }
        if is_systemui(&pkg) {
            if marks(&KEYGUARD_MARKS) {
                f.keyguard = true;
            }
            if marks(&PREVIEW_MARKS) {
                f.preview = true;
            }
            if names_protected(&n.text) || names_protected(&n.desc) {
                f.protected_mention = true;
            }
        }
        if (is_systemui(&pkg) || is_launcher(&pkg)) && marks(&RECENTS_MARKS) {
            f.recents = true;
        }
        let label = format!("{} {}", n.text, n.desc).to_lowercase();
        if marks(&PIN_MARKS) || n.flags.password || PIN_TEXTS.iter().any(|t| label.contains(t)) {
            f.pin_prompt = true;
        }
        if let Some(b) = n.bounds {
            if names_protected(&n.text) || names_protected(&n.desc) || names_protected(&rid) {
                f.protected_bounds.push((b.l, b.t, b.r, b.b));
            }
            let t = n.text.trim();
            let d = n.desc.trim();
            let one_digit = |s: &str| s.len() == 1 && s.chars().all(|c| c.is_ascii_digit());
            if one_digit(t) || one_digit(d) {
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

/// A refusal for any call on this screen, reads included: the keyguard, notification previews,
/// Recents, a system bar naming a protected app, or a protected app on screen.
pub fn read_refusal(f: &ScreenFacts) -> Option<(&'static str, String)> {
    if f.keyguard {
        return Some((
            "preview",
            "the keyguard is showing (notification previews and the PIN pad live there)".into(),
        ));
    }
    if f.preview {
        return Some((
            "preview",
            "the notification shade or a notification is showing".into(),
        ));
    }
    if f.recents {
        return Some(("preview", "Recents is showing (app thumbnails)".into()));
    }
    if f.protected_mention {
        return Some((
            "preview",
            "a system bar names a protected app (a notification from it)".into(),
        ));
    }
    if let Some(p) = &f.protected_app {
        return Some(("app", format!("{p} is on screen")));
    }
    None
}

/// The screen check for `call` before it is sent: exit keys always go; then [`read_refusal`]; then
/// digits on a PIN prompt (typed, pressed or tapped, one at a time included).
pub fn screen_refusal(
    f: &ScreenFacts,
    call: &Call,
    at: Option<(i32, i32)>,
) -> Option<(&'static str, String)> {
    if let Call::Key { name } = call {
        if exit_key(name) {
            return None;
        }
    }
    if let Some(r) = read_refusal(f) {
        return Some(r);
    }
    if f.pin_prompt {
        let digits = match call {
            Call::TypeText { text, .. } => text.chars().any(|c| c.is_ascii_digit()),
            Call::Key { name } => digit_key(name),
            _ => false,
        };
        if digits {
            return Some((
                "pin",
                "a PIN prompt is showing, and digits are never entered there".into(),
            ));
        }
    }
    if let Some((x, y)) = at {
        let inside = |b: &(i32, i32, i32, i32)| x >= b.0 && x < b.2 && y >= b.1 && y < b.3;
        if f.protected_bounds.iter().any(inside) {
            return Some(("app", "the touch lands on a protected app".into()));
        }
        if f.digit_bounds.iter().any(inside) {
            return Some((
                "pin",
                "the touch lands on a PIN digit, and digits are never entered there".into(),
            ));
        }
    }
    None
}

/// Does a URI open a protected app (its scheme, host or package)? `mailto:` counts: it opens the
/// mail app.
pub fn uri_names_protected(uri: &str) -> bool {
    let u = uri.trim().to_lowercase();
    names_protected(&u)
        || u.contains("wa.me")
        || u.contains("com.google.android.gm")
        || u.starts_with("mailto:")
}

/// A launch that would open a protected app: `open_app`, `open_url`, `start_activity`.
pub fn launch_refusal(call: &Call) -> Option<(&'static str, String)> {
    let hit = match call {
        Call::OpenApp { package } => is_protected_package(package) || names_protected(package),
        Call::OpenUrl { uri, package } => {
            uri_names_protected(uri)
                || package
                    .as_deref()
                    .is_some_and(|p| is_protected_package(p) || names_protected(p))
        }
        Call::StartActivity {
            component,
            action: _,
            data,
        } => {
            let pkg = component.split('/').next().unwrap_or_default();
            is_protected_package(pkg)
                || names_protected(component)
                || data.as_deref().is_some_and(uri_names_protected)
        }
        _ => false,
    };
    hit.then(|| ("app", "it would open a protected app".to_string()))
}

/// A phone path as the protected-path check compares it: lower case, `\` as `/`, `//` and `/./`
/// collapsed.
pub fn normalise_path(p: &str) -> String {
    let p = p.trim().to_lowercase().replace('\\', "/");
    let mut out: Vec<&str> = vec![];
    for seg in p.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        out.push(seg);
    }
    let lead = if p.starts_with('/') { "/" } else { "" };
    format!("{lead}{}", out.join("/"))
}

/// A push or pull path that names a protected app (`whatsapp`, `gmail`, `com.google.android.gm`,
/// which also catches `com.google.android.gms`) is refused on the protected phone, in any case or
/// spelling of its separators.
pub fn path_refusal(p: &str) -> Option<String> {
    let n = normalise_path(p);
    (n.contains("whatsapp") || n.contains("gmail") || n.contains("com.google.android.gm"))
        .then(|| "the path names a protected app".to_string())
}

// ---------------------------------------------------------------------------------------------
// the lease
// ---------------------------------------------------------------------------------------------

/// Who holds the protected phone in the daemon for this call (`None`: nobody, or not this caller).
pub type HolderFn = Box<dyn Fn(&DeviceInfo) -> Option<Holder> + Send>;

#[derive(Debug, Clone)]
struct Pending {
    req: Request,
    holder: Holder,
}

#[derive(Debug, Clone)]
struct Held {
    req: Request,
    at_ms: u64,
    /// Denied while the owner was asleep: held until a trusted presence file says otherwise.
    asleep: bool,
}

#[derive(Debug, Clone)]
struct Running {
    req: Request,
    last_poll_ms: u64,
    fail_since: Option<u64>,
}

/// The lease's state in core.
pub struct Protected {
    pub cfg: ProtectedConfig,
    book: Arc<LeaseBook>,
    holder_of: Option<HolderFn>,
    jobs: HashMap<u64, String>,
    pending: Option<Pending>,
    held: Option<Held>,
    running: Option<Running>,
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

impl Protected {
    pub fn new(cfg: ProtectedConfig, book: Arc<LeaseBook>) -> Self {
        Protected {
            cfg,
            book,
            holder_of: None,
            jobs: HashMap::new(),
            pending: None,
            held: None,
            running: None,
        }
    }

    /// How core learns who holds the protected phone (the daemon's registry). Without it no lease
    /// opens.
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

    /// Close the lease of daemon lease `epoch` (its holder released the phone or went).
    pub fn close_epoch(&mut self, epoch: u64) -> Option<Lease> {
        let closed = self.book.close_epoch(epoch);
        if closed.is_some() {
            self.running = None;
        }
        self.jobs.remove(&epoch);
        closed
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

    /// Admit a call for the protected phone `d` under a lease: the open one when it is this
    /// holder's, or a new one (overnight), or an ask (refused as pending). Binds `d.serial` for the
    /// adb guard on success.
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
                self.book.close();
                self.running = None;
                log(
                    audit.as_deref_mut(),
                    "blocked",
                    "lease:closed",
                    format!(
                        "lease {} was opened for {} (epoch {}), not for {} (epoch {}): closed",
                        reference(&lease.id),
                        lease.holder.name,
                        lease.holder.epoch,
                        holder.name,
                        holder.epoch
                    ),
                )?;
            } else {
                // a Deny, or a Deny channel down too long, closes it
                if let Some(run) = self.running.as_mut() {
                    if now.saturating_sub(run.last_poll_ms) >= self.cfg.deny_poll_ms {
                        run.last_poll_ms = now;
                        let polled = match approver.as_deref_mut() {
                            Some(a) => a.poll_deny(&run.req),
                            None => Err("no approval channel".into()),
                        };
                        match polled {
                            Ok(false) => run.fail_since = None,
                            Ok(true) => {
                                let req = run.req.clone();
                                self.running = None;
                                self.book.close();
                                let asleep = lease.granted == Granted::Overnight
                                    || self.presence(now).asleep();
                                self.held = Some(Held {
                                    req,
                                    at_ms: now,
                                    asleep,
                                });
                                log(
                                    audit.as_deref_mut(),
                                    "blocked",
                                    "lease:denied",
                                    format!(
                                        "the owner tapped Deny on lease {}: closed and held",
                                        reference(&lease.id)
                                    ),
                                )?;
                                return Err(refused(
                                    "the owner tapped Deny, so the lease was closed".into(),
                                ));
                            }
                            Err(e) => {
                                let since = *run.fail_since.get_or_insert(now);
                                if now.saturating_sub(since) >= self.cfg.deny_poll_fail_ms {
                                    self.running = None;
                                    self.book.close();
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
                self.book.close();
                self.running = None;
                log(
                    audit.as_deref_mut(),
                    "blocked",
                    "lease:closed",
                    format!(
                        "the overnight lease {} ended: the presence file says {p}",
                        reference(&lease.id)
                    ),
                )?;
            }
        }

        // 2. a hold after a Deny (or no answer)
        if let Some(h) = self.held.clone() {
            if let Some(ev) = approver
                .as_deref_mut()
                .and_then(|a| a.reallowed(&h.req, h.at_ms))
            {
                self.held = None;
                let lease = self.open(
                    now,
                    &holder,
                    Granted::Approved {
                        request_id: h.req.id.clone(),
                    },
                    h.req.clone(),
                );
                log(
                    audit.as_deref_mut(),
                    "allowed",
                    "lease:reallowed",
                    format!(
                        "the owner re-allowed ask {} (answer {ev}): lease open for {} min",
                        reference(&h.req.id),
                        self.cfg.lease_ms / 60_000
                    ),
                )?;
                self.book.bind(&d.serial);
                return Ok(lease);
            }
            let over = if h.asleep {
                matches!(self.presence(now), Presence::Other(_))
            } else {
                now >= h.at_ms.saturating_add(self.cfg.ask_window_ms)
            };
            if !over {
                return Err(refused(format!(
                    "held after the owner's Deny on ask {} ({}); the owner can lift it by answering allow to that ask",
                    reference(&h.req.id),
                    if h.asleep {
                        "until the presence file says the owner is awake".to_string()
                    } else {
                        format!("for {} min", self.cfg.ask_window_ms / 60_000)
                    }
                )));
            }
            self.held = None;
        }

        // 3. a pending ask
        if let Some(p) = self.pending.take() {
            if p.holder != holder {
                log(
                    audit.as_deref_mut(),
                    "blocked",
                    "lease:stale",
                    format!(
                        "ask {} was for {} (epoch {}): dropped, asking again for {} (epoch {})",
                        reference(&p.req.id),
                        p.holder.name,
                        p.holder.epoch,
                        holder.name,
                        holder.epoch
                    ),
                )?;
            } else {
                let Some(a) = approver.as_deref_mut() else {
                    return Err(refused("no approval channel is configured".into()));
                };
                match a.check(&p.req, now) {
                    None => {
                        let r = reference(&p.req.id);
                        self.pending = Some(p);
                        return Err(Refusal::LeasePending { tool, reference: r });
                    }
                    Some(Answer::Approved { event_id }) => {
                        let lease = self.open(
                            now,
                            &holder,
                            Granted::Approved {
                                request_id: p.req.id.clone(),
                            },
                            p.req.clone(),
                        );
                        log(
                            audit.as_deref_mut(),
                            "allowed",
                            "lease:opened",
                            format!(
                                "the owner allowed ask {} (answer {event_id}): lease open for {} min for {} (epoch {})",
                                reference(&p.req.id),
                                self.cfg.lease_ms / 60_000,
                                holder.name,
                                holder.epoch
                            ),
                        )?;
                        self.book.bind(&d.serial);
                        return Ok(lease);
                    }
                    Some(Answer::Denied { .. }) => {
                        let asleep = self.presence(now).asleep();
                        self.held = Some(Held {
                            req: p.req.clone(),
                            at_ms: now,
                            asleep,
                        });
                        log(
                            audit.as_deref_mut(),
                            "blocked",
                            "lease:denied",
                            format!("the owner denied ask {}", reference(&p.req.id)),
                        )?;
                        return Err(refused("the owner tapped Deny".into()));
                    }
                    Some(other) => {
                        self.held = Some(Held {
                            req: p.req.clone(),
                            at_ms: now,
                            asleep: false,
                        });
                        log(
                            audit.as_deref_mut(),
                            "blocked",
                            "lease:expired",
                            format!("ask {}: {other}", reference(&p.req.id)),
                        )?;
                        return Err(refused(format!("the ask ended unanswered ({other})")));
                    }
                }
            }
        }

        // 4. overnight, or 5. a new ask
        let Some(a) = approver else {
            return Err(refused("no approval channel is configured".into()));
        };
        let presence = self.presence(now);
        let minutes = self.cfg.lease_ms / 60_000;
        let who = clean(&holder.name, 40);
        let job = self
            .jobs
            .get(&holder.epoch)
            .map_or(String::new(), |j| format!(", job {j}"));
        if presence.asleep() {
            let mut req = a.request(
                now,
                &device,
                tool,
                "lease",
                &format!(
                    "Overnight: {who}{job} opened a {minutes}-minute lease on the protected phone (the presence file says asleep). Gmail, WhatsApp, the lock screen, notifications and PIN entry stay off-limits."
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
            let lease = self.open(now, &holder, Granted::Overnight, req);
            log(
                audit.as_deref_mut(),
                "allowed",
                "lease:overnight",
                format!(
                    "a signed, fresh presence file says asleep: lease {} open for {minutes} min for {} (epoch {}), notice sent",
                    reference(&id),
                    holder.name,
                    holder.epoch
                ),
            )?;
            self.book.bind(&d.serial);
            return Ok(lease);
        }
        let mut req = a.request(
            now,
            &device,
            tool,
            "lease",
            &format!(
                "{who}{job} asks for a {minutes}-minute lease on the protected phone (first call: {tool}). Allow covers every call for {minutes} min; Gmail, WhatsApp, the lock screen, notifications and PIN entry stay off-limits."
            ),
            "",
        );
        req.style = Style::Lease;
        req.expires_ms = now.saturating_add(self.cfg.ask_window_ms);
        if let Err(e) = a.send(&req) {
            return Err(refused(format!("the ask could not be sent ({e})")));
        }
        let r = reference(&req.id);
        self.pending = Some(Pending {
            req,
            holder: holder.clone(),
        });
        log(
            audit,
            "asked",
            "lease:asked",
            format!(
                "asked the owner for a {minutes}-minute lease for {} (epoch {}), ask {r}, {} min to answer; overnight rule: {presence}",
                holder.name,
                holder.epoch,
                self.cfg.ask_window_ms / 60_000
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

    #[test]
    fn previews_are_system_ui_rows_and_the_status_bar_is_not_one() {
        let bar = node(
            "com.android.systemui",
            "com.android.systemui:id/notification_icon_area",
            "",
            "",
            "[0,0][100,40]",
        );
        let home = node("com.example.home", "", "Clock", "", "[0,100][100,200]");
        let f = screen_facts(&screen(&[bar.clone(), home.clone()])).unwrap();
        assert_eq!(read_refusal(&f), None, "a status bar and a home screen");
        for rid in [
            "com.android.systemui:id/expandableNotificationRow",
            "com.android.systemui:id/notification_stack_scroller",
            "com.android.systemui:id/keyguard_status_view",
            "com.android.systemui:id/qs_frame",
        ] {
            let n = node("com.android.systemui", rid, "Hi", "", "[0,0][10,10]");
            let f = screen_facts(&screen(&[bar.clone(), n])).unwrap();
            assert_eq!(read_refusal(&f).map(|r| r.0), Some("preview"), "{rid}");
        }
        let recents = node(
            "com.google.android.apps.nexuslauncher",
            "com.google.android.apps.nexuslauncher:id/task_thumbnail",
            "",
            "",
            "[0,0][10,10]",
        );
        assert!(screen_facts(&screen(&[recents])).unwrap().recents);
        let icon = node(
            "com.android.systemui",
            "com.android.systemui:id/icon",
            "",
            "WhatsApp notification: Dinner?",
            "[0,0][10,10]",
        );
        assert!(screen_facts(&screen(&[icon])).unwrap().protected_mention);
    }

    #[test]
    fn protected_apps_include_gmail_go_and_any_whatsapp() {
        for p in [
            "com.google.android.gm",
            "com.google.android.gm.lite",
            "COM.WHATSAPP",
            "com.whatsapp.w4b",
            "com.gbwhatsapp",
        ] {
            assert!(is_protected_package(p), "{p}");
        }
        assert!(!is_protected_package("com.google.android.gms"));
        assert!(!is_protected_package("com.android.settings"));
        let f = screen_facts(&screen(&[node(
            "com.google.android.gm.lite",
            "",
            "Inbox",
            "",
            "[0,0][10,10]",
        )]))
        .unwrap();
        assert_eq!(read_refusal(&f).map(|r| r.0), Some("app"));
        let back = Call::Key {
            name: "KEYCODE_BACK".into(),
        };
        assert_eq!(screen_refusal(&f, &back, None), None, "exit keys go");
        let recents = Call::Key {
            name: "recents".into(),
        };
        assert!(screen_refusal(&f, &recents, None).is_some());
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
        let f = screen_facts(&screen(&[prompt, two])).unwrap();
        assert!(f.pin_prompt);
        for call in [
            Call::TypeText {
                text: "7".into(),
                index: None,
                submit: false,
            },
            Call::TypeText {
                text: "ab3".into(),
                index: None,
                submit: false,
            },
            Call::Key {
                name: "KEYCODE_5".into(),
            },
            Call::Key {
                name: "KEYCODE_NUMPAD_0".into(),
            },
        ] {
            assert_eq!(
                screen_refusal(&f, &call, None).map(|r| r.0),
                Some("pin"),
                "{call:?}"
            );
        }
        let tap = Call::Tap {
            target: crate::gate::Target::Index(0),
        };
        assert_eq!(
            screen_refusal(&f, &tap, Some((10, 120))).map(|r| r.0),
            Some("pin")
        );
        assert_eq!(screen_refusal(&f, &tap, Some((10, 300))), None);
        let letters = Call::TypeText {
            text: "abc".into(),
            index: None,
            submit: false,
        };
        assert_eq!(screen_refusal(&f, &letters, None), None);
    }

    #[test]
    fn launches_and_paths_naming_a_protected_app_are_refused() {
        let refused = [
            Call::OpenApp {
                package: "whatsapp".into(),
            },
            Call::OpenApp {
                package: "com.google.android.gm.lite".into(),
            },
            Call::OpenUrl {
                uri: "https://wa.me/15550000".into(),
                package: None,
            },
            Call::OpenUrl {
                uri: "mailto:a@example.com".into(),
                package: None,
            },
            Call::OpenUrl {
                uri: "https://example.com".into(),
                package: Some("com.whatsapp".into()),
            },
            Call::StartActivity {
                component: "com.google.android.gm/.ConversationListActivityGmail".into(),
                action: None,
                data: None,
            },
        ];
        for c in &refused {
            assert!(launch_refusal(c).is_some(), "{c:?}");
        }
        assert!(launch_refusal(&Call::OpenApp {
            package: "com.android.settings".into()
        })
        .is_none());
        for p in [
            "/sdcard/WhatsApp/Media/x.jpg",
            "/SDCARD//Android/media/COM.WHATSAPP/x",
            "\\sdcard\\Android\\data\\com.google.android.gm\\x",
            "/sdcard/./Gmail/x",
            "/sdcard/Android/data/com.google.android.gms/x",
        ] {
            assert!(path_refusal(p).is_some(), "{p}");
        }
        assert!(path_refusal("/sdcard/DCIM/Camera/x.jpg").is_none());
        assert_eq!(normalise_path("/A//b/./c\\D"), "/a/b/c/d");
    }
}
