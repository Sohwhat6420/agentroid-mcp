//! The [`Transport`] seam and [`Core`], which serves every tool call on top of it.
//!
//! `Transport` is the only thing that touches a phone: the live one (adb server socket plus the
//! helper link) is a later task; tests and the selftest scaffold use [`crate::fake::FakeDevice`].
//! `Core` owns the gate, the per-device state (last tree, last screenshot geometry) and the
//! tool semantics ported from phone-hands.

use crate::approval::{Approver, Outcome as Approval};
use crate::audit::{AuditLog, Entry};
use crate::fileguard::{self, FileOp};
use crate::frame::{Frame, FrameError, Shot};
use crate::gate::{self, Call, DeviceMode, Refusal, Subject, Target};
use crate::require::{read_require, RequireOpts, Required};
use crate::screenguard::{self, Decision, Screen, Touch};
use crate::screenlock::{Admitted, ScreenLock};
use crate::selector::{resolve, Chosen, Resolution, ResolveError, Selector, Status};
use crate::settle::{settle, Settle, SettleOpts, TreeSource};
use crate::shell;
use crate::shellguard;
use crate::tree::parse;
use agentroid_adb_link::runner::Clock;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Hardware serial of the POCO X6 Pro, the default allowlist.
pub const POCO_SERIAL: &str = "DEVICE0000EXAMPLE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(pub String);

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransportError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// What adb calls it (USB serial, `ip:port` or an mDNS name).
    pub serial: String,
    pub model: String,
    /// `ro.serialno`.
    pub hw_serial: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    Uiautomator,
    Helper,
}

impl Backend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Uiautomator => "uiautomator",
            Backend::Helper => "helper",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screenshot {
    pub jpeg: Vec<u8>,
    pub shot: Shot,
}

/// One input event, in device pixels of `display`.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Tap {
        x: i32,
        y: i32,
        display: u32,
    },
    LongPress {
        x: i32,
        y: i32,
        ms: u32,
        display: u32,
    },
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        ms: u32,
        display: u32,
    },
    Key(String),
    /// ASCII for `input text`, already escaped with [`shell::escape_input_text`].
    Text(String),
    /// Non-ASCII text, through the helper.
    UnicodeText(String),
}

/// What a transport already knows about the phone from the replies it has served (2.4 health member).
/// Reading it costs no command: a transport reports only what its last replies carried, never asks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HealthHint {
    /// Percent, 0 to 100.
    pub battery: Option<u8>,
    pub powered: Option<bool>,
    pub temp_c: Option<f64>,
    /// `on` or `off`.
    pub screen: Option<String>,
    /// `locked` or `unlocked`.
    pub lock: Option<String>,
    pub foreground_package: Option<String>,
    pub foreground_title: Option<String>,
    /// How old the oldest reported field is, in seconds.
    pub age_s: f64,
}

impl HealthHint {
    pub fn is_empty(&self) -> bool {
        self.battery.is_none()
            && self.powered.is_none()
            && self.temp_c.is_none()
            && self.screen.is_none()
            && self.lock.is_none()
            && self.foreground_package.is_none()
            && self.foreground_title.is_none()
    }
}

/// Everything the core needs from a phone.
pub trait Transport {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError>;
    /// Current screen size, rotation included.
    fn screen_size(&mut self, serial: &str) -> Result<(i32, i32), TransportError>;
    /// `Awake`, `Dozing`, ...
    fn screen_state(&mut self, serial: &str) -> Result<String, TransportError>;
    fn screenshot(&mut self, serial: &str, max_width: i32) -> Result<Screenshot, TransportError>;
    fn tree_xml(&mut self, serial: &str, backend: Backend) -> Result<String, TransportError>;
    fn input(&mut self, serial: &str, input: &Input) -> Result<(), TransportError>;
    fn packages(&mut self, serial: &str) -> Result<Vec<String>, TransportError>;
    /// Run one line in the phone's shell and return its output.
    fn shell(&mut self, serial: &str, line: &str) -> Result<String, TransportError>;
    /// `Ok` when the Artemis-style helper is installed and enabled; otherwise why not.
    fn helper_status(&mut self, serial: &str) -> Result<(), String>;
    /// `adb push` (2.4): copy a laptop file to a phone path. The argv is built without a host shell;
    /// the paths are passed whole. Transports without file transfer refuse.
    fn push(
        &mut self,
        _serial: &str,
        _local: &std::path::Path,
        _remote: &str,
    ) -> Result<(), TransportError> {
        Err(TransportError("this transport cannot push files".into()))
    }
    /// `adb pull` (2.4): copy a phone file to a laptop path.
    fn pull(
        &mut self,
        _serial: &str,
        _remote: &str,
        _local: &std::path::Path,
    ) -> Result<(), TransportError> {
        Err(TransportError("this transport cannot pull files".into()))
    }
    /// Test hook for the "lost token is re-pushed" check; transports without a helper return `Err`.
    fn helper_forget_token(&mut self, _serial: &str) -> Result<(), TransportError> {
        Err(TransportError("this transport has no helper token".into()))
    }
    /// What the replies served so far say about battery, power, temperature, lock and foreground.
    /// MUST NOT send anything to the phone (the protected phone rule: health rides on the call path, it is never probed).
    /// Default: nothing known.
    fn health_hint(&mut self, _serial: &str) -> HealthHint {
        HealthHint::default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CoreError {
    Refused(Refusal),
    /// Bad arguments or state (phone-hands' `ValueError`).
    Invalid(String),
    /// The phone refused input (phone-hands' `PermissionError` with the Xiaomi hint).
    InputRefused(String),
    Frame(FrameError),
    Resolve(ResolveError),
    /// A selector matched several elements and none was strictly better; nothing was touched.
    Ambiguous(Box<Resolution>),
    /// `require` timed out: these selectors were missing.
    RequireTimeout {
        missing: Vec<Selector>,
        detail: String,
    },
    Transport(TransportError),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoreError::Refused(r) => write!(f, "{r}"),
            CoreError::Invalid(s) | CoreError::InputRefused(s) => f.write_str(s),
            CoreError::Frame(e) => write!(f, "{e}"),
            CoreError::Resolve(e) => write!(f, "{e}"),
            CoreError::Ambiguous(r) => write!(
                f,
                "selector is ambiguous, nothing was touched: {}",
                r.describe()
            ),
            CoreError::RequireTimeout { detail, .. } => f.write_str(detail),
            CoreError::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CoreError {}

impl From<TransportError> for CoreError {
    fn from(e: TransportError) -> Self {
        CoreError::Transport(e)
    }
}
impl From<FrameError> for CoreError {
    fn from(e: FrameError) -> Self {
        CoreError::Frame(e)
    }
}
impl From<Refusal> for CoreError {
    fn from(e: Refusal) -> Self {
        CoreError::Refused(e)
    }
}

/// The answer to a call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub text: String,
    pub image: Option<Vec<u8>>,
    /// How a selector resolved (disclosure), for calls that take one.
    pub resolution: Option<Resolution>,
    pub required: Option<Required>,
    /// Geometry of the screenshot just taken.
    pub shot: Option<Shot>,
}

impl Outcome {
    fn text(s: impl Into<String>) -> Outcome {
        Outcome {
            text: s.into(),
            ..Default::default()
        }
    }
}

/// Reads trees from a transport, for `settle` and `require`.
struct LiveSource<'a, T: Transport> {
    transport: &'a mut T,
    serial: &'a str,
    backend: Backend,
}

impl<T: Transport> TreeSource for LiveSource<'_, T> {
    fn dump(&mut self) -> Result<String, String> {
        self.transport
            .tree_xml(self.serial, self.backend)
            .map_err(|e| e.0)
    }
}

#[derive(Default)]
struct DeviceState {
    shot: Option<Shot>,
    /// `Awake`, `Dozing`, ... as the last `screen_state` read inside a call said, with the clock then.
    screen: Option<(String, u64)>,
    /// Clock reading when the latest tree was read.
    tree_at: u64,
    /// Latest tree, with the `interactive_only` it was parsed under (indexes depend on it).
    tree: Option<(String, bool)>,
    /// The last few trees as ref rows, for `since` (see [`crate::refs`]).
    snaps: crate::refs::Snapshots,
    /// An acting call went through since `tree` was read, so the screen guard must read it again.
    dirty: bool,
}

/// The screen guard's settings (D3). On by default; there is no switch an agent can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenGuard {
    pub enabled: bool,
    /// The guard reuses the tree the last read stored when it is this fresh and nothing acted since;
    /// otherwise it reads the screen again before the call lands (one `tree_xml`).
    pub max_tree_age_ms: u64,
}

impl Default for ScreenGuard {
    fn default() -> Self {
        ScreenGuard {
            enabled: true,
            max_tree_age_ms: 2000,
        }
    }
}

/// Does an allowlist entry cover this adb serial? Case is ignored. An entry ending in `-`
/// (`emulator-`) is a prefix and nothing else, because adb also names other local listeners
/// `emulator-N`-style. Any other entry is a hardware serial: it covers an adb serial that equals it
/// or the wireless adb name `adb-<serial>-<suffix>._adb-tls-connect._tcp`, never a bare substring
/// (a fragment such as `_tcp` or `192.168.1.` must not admit a class of phones).
/// The guard's deny-by-default verdict for a screen it could not read, with why in the reason, so
/// the audit says what failed (11 Nov: a running stopwatch was refused and the log could not say
/// why). `why` is capped at 200 characters and never holds screen text.
fn unreadable(call: &Call, t: Touch, why: &str) -> Decision {
    let mut d = screenguard::decide(None, call, t);
    if d.rule == "unreadable" {
        let why: String = why.chars().take(200).collect();
        d.reason = format!("{} [{}]", d.reason, why.trim());
    }
    d
}

/// The name the audit log records a device under: the hardware serial, else the adb serial.
fn device_name(d: &DeviceInfo) -> String {
    if d.hw_serial.is_empty() {
        d.serial.clone()
    } else {
        d.hw_serial.clone()
    }
}

fn matches_adb_serial(entry: &str, adb_serial: &str) -> bool {
    let (entry, adb) = (entry.to_ascii_lowercase(), adb_serial.to_ascii_lowercase());
    if entry.ends_with('-') {
        adb.starts_with(&entry)
    } else {
        adb == entry || adb.starts_with(&format!("adb-{entry}-"))
    }
}

pub struct Core<T: Transport, C: Clock> {
    pub transport: T,
    clock: C,
    allow: HashSet<String>,
    modes: HashMap<String, DeviceMode>,
    state: HashMap<String, DeviceState>,
    pub backend: Backend,
    guard: ScreenGuard,
    audit: Option<AuditLog>,
    /// The approval channel (D3b). `None`: every `asked` verdict stays refused.
    approver: Option<Approver>,
    /// Set by the screen guard for the refusal record: (device, screen summary).
    pending: Option<(String, String)>,
    /// The screen lock: `None` (tests, the fake device) lets every acting call through to the
    /// other gates; the daemon sets one for real phones.
    lock: Option<ScreenLock>,
    /// The phone (adb serial, hardware serial) the lock last admitted an acting call for because
    /// of the phone's own state (free, or the agent's burst), so the end of that call counts as
    /// the agent's own activity. A lease or an emulator admits without setting it.
    lock_admitted: Option<(String, String)>,
    /// Screen-changing work the lock admitted outside [`Core::call`] (a script's shell line, a
    /// link segment) that has not reported its end yet, by the serial it was named with: the
    /// ticket's number and the phone. Only [`Core::lock_done`] with that very ticket makes the
    /// end the agent's own activity; any newer call on the phone drops the entry.
    lock_running: HashMap<String, (u64, String, String)>,
    /// The last ticket number handed out.
    lock_tickets: u64,
}

