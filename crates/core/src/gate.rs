//! The tool calls and the device-mode gate (task 2.2, item 4: "`safety` in Phase 2 is
//! this task's device-mode gate (`full` / `helper` / `read_only`)").
//!
//! * `read_only`: refuses every acting call. Reads (screenshot, tree, look, app list) are allowed.
//! * `helper`: acting calls are allowed only when they have a helper route (gestures, keys, typing
//!   through the accessibility helper); calls that need `adb shell` (`unlock`, `open_app`,
//!   `open_url`, `start_activity`) are refused. ASSUMPTION: the plan names the mode but not its
//!   exact scope; this is the narrowest reading and is easy to widen.
//! * `full`: everything.
//! * The protected phone is always `read_only`, and never a *target*, with one exception: the
//!   "serial + approved probes" rule. It answers only a call whose shell form is exactly
//!   an entry of adb-link's `S24_READS` (`getprop ro.serialno` and the 10 approved probes); every other call that names
//!   it is refused, reads included, in every mode and every spelling of its name.

use crate::selector::Selector;
use agentroid_adb_link::guard::{S24_READS, S24_SERIAL};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceMode {
    #[default]
    Full,
    Helper,
    ReadOnly,
}

impl DeviceMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeviceMode::Full => "full",
            DeviceMode::Helper => "helper",
            DeviceMode::ReadOnly => "read_only",
        }
    }
    pub fn parse(s: &str) -> Option<DeviceMode> {
        match s {
            "full" => Some(DeviceMode::Full),
            "helper" => Some(DeviceMode::Helper),
            "read_only" => Some(DeviceMode::ReadOnly),
            _ => None,
        }
    }
}

/// True when an adb serial, hardware serial or mDNS name is the protected phone's.
pub fn is_s24(id: &str) -> bool {
    id.to_ascii_uppercase().contains(S24_SERIAL)
}

/// The mode a device really runs in: whatever was configured, except the protected phone, which is always `read_only`.
pub fn effective_mode(ids: &[&str], configured: DeviceMode) -> DeviceMode {
    if ids.iter().any(|i| is_s24(i)) {
        DeviceMode::ReadOnly
    } else {
        configured
    }
}

/// Where a tap or a point comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// Index in the latest tree.
    Index(usize),
    Point {
        x: f64,
        y: f64,
        space: Option<String>,
        image_width: Option<i32>,
    },
    /// Resolved by the shared matcher against a fresh tree.
    Select(Selector),
}

/// Every tool call the core serves.
#[derive(Debug, Clone, PartialEq)]
pub enum Call {
    // reads
    ListDevices,
    Screenshot {
        max_width: i32,
    },
    UiTree {
        interactive_only: bool,
        require: Vec<Selector>,
        timeout_ms: Option<u64>,
    },
    Look {
        max_width: i32,
    },
    ListApps {
        filter: String,
    },
    Wait {
        seconds: f64,
    },
    /// `adb shell getprop <key>`; the key is validated by [`valid_prop_key`].
    Getprop {
        key: String,
    },
    // acting
    Tap {
        target: Target,
    },
    LongPress {
        target: Target,
        ms: u32,
    },
    Swipe {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        ms: u32,
        space: Option<String>,
        image_width: Option<i32>,
    },
    Scroll {
        direction: String,
    },
    TypeText {
        text: String,
        index: Option<usize>,
        submit: bool,
    },
    Key {
        name: String,
    },
    Unlock,
    OpenApp {
        package: String,
    },
    OpenUrl {
        uri: String,
        package: Option<String>,
    },
    StartActivity {
        component: String,
        action: Option<String>,
        data: Option<String>,
    },
}

pub const READ_TOOLS: [&str; 7] = [
    "list_devices",
    "screenshot",
    "ui_tree",
    "look",
    "list_apps",
    "wait",
    "getprop",
];

/// A property key the transport may pass to `getprop`: `[A-Za-z0-9._-]`, 1 to 64 characters.
pub fn valid_prop_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}
pub const ACTING_TOOLS: [&str; 10] = [
    "tap",
    "long_press",
    "swipe",
    "scroll",
    "type_text",
    "key",
    "unlock",
    "open_app",
    "open_url",
    "start_activity",
];