/// One piece of screen-changing work the screen lock let through outside [`Core::call`] (a link
/// segment, a script's shell line). Give it back to [`Core::lock_done`] once the work has run;
/// drop it when the work never ran. It counts only while it is the newest work on its phone.
#[derive(Debug, PartialEq, Eq)]
pub struct LockTicket {
    id: u64,
    serial: String,
}

impl<T: Transport, C: Clock> Core<T, C> {
    /// A core allowing only the POCO, every device in `full` mode (the protected phone is forced `read_only`).
    pub fn new(transport: T, clock: C) -> Self {
        Core {
            transport,
            clock,
            allow: HashSet::from([POCO_SERIAL.to_string()]),
            modes: HashMap::new(),
            state: HashMap::new(),
            backend: Backend::default(),
            guard: ScreenGuard::default(),
            audit: None,
            approver: None,
            pending: None,
            lock: None,
            lock_admitted: None,
            lock_running: HashMap::new(),
            lock_tickets: 0,
        }
    }

    /// Lock the screen (`screenlock.rs`): every acting call, and every script shell line that is
    /// not on the read-only list, needs a live `ask-screen` lease or a phone not in use.
    pub fn set_screen_lock(&mut self, lock: ScreenLock) {
        self.lock = Some(lock);
    }

    pub fn screen_lock(&self) -> Option<&ScreenLock> {
        self.lock.as_ref()
    }

    /// The lock's check for an acting call on `d`. Without a lock: allowed.
    fn lock_check(&mut self, d: &DeviceInfo, tool: &'static str) -> Result<(), Refusal> {
        let Some(lock) = self.lock.as_mut() else {
            return Ok(());
        };
        // any earlier work on this phone that never reported its end is stale now
        self.lock_running.retain(|_, (_, s, _)| s != &d.serial);
        match lock.admit(&d.serial, &d.hw_serial) {
            Ok(a) => {
                // only the phone's own state (free, or the agent's burst) makes the end of the
                // call the agent's own: a lease never starts or carries on a burst
                if matches!(a, Admitted::Free(_) | Admitted::Burst) {
                    self.lock_admitted = Some((d.serial.clone(), d.hw_serial.clone()));
                }
                Ok(())
            }
            Err(why) => Err(Refusal::ScreenLock { tool, why }),
        }
    }

    /// A ticket for work the lock admitted for `serial` (the key it was named with).
    fn lock_ticket(&mut self, serial: &str, phone: (String, String)) -> LockTicket {
        self.lock_tickets += 1;
        let id = self.lock_tickets;
        self.lock_running
            .insert(serial.to_string(), (id, phone.0, phone.1));
        LockTicket {
            id,
            serial: serial.to_string(),
        }
    }

    /// The screen lock alone, for screen-changing work that does not go through [`Core::call`]
    /// (a link segment, which the link signs and sends itself). `Ok`: it may go. `Ok(Some)`: give
    /// the ticket to [`Core::lock_done`] once the work has been sent and answered, or drop it if
    /// it never ran. `Ok(None)`: nothing to report (no lock, a lease, an emulator). A refusal is
    /// logged.
    pub fn lock_admit(
        &mut self,
        serial: Option<&str>,
        tool: &'static str,
    ) -> Result<Option<LockTicket>, CoreError> {
        self.pending = None;
        self.lock_admitted = None;
        // (the lock's own check drops any older ticket for this phone, under any name)
        let key = serial.unwrap_or_default();
        let r = self
            .find(serial, tool)
            .and_then(|d| self.lock_check(&d, tool).map_err(CoreError::from));
        let ticket = match (&r, self.lock_admitted.take()) {
            (Ok(()), Some(phone)) => Some(self.lock_ticket(key, phone)),
            _ => None,
        };
        if let Err(CoreError::Refused(refusal)) = &r {
            self.record_refusal(serial, tool, refusal, "");
        }
        r.map(|()| ticket)
    }

    /// The work `ticket` names (from [`Core::lock_admit`] or [`Core::shell_gate`]) has run (the
    /// runner or the link has returned): its end is the agent's own activity, so the burst goes on
    /// past it. A ticket that is no longer the newest work on its phone records nothing.
    pub fn lock_done(&mut self, ticket: LockTicket) {
        if self
            .lock_running
            .get(&ticket.serial)
            .is_some_and(|(id, _, _)| *id == ticket.id)
        {
            if let Some((_, s, h)) = self.lock_running.remove(&ticket.serial) {
                if let Some(lock) = self.lock.as_mut() {
                    lock.acted(&s, &h);
                }
            }
        }
    }

    /// Turn `asked` verdicts into a real question to a human (D3b). Without one they are refused.
    pub fn set_approver(&mut self, a: Approver) {
        self.approver = Some(a);
    }

    pub fn approver(&self) -> Option<&Approver> {
        self.approver.as_ref()
    }

    /// Replace the screen guard's settings (D3).
    pub fn set_screen_guard(&mut self, g: ScreenGuard) {
        self.guard = g;
    }

    /// Log every acting call and every refusal to this hash-chained log (D3).
    pub fn set_audit(&mut self, log: AuditLog) {
        self.audit = Some(log);
    }

    pub fn audit(&self) -> Option<&AuditLog> {
        self.audit.as_ref()
    }

    /// Replace the allowlist (hardware serials). The protected phone can never be put on it effectively.
    pub fn set_allow(&mut self, hw_serials: &[&str]) {
        self.allow = hw_serials.iter().map(|s| (*s).to_string()).collect();
    }