impl Call {
    pub fn name(&self) -> &'static str {
        match self {
            Call::ListDevices => "list_devices",
            Call::Screenshot { .. } => "screenshot",
            Call::UiTree { .. } => "ui_tree",
            Call::Look { .. } => "look",
            Call::ListApps { .. } => "list_apps",
            Call::Wait { .. } => "wait",
            Call::Getprop { .. } => "getprop",
            Call::Tap { .. } => "tap",
            Call::LongPress { .. } => "long_press",
            Call::Swipe { .. } => "swipe",
            Call::Scroll { .. } => "scroll",
            Call::TypeText { .. } => "type_text",
            Call::Key { .. } => "key",
            Call::Unlock => "unlock",
            Call::OpenApp { .. } => "open_app",
            Call::OpenUrl { .. } => "open_url",
            Call::StartActivity { .. } => "start_activity",
        }
    }

    /// Does the call change anything on the phone? The match is exhaustive on purpose: a new call
    /// cannot be added without deciding this.
    pub fn is_acting(&self) -> bool {
        match self {
            Call::ListDevices
            | Call::Screenshot { .. }
            | Call::UiTree { .. }
            | Call::Look { .. }
            | Call::ListApps { .. }
            | Call::Wait { .. }
            | Call::Getprop { .. } => false,
            Call::Tap { .. }
            | Call::LongPress { .. }
            | Call::Swipe { .. }
            | Call::Scroll { .. }
            | Call::TypeText { .. }
            | Call::Key { .. }
            | Call::Unlock
            | Call::OpenApp { .. }
            | Call::OpenUrl { .. }
            | Call::StartActivity { .. } => true,
        }
    }

    /// Can the helper link carry this acting call (gestures, keys, typing)?
    pub fn has_helper_route(&self) -> bool {
        matches!(
            self,
            Call::Tap { .. }
                | Call::LongPress { .. }
                | Call::Swipe { .. }
                | Call::Scroll { .. }
                | Call::TypeText { .. }
                | Call::Key { .. }
        )
    }

    /// One of each call, for tests and for the selftest's "S24 refused for every tool".
    pub fn samples() -> Vec<Call> {
        vec![
            Call::ListDevices,
            Call::Screenshot { max_width: 720 },
            Call::UiTree {
                interactive_only: false,
                require: vec![],
                timeout_ms: None,
            },
            Call::Look { max_width: 720 },
            Call::ListApps {
                filter: String::new(),
            },
            Call::Wait { seconds: 0.0 },
            // not an S24 read, so the protected phone refuses this sample like every other
            Call::Getprop {
                key: "ro.product.brand".into(),
            },
            Call::Tap {
                target: Target::Point {
                    x: 10.0,
                    y: 10.0,
                    space: Some("device".into()),
                    image_width: None,
                },
            },
            Call::LongPress {
                target: Target::Index(0),
                ms: 800,
            },
            Call::Swipe {
                x1: 1.0,
                y1: 1.0,
                x2: 2.0,
                y2: 2.0,
                ms: 300,
                space: Some("device".into()),
                image_width: None,
            },
            Call::Scroll {
                direction: "down".into(),
            },
            Call::TypeText {
                text: "hi".into(),
                index: None,
                submit: false,
            },
            Call::Key {
                name: "back".into(),
            },
            Call::Unlock,
            Call::OpenApp {
                package: "com.android.settings".into(),
            },
            Call::OpenUrl {
                uri: "https://example.com".into(),
                package: None,
            },
            Call::StartActivity {
                component: "com.android.settings/.Settings".into(),
                action: None,
                data: None,
            },
        ]
    }
}