    /// Configure a device's mode by hardware or adb serial.
    pub fn set_mode(&mut self, serial: &str, mode: DeviceMode) {
        self.modes.insert(serial.to_string(), mode);
    }

    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// On the allowlist (and not the protected phone). Public since 2.10b: the daemon's `run_script` bridge
    /// hands it to the sandbox's own gate check.
    pub fn allowed(&self, d: &DeviceInfo) -> bool {
        !gate::is_s24(&d.serial)
            && !gate::is_s24(&d.hw_serial)
            && self
                .allow
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&d.hw_serial) || matches_adb_serial(a, &d.serial))
    }

    fn mode_of(&self, d: &DeviceInfo) -> DeviceMode {
        let configured = self
            .modes
            .get(&d.hw_serial)
            .or_else(|| self.modes.get(&d.serial))
            .copied()
            .unwrap_or_default();
        gate::effective_mode(&[&d.serial, &d.hw_serial], configured)
    }

    /// phone-hands' `_pick`, followed by the gate.
    fn pick(&mut self, serial: Option<&str>, call: &Call) -> Result<DeviceInfo, CoreError> {
        let info = self.find(serial, call.name())?;
        let mode = self.mode_of(&info);
        let subject = Subject {
            serial: &info.serial,
            hw_serial: &info.hw_serial,
            allowed: self.allowed(&info),
        };
        gate::check(&subject, mode, call)?;
        if call.is_acting() {
            self.lock_check(&info, call.name())?;
        }
        Ok(info)
    }

    /// [`pick`](Self::pick) for a raw adb action (`shell`, `push`, `pull`): the same device lookup,
    /// then [`gate::check_raw`].
    fn pick_raw(
        &mut self,
        serial: Option<&str>,
        tool: &'static str,
    ) -> Result<DeviceInfo, CoreError> {
        let info = self.find(serial, tool)?;
        let mode = self.mode_of(&info);
        let subject = Subject {
            serial: &info.serial,
            hw_serial: &info.hw_serial,
            allowed: self.allowed(&info),
        };
        gate::check_raw(&subject, mode, tool)?;
        Ok(info)
    }

    /// The device a call is aimed at, before any gate.
    fn find(&mut self, serial: Option<&str>, tool: &'static str) -> Result<DeviceInfo, CoreError> {
        let devices = self.transport.devices()?;
        let info = match serial {
            Some(s) => {
                let found = devices.iter().find(|d| d.serial == s || d.hw_serial == s).or_else(|| devices.iter().find(|d| d.serial.contains(s) || d.hw_serial.contains(s)));
                match found {
                    Some(d) => d.clone(),
                    None => {
                        // A request naming the protected phone is refused even when it is not connected.
                        if gate::is_s24(s) {
                            return Err(Refusal::S24 { tool }.into());
                        }
                        let names: Vec<&str> = devices.iter().map(|d| d.serial.as_str()).collect();
                        return Err(CoreError::Invalid(format!("No connected device matches {s:?}. Connected: {}", if names.is_empty() { "none".to_string() } else { names.join(", ") })));
                    }
                }
            }
            None => match devices.iter().find(|d| self.allowed(d)) {
                Some(d) => d.clone(),
                None => return Err(CoreError::Invalid("No allowed phone is connected. Check `adb devices`, and that the phone's hardware serial is allowed (bridged --allow-serial <serial>, or allow.txt in the state directory).".into())),
            },
        };
        Ok(info)
    }

    /// Run only the gate for `call` (device pick, S24, allowlist, mode) and serve nothing. Added for
    /// the 2.10 runner, which must refuse an acting flow step before it does anything else.
    pub fn admit(&mut self, serial: Option<&str>, call: &Call) -> Result<(), CoreError> {
        self.pick(serial, call).map(|_| ())
    }

    /// Serve one call. `serial` picks the device (default: the first allowed one). `list_devices`
    /// and `wait` take no target. Every refusal is written to the audit log when one is set.
    pub fn call(&mut self, serial: Option<&str>, call: &Call) -> Result<Outcome, CoreError> {
        self.pending = None;
        self.lock_admitted = None;
        // a ticket handed out before this call is stale now, whatever the call is
        self.lock_running.remove(serial.unwrap_or_default());
        let r = self.call_inner(serial, call);
        if let Err(CoreError::Refused(refusal)) = &r {
            self.record_refusal(serial, call.name(), refusal, "");
        }
        // anything that got past every gate may have touched the screen, even if it then failed:
        // its end is the agent's own activity, so the burst goes on past it
        if let Some((s, h)) = self.lock_admitted.take() {
            if !matches!(r, Err(CoreError::Refused(_))) {
                if let Some(lock) = self.lock.as_mut() {
                    lock.acted(&s, &h);
                }
            }
        }
        r
    }

    /// Write a refusal to the audit log (when one is set). The call is refused either way; a failed
    /// write cannot make it less refused.
    fn record_refusal(
        &mut self,
        serial: Option<&str>,
        tool: &str,
        refusal: &Refusal,
        target: &str,
    ) {
        if let Some(log) = self.audit.as_mut() {
            let (device, screen) = self.pending.take().unwrap_or_else(|| {
                (
                    serial.unwrap_or("(first allowed)").to_string(),
                    String::new(),
                )
            });
            let _ = log.append(&Entry {
                device,
                tool: tool.to_string(),
                decision: refusal.verdict().to_string(),
                rule: refusal.rule(),
                reason: refusal.to_string(),
                target: target.to_string(),
                screen,
            });
        }
    }

    /// A raw shell line (the sandbox's `shell` bridge call) through the device gate, the shell guard
    /// (D3b), the approval channel and the audit log. `Ok` means the runner may send it; every
    /// refusal is logged. The guard sees the whole line, so a refused line never reaches adb.
    /// `Ok(Some(ticket))`: the screen lock admitted the line; once the runner has returned, give
    /// the ticket to [`Core::lock_done`] (only then does the line count as the agent's own
    /// activity). `Ok(None)`: a read-only line (or no lock): there is nothing to report.
    pub fn shell_gate(
        &mut self,
        serial: Option<&str>,
        line: &str,
    ) -> Result<Option<LockTicket>, CoreError> {
        self.pending = None;
        self.lock_admitted = None;
        let key = serial.unwrap_or_default();
        // a ticket handed out before this line is stale now
        self.lock_running.remove(key);
        let r = self.pick_raw(serial, "shell").and_then(|d| {
            // the lock first, so a phone in use never gets an approval push for a line
            // a line on the read-only list changes nothing; any other may (an `input tap`)
            let argv: Vec<&str> = line.split(' ').filter(|a| !a.is_empty()).collect();
            if !shellguard::is_read_only(&argv) {
                self.lock_check(&d, "shell")?;
            }
            self.raw_gate(&d, "shell", shellguard::classify_shell(line))
        });
        // a line the lock admitted counts only once every gate has allowed it and it has run
        let ticket = match (&r, self.lock_admitted.take()) {
            (Ok(()), Some(phone)) => Some(self.lock_ticket(key, phone)),
            _ => None,
        };
        if let Err(CoreError::Refused(refusal)) = &r {
            let target = shellguard::classify_shell(line).target;
            self.record_refusal(serial, "shell", refusal, &target);
        }
        r.map(|()| ticket)
    }

    /// `push` to or `pull` from `phone_path` through the device gate, the file guard (D3b), the
    /// approval channel and the audit log. `Ok` means the devtools may run adb.
    pub fn file_gate(
        &mut self,
        serial: Option<&str>,
        op: FileOp,
        phone_path: &str,
    ) -> Result<(), CoreError> {
        self.pending = None;
        let tool = op.as_str();
        let r = self
            .pick_raw(serial, tool)
            .and_then(|d| self.raw_gate(&d, tool, fileguard::classify_file(op, phone_path)));
        if let Err(CoreError::Refused(refusal)) = &r {
            let target = fileguard::classify_file(op, phone_path).target;
            self.record_refusal(serial, tool, refusal, &target);
        }
        r
    }

    /// Record a shell line the `run_script` sandbox refused before it reached the bridge (D3b), so
    /// the audit log holds every refused command and not only the ones the guard saw. Nothing is
    /// sent and nothing is asked.
    pub fn record_shell_refusal(&mut self, serial: Option<&str>, line: &str, reason: &str) {
        let Some(log) = self.audit.as_mut() else {
            return;
        };
        let device = match self.transport.devices() {
            Ok(ds) => ds
                .iter()
                .find(|d| serial.is_some_and(|s| d.serial == s || d.hw_serial == s))
                .map(device_name)
                .unwrap_or_else(|| serial.unwrap_or("(first allowed)").to_string()),
            Err(_) => serial.unwrap_or("(first allowed)").to_string(),
        };
        let _ = log.append(&Entry {
            device,
            tool: "shell".into(),
            decision: "blocked".into(),
            rule: "shell:sandbox".into(),
            reason: reason.to_string(),
            target: shellguard::classify_shell(line).target,
            screen: String::new(),
        });
    }

    /// The shared tail of [`shell_gate`](Self::shell_gate) and [`file_gate`](Self::file_gate):
    /// the decision through [`gate::check_decision`], an `asked` verdict to the approver, and the
    /// allowed record (refused when the log cannot be written).
    fn raw_gate(
        &mut self,
        d: &DeviceInfo,
        tool: &'static str,
        decision: Decision,
    ) -> Result<(), CoreError> {
        let device = device_name(d);
        let (rule, reason) = match gate::check_decision(tool, &decision) {
            Ok(()) => (decision.rule.clone(), decision.reason.clone()),
            Err(Refusal::Screen {
                verdict: "asked", ..
            }) => match self.approve(&device, tool, &decision, "") {
                Ok(yes) => yes,
                Err(refusal) => {
                    self.pending = Some((device, String::new()));
                    return Err(refusal.into());
                }
            },
            Err(refusal) => {
                self.pending = Some((device, String::new()));
                return Err(refusal.into());
            }
        };
        if let Some(log) = self.audit.as_mut() {
            let e = Entry {
                device,
                tool: tool.to_string(),
                decision: "allowed".into(),
                rule,
                reason,
                target: decision.target.clone(),
                screen: String::new(),
            };
            if let Err(err) = log.append(&e) {
                return Err(Refusal::Audit {
                    tool,
                    why: err.to_string(),
                }
                .into());
            }
        }
        self.st(&d.serial).dirty = true;
        Ok(())
    }

    /// Take an `asked` decision to the approval channel (D3b). On a human yes, the `(rule, reason)`
    /// for the allowed record; otherwise the refusal. The ask is logged as `asked` with the request
    /// id BEFORE the wait, so the log shows it even if the daemon dies while waiting; the final
    /// `asked` refusal (denied / expired / unavailable) is written by the caller.
    fn approve(
        &mut self,
        device: &str,
        tool: &'static str,
        decision: &Decision,
        screen: &str,
    ) -> Result<(String, String), Refusal> {
        let kind = decision.kind.map_or("unknown", |k| k.as_str());
        let with_target = |reason: &str| {
            if decision.target.is_empty() {
                reason.to_string()
            } else {
                format!("{reason} on {}", decision.target)
            }
        };
        let Some(approver) = self.approver.as_mut() else {
            return Err(Refusal::Screen {
                tool,
                verdict: "asked",
                kind,
                reason: format!(
                    "{} (no approval channel is configured)",
                    with_target(&decision.reason)
                ),
            });
        };
        let req = approver.request(
            self.clock.now_ms(),
            device,
            tool,
            kind,
            &decision.reason,
            &decision.target,
        );
        let channel = approver.channel_name();
        if let Some(log) = self.audit.as_mut() {
            let e = Entry {
                device: device.to_string(),
                tool: tool.to_string(),
                decision: "asked".into(),
                rule: format!("screen:{kind}"),
                reason: format!(
                    "pending human approval {} via {channel}, {} s window: {}",
                    req.id,
                    approver.expiry_ms / 1000,
                    decision.reason
                ),
                target: decision.target.clone(),
                screen: screen.to_string(),
            };
            if let Err(err) = log.append(&e) {
                return Err(Refusal::Audit {
                    tool,
                    why: err.to_string(),
                });
            }
        }
        let clock = &self.clock;
        let outcome = approver.ask(&req, &|| clock.now_ms(), &|ms| clock.sleep_ms(ms));
        match outcome {
            Approval::Approved { event_id } => Ok((
                format!("approved:{channel}"),
                format!(
                    "{}; approved by {} (request {}, answer {event_id})",
                    decision.reason,
                    if channel == "bench" {
                        "the bench stand-in, not a human"
                    } else {
                        "a human"
                    },
                    req.id
                ),
            )),
            other => Err(Refusal::Screen {
                tool,
                verdict: "asked",
                kind,
                reason: format!("{} ({other})", with_target(&decision.reason)),
            }),
        }
    }

    /// The tree the screen guard judges: the stored one when it is fresh and nothing acted since,
    /// else a new read (not stored, so the indexes the model holds stay valid).
    fn guard_xml(&mut self, serial: &str) -> Result<String, TransportError> {
        let now = self.clock.now_ms();
        if let Some(st) = self.state.get(serial) {
            if let Some((xml, _)) = &st.tree {
                if !st.dirty && now.saturating_sub(st.tree_at) <= self.guard.max_tree_age_ms {
                    return Ok(xml.clone());
                }
            }
        }
        self.transport.tree_xml(serial, self.backend)
    }

    /// The screen guard (D3), after the device gate and before anything reaches the phone. `touch`
    /// is what a screen-dependent call would touch; `None` for launches and calls that commit nothing.
    /// An allowed call is logged before it is sent; if the log cannot be written it is refused.
    fn screen_gate(
        &mut self,
        d: &DeviceInfo,
        call: &Call,
        touch: Option<Touch>,
    ) -> Result<(), CoreError> {
        let device = device_name(d);
        let mut screen = String::new();
        let decision = if !self.guard.enabled {
            Decision::allowed("guard_off", "")
        } else if let Some(t) = touch {
            match self.guard_xml(&d.serial) {
                Ok(xml) => match parse(&xml, false) {
                    Ok(tree) => {
                        let sc = Screen::new(&tree);
                        screen =
                            format!("{} [{}]", sc.report.kinds(), sc.report.packages.join(","));
                        screenguard::decide(Some(&sc), call, t)
                    }
                    // The parser's message can quote the dump, which is screen text: keep it out.
                    Err(_) => unreadable(call, t, "the tree did not parse"),
                },
                Err(e) => unreadable(call, t, &format!("the read failed: {}", e.0)),
            }
        } else {
            screenguard::decide_launch(call)
                .unwrap_or_else(|| Decision::allowed("no_screen_rule", ""))
        };
        let (rule, reason) = match gate::check_screen(call, &decision) {
            Ok(()) => (decision.rule.clone(), decision.reason.clone()),
            // D3b: an `asked` verdict goes to a human; only an exact `allow <id>` lets it through
            Err(Refusal::Screen {
                verdict: "asked", ..
            }) => match self.approve(&device, call.name(), &decision, &screen) {
                Ok(yes) => yes,
                Err(refusal) => {
                    self.pending = Some((device, screen));
                    return Err(refusal.into());
                }
            },
            Err(refusal) => {
                self.pending = Some((device, screen));
                return Err(refusal.into());
            }
        };
        if let Some(log) = self.audit.as_mut() {
            let e = Entry {
                device,
                tool: call.name().to_string(),
                decision: "allowed".into(),
                rule,
                reason,
                target: decision.target.clone(),
                screen,
            };
            if let Err(err) = log.append(&e) {
                return Err(Refusal::Audit {
                    tool: call.name(),
                    why: err.to_string(),
                }
                .into());
            }
        }
        self.st(&d.serial).dirty = true;
        Ok(())
    }

    fn call_inner(&mut self, serial: Option<&str>, call: &Call) -> Result<Outcome, CoreError> {
        match call {
            Call::ListDevices => return self.list_devices(),
            Call::Wait { seconds } => {
                let s = if seconds.is_finite() {
                    seconds.clamp(0.0, 10.0)
                } else {
                    0.0
                };
                self.clock.sleep_ms((s * 1000.0) as u64);
                return Ok(Outcome::text(format!("waited {s} s")));
            }
            _ => {}
        }
        let p = self.pick(serial, call)?;
        let s = p.serial.clone();
        match call {
            Call::ListDevices | Call::Wait { .. } => unreachable!("handled above"),
            Call::Screenshot { max_width } => {
                let (img, note, shot) = self.shoot(&s, *max_width)?;
                Ok(Outcome {
                    text: note,
                    image: Some(img),
                    shot: Some(shot),
                    ..Default::default()
                })
            }
            Call::UiTree {
                interactive_only,
                require,
                timeout_ms,
            } => self.ui_tree(&s, *interactive_only, require, *timeout_ms),
            Call::Look { max_width } => {
                let (img, note, shot) = self.shoot(&s, *max_width)?;
                let listing = match self.read_tree(&s, false) {
                    Ok(t) => t,
                    Err(e) => format!("(UI tree unavailable: {e})"),
                };
                Ok(Outcome {
                    text: format!("{note}\n{listing}"),
                    image: Some(img),
                    shot: Some(shot),
                    ..Default::default()
                })
            }
            Call::Getprop { key } => {
                if !gate::valid_prop_key(key) {
                    return Err(CoreError::Invalid(format!("{key:?} is not a property key (only A-Z a-z 0-9 . _ -, 1 to 64 characters).")));
                }
                let out = self.transport.shell(&s, &format!("getprop {key}"))?;
                Ok(Outcome::text(out.trim().to_string()))
            }
            Call::ListApps { filter } => {
                let mut pk = self.transport.packages(&s)?;
                pk.sort();
                let f = filter.to_lowercase();
                let hits: Vec<String> = pk
                    .into_iter()
                    .filter(|p| p.to_lowercase().contains(&f))
                    .collect();
                Ok(Outcome::text(if hits.is_empty() {
                    format!("No package contains {filter:?}.")
                } else {
                    hits.join("\n")
                }))
            }
            Call::Tap { target } => {
                let (x, y, display, how, res) = self.locate(&s, target)?;
                self.screen_gate(&p, call, Some(Touch::At(x, y)))?;
                self.send(&s, &Input::Tap { x, y, display })?;
                Ok(Outcome {
                    text: format!("tapped device ({x},{y}) from {how}"),
                    resolution: res,
                    ..Default::default()
                })
            }
            Call::LongPress { target, ms } => {
                let (x, y, display, how, res) = self.locate(&s, target)?;
                self.screen_gate(&p, call, Some(Touch::At(x, y)))?;
                self.send(
                    &s,
                    &Input::LongPress {
                        x,
                        y,
                        ms: *ms,
                        display,
                    },
                )?;
                Ok(Outcome {
                    text: format!("long-pressed device ({x},{y}) for {ms} ms from {how}"),
                    resolution: res,
                    ..Default::default()
                })
            }
            Call::Swipe {
                x1,
                y1,
                x2,
                y2,
                ms,
                space,
                image_width,
            } => {
                let frame = self.frame(&s, space.as_deref(), *image_width)?;
                let (a, b) = (frame.map(*x1, *y1)?, frame.map(*x2, *y2)?);
                self.screen_gate(&p, call, Some(Touch::SwipeFrom(a.x, a.y)))?;
                self.send(
                    &s,
                    &Input::Swipe {
                        x1: a.x,
                        y1: a.y,
                        x2: b.x,
                        y2: b.y,
                        ms: *ms,
                        display: a.display,
                    },
                )?;
                let notes: Vec<String> = a.notes.iter().chain(b.notes.iter()).cloned().collect();
                let tail = if notes.is_empty() {
                    String::new()
                } else {
                    format!("; {}", notes.join("; "))
                };
                Ok(Outcome::text(format!(
                    "swiped device ({},{}) -> ({},{}) from {}{tail}",
                    a.x,
                    a.y,
                    b.x,
                    b.y,
                    frame.describe()
                )))
            }
            Call::Scroll { direction } => {
                let (w, h) = self.transport.screen_size(&s)?;
                let (x1, y1, x2, y2) =
                    shell::scroll_swipe(direction, w, h).map_err(CoreError::Invalid)?;
                self.screen_gate(&p, call, None)?;
                self.send(
                    &s,
                    &Input::Swipe {
                        x1,
                        y1,
                        x2,
                        y2,
                        ms: 350,
                        display: 0,
                    },
                )?;
                Ok(Outcome::text(format!("scrolled {direction}")))
            }
            Call::TypeText {
                text,
                index,
                submit,
            } => {
                let at = match index {
                    Some(i) => {
                        let (x, y, ..) = self.locate(&s, &Target::Index(*i))?;
                        Some((x, y))
                    }
                    None => None,
                };
                self.screen_gate(&p, call, Some(Touch::Type(at)))?;
                self.type_text(&s, text, *index, *submit)
            }
            Call::Key { name } => {
                let code = shell::keycode(name).map_err(CoreError::Invalid)?;
                // an escape key (back, home, volume...) needs no screen read; it is still logged
                let touch = (!screenguard::escape_key(name)).then_some(Touch::Key);
                self.screen_gate(&p, call, touch)?;
                self.send(&s, &Input::Key(code.clone()))?;
                Ok(Outcome::text(format!("pressed {code}")))
            }
            Call::Unlock => {
                self.screen_gate(&p, call, None)?;
                let (w, h) = self.transport.screen_size(&s)?;
                let out = self.transport.shell(&s, &shell::unlock_script(w, h))?;
                shell::check_input(&out).map_err(CoreError::InputRefused)?;
                Ok(Outcome::text(shell::parse_unlock(&out)))
            }
            Call::OpenApp { package } => {
                // the request first (refused before anything is read), then the package it resolves to
                self.screen_gate_launch_request(&p, call)?;
                let pkg = self.resolve_package(&s, package)?;
                self.screen_gate(
                    &p,
                    &Call::OpenApp {
                        package: pkg.clone(),
                    },
                    None,
                )?;
                self.launch_package(&s, &pkg)
            }
            Call::OpenUrl { uri, package } => {
                shell::check_uri(uri).map_err(CoreError::Invalid)?;
                self.screen_gate(&p, call, None)?;
                let r = self.am_start(&s, &shell::open_url_args(uri, package.as_deref()))?;
                Ok(Outcome::text(format!("opened {uri}: {r}")))
            }
            Call::StartActivity {
                component,
                action,
                data,
            } => {
                let args =
                    shell::start_activity_args(component, action.as_deref(), data.as_deref())
                        .map_err(CoreError::Invalid)?;
                self.screen_gate(&p, call, None)?;
                let r = self.am_start(&s, &args)?;
                Ok(Outcome::text(format!("started {component}: {r}")))
            }
        }
    }

    /// Settle the screen with the planned contract (200 ms polls, 2 equal signatures, 3 s cap).
    /// For the replay runner and for callers that want a stable tree after acting.
    pub fn settle(&mut self, serial: Option<&str>, opts: SettleOpts) -> Result<Settle, CoreError> {
        let p = self.pick(
            serial,
            &Call::UiTree {
                interactive_only: false,
                require: vec![],
                timeout_ms: None,
            },
        )?;
        let backend = self.backend;
        let mut src = LiveSource {
            transport: &mut self.transport,
            serial: &p.serial,
            backend,
        };
        Ok(settle(&mut src, &self.clock, opts))
    }

    fn list_devices(&mut self) -> Result<Outcome, CoreError> {
        let devices = self.transport.devices()?;
        if devices.is_empty() {
            return Ok(Outcome::text("No devices connected. Is wireless debugging on, and does `adb devices` show the phone?"));
        }
        let lines: Vec<String> = devices
            .iter()
            .map(|d| {
                let verdict = if self.allowed(d) {
                    format!("ALLOWED  mode={}", self.mode_of(d).as_str())
                } else {
                    "refused (not on allowlist)".to_string()
                };
                format!(
                    "{}  hw={}  model={}  {verdict}",
                    d.serial, d.hw_serial, d.model
                )
            })
            .collect();
        Ok(Outcome::text(lines.join("\n")))
    }

    fn st(&mut self, serial: &str) -> &mut DeviceState {
        self.state.entry(serial.to_string()).or_default()
    }

    fn screen_note(&mut self, serial: &str) -> String {
        match self.transport.screen_state(serial) {
            Ok(s) => {
                let now = self.clock.now_ms();
                self.st(serial).screen = Some((s.clone(), now));
                if s == "Awake" {
                    String::new()
                } else {
                    format!("SCREEN IS {}: call unlock() first. ", s.to_uppercase())
                }
            }
            Err(_) => String::new(),
        }
    }

    /// The latest tree a call stored for this phone (adb serial) and the `interactive_only` it was parsed under.
    /// Nothing is read from the phone.
    pub fn latest_tree(&self, serial: &str) -> Option<(String, bool)> {
        self.state.get(serial).and_then(|s| s.tree.clone())
    }

    /// The health the call path has already seen for a phone: the transport's hint (no command; skipped
    /// for the protected phone) filled in with the screen state and foreground package of the last `look`/`ui_tree`.
    /// `age_s` is the oldest field given. Sends nothing to the phone.
    pub fn health_of(&mut self, d: &DeviceInfo) -> HealthHint {
        let s24 = gate::is_s24(&d.serial) || gate::is_s24(&d.hw_serial);
        let mut h = if s24 {
            HealthHint::default()
        } else {
            self.transport.health_hint(&d.serial)
        };
        let now = self.clock.now_ms();
        let age = |at: u64| now.saturating_sub(at) as f64 / 1000.0;
        let mut oldest = if h.is_empty() { 0.0 } else { h.age_s };
        if let Some(st) = self.state.get(&d.serial) {
            if h.screen.is_none() {
                if let Some((s, at)) = &st.screen {
                    h.screen = Some(if s == "Awake" { "on" } else { "off" }.to_string());
                    oldest = f64::max(oldest, age(*at));
                }
            }
            if h.foreground_package.is_none() {
                if let Some((xml, io)) = &st.tree {
                    if let Ok(t) = parse(xml, *io) {
                        // the first node with a package that is not the status bar or the navigation bar
                        let pkg = t
                            .nodes
                            .iter()
                            .map(|n| n.package.as_ref())
                            .find(|p| !p.is_empty() && *p != "com.android.systemui");
                        if let Some(p) = pkg {
                            h.foreground_package = Some(p.to_string());
                            oldest = f64::max(oldest, age(st.tree_at));
                        }
                    }
                }
            }
        }
        h.age_s = oldest;
        h
    }

    fn shoot(
        &mut self,
        serial: &str,
        max_width: i32,
    ) -> Result<(Vec<u8>, String, Shot), CoreError> {
        let s = self.transport.screenshot(serial, max_width)?;
        self.st(serial).shot = Some(s.shot);
        let note = format!(
            "{}screen {}x{} device px; image {}x{}. tap/swipe x,y default to THIS image's pixels (x{:.3} to device); pass space='device' for the @(x,y) shown in the tree.",
            self.screen_note(serial),
            s.shot.dev_w,
            s.shot.dev_h,
            s.shot.img_w,
            s.shot.img_h,
            s.shot.scale()
        );
        Ok((s.jpeg, note, s.shot))
    }

    /// Read, remember and render the tree.
    fn read_tree(&mut self, serial: &str, interactive_only: bool) -> Result<String, CoreError> {
        let t0 = self.clock.now_ms();
        let xml = self.transport.tree_xml(serial, self.backend)?;
        let took = self.clock.now_ms().saturating_sub(t0);
        self.render_tree(serial, xml, interactive_only, took)
    }

    fn render_tree(
        &mut self,
        serial: &str,
        xml: String,
        interactive_only: bool,
        took_ms: u64,
    ) -> Result<String, CoreError> {
        let (listing, snap_id, injection) = {
            let tree = parse(&xml, interactive_only)
                .map_err(|e| CoreError::Invalid(format!("UI tree unavailable: {e}")))?;
            // LLM01: text on screen that addresses an AI agent is flagged as data in the reply
            let injection = if self.guard.enabled {
                screenguard::injection_note(&screenguard::injection_only(&tree))
            } else {
                None
            };
            let full = if interactive_only {
                Some(
                    parse(&xml, false)
                        .map_err(|e| CoreError::Invalid(format!("UI tree unavailable: {e}")))?,
                )
            } else {
                None
            };
            let snap = self
                .st(serial)
                .snaps
                .push(interactive_only, &tree, full.as_ref());
            let listing = if tree.elements.is_empty() {
                "No elements (the screen may be a game, video or WebView canvas; use screenshot + tap(x, y)).".to_string()
            } else {
                crate::refs::listing(snap)
            };
            (listing, snap.id.clone(), injection)
        };
        let now = self.clock.now_ms();
        let stt = self.st(serial);
        stt.tree = Some((xml, interactive_only));
        stt.tree_at = now;
        stt.dirty = false;
        let note = self.screen_note(serial);
        let injection = injection.map_or(String::new(), |n| format!("\n{n}"));
        Ok(format!(
            "{note}(tree from {} in {:.2} s, snapshot {snap_id})\n{listing}{injection}",
            self.backend.as_str(),
            took_ms as f64 / 1000.0
        ))
    }

    /// What changed in the latest tree since snapshot `since` (an id from an earlier read's header), as
    /// `+` added, `~` changed and `-` removed lines by stable ref. Reads nothing from the phone.
    pub fn diff_since(&mut self, serial: &str, since: &str) -> Result<String, CoreError> {
        let snaps = &self.st(serial).snaps;
        let Some(new) = snaps.latest() else {
            return Err(CoreError::Invalid(
                "no tree has been read yet; call ui_tree or look first".into(),
            ));
        };
        let Some(old) = snaps.get(since) else {
            return Err(CoreError::Invalid(format!(
                "no snapshot {since:?} (the last {} reads are kept); read the full tree again",
                crate::refs::KEEP_SNAPSHOTS
            )));
        };
        if old.id == new.id {
            return Ok(format!(
                "snapshot {} since {}: nothing read since",
                new.id, old.id
            ));
        }
        if old.interactive_only != new.interactive_only {
            return Err(CoreError::Invalid(format!("snapshot {since:?} was read with interactive_only={}, this one with {}; compare like with like", old.interactive_only, new.interactive_only)));
        }
        Ok(crate::refs::diff(old, new).text)
    }

    fn ui_tree(
        &mut self,
        serial: &str,
        interactive_only: bool,
        require: &[Selector],
        timeout_ms: Option<u64>,
    ) -> Result<Outcome, CoreError> {
        if require.is_empty() {
            return match self.read_tree(serial, interactive_only) {
                Ok(text) => Ok(Outcome::text(text)),
                // a dozing screen gives "null root node"; say why instead of failing bare
                Err(e) => Ok(Outcome::text(format!(
                    "{}(UI tree unavailable: {e})",
                    self.screen_note(serial)
                ))),
            };
        }
        let opts = RequireOpts {
            timeout_ms: timeout_ms.unwrap_or(RequireOpts::default().timeout_ms),
            ..Default::default()
        };
        let backend = self.backend;
        let t0 = self.clock.now_ms();
        let req = {
            let mut src = LiveSource {
                transport: &mut self.transport,
                serial,
                backend,
            };
            read_require(&mut src, &self.clock, require, opts)
        };
        match &req {
            Required::Met {
                xml, resolutions, ..
            } => {
                let took = self.clock.now_ms().saturating_sub(t0);
                let text = self.render_tree(serial, xml.clone(), interactive_only, took)?;
                let disclosure: Vec<String> =
                    resolutions.iter().map(Resolution::describe).collect();
                Ok(Outcome {
                    text: format!(
                        "{}\n{text}\nrequire: {}",
                        req.describe(),
                        disclosure.join("\n         ")
                    ),
                    required: Some(req),
                    ..Default::default()
                })
            }
            Required::Timeout { missing, .. } => Err(CoreError::RequireTimeout {
                missing: missing.clone(),
                detail: req.describe(),
            }),
        }
    }

    fn frame(
        &mut self,
        serial: &str,
        space: Option<&str>,
        image_width: Option<i32>,
    ) -> Result<Frame, CoreError> {
        let (w, h) = self.transport.screen_size(serial)?;
        let shot = self.st(serial).shot;
        // The latest shot's device size wins over a fresh read, as in phone-hands.
        let (dw, dh) = shot.map_or((w, h), |s| (s.dev_w, s.dev_h));
        Ok(Frame::resolve(space, dw, dh, shot, image_width)?)
    }

    /// phone-hands' `_target`, with selectors: a device point, the frame it came from, and (for
    /// selectors) the resolution disclosure.
    #[allow(clippy::type_complexity)]
    fn locate(
        &mut self,
        serial: &str,
        target: &Target,
    ) -> Result<(i32, i32, u32, String, Option<Resolution>), CoreError> {
        match target {
            Target::Index(i) => {
                let (xml, io) = self.st(serial).tree.clone().ok_or_else(|| {
                    CoreError::Invalid(
                        "No UI tree yet. Call ui_tree or look first, then tap by index.".into(),
                    )
                })?;
                let tree = parse(&xml, io).map_err(|e| CoreError::Invalid(e.to_string()))?;
                let Some(e) = tree.element(*i) else {
                    return Err(CoreError::Invalid(format!(
                        "Index {i} is out of range (0-{}). Call ui_tree again.",
                        tree.elements.len().saturating_sub(1)
                    )));
                };
                let (cx, cy) = e.centre();
                let label: String = e.label().chars().take(40).collect();
                let how = if label.is_empty() {
                    format!("[{i}] {}", e.class)
                } else {
                    format!("[{i}] {label:?}")
                };
                Ok((cx, cy, 0, how, None))
            }
            Target::Point {
                x,
                y,
                space,
                image_width,
            } => {
                let frame = self.frame(serial, space.as_deref(), *image_width)?;
                let m = frame.map(*x, *y)?;
                let tail = if m.notes.is_empty() {
                    String::new()
                } else {
                    format!("; {}", m.notes.join("; "))
                };
                Ok((
                    m.x,
                    m.y,
                    m.display,
                    format!("({x},{y}) in {}{tail}", frame.describe()),
                    None,
                ))
            }
            Target::Select(sel) => {
                let xml = self.transport.tree_xml(serial, self.backend)?;
                let now = self.clock.now_ms();
                let stt = self.st(serial);
                stt.tree = Some((xml.clone(), false));
                stt.tree_at = now;
                stt.dirty = false;
                let tree = parse(&xml, false).map_err(|e| CoreError::Invalid(e.to_string()))?;
                let res = resolve(&tree, sel).map_err(CoreError::Resolve)?;
                if res.status == Status::Ambiguous {
                    return Err(CoreError::Ambiguous(Box::new(res)));
                }
                let how = res.describe();
                match &res.chosen {
                    Some(Chosen::Element(a)) => {
                        let (cx, cy) = a.bounds.centre();
                        Ok((cx, cy, 0, how, Some(res)))
                    }
                    Some(Chosen::Point { fx, fy }) => {
                        let (w, h) = self.transport.screen_size(serial)?;
                        let m = Frame::device(w, h)?.map_rel(*fx, *fy)?;
                        Ok((m.x, m.y, m.display, how, Some(res)))
                    }
                    None => Err(CoreError::Invalid("selector resolved to nothing".into())),
                }
            }
        }
    }

    fn send(&mut self, serial: &str, input: &Input) -> Result<(), CoreError> {
        match self.transport.input(serial, input) {
            Ok(()) => Ok(()),
            Err(e) => match shell::check_input(&e.0) {
                Err(hint) => Err(CoreError::InputRefused(hint)),
                Ok(()) => Err(e.into()),
            },
        }
    }

    fn type_text(
        &mut self,
        serial: &str,
        text: &str,
        index: Option<usize>,
        submit: bool,
    ) -> Result<Outcome, CoreError> {
        if let Some(i) = index {
            let (x, y, display, ..) = self.locate(serial, &Target::Index(i))?;
            self.send(serial, &Input::Tap { x, y, display })?;
            self.clock.sleep_ms(300);
        }
        let mut routes: Vec<&str> = Vec::new();
        for (i, chunk) in text.split('\n').enumerate() {
            if i > 0 {
                self.send(serial, &Input::Key("KEYCODE_ENTER".into()))?;
            }
            if chunk.is_empty() {
                continue;
            }
            let route = if shell::needs_unicode_route(chunk) {
                if let Err(why) = self.transport.helper_status(serial) {
                    return Err(CoreError::Invalid(format!("Non-ASCII text needs the Artemis helper on the phone ({why}); adb `input text` only types ASCII. Type the ASCII parts, or install the helper.")));
                }
                self.send(serial, &Input::UnicodeText(chunk.to_string()))?;
                "helper set_text"
            } else {
                self.send(serial, &Input::Text(shell::escape_input_text(chunk)))?;
                "input text"
            };
            if !routes.contains(&route) {
                routes.push(route);
            }
        }
        if submit {
            self.send(serial, &Input::Key("KEYCODE_ENTER".into()))?;
        }
        routes.sort_unstable();
        let via = if routes.is_empty() {
            "nothing".to_string()
        } else {
            routes.join(", ")
        };
        Ok(Outcome::text(format!(
            "typed {} characters via {via}{}",
            text.chars().count(),
            if submit { " and pressed Enter" } else { "" }
        )))
    }

    /// `open_app`'s request checked before any lookup: a refused fragment ("digibank") reads nothing.
    /// Only a refusal is recorded here; the resolved package's check logs the allowed call.
    fn screen_gate_launch_request(&mut self, d: &DeviceInfo, call: &Call) -> Result<(), CoreError> {
        if !self.guard.enabled {
            return Ok(());
        }
        if let Some(dec) = screenguard::decide_launch(call) {
            // only a `blocked` request is refused here; an `asked` one is asked once, on the
            // resolved package (D3b), not twice
            if let Err(
                r @ Refusal::Screen {
                    verdict: "blocked", ..
                },
            ) = gate::check_screen(call, &dec)
            {
                self.pending = Some((device_name(d), String::new()));
                return Err(r.into());
            }
        }
        Ok(())
    }

    /// `open_app`'s matching: exact package, else the one installed package containing the text.
    fn resolve_package(&mut self, serial: &str, package: &str) -> Result<String, CoreError> {
        let pkgs = self.transport.packages(serial)?;
        let lower = package.to_lowercase();
        let mut m: Vec<&String> = pkgs.iter().filter(|p| p.as_str() == package).collect();
        if m.is_empty() {
            m = pkgs
                .iter()
                .filter(|p| p.to_lowercase().contains(&lower))
                .collect();
        }
        let Some(first) = m.first() else {
            return Err(CoreError::Invalid(format!(
                "No installed package matches {package:?}."
            )));
        };
        if m.len() > 1 && first.as_str() != package {
            let list: Vec<&str> = m.iter().take(10).map(|s| s.as_str()).collect();
            return Err(CoreError::Invalid(format!(
                "{package:?} matches several apps: {}",
                list.join(", ")
            )));
        }
        Ok((*first).clone())
    }

    fn launch_package(&mut self, serial: &str, pkg: &str) -> Result<Outcome, CoreError> {
        let out = self.transport.shell(
            serial,
            &format!(
                "monkey -p {} -c android.intent.category.LAUNCHER 1",
                shell::sh_quote(pkg)
            ),
        )?;
        if out.contains("No activities found") {
            return Err(CoreError::Invalid(format!(
                "{pkg} has no launcher activity."
            )));
        }
        Ok(Outcome::text(format!("opened {pkg}")))
    }

    fn am_start(&mut self, serial: &str, args: &[String]) -> Result<String, CoreError> {
        let out = self.transport.shell(serial, &shell::am_start_line(args))?;
        shell::parse_am_start(&out).map_err(CoreError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeClock, FakeDevice, FAKE_POCO_ADB, FAKE_S24_ADB};

    fn core() -> Core<FakeDevice, FakeClock> {
        Core::new(FakeDevice::new(), FakeClock::new())
    }

    #[test]
    fn an_unreadable_screen_says_why_in_the_reason_capped_at_200_characters() {
        let tap = Call::Tap {
            target: Target::Index(3),
        };
        let d = unreadable(
            &tap,
            Touch::At(10, 20),
            "the read failed: ERROR: could not get idle state.",
        );
        assert_eq!(d.rule, "unreadable");
        assert!(
            d.reason
                .ends_with("[the read failed: ERROR: could not get idle state.]"),
            "{}",
            d.reason
        );
        let long = "x".repeat(500);
        let d = unreadable(&tap, Touch::At(10, 20), &long);
        assert!(
            d.reason.contains(&"x".repeat(200)) && !d.reason.contains(&"x".repeat(201)),
            "{}",
            d.reason
        );
    }

    #[test]
    fn an_entry_ending_in_a_dash_is_a_prefix_not_a_substring() {
        assert!(matches_adb_serial("emulator-", "emulator-5554"));
        assert!(
            !matches_adb_serial("emulator-", "my-emulator-5554"),
            "a serial that merely contains it is not an emulator"
        );
        assert!(!matches_adb_serial(
            "emulator-",
            "adb-ABC-emulator-x._adb-tls-connect._tcp"
        ));
        // a hardware serial entry still matches inside a wireless adb name
        assert!(matches_adb_serial(
            "ABC123",
            "adb-ABC123-xyz._adb-tls-connect._tcp"
        ));
        assert!(!matches_adb_serial(
            "ABC123",
            "adb-OTHER-xyz._adb-tls-connect._tcp"
        ));
        // case is ignored, and an entry is never a bare substring
        assert!(matches_adb_serial(
            "abc123",
            "adb-ABC123-xyz._adb-tls-connect._tcp"
        ));
        assert!(matches_adb_serial("ABC123", "abc123"));
        assert!(
            !matches_adb_serial("ABC123", "xABC123"),
            "a serial that merely contains the entry is not covered"
        );
        assert!(!matches_adb_serial("emulator", "my-emulator-5554"));
        assert!(!matches_adb_serial(
            "_tcp",
            "adb-ABC123-xyz._adb-tls-connect._tcp"
        ));
        assert!(!matches_adb_serial(
            "192.168.1.99:5555",
            "192.168.1.99:55551"
        ));
    }

    #[test]
    fn read_only_poco_refuses_every_acting_call_and_nothing_reaches_the_phone() {
        let mut c = core();
        c.set_mode(POCO_SERIAL, DeviceMode::ReadOnly);
        let mut refused = 0;
        for call in Call::samples().into_iter().filter(Call::is_acting) {
            match c.call(None, &call) {
                Err(CoreError::Refused(Refusal::ReadOnly { tool })) => {
                    assert_eq!(tool, call.name());
                    refused += 1;
                }
                other => panic!("{} was not refused: {other:?}", call.name()),
            }
        }
        assert_eq!(refused, 10);
        assert!(c.transport.acted.is_empty());
        // reads still work on a read_only device
        assert!(c.call(None, &Call::Screenshot { max_width: 720 }).is_ok());
        assert!(c
            .call(
                None,
                &Call::UiTree {
                    interactive_only: false,
                    require: vec![],
                    timeout_ms: None
                }
            )
            .is_ok());
        assert!(c.transport.acted.is_empty());
    }

    #[test]
    fn s24_is_refused_even_when_configured_full_and_named_any_way() {
        let mut c = core();
        c.set_mode(S24, DeviceMode::Full);
        c.set_allow(&[POCO_SERIAL, S24]);
        for name in [S24, FAKE_S24_ADB] {
            for call in Call::samples()
                .into_iter()
                .filter(|k| !matches!(k, Call::ListDevices | Call::Wait { .. }))
            {
                assert!(
                    matches!(
                        c.call(Some(name), &call),
                        Err(CoreError::Refused(Refusal::S24 { .. }))
                    ),
                    "{name} {}",
                    call.name()
                );
            }
        }
        assert!(!c.transport.touched());
        // not even connected: still refused, not "no such device"
        c.transport.s24_connected = false;
        assert!(matches!(
            c.call(Some(S24), &Call::Unlock),
            Err(CoreError::Refused(Refusal::S24 { .. }))
        ));
        // and the default pick never lands on it
        let o = c.call(None, &Call::Screenshot { max_width: 720 }).unwrap();
        assert!(o.shot.is_some());
    }

    #[test]
    fn getprop_runs_trimmed_and_bad_keys_never_reach_the_phone() {
        let mut c = core();
        let o = c
            .call(
                None,
                &Call::Getprop {
                    key: "ro.product.model".into(),
                },
            )
            .unwrap();
        assert_eq!(o.text, "value-of-ro.product.model");
        assert_eq!(
            c.transport.acted,
            vec!["getprop ro.product.model".to_string()]
        );
        for key in ["", "ro.x;reboot", "a b", &"a".repeat(65)] {
            let before = c.transport.acted.len();
            assert!(
                matches!(
                    c.call(None, &Call::Getprop { key: key.into() }),
                    Err(CoreError::Invalid(_))
                ),
                "{key:?}"
            );
            assert_eq!(c.transport.acted.len(), before, "{key:?}");
        }
    }

    #[test]
    fn s24_answers_only_its_serial() {
        let mut c = core();
        c.set_allow(&[POCO_SERIAL, S24]);
        let ok = c.call(
            Some(S24),
            &Call::Getprop {
                key: "ro.serialno".into(),
            },
        );
        assert!(ok.is_ok(), "{ok:?}");
        let bad = c.call(
            Some(S24),
            &Call::Getprop {
                key: "ro.product.brand".into(),
            },
        );
        assert!(matches!(bad, Err(CoreError::Refused(Refusal::S24 { .. }))));
    }

    #[test]
    fn helper_mode_refuses_adb_only_calls() {
        let mut c = core();
        c.set_mode(FAKE_POCO_ADB, DeviceMode::Helper);
        assert!(matches!(
            c.call(None, &Call::Unlock),
            Err(CoreError::Refused(Refusal::NeedsAdbShell { .. }))
        ));
        assert!(c
            .call(
                None,
                &Call::Key {
                    name: "back".into()
                }
            )
            .is_ok());
    }

    #[test]
    fn tap_by_selector_discloses_the_resolution() {
        let mut c = core();
        let o = c
            .call(
                None,
                &Call::Tap {
                    target: Target::Select(Selector {
                        id: Some("nope".into()),
                        desc: Some("Wi-Fi".into()),
                        ..Default::default()
                    }),
                },
            )
            .unwrap();
        assert_eq!(o.text.lines().next().unwrap(), "tapped device (610,880) from matched by desc=\"Wi-Fi\": unique, [2] \"Wi-Fi\"; earlier keys with no match: id=\"nope\"");
        let r = o.resolution.unwrap();
        assert_eq!(r.matched_by, crate::selector::Kind::Desc);
        assert_eq!(c.transport.acted.len(), 1);
        // an ambiguous selector touches nothing
        c.transport.xml_override = Some(
            r#"<hierarchy rotation="0"><node class="a.B" text="Open mail" clickable="true" bounds="[0,0][100,100]"/><node class="a.B" text="Open maps" clickable="true" bounds="[0,200][100,300]"/></hierarchy>"#.to_string(),
        );
        let before = c.transport.acted.len();
        let e = c
            .call(
                None,
                &Call::Tap {
                    target: Target::Select(Selector::text("open")),
                },
            )
            .unwrap_err();
        assert!(matches!(e, CoreError::Ambiguous(_)), "{e}");
        assert_eq!(c.transport.acted.len(), before);
        // a rel point is last and lands inside the frame
        let o = c
            .call(
                None,
                &Call::Tap {
                    target: Target::Select(Selector::rel(1.0, 1.0)),
                },
            )
            .unwrap();
        assert!(o.text.contains("tapped device (1219,2711)"), "{}", o.text);
    }

    /// Three rows of a list at `top` pixels, each a clickable row holding a title and a "Star" button.
    /// (It was "Delete" before D3; the screen guard now asks before any Delete, so the ref tests use a
    /// harmless button.)
    fn rows_screen(first: usize, top: i32) -> String {
        let mut x = String::from("<hierarchy rotation=\"0\"><node class=\"androidx.recyclerview.widget.RecyclerView\" resource-id=\"a:id/list\" scrollable=\"true\" bounds=\"[0,0][1000,2000]\">");
        for (n, k) in (first..first + 3).enumerate() {
            let t = top + 300 * n as i32;
            x += &format!(
                "<node class=\"a.Row\" resource-id=\"a:id/row\" clickable=\"true\" bounds=\"[0,{t}][1000,{}]\"><node class=\"a.T\" text=\"Item {k}\" bounds=\"[0,{t}][500,{}]\"/><node class=\"a.B\" text=\"Star\" resource-id=\"a:id/star\" clickable=\"true\" bounds=\"[600,{t}][900,{}]\"/></node>",
                t + 280,
                t + 100,
                t + 100
            );
        }
        x + "</node></hierarchy>"
    }

    #[test]
    fn a_ref_from_the_listing_still_taps_the_same_row_after_the_list_scrolled() {
        let mut c = core();
        c.transport.xml_override = Some(rows_screen(1, 0));
        let o = c
            .call(
                None,
                &Call::UiTree {
                    interactive_only: true,
                    require: vec![],
                    timeout_ms: None,
                },
            )
            .unwrap();
        assert!(o.text.contains("snapshot s1"), "{}", o.text);
        // the second row's Star button (the listing is interactive elements only: row, button, row, button, ...)
        let lines: Vec<&str> = o.text.lines().filter(|l| l.contains("id=star")).collect();
        assert_eq!(lines.len(), 3, "{}", o.text);
        let r = lines[1].rsplit(' ').next().unwrap().to_string();
        assert!(r.starts_with('r') && r.len() == 7, "{r}");
        // the list scrolls by one row (and a half): the same row is now first, higher up
        c.transport.xml_override = Some(rows_screen(2, -150));
        let o = c
            .call(
                None,
                &Call::Tap {
                    target: Target::Select(Selector::ref_id(&r)),
                },
            )
            .unwrap();
        assert!(o.text.contains("matched by ref="), "{}", o.text);
        // Star of "Item 2" is at x 750, y -150 + 50: the row that was second is now first and above where it was
        assert!(o.text.contains("tapped device (750,"), "{}", o.text);
        let acted = c.transport.acted.last().unwrap().clone();
        assert!(acted.contains("750"), "{acted}");
        // a ref that is not on screen touches nothing
        let before = c.transport.acted.len();
        let e = c
            .call(
                None,
                &Call::Tap {
                    target: Target::Select(Selector::ref_id("r000000")),
                },
            )
            .unwrap_err();
        assert!(matches!(e, CoreError::Resolve(_)), "{e}");
        assert_eq!(c.transport.acted.len(), before);
    }

    #[test]
    fn diff_since_lists_only_what_changed_and_says_when_it_cannot() {
        let mut c = core();
        c.transport.xml_override = Some(rows_screen(1, 0));
        let o = c
            .call(
                None,
                &Call::UiTree {
                    interactive_only: false,
                    require: vec![],
                    timeout_ms: None,
                },
            )
            .unwrap();
        let full_len = o.text.len();
        assert!(o.text.contains("snapshot s1"));
        // nothing read yet since s1
        assert!(c
            .diff_since(FAKE_POCO_ADB, "s1")
            .unwrap()
            .contains("nothing read since"));
        c.transport.xml_override = Some(rows_screen(2, 0)); // one row off the top, one on at the bottom
        let o2 = c
            .call(
                None,
                &Call::UiTree {
                    interactive_only: false,
                    require: vec![],
                    timeout_ms: None,
                },
            )
            .unwrap();
        assert!(o2.text.contains("snapshot s2"));
        let d = c.diff_since(FAKE_POCO_ADB, "s1").unwrap();
        assert!(
            d.starts_with("snapshot s2 since s1: 3 added, 0 changed, 3 removed, 7 unchanged"),
            "{d}"
        );
        assert!(
            d.lines()
                .any(|l| l.starts_with("+ ") && l.contains("Item 4")),
            "{d}"
        );
        assert!(
            d.lines()
                .any(|l| l.starts_with("- ") && l.contains("Item 1")),
            "{d}"
        );
        assert!(d.len() < full_len, "{} vs {}", d.len(), full_len);
        // an unknown snapshot, and one read under another view, are errors that say so
        let e = c.diff_since(FAKE_POCO_ADB, "s99").unwrap_err();
        assert!(e.to_string().contains("no snapshot"), "{e}");
        c.call(
            None,
            &Call::UiTree {
                interactive_only: true,
                require: vec![],
                timeout_ms: None,
            },
        )
        .unwrap();
        let e = c.diff_since(FAKE_POCO_ADB, "s2").unwrap_err();
        assert!(e.to_string().contains("interactive_only"), "{e}");
        // the ref the diff gives an added button is the one the new listing prints for it
        let added = d
            .lines()
            .find(|l| l.starts_with("+ ") && l.contains("Star"))
            .expect("an added Star button");
        let r = added.rsplit(' ').next().unwrap();
        assert!(r.starts_with('r') && r.len() == 7, "{added}");
        assert!(
            o2.text
                .lines()
                .any(|l| l.contains("Star") && l.ends_with(r)),
            "{r} in {}",
            o2.text
        );
    }

    #[test]
    fn require_on_a_read_returns_the_missing_selectors() {
        let mut c = core();
        let ok = c
            .call(
                None,
                &Call::UiTree {
                    interactive_only: false,
                    require: vec![Selector::desc("Search")],
                    timeout_ms: Some(1000),
                },
            )
            .unwrap();
        assert!(ok.required.as_ref().is_some_and(Required::is_met));
        assert!(ok.text.contains("require met"));
        let want = vec![
            Selector::desc("Search"),
            Selector::text("Checkout"),
            Selector::id("cart"),
        ];
        let t0 = c.clock().now_ms();
        match c.call(
            None,
            &Call::UiTree {
                interactive_only: false,
                require: want,
                timeout_ms: Some(1000),
            },
        ) {
            Err(CoreError::RequireTimeout { missing, detail }) => {
                assert_eq!(
                    missing,
                    vec![Selector::text("Checkout"), Selector::id("cart")]
                );
                assert!(detail.contains("Checkout") && detail.contains("cart"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(c.clock().now_ms() - t0, 1000);
    }

    #[test]
    fn settle_through_core_uses_the_planned_contract() {
        let mut c = core();
        match c.settle(None, SettleOpts::default()).unwrap() {
            Settle::Settled {
                polls, elapsed_ms, ..
            } => assert_eq!((polls, elapsed_ms), (2, 200)), // one poll interval between two reads
            other => panic!("{other:?}"),
        }
        assert!(c.clock().now_ms() >= 200);
    }

    #[test]
    fn index_taps_need_a_tree_and_a_valid_index() {
        let mut c = core();
        let e = c
            .call(
                None,
                &Call::Tap {
                    target: Target::Index(0),
                },
            )
            .unwrap_err();
        assert!(e.to_string().contains("No UI tree yet"));
        c.call(
            None,
            &Call::UiTree {
                interactive_only: false,
                require: vec![],
                timeout_ms: None,
            },
        )
        .unwrap();
        let e = c
            .call(
                None,
                &Call::Tap {
                    target: Target::Index(99),
                },
            )
            .unwrap_err();
        assert!(e.to_string().contains("out of range"), "{e}");
        assert!(c
            .call(
                None,
                &Call::Tap {
                    target: Target::Index(1)
                }
            )
            .is_ok());
    }

    #[test]
    fn point_taps_use_the_latest_screenshot_scale_and_clamp() {
        let mut c = core();
        c.call(None, &Call::Screenshot { max_width: 480 }).unwrap();
        let o = c
            .call(
                None,
                &Call::Tap {
                    target: Target::Point {
                        x: 360.0,
                        y: 1400.0,
                        space: None,
                        image_width: None,
                    },
                },
            )
            .unwrap();
        assert!(
            o.text.contains("tapped device (915,2711)")
                && o.text.contains("CLAMPED")
                && o.text.contains("image px"),
            "{}",
            o.text
        );
    }

    #[test]
    fn typing_ascii_and_unicode_routes() {
        let mut c = core();
        c.call(
            None,
            &Call::UiTree {
                interactive_only: false,
                require: vec![],
                timeout_ms: None,
            },
        )
        .unwrap();
        let o = c
            .call(
                None,
                &Call::TypeText {
                    text: "a b\ncafé".into(),
                    index: Some(1),
                    submit: true,
                },
            )
            .unwrap();
        assert_eq!(
            o.text,
            "typed 8 characters via helper set_text, input text and pressed Enter"
        );
        c.transport.helper_installed = false;
        let e = c
            .call(
                None,
                &Call::TypeText {
                    text: "東京".into(),
                    index: None,
                    submit: false,
                },
            )
            .unwrap_err();
        assert!(e.to_string().contains("needs the Artemis helper"));
    }

    #[test]
    fn open_app_matching() {
        let mut c = core();
        assert_eq!(
            c.call(
                None,
                &Call::OpenApp {
                    package: "vending".into()
                }
            )
            .unwrap()
            .text,
            "opened com.android.vending"
        );
        let e = c
            .call(
                None,
                &Call::OpenApp {
                    package: "google.android.apps".into(),
                },
            )
            .unwrap_err();
        assert!(e.to_string().contains("matches several apps"));
        assert!(c
            .call(
                None,
                &Call::OpenApp {
                    package: "zzz".into()
                }
            )
            .is_err());
    }

    const S24: &str = agentroid_adb_link::guard::S24_SERIAL;

    #[test]
    fn the_screen_lock_refuses_acting_calls_and_shell_lines_on_a_phone_in_use_and_never_reads() {
        use crate::askscreen::Signals;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        const T0: u64 = 1_800_000_000_000;
        let on = |focus: &str, ime: bool, idle_ms: u64| Signals {
            wakefulness: Some("Awake".into()),
            idle_ms: Some(idle_ms),
            screen_kept_on: Some(false),
            focus: Some(focus.into()),
            ime_shown: Some(ime),
            keyguard: Some(false),
            in_call: Some(false),
        };
        let phone = Arc::new(Mutex::new(on("com.anthropic.claude", true, 1_000)));
        let clock = Arc::new(AtomicU64::new(T0));
        let dir = std::env::temp_dir().join(format!("agentroid-core-lock-{}", std::process::id()));
        let mut c = core();
        c.set_screen_lock(test_lock(&dir, &phone, &clock));
        let back = Call::Key {
            name: "back".into(),
        };
        // in use: refused, and nothing reached the phone
        let r = c.call(None, &back);
        match &r {
            Err(CoreError::Refused(Refusal::ScreenLock { tool: "key", why })) => {
                assert!(why.contains("agentroid ask-screen"), "{why}")
            }
            other => panic!("expected a screen-lock refusal, got {other:?}"),
        }
        assert!(c.transport.acted.is_empty(), "{:?}", c.transport.acted);
        assert!(r.unwrap_err().to_string().contains("screen lock"));
        // a script's `input tap` is refused too; a read-only line is not locked
        assert!(matches!(
            c.shell_gate(None, "input tap 10 20"),
            Err(CoreError::Refused(Refusal::ScreenLock {
                tool: "shell",
                ..
            }))
        ));
        assert!(c.shell_gate(None, "getprop ro.build.version.sdk").is_ok());
        // reads still answer
        assert!(c
            .call(
                None,
                &Call::Getprop {
                    key: "ro.serialno".into()
                }
            )
            .is_ok());
        assert!(c.call(None, &Call::ListDevices).is_ok());
        let n = c.transport.acted.len();
        // put down: allowed, and the key reaches the phone
        *phone.lock().unwrap() = Signals {
            wakefulness: Some("Asleep".into()),
            in_call: Some(false),
            ..Signals::default()
        };
        assert!(c.call(None, &back).is_ok());
        assert_eq!(c.transport.acted.len(), n + 1);
        // its own key woke the phone and dated the last touch: the burst goes on
        clock.store(T0 + 3_000, Ordering::SeqCst);
        *phone.lock().unwrap() = on("com.whatsapp", true, 3_000);
        assert!(c.call(None, &back).is_ok());
        // someone touches it 500 ms ago, after the agent's last key: refused again
        clock.store(T0 + 6_000, Ordering::SeqCst);
        *phone.lock().unwrap() = on("com.whatsapp", true, 500);
        assert!(matches!(
            c.call(None, &back),
            Err(CoreError::Refused(Refusal::ScreenLock { .. }))
        ));
        assert_eq!(c.transport.acted.len(), n + 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A screen lock over a fake phone; one fake clock serves as both the wall and the monotonic
    /// clock, and the power read is instant.
    fn test_lock(
        dir: &std::path::Path,
        phone: &std::sync::Arc<std::sync::Mutex<crate::askscreen::Signals>>,
        clock: &std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> ScreenLock {
        use std::sync::atomic::Ordering;
        let (p, k1, k2) = (phone.clone(), clock.clone(), clock.clone());
        ScreenLock::new(
            dir.to_path_buf(),
            Box::new(move |_, mono| {
                let at = mono();
                Some(crate::screenlock::Reading {
                    signals: p.lock().unwrap().clone(),
                    power_from: at,
                    power_to: at,
                })
            }),
            Box::new(move || k1.load(Ordering::SeqCst)),
            Box::new(move || k2.load(Ordering::SeqCst)),
        )
    }

    #[test]
    fn a_shell_line_counts_as_the_agents_own_only_once_it_was_allowed_and_has_run() {
        use crate::approval::{Approver, FakeChannel, Inbound};
        use crate::askscreen::Signals;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        const T0: u64 = 1_800_000_000_000;
        let on = |idle_ms: u64| Signals {
            wakefulness: Some("Awake".into()),
            idle_ms: Some(idle_ms),
            screen_kept_on: Some(false),
            focus: Some("com.whatsapp".into()),
            ime_shown: Some(true),
            keyguard: Some(false),
            in_call: Some(false),
        };
        let phone = Arc::new(Mutex::new(Signals {
            wakefulness: Some("Asleep".into()),
            in_call: Some(false),
            ..Signals::default()
        }));
        let clock = Arc::new(AtomicU64::new(T0));
        let dir =
            std::env::temp_dir().join(format!("agentroid-core-lock-sh-{}", std::process::id()));
        let mut c = core();
        c.set_screen_lock(test_lock(&dir, &phone, &clock));
        let back = Call::Key {
            name: "back".into(),
        };
        // free: the key goes; its end (T0) starts the burst
        assert!(c.call(None, &back).is_ok());
        // inside the burst, a line the shell guard refuses outright (input injection): the lock
        // admitted it, but it never ran, so it gives no ticket and is not the agent's own activity
        clock.store(T0 + 2_000, Ordering::SeqCst);
        *phone.lock().unwrap() = on(2_000);
        assert!(matches!(
            c.shell_gate(None, "input tap 10 20"),
            Err(CoreError::Refused(Refusal::Screen { .. }))
        ));
        // someone touches it at T0 + 3000: that touch came after the key, refused
        clock.store(T0 + 4_000, Ordering::SeqCst);
        *phone.lock().unwrap() = on(1_000);
        assert!(matches!(
            c.shell_gate(None, "input tap 10 20"),
            Err(CoreError::Refused(Refusal::ScreenLock { .. }))
        ));
        // an allowed line (a human says yes) that has run: its end continues the burst
        let ch = FakeChannel {
            answer_with: Some(Box::new(|r, _| {
                vec![Inbound {
                    event_id: format!("e-{}", r.id),
                    time_ms: r.created_ms + 1,
                    message: format!("allow {}", r.id),
                }]
            })),
            ..FakeChannel::default()
        };
        c.set_approver(Approver::new(Box::new(ch)));
        clock.store(T0 + 10_000, Ordering::SeqCst);
        *phone.lock().unwrap() = Signals {
            wakefulness: Some("Asleep".into()),
            in_call: Some(false),
            ..Signals::default()
        };
        assert!(c.call(None, &back).is_ok());
        clock.store(T0 + 12_000, Ordering::SeqCst);
        *phone.lock().unwrap() = on(2_000);
        let ticket = match c.shell_gate(None, "some_unknown_binary --flag") {
            Ok(Some(t)) => t,
            other => panic!("{other:?}"),
        };
        clock.store(T0 + 15_000, Ordering::SeqCst);
        c.lock_done(ticket);
        // the line ended at T0 + 15000 and its own touch was at T0 + 14000: the burst goes on
        clock.store(T0 + 17_000, Ordering::SeqCst);
        *phone.lock().unwrap() = on(3_000);
        let r = c.shell_gate(None, "some_unknown_binary --flag");
        assert!(r.is_ok(), "{r:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_segment_needs_the_lock_and_its_end_continues_the_burst() {
        use crate::askscreen::Signals;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        const T0: u64 = 1_800_000_000_000;
        let on = |idle_ms: u64| Signals {
            wakefulness: Some("Awake".into()),
            idle_ms: Some(idle_ms),
            screen_kept_on: Some(false),
            focus: Some("com.whatsapp".into()),
            ime_shown: Some(true),
            keyguard: Some(false),
            in_call: Some(false),
        };
        let phone = Arc::new(Mutex::new(on(1_000)));
        let clock = Arc::new(AtomicU64::new(T0));
        let dir =
            std::env::temp_dir().join(format!("agentroid-core-lock-seg-{}", std::process::id()));
        let mut c = core();
        // no lock: allowed, with nothing to report
        assert!(matches!(c.lock_admit(None, "link.segment"), Ok(None)));
        c.set_screen_lock(test_lock(&dir, &phone, &clock));
        match c.lock_admit(None, "link.segment") {
            Err(CoreError::Refused(Refusal::ScreenLock {
                tool: "link.segment",
                ..
            })) => {}
            other => panic!("{other:?}"),
        }
        // put down: allowed; the segment's answer at T0 + 1000 starts the burst
        *phone.lock().unwrap() = Signals {
            wakefulness: Some("Asleep".into()),
            in_call: Some(false),
            ..Signals::default()
        };
        let t = c.lock_admit(None, "link.segment").unwrap().unwrap();
        clock.store(T0 + 1_000, Ordering::SeqCst);
        c.lock_done(t);
        clock.store(T0 + 4_000, Ordering::SeqCst);
        *phone.lock().unwrap() = on(3_000);
        assert!(matches!(c.lock_admit(None, "link.segment"), Ok(Some(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A screen-on phone with an app and the keyboard up, last touched `idle_ms` ago.
    fn busy(idle_ms: u64) -> crate::askscreen::Signals {
        crate::askscreen::Signals {
            wakefulness: Some("Awake".into()),
            idle_ms: Some(idle_ms),
            screen_kept_on: Some(false),
            focus: Some("com.whatsapp".into()),
            ime_shown: Some(true),
            keyguard: Some(false),
            in_call: Some(false),
        }
    }

    fn put_down() -> crate::askscreen::Signals {
        crate::askscreen::Signals {
            wakefulness: Some("Asleep".into()),
            in_call: Some(false),
            ..crate::askscreen::Signals::default()
        }
    }

    #[test]
    fn a_stale_ticket_never_turns_a_humans_touch_into_the_agents_own() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        const T0: u64 = 1_800_000_000_000;
        let phone = Arc::new(Mutex::new(put_down()));
        let clock = Arc::new(AtomicU64::new(T0));
        let dir =
            std::env::temp_dir().join(format!("agentroid-core-lock-stale-{}", std::process::id()));
        let mut c = core();
        c.set_screen_lock(test_lock(&dir, &phone, &clock));
        let back = Call::Key {
            name: "back".into(),
        };
        let at = |t: u64, s: crate::askscreen::Signals| {
            clock.store(t, Ordering::SeqCst);
            *phone.lock().unwrap() = s;
        };
        let refused = |r: Result<Outcome, CoreError>| {
            matches!(r, Err(CoreError::Refused(Refusal::ScreenLock { .. })))
        };
        // the review's probe: a real key at T0 starts the burst
        at(T0, put_down());
        assert!(c.call(None, &back).is_ok());
        // a segment admitted at T0 + 1 s (its touch is the key's) fails before it runs: the
        // link drops its ticket
        at(T0 + 1_000, busy(1_000));
        let seg = c.lock_admit(None, "link.segment").unwrap();
        assert!(seg.is_some());
        drop(seg);
        // a human touches it at T0 + 20 s; at T0 + 21 s a read-only shell line runs: it has no
        // ticket, so there is nothing for it to report
        at(T0 + 21_000, busy(1_000));
        assert!(matches!(
            c.shell_gate(None, "getprop ro.build.version.sdk"),
            Ok(None)
        ));
        // at T0 + 22 s the human's touch is still nobody's but theirs: refused
        at(T0 + 22_000, busy(2_000));
        assert!(refused(c.call(None, &back)));
        // a ticket given back after newer work on the phone records nothing (a shell gate that
        // timed out and answered late; a segment whose answer came after another call)
        for newer in 0..4u64 {
            let base = T0 + 40_000 * (newer + 1);
            at(base, put_down());
            assert!(c.call(None, &back).is_ok());
            at(base + 1_000, busy(1_000));
            let late = c.lock_admit(None, "link.segment").unwrap().unwrap();
            match newer {
                0 => assert!(matches!(
                    c.shell_gate(None, "getprop ro.build.version.sdk"),
                    Ok(None)
                )),
                1 => assert!(c
                    .call(
                        None,
                        &Call::Getprop {
                            key: "ro.serialno".into()
                        }
                    )
                    .is_ok()),
                2 => {
                    // a second admission replaces the first; only the newer ticket counts
                    let _newer = c.lock_admit(None, "link.segment").unwrap().unwrap();
                }
                _ => {
                    // ... also when it names the same phone another way
                    let _newer = c
                        .lock_admit(Some(crate::fake::FAKE_POCO_ADB), "link.segment")
                        .unwrap()
                        .unwrap();
                }
            }
            // the human at base + 20 s; the late ticket comes back at base + 21 s
            at(base + 21_000, busy(1_000));
            c.lock_done(late);
            at(base + 22_000, busy(2_000));
            assert!(refused(c.call(None, &back)), "newer work {newer}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_ticket_given_back_after_its_work_ran_carries_the_burst_on() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        const T0: u64 = 1_800_000_000_000;
        let phone = Arc::new(Mutex::new(put_down()));
        let clock = Arc::new(AtomicU64::new(T0));
        let dir =
            std::env::temp_dir().join(format!("agentroid-core-lock-ticket-{}", std::process::id()));
        let mut c = core();
        c.set_screen_lock(test_lock(&dir, &phone, &clock));
        let back = Call::Key {
            name: "back".into(),
        };
        assert!(c.call(None, &back).is_ok());
        // the segment runs from T0 + 1 s to T0 + 21 s; its own touch is at T0 + 20.9 s
        clock.store(T0 + 1_000, Ordering::SeqCst);
        *phone.lock().unwrap() = busy(1_000);
        let seg = c.lock_admit(None, "link.segment").unwrap().unwrap();
        clock.store(T0 + 21_000, Ordering::SeqCst);
        c.lock_done(seg);
        clock.store(T0 + 22_000, Ordering::SeqCst);
        *phone.lock().unwrap() = busy(1_100);
        assert!(c.call(None, &back).is_ok());
        // the same for a second admission: the newer ticket counts
        clock.store(T0 + 23_000, Ordering::SeqCst);
        *phone.lock().unwrap() = busy(1_000);
        let _old = c.lock_admit(None, "link.segment").unwrap().unwrap();
        let new = c.lock_admit(None, "link.segment").unwrap().unwrap();
        clock.store(T0 + 40_000, Ordering::SeqCst);
        c.lock_done(new);
        clock.store(T0 + 41_000, Ordering::SeqCst);
        *phone.lock().unwrap() = busy(1_100);
        assert!(c.call(None, &back).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_call_a_lease_let_through_never_starts_or_carries_on_a_burst() {
        use crate::askscreen::{save_memory, Memory};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Mutex};
        const T0: u64 = 1_800_000_000_000;
        let phone = Arc::new(Mutex::new(busy(1_000)));
        let clock = Arc::new(AtomicU64::new(T0));
        let dir =
            std::env::temp_dir().join(format!("agentroid-core-lock-lease-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = Memory::default();
        m.leases.insert("0123456789abcdef".into(), T0 + 60_000);
        save_memory(&dir, POCO_SERIAL, &m).unwrap();
        let mut c = core();
        c.set_screen_lock(test_lock(&dir, &phone, &clock));
        let back = Call::Key {
            name: "back".into(),
        };
        // the review's probe: the phone in use, the agent acts under the lease at T0 + 59 s
        clock.store(T0 + 59_000, Ordering::SeqCst);
        assert!(c.call(None, &back).is_ok());
        // a segment under the lease gives no ticket
        assert!(matches!(c.lock_admit(None, "link.segment"), Ok(None)));
        // at T0 + 120 s the lease is over; an app and the keyboard are up and the agent's touch
        // (T0 + 59 s) is the last one: in use, not a burst
        clock.store(T0 + 120_000, Ordering::SeqCst);
        *phone.lock().unwrap() = busy(61_000);
        match c.call(None, &back) {
            Err(CoreError::Refused(Refusal::ScreenLock { why, .. })) => {
                assert!(why.contains("in use"), "{why}")
            }
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