/// Why a call was refused before it reached the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The protected phone is never a target (any call).
    S24 { tool: &'static str },
    /// Not on the allowlist.
    NotAllowed { serial: String },
    /// `read_only` device and an acting call.
    ReadOnly { tool: &'static str },
    /// `helper` device and an acting call that has no helper route.
    NeedsAdbShell { tool: &'static str },
    /// The screen guard (D3), the shell guard or the file guard (D3b): the call would land on a
    /// payment, destructive, credential, security, permission, install or send target, or on a
    /// screen it could not read. `verdict` is `blocked` or `asked` (needs a human yes; with no
    /// approval channel, or when the human denies or does not answer in time, it is refused too).
    Screen {
        tool: &'static str,
        verdict: &'static str,
        kind: &'static str,
        reason: String,
    },
    /// The audit log could not record the call, so it was not sent (no unlogged action).
    Audit { tool: &'static str, why: String },
    /// The screen lock (`screenlock.rs`): the phone is in use and `ask-screen` gave no lease.
    ScreenLock { tool: &'static str, why: String },
    /// The protected phone has no open lease, and one was asked for on the owner's phone (or the
    /// ask is still waiting). Nothing was sent; retry once the owner has tapped Allow. `reference`
    /// is a short, non-secret handle on the ask: the request id itself never goes to the agent, so
    /// it cannot forge `allow <id>`.
    LeasePending {
        tool: &'static str,
        reference: String,
    },
    /// The protected phone has no open lease and none could be opened (denied, held after a Deny,
    /// no channel, no daemon lease, the clock or the presence file could not be trusted).
    LeaseRefused { tool: &'static str, why: String },
    /// A fixed protection of the protected phone that no lease lifts (Gmail and WhatsApp, the
    /// keyguard and notification previews, the owner's PIN, a path naming the protected apps, an unknown
    /// screen). `rule` is `app`, `preview`, `pin`, `unlock`, `path` or `screen`.
    Protected {
        tool: &'static str,
        rule: &'static str,
        why: String,
    },
}

impl Refusal {
    /// `blocked` or `asked`, as the refusal log names it.
    pub fn verdict(&self) -> &'static str {
        match self {
            Refusal::Screen { verdict, .. } => verdict,
            Refusal::LeasePending { .. } => "asked",
            _ => "blocked",
        }
    }
    /// The rule family that refused: `s24`, `allowlist`, `read_only`, `helper`, `screen:<kind>`, `audit`,
    /// `screen_lock`, `lease:pending`, `lease:refused`, `protected:<rule>`.
    pub fn rule(&self) -> String {
        match self {
            Refusal::S24 { .. } => "s24".into(),
            Refusal::NotAllowed { .. } => "allowlist".into(),
            Refusal::ReadOnly { .. } => "read_only".into(),
            Refusal::NeedsAdbShell { .. } => "helper".into(),
            Refusal::Screen { kind, .. } => format!("screen:{kind}"),
            Refusal::Audit { .. } => "audit".into(),
            Refusal::ScreenLock { .. } => "screen_lock".into(),
            Refusal::LeasePending { .. } => "lease:pending".into(),
            Refusal::LeaseRefused { .. } => "lease:refused".into(),
            Refusal::Protected { rule, .. } => format!("protected:{rule}"),
        }
    }
}

/// The screen guard's half of the gate (D3): turn the guard's decision into a refusal. Deny by
/// default: anything but `allowed` is refused here; the caller (`transport.rs`) may then take an
/// `asked` refusal to the approval channel (D3b) and let the call through on a human yes.
pub fn check_screen(call: &Call, d: &crate::screenguard::Decision) -> Result<(), Refusal> {
    check_decision(call.name(), d)
}

/// [`check_screen`] for a named tool (`shell`, `push`, `pull`) rather than a [`Call`].
pub fn check_decision(tool: &'static str, d: &crate::screenguard::Decision) -> Result<(), Refusal> {
    use crate::screenguard::Verdict;
    match d.verdict {
        Verdict::Allowed => Ok(()),
        v => Err(Refusal::Screen {
            tool,
            verdict: v.as_str(),
            kind: d.kind.map_or("unknown", |k| k.as_str()),
            reason: if d.target.is_empty() {
                d.reason.clone()
            } else {
                format!("{} on {}", d.reason, d.target)
            },
        }),
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Screen { tool, verdict: "asked", kind, reason } => write!(f, "{tool} refused, nothing was sent: this needs a human yes ({kind}: {reason})"),
            Refusal::Screen { tool, kind, reason, .. } => write!(
                f,
                "{tool} blocked, nothing was sent: the {} refuses {kind} actions ({reason}). On-screen text cannot approve this.",
                match *tool {
                    "shell" => "shell guard",
                    "push" | "pull" => "file guard",
                    _ => "screen guard",
                }
            ),
            Refusal::Audit { tool, why } => write!(f, "{tool} refused, nothing was sent: the audit log could not record it ({why})"),
            Refusal::S24 { tool } => write!(f, "{tool} refused: the protected phone is read-only and never a target"),
            Refusal::NotAllowed { serial } => write!(f, "{serial} is not on the allowlist. Start bridged with --allow-serial <hardware serial> (or list it in allow.txt) to drive it."),
            Refusal::ReadOnly { tool } => write!(f, "{tool} refused: this device is read_only, so it only answers reads"),
            Refusal::NeedsAdbShell { tool } => write!(f, "{tool} refused: this device is in helper mode and {tool} needs adb shell"),
            Refusal::ScreenLock { tool, why } => write!(f, "{tool} refused by the screen lock, nothing was sent: {why}"),
            Refusal::LeasePending { tool, reference } => write!(f, "{tool} refused, nothing was sent: a lease on the protected phone is pending (ask {reference}); the owner must tap Allow on their phone, then retry"),
            Refusal::LeaseRefused { tool, why } => write!(f, "{tool} refused, nothing was sent: the protected phone has no open lease ({why})"),
            Refusal::Protected { tool, why, .. } => write!(f, "{tool} refused on the protected phone, nothing was sent: {why}. A lease never lifts this."),
        }
    }
}

impl std::error::Error for Refusal {}

/// Who a call is aimed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject<'a> {
    pub serial: &'a str,
    pub hw_serial: &'a str,
    pub allowed: bool,
}

/// Is this call one of the protected phone's approved reads (a `getprop` whose shell form is an entry
/// of `S24_READS`)? Those need no lease.
pub fn is_ruled_read(call: &Call) -> bool {
    matches!(call, Call::Getprop { key } if S24_READS.contains(&format!("shell getprop {key}").as_str()))
}

/// The protected phone under a lease (`crate::protected`): the mode the owner configured for it
/// (`full` unless the owner set another) decides first, so `read_only` and `helper` win over any lease;
/// then `unlock`, which is never sent to it (the owner's PIN is never used). The allowlist does not apply:
/// the lease is the protected phone's only way in, and it is never on the allowlist.
pub fn check_leased(configured: DeviceMode, call: &Call) -> Result<(), Refusal> {
    if call.is_acting() {
        match configured {
            DeviceMode::Full => {}
            DeviceMode::ReadOnly => return Err(Refusal::ReadOnly { tool: call.name() }),
            DeviceMode::Helper if call.has_helper_route() => {}
            DeviceMode::Helper => return Err(Refusal::NeedsAdbShell { tool: call.name() }),
        }
    }
    if matches!(call, Call::Unlock) {
        return Err(Refusal::Protected {
            tool: "unlock",
            rule: "unlock",
            why: "unlock is never used on the protected phone (its PIN is never typed)".into(),
        });
    }
    Ok(())
}

/// [`check_leased`] for a raw adb action (`shell`, `push`, `pull`): only a `full` protected phone.
pub fn check_leased_raw(configured: DeviceMode, tool: &'static str) -> Result<(), Refusal> {
    match configured {
        DeviceMode::Full => Ok(()),
        DeviceMode::ReadOnly => Err(Refusal::ReadOnly { tool }),
        DeviceMode::Helper => Err(Refusal::NeedsAdbShell { tool }),
    }
}

/// The gate. Order: S24, allowlist, then the mode.
pub fn check(subject: &Subject<'_>, mode: DeviceMode, call: &Call) -> Result<(), Refusal> {
    if is_s24(subject.serial) || is_s24(subject.hw_serial) {
        let ruled_read = is_ruled_read(call);
        if !ruled_read {
            return Err(Refusal::S24 { tool: call.name() });
        }
        // a ruled read: a read, so no mode can refuse it, and the protected phone is never on the allowlist
        return Ok(());
    }
    if !subject.allowed {
        return Err(Refusal::NotAllowed {
            serial: subject.serial.to_string(),
        });
    }
    if !call.is_acting() {
        return Ok(());
    }
    match mode {
        DeviceMode::Full => Ok(()),
        DeviceMode::ReadOnly => Err(Refusal::ReadOnly { tool: call.name() }),
        DeviceMode::Helper if call.has_helper_route() => Ok(()),
        DeviceMode::Helper => Err(Refusal::NeedsAdbShell { tool: call.name() }),
    }
}

/// The gate for a raw adb action that is not a [`Call`] (D3b: `shell`, `push`, `pull`). Every one
/// of them changes or reads the phone over adb shell, so it is treated as an acting call with no
/// helper route: the protected phone never, the allowlist always, and only a `full` device.
pub fn check_raw(
    subject: &Subject<'_>,
    mode: DeviceMode,
    tool: &'static str,
) -> Result<(), Refusal> {
    if is_s24(subject.serial) || is_s24(subject.hw_serial) {
        return Err(Refusal::S24 { tool });
    }
    if !subject.allowed {
        return Err(Refusal::NotAllowed {
            serial: subject.serial.to_string(),
        });
    }
    match mode {
        DeviceMode::Full => Ok(()),
        DeviceMode::ReadOnly => Err(Refusal::ReadOnly { tool }),
        DeviceMode::Helper => Err(Refusal::NeedsAdbShell { tool }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_actions_need_a_full_allowed_device_that_is_not_the_s24() {
        assert!(check_raw(&POCO, DeviceMode::Full, "shell").is_ok());
        assert!(matches!(
            check_raw(&POCO, DeviceMode::ReadOnly, "pull"),
            Err(Refusal::ReadOnly { tool: "pull" })
        ));
        assert!(matches!(
            check_raw(&POCO, DeviceMode::Helper, "push"),
            Err(Refusal::NeedsAdbShell { tool: "push" })
        ));
        let s24 = Subject {
            serial: S24_SERIAL,
            hw_serial: S24_SERIAL,
            allowed: true,
        };
        assert!(matches!(
            check_raw(&s24, DeviceMode::Full, "shell"),
            Err(Refusal::S24 { .. })
        ));
        let stranger = Subject {
            serial: "abc",
            hw_serial: "abc",
            allowed: false,
        };
        assert!(matches!(
            check_raw(&stranger, DeviceMode::Full, "shell"),
            Err(Refusal::NotAllowed { .. })
        ));
    }

    const POCO: Subject<'static> = Subject {
        serial: "192.0.2.50:37123",
        hw_serial: "DEVICE0000EXAMPLE",
        allowed: true,
    };

    #[test]
    fn call_lists_match_the_samples() {
        let samples = Call::samples();
        assert_eq!(samples.len(), READ_TOOLS.len() + ACTING_TOOLS.len());
        for c in &samples {
            assert_eq!(
                c.is_acting(),
                ACTING_TOOLS.contains(&c.name()),
                "{}",
                c.name()
            );
            assert_eq!(
                !c.is_acting(),
                READ_TOOLS.contains(&c.name()),
                "{}",
                c.name()
            );
        }
        let mut names: Vec<_> = samples.iter().map(|c| c.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), samples.len());
    }

    #[test]
    fn read_only_refuses_every_acting_call_and_allows_every_read() {
        let mut refused = 0;
        for c in Call::samples() {
            let r = check(&POCO, DeviceMode::ReadOnly, &c);
            if c.is_acting() {
                assert_eq!(r, Err(Refusal::ReadOnly { tool: c.name() }), "{}", c.name());
                refused += 1;
            } else {
                assert_eq!(r, Ok(()), "{}", c.name());
            }
        }
        assert_eq!(refused, ACTING_TOOLS.len());
    }

    #[test]
    fn full_allows_everything_on_an_allowed_device() {
        for c in Call::samples() {
            assert_eq!(check(&POCO, DeviceMode::Full, &c), Ok(()), "{}", c.name());
        }
    }

    #[test]
    fn helper_mode_allows_only_helper_routes() {
        for c in Call::samples().into_iter().filter(Call::is_acting) {
            let r = check(&POCO, DeviceMode::Helper, &c);
            if c.has_helper_route() {
                assert_eq!(r, Ok(()), "{}", c.name());
            } else {
                assert_eq!(
                    r,
                    Err(Refusal::NeedsAdbShell { tool: c.name() }),
                    "{}",
                    c.name()
                );
            }
        }
    }

    #[test]
    fn s24_is_refused_for_every_tool_under_every_mode() {
        let by_hw = Subject {
            serial: "192.0.2.60:5555",
            hw_serial: "PROTECTED0000EXAMPLE",
            allowed: true,
        };
        let by_serial = Subject {
            serial: "PROTECTED0000EXAMPLE",
            hw_serial: "",
            allowed: true,
        };
        let by_mdns = Subject {
            serial: "adb-protected0000example-abc123._adb-tls-connect._tcp",
            hw_serial: "",
            allowed: true,
        };
        for s in [&by_hw, &by_serial, &by_mdns] {
            for mode in [DeviceMode::Full, DeviceMode::Helper, DeviceMode::ReadOnly] {
                for c in Call::samples() {
                    assert_eq!(
                        check(s, mode, &c),
                        Err(Refusal::S24 { tool: c.name() }),
                        "{} {:?}",
                        c.name(),
                        mode
                    );
                }
                assert_eq!(
                    check(
                        s,
                        mode,
                        &Call::Getprop {
                            key: "ro.serialno".into()
                        }
                    ),
                    Ok(()),
                    "{mode:?}"
                );
                for key in [
                    "ro.product.brand",
                    "ro.mi.os.version.name",
                    "ro.serialno;reboot",
                    "ro.serialno extra",
                    "",
                    &"a".repeat(65),
                ] {
                    let c = Call::Getprop { key: key.into() };
                    assert_eq!(
                        check(s, mode, &c),
                        Err(Refusal::S24 { tool: "getprop" }),
                        "{key:?} {mode:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn s24_is_always_read_only_whatever_is_configured() {
        for cfg in [DeviceMode::Full, DeviceMode::Helper, DeviceMode::ReadOnly] {
            assert_eq!(
                effective_mode(&["x", "PROTECTED0000EXAMPLE"], cfg),
                DeviceMode::ReadOnly
            );
            assert_eq!(
                effective_mode(&["adb-PROTECTED0000EXAMPLE-aa"], cfg),
                DeviceMode::ReadOnly
            );
            assert_eq!(
                effective_mode(&["192.0.2.50:1", "DEVICE0000EXAMPLE"], cfg),
                cfg
            );
        }
    }

    #[test]
    fn prop_keys_are_validated() {
        for k in ["ro.serialno", "a", "ro.product-x_1", &"a".repeat(64)] {
            assert!(valid_prop_key(k), "{k}");
        }
        for k in ["", "a b", "a;b", "a$(x)", "a\nb", "é", &"a".repeat(65)] {
            assert!(!valid_prop_key(k), "{k:?}");
        }
    }

    #[test]
    fn unlisted_devices_are_refused() {
        let other = Subject {
            serial: "XYZ",
            hw_serial: "XYZ",
            allowed: false,
        };
        assert!(matches!(
            check(&other, DeviceMode::Full, &Call::ListDevices),
            Err(Refusal::NotAllowed { .. })
        ));
    }

    #[test]
    fn mode_names_round_trip() {
        for m in [DeviceMode::Full, DeviceMode::Helper, DeviceMode::ReadOnly] {
            assert_eq!(DeviceMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(DeviceMode::parse("rw"), None);
    }
}
