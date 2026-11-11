//! The command guard: deny by default, ported from phone-guard `devices.py` (`gate`, `parse_argv`).
//!
//! Differences from the Python, all stricter and listed in ADB-LINK-REPORT.md:
//! * `start-server`, `pair` and `reconnect` (any form) are refused outright;
//! * commands that act on "the default device" (shell, get-state, ...) need an explicit selector
//!   instead of being pinned to a transport id;
//! * the protected phone is hardcoded as never-a-target instead of coming from readonly.json.

use std::collections::HashSet;
use std::sync::Mutex;

use crate::runner::{RunError, RunOutput, Runner};

/// Hardware serial of the protected phone: read-only, never a target.
pub const S24_SERIAL: &str = "PROTECTED0000EXAMPLE";

/// The commands the protected phone may answer. The rule is "serial + approved probes
/// only", and the approved set is the 10 commands of
/// `probes/readonly-s24.txt` (`getprop ro.serialno` plus the 9 below). Each is an exact string
/// (`shell ` + the file line), never a pattern. Adding one is a deliberate policy change; the test
/// `s24_reads_equal_the_approved_file` builds the file in with `include_str!`, so the list and the
/// file cannot drift.
pub const S24_READS: &[&str] = &[
    "shell getprop ro.serialno",
    "shell getprop ro.build.version.sdk",
    "shell getprop ro.build.version.release",
    "shell getprop ro.product.model",
    "shell getprop ro.build.version.oneui",
    "shell wm size",
    "shell wm density",
    "shell pm list features",
    "shell dumpsys battery",
    "shell dumpsys thermalservice",
];

/// Server-wide, read-only commands that never touch one device.
pub const GLOBAL_SAFE: [&str; 3] = ["devices", "version", "mdns"];
/// Commands that act on a device's transport or state: refused without an explicit target.
pub const TRANSPORT_CMDS: [&str; 22] = [
    "disconnect",
    "reconnect",
    "usb",
    "tcpip",
    "root",
    "unroot",
    "reboot",
    "install",
    "install-multiple",
    "uninstall",
    "push",
    "pull",
    "sync",
    "forward",
    "reverse",
    "connect",
    "pair",
    "remount",
    "sideload",
    "emu",
    "disable-verity",
    "enable-verity",
];
/// Take `host:port` tokens instead of a selector.
pub const HOST_CMDS: [&str; 3] = ["connect", "disconnect", "pair"];
/// Commands adb sends over the selected transport, or that only read.
pub const PER_DEVICE_CMDS: [&str; 15] = [
    "shell",
    "exec-out",
    "logcat",
    "bugreport",
    "get-devpath",
    "jdwp",
    "backup",
    "restore",
    "install-multi-package",
    "attach",
    "detach",
    "track-devices",
    "host-features",
    "features",
    "get-serialno",
];
/// Commands that reach the whole adb server or mint credentials, whatever `-s` says.
pub const ALWAYS_REFUSED: [&str; 5] =
    ["kill-server", "start-server", "keygen", "pair", "reconnect"];

/// Task 2.15, Detector 1: the only `getevent` argv shapes the hub sends, after `-s <serial>`. Each is
/// exact (no pattern, no extra word). **Full devices only, never the protected phone**: [`Guard::check_getevent`]
/// refuses any other device mode, and the protected phone is refused in every spelling because no entry of
/// [`S24_READS`] is a `getevent` (only the 10 approved reads reach it).
pub const GETEVENT_SHAPES: [&[&str]; 3] = [
    // the device map (the same read as probes/readonly.txt, through exec-out so no pty rewrites it)
    &["exec-out", "getevent", "-pl"],
    // the device map with bus ids (`bus: 0006` is BUS_VIRTUAL: uinput, uhid, scrcpy's UHID devices)
    &["exec-out", "getevent", "-pli"],
    // the labelled, timestamped event stream
    &["exec-out", "getevent", "-lt"],
];

/// Task 2.12 pairing (`agentroid link pair <serial>`): the helper's link service, from
/// `android/link/src/main/AndroidManifest.xml` (`dev.agentroid.helper.link.LinkService`) and the
/// applicationId `dev.agentroid.helper` (`android/app/build.gradle.kts`). Its `dump()` prints only
/// the phone's PUBLIC key (`DumpText.java`).
pub const LINK_SERVICE_COMPONENT: &str = "dev.agentroid.helper/.link.LinkService";
/// The DUMP-protected pairing receiver and its action (`ConfigReceiver.java`, `Pairing.java`).
pub const LINK_PAIR_RECEIVER: &str = "dev.agentroid.helper/.link.ConfigReceiver";
pub const LINK_PAIR_ACTION: &str = "dev.agentroid.helper.PAIR";
/// Base64 of the fixed DER prefix of a P-256 SubjectPublicKeyInfo (uncompressed point): the only
/// key `hub_key` may carry. A PKCS#8 private key starts differently ("MIGHAgEA...").
pub const P256_SPKI_B64_PREFIX: &str = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE";

/// The two link-pairing command shapes. They are allowed for full devices only: neither is in
/// [`S24_READS`], so the protected phone refuses both, by serial in [`Guard::check`] and by hardware serial
/// behind any address in [`GuardedRunner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkCommand {
    /// `shell dumpsys activity service dev.agentroid.helper/.link.LinkService`
    DumpPublicKey,
    /// `shell am broadcast -a dev.agentroid.helper.PAIR -n dev.agentroid.helper/.link.ConfigReceiver
    /// --es hub_key <base64 P-256 SPKI> --es hub <100.64.0.0/10 address>:<port> --es serial <serial>
    /// [--ez rekey true]`
    Pair,
}

/// `-s <selector> shell dumpsys activity service dev.agentroid.helper/.link.LinkService`
pub fn link_dump_argv(selector: &str) -> Vec<String> {
    [
        "-s",
        selector,
        "shell",
        "dumpsys",
        "activity",
        "service",
        LINK_SERVICE_COMPONENT,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `-s <selector> shell am broadcast -a dev.agentroid.helper.PAIR -n dev.agentroid.helper/.link.ConfigReceiver
/// --es hub_key <hub_key_b64> --es hub <hub> --es serial <serial>`, plus `--ez rekey true` when `rekey`.
pub fn link_pair_argv(
    selector: &str,
    hub_key_b64: &str,
    hub: &str,
    serial: &str,
    rekey: bool,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "-s",
        selector,
        "shell",
        "am",
        "broadcast",
        "-a",
        LINK_PAIR_ACTION,
        "-n",
        LINK_PAIR_RECEIVER,
        "--es",
        "hub_key",
        hub_key_b64,
        "--es",
        "hub",
        hub,
        "--es",
        "serial",
        serial,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if rekey {
        a.extend(["--ez", "rekey", "true"].iter().map(|s| s.to_string()));
    }
    a
}

/// A P-256 public key SPKI in standard base64: 124 characters with the fixed prefix.
fn is_p256_spki_b64(s: &str) -> bool {
    s.len() == 124
        && s.starts_with(P256_SPKI_B64_PREFIX)
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'=')
        && s.trim_end_matches('=').bytes().all(|c| c != b'=')
}

/// A decimal number without leading zeros, at most `max`.
fn plain_number(s: &str, max: u32) -> Option<u32> {
    if s.is_empty()
        || s.len() > 5
        || !s.bytes().all(|c| c.is_ascii_digit())
        || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse::<u32>().ok().filter(|n| *n <= max)
}

/// `100.x.y.z:port` in 100.64.0.0/10 (not Tailscale's 100.100.100.100), port 1..=65535.
fn is_tailnet_hub(s: &str) -> bool {
    let Some((host, port)) = s.split_once(':') else {
        return false;
    };
    let o: Vec<Option<u32>> = host.split('.').map(|p| plain_number(p, 255)).collect();
    o.len() == 4
        && o.iter().all(Option::is_some)
        && o[0] == Some(100)
        && o[1].is_some_and(|b| (64..=127).contains(&b))
        && host != "100.100.100.100"
        && plain_number(port, 65535).is_some_and(|p| p >= 1)
}

fn is_link_serial(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'))
}

/// Why the guard said no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardError(pub String);

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "refused: {}", self.0)
    }
}

impl std::error::Error for GuardError {}

fn refuse<T>(msg: impl Into<String>) -> Result<T, GuardError> {
    Err(GuardError(msg.into()))
}

/// A command line the guard let through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// `-s X`, `-t X`, `-d` or `-e`, as given.
    pub selector: Option<Vec<String>>,
    pub sub: String,
    pub rest: Vec<String>,
    /// For connect/disconnect: the single address.
    pub addresses: Vec<String>,
}

impl Checked {
    /// True for the one command that learns identities: `shell getprop ro.serialno`.
    pub fn is_identity_probe(&self) -> bool {
        let mut words = vec![self.sub.as_str()];
        words.extend(self.rest.iter().map(String::as_str));
        words.join(" ") == "shell getprop ro.serialno"
    }

    /// True when `sub` + `rest` joined by one space is exactly one entry of [`S24_READS`].
    pub fn is_s24_read(&self) -> bool {
        let mut words = vec![self.sub.as_str()];
        words.extend(self.rest.iter().map(String::as_str));
        S24_READS.contains(&words.join(" ").as_str())
    }

    /// True when `sub` + `rest` is exactly one of [`GETEVENT_SHAPES`].
    pub fn is_getevent_shape(&self) -> bool {
        let mut words = vec![self.sub.as_str()];
        words.extend(self.rest.iter().map(String::as_str));
        GETEVENT_SHAPES.contains(&words.as_slice())
    }

    /// Which link-pairing shape this is, when `sub` + `rest` is exactly one of the two, word for
    /// word, with a P-256 public SPKI, a tailnet `host:port` and a plain serial in the slots.
    pub fn link_command(&self) -> Option<LinkCommand> {
        if self.sub != "shell" {
            return None;
        }
        let w: Vec<&str> = self.rest.iter().map(String::as_str).collect();
        if w == ["dumpsys", "activity", "service", LINK_SERVICE_COMPONENT] {
            return Some(LinkCommand::DumpPublicKey);
        }
        let fixed = [
            "am",
            "broadcast",
            "-a",
            LINK_PAIR_ACTION,
            "-n",
            LINK_PAIR_RECEIVER,
            "--es",
            "hub_key",
        ];
        let shape_ok = (w.len() == 15 || (w.len() == 18 && w[15..] == ["--ez", "rekey", "true"]))
            && w[..8] == fixed
            && w[9] == "--es"
            && w[10] == "hub"
            && w[12] == "--es"
            && w[13] == "serial";
        if shape_ok && is_p256_spki_b64(w[8]) && is_tailnet_hub(w[11]) && is_link_serial(w[14]) {
            return Some(LinkCommand::Pair);
        }
        None
    }

    /// True when any word names the link service or the PAIR action (any case): such a command
    /// must be one of the two exact [`LinkCommand`] shapes.
    pub fn touches_link(&self) -> bool {
        std::iter::once(&self.sub).chain(self.rest.iter()).any(|w| {
            let l = w.to_ascii_lowercase();
            l.contains("agentroid.helper.pair") || l.contains("link.linkservice")
        })
    }
}

/// The guard. Holds the set of tokens learnt to be forbidden at run time (S24 addresses).
#[derive(Debug, Default)]
pub struct Guard {
    denied: Mutex<HashSet<String>>,
}

fn lock(m: &Mutex<HashSet<String>>) -> std::sync::MutexGuard<'_, HashSet<String>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Guard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forbid a token (for example the address an S24 was seen at).
    pub fn deny(&self, token: &str) {
        lock(&self.denied).insert(token.trim().to_string());
    }

    fn is_forbidden(&self, token: &str) -> bool {
        token.to_ascii_uppercase().contains(S24_SERIAL) || lock(&self.denied).contains(token.trim())
    }

    /// Check a full adb argv (without the executable).
    pub fn check(&self, argv: &[String]) -> Result<Checked, GuardError> {
        let (selector, sub, rest) = parse_argv(argv)?;
        if let Some(sel) = &selector {
            if let Some(v) = sel.get(1) {
                let read = Checked {
                    selector: None,
                    sub: sub.clone(),
                    rest: rest.clone(),
                    addresses: vec![],
                }
                .is_s24_read();
                if self.is_forbidden(v) && !read {
                    return refuse(format!("{v} is the read-only S24, never a target"));
                }
            }
        }
        let probe = Checked {
            selector: None,
            sub: sub.clone(),
            rest: rest.clone(),
            addresses: vec![],
        };
        if probe.touches_link() && probe.link_command().is_none() {
            return refuse(
                "only the two exact link-pairing shapes (dumpsys of the LinkService, am broadcast of PAIR with a P-256 public key, a tailnet hub address and a serial) may name the helper's link",
            );
        }
        if ALWAYS_REFUSED.contains(&sub.as_str()) {
            return refuse(format!(
                "adb {sub} acts on the whole adb server or on credentials (every phone, the protected phone included)"
            ));
        }
        if GLOBAL_SAFE.contains(&sub.as_str()) {
            let ok = match sub.as_str() {
                "devices" => rest.iter().all(|a| a == "-l"),
                "version" => rest.is_empty(),
                _ => rest == ["services"] || rest == ["check"],
            };
            if !ok {
                return refuse(format!("unexpected arguments to adb {sub}"));
            }
            return Ok(Checked {
                selector,
                sub,
                rest,
                addresses: vec![],
            });
        }
        let known = TRANSPORT_CMDS.contains(&sub.as_str())
            || HOST_CMDS.contains(&sub.as_str())
            || PER_DEVICE_CMDS.contains(&sub.as_str())
            || sub == "get-state"
            || sub.starts_with("wait-for-");
        if !known {
            return refuse(format!(
                "adb {sub} is not a known per-device command (it may act on the whole adb server)"
            ));
        }
        if sub == "forward"
            && (rest.iter().any(|a| a == "--remove-all" || a == "--remove")
                || (!rest.iter().any(|a| a == "--list")
                    && !rest.iter().any(|a| a == "--no-rebind")))
        {
            return refuse("adb forward may rebind or remove a listener the whole server shares");
        }
        if HOST_CMDS.contains(&sub.as_str()) {
            // connect/disconnect go straight to the server and ignore -s: only the address counts.
            if rest.iter().any(|a| a.starts_with('-')) {
                return refuse(format!("adb {sub} takes no options"));
            }
            if rest
                .iter()
                .any(|t| t.trim().is_empty() || t != t.trim() || t.contains(char::is_whitespace))
            {
                return refuse(format!("adb {sub} with an empty or padded address (an empty disconnect drops every Wi-Fi device)"));
            }
            if rest.len() != 1 {
                return refuse(format!(
                    "adb {sub} needs exactly one explicit address (got {}); a bare disconnect drops every Wi-Fi device",
                    rest.len()
                ));
            }
            if self.is_forbidden(&rest[0]) {
                return refuse(format!("{} is the read-only S24, never a target", rest[0]));
            }
            let addresses = rest.clone();
            return Ok(Checked {
                selector,
                sub,
                rest,
                addresses,
            });
        }
        if selector.is_none() {
            return refuse(format!("adb {sub} needs an explicit target (-s)"));
        }
        Ok(Checked {
            selector,
            sub,
            rest,
            addresses: vec![],
        })
    }

    /// Task 2.15: check a `getevent` argv. It must pass [`Guard::check`] (so the protected phone is refused in
    /// every spelling), name its target with `-s`, be exactly one of [`GETEVENT_SHAPES`], and the
    /// target must be a `full` device (`device_is_full`, from core's device mode).
    pub fn check_getevent(
        &self,
        argv: &[String],
        device_is_full: bool,
    ) -> Result<Checked, GuardError> {
        let c = self.check(argv)?;
        match &c.selector {
            Some(sel) if sel.len() == 2 && sel[0] == "-s" => {}
            _ => return refuse("getevent needs an explicit -s <serial> target"),
        }
        if !c.is_getevent_shape() {
            return refuse(format!(
                "{argv:?} is not one of the getevent shapes 2.15 may send"
            ));
        }
        if !device_is_full {
            return refuse("getevent streams input events: full devices only");
        }
        Ok(c)
    }
}

/// Port of `parse_argv`: leading options must be exactly one selector; anything else is refused.
type ParsedArgv = (Option<Vec<String>>, String, Vec<String>);

fn parse_argv(argv: &[String]) -> Result<ParsedArgv, GuardError> {
    let mut sel: Vec<Vec<String>> = Vec::new();
    let mut i = 0;
    while i < argv.len() && argv[i].starts_with('-') && argv[i] != "-" {
        let a = argv[i].as_str();
        match a {
            "-s" | "-t" => {
                match argv.get(i + 1) {
                    Some(v) if !v.trim().is_empty() => sel.push(vec![a.to_string(), v.clone()]),
                    _ => return refuse(format!("{a} without a value")),
                }
                i += 2;
            }
            "-d" | "-e" => {
                sel.push(vec![a.to_string()]);
                i += 1;
            }
            _ => return refuse(format!("adb option {a} is not allowed")),
        }
    }
    if sel.len() > 1 {
        return refuse(format!("more than one target selector in {argv:?}"));
    }
    if i >= argv.len() {
        return refuse("no adb subcommand");
    }
    Ok((sel.pop(), argv[i].clone(), argv[i + 1..].to_vec()))
}

/// A runner that passes every command through the guard, and checks the real hardware serial of
/// the target behind any selector (never from how the transport is spelt) before it lets a
/// non-probe command out. Fails closed.
pub struct GuardedRunner<R: Runner> {
    inner: R,
    guard: Guard,
}

impl<R: Runner> GuardedRunner<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            guard: Guard::new(),
        }
    }

    pub fn guard(&self) -> &Guard {
        &self.guard
    }

    fn hardware_serial(&self, selector: &[String]) -> Result<String, RunError> {
        let mut a: Vec<String> = selector.to_vec();
        a.push("shell".into());
        a.push("getprop ro.serialno".into());
        let out = self.inner.run(&a, 15_000)?;
        let val = out.stdout.trim().to_string();
        let plain = !val.is_empty()
            && val.len() <= 64
            && val
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '_' | '-'));
        if !out.ok() || !plain {
            return Err(RunError::Refused(format!(
                "cannot identify target {selector:?} (no ro.serialno); failing closed"
            )));
        }
        Ok(val)
    }

    /// Task 2.15: admit a long-running `getevent` stream without running it. Runs
    /// [`Guard::check_getevent`], then reads the target's real hardware serial (fails closed) and
    /// refuses the protected phone behind any spelling, denying the address it was reached at. The caller spawns
    /// the stream only on `Ok`, with the argv it passed here.
    pub fn admit_getevent(
        &self,
        argv: &[String],
        device_is_full: bool,
    ) -> Result<Checked, RunError> {
        let c = self
            .guard
            .check_getevent(argv, device_is_full)
            .map_err(|e| RunError::Refused(e.0))?;
        let sel = c.selector.clone().unwrap_or_default();
        let hw = self.hardware_serial(&sel)?;
        if hw.eq_ignore_ascii_case(S24_SERIAL) {
            if let Some(v) = sel.get(1) {
                self.guard.deny(v);
            }
            return Err(RunError::Refused(
                "target is the read-only S24: getevent is never sent to it".into(),
            ));
        }
        Ok(c)
    }
}

impl<R: Runner> GuardedRunner<R> {
    /// The checks every command passes before it is sent: the argv guard, then the target's real
    /// hardware serial (fails closed).
    fn admit(&self, args: &[String]) -> Result<(), RunError> {
        let c = self.guard.check(args).map_err(|e| RunError::Refused(e.0))?;
        if let Some(sel) = &c.selector {
            if !c.is_identity_probe()
                && !HOST_CMDS.contains(&c.sub.as_str())
                && !GLOBAL_SAFE.contains(&c.sub.as_str())
            {
                let hw = self.hardware_serial(sel)?;
                if hw.eq_ignore_ascii_case(S24_SERIAL) {
                    if let Some(v) = sel.get(1) {
                        self.guard.deny(v);
                    }
                    if !c.is_s24_read() {
                        return Err(RunError::Refused(
                            "target is the read-only S24, never a target".into(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

impl<R: Runner> Runner for GuardedRunner<R> {
    fn run(&self, args: &[String], timeout_ms: u64) -> Result<RunOutput, RunError> {
        self.admit(args)?;
        self.inner.run(args, timeout_ms)
    }

    fn run_bytes(
        &self,
        args: &[String],
        timeout_ms: u64,
    ) -> Result<crate::runner::BytesOutput, RunError> {
        self.admit(args)?;
        self.inner.run_bytes(args, timeout_ms)
    }

    fn deny(&self, token: &str) {
        self.guard.deny(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::args;
    use std::cell::RefCell;

    fn g(a: &[&str]) -> Result<Checked, GuardError> {
        Guard::new().check(&args(a))
    }

    fn refused(a: &[&str]) -> String {
        g(a).expect_err(&format!("should refuse {a:?}")).0
    }

    #[test]
    fn allows_the_commands_the_watcher_needs() {
        assert!(g(&["devices", "-l"]).is_ok());
        assert!(g(&["mdns", "services"]).is_ok());
        assert!(g(&["version"]).is_ok());
        let c = g(&["connect", "192.0.2.78:37123"]).unwrap();
        assert_eq!(c.addresses, vec!["192.0.2.78:37123"]);
        assert!(g(&["disconnect", "192.0.2.78:37123"]).is_ok());
        assert!(
            g(&["-s", "192.0.2.78:37123", "shell", "getprop ro.serialno"])
                .unwrap()
                .is_identity_probe()
        );
        assert!(g(&["-s", "DEVICE0000EXAMPLE", "shell", "input", "tap", "1", "2"]).is_ok());
    }

    #[test]
    fn refuses_each_forbidden_server_command() {
        for cmd in ["kill-server", "start-server", "keygen", "pair", "reconnect"] {
            assert!(
                refused(&[cmd]).contains("refused") || !refused(&[cmd]).is_empty(),
                "{cmd}"
            );
            assert!(g(&["-s", "ABC12345", cmd]).is_err(), "{cmd} with -s");
        }
        assert!(g(&["pair", "192.168.1.2:4000", "123456"]).is_err());
        assert!(g(&["-s", "ABC12345", "reconnect", "offline"]).is_err());
        assert!(g(&["reconnect", "device"]).is_err());
    }

    #[test]
    fn refuses_host_commands_without_exactly_one_address() {
        assert!(refused(&["connect"]).contains("exactly one"));
        assert!(refused(&["disconnect"]).contains("exactly one"));
        assert!(g(&["connect", "1.2.3.4:5555", "5.6.7.8:5555"]).is_err());
        assert!(g(&["disconnect", "1.2.3.4:5555", "5.6.7.8:5555"]).is_err());
        assert!(g(&["disconnect", ""]).is_err());
        assert!(g(&["disconnect", " 1.2.3.4:5555"]).is_err());
        assert!(g(&["connect", "1.2.3.4 :5555"]).is_err());
        assert!(g(&["connect", "-x"]).is_err());
        // -s does not stand in for an address
        assert!(g(&["-s", "ABC12345", "disconnect"]).is_err());
    }

    #[test]
    fn refuses_unknown_and_untargeted_commands() {
        assert!(g(&["raw", "host:kill"]).is_err());
        assert!(g(&["shell", "ls"]).is_err()); // no selector
        assert!(g(&["reboot"]).is_err());
        assert!(g(&["-H", "1.2.3.4", "devices"]).is_err());
        assert!(g(&["-s", "A1", "-s", "B2", "shell", "ls"]).is_err());
        assert!(g(&["-s", "A1", "-d", "shell", "ls"]).is_err());
        assert!(g(&["devices", "-x"]).is_err());
        assert!(g(&["mdns", "bogus"]).is_err());
        assert!(g(&["forward", "--remove-all"]).is_err());
        assert!(g(&["-s", "A1", "forward", "tcp:1", "tcp:2"]).is_err());
        assert!(g(&["-s", "A1", "forward", "--no-rebind", "tcp:1", "tcp:2"]).is_ok());
        assert!(g(&[]).is_err());
        assert!(g(&["-s"]).is_err());
    }

    #[test]
    fn refuses_the_s24_in_every_spelling() {
        assert!(refused(&["-s", S24_SERIAL, "shell", "ls"]).contains("S24"));
        assert!(g(&["-s", "protected0000example", "shell", "ls"]).is_err());
        assert!(g(&[
            "-s",
            "adb-PROTECTED0000EXAMPLE-abc123 (2)._adb-tls-connect._tcp",
            "shell",
            "ls"
        ])
        .is_err());
        assert!(g(&["-s", S24_SERIAL, "shell", "getprop ro.serialno"])
            .unwrap()
            .is_s24_read());
        let spellings = [
            S24_SERIAL,
            "protected0000example",
            "adb-PROTECTED0000EXAMPLE-abc123 (2)._adb-tls-connect._tcp",
        ];
        for sp in spellings {
            assert!(
                g(&["-s", sp, "shell", "getprop", "ro.serialno"]).is_ok(),
                "{sp}"
            );
            let near: [&[&str]; 11] = [
                &["shell", "ls"],
                &["shell", "getprop"],
                &["shell", "getprop", "ro.serialno;", "reboot"],
                &["shell", "getprop ro.serialno; reboot"],
                &["shell", "getprop ro.serialno && input tap 1 1"],
                &["shell", "getprop", "ro.serialno", "extra"],
                &["exec-out", "getprop", "ro.serialno"],
                &["shell", "getprop ro.serialno "],
                &["shell", "", "getprop", "ro.serialno"],
                &["shell", "getprop", "ro.boot.serialno"],
                &["connect", "getprop", "ro.serialno"],
            ];
            for n in near {
                let mut a = vec!["-s", sp];
                a.extend_from_slice(n);
                assert!(g(&a).is_err(), "{sp} {n:?}");
            }
        }
        assert!(g(&["connect", "adb-PROTECTED0000EXAMPLE-abc123"]).is_err());
        assert!(g(&["disconnect", S24_SERIAL]).is_err());
    }

    /// The approved file, compiled in. Lines that are blank or start with `#` are not commands.
    const APPROVED_S24_FILE: &str = include_str!("../../../probes/readonly-s24.txt");

    #[test]
    fn s24_reads_equal_the_approved_file() {
        let from_file: Vec<String> = APPROVED_S24_FILE
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| format!("shell {l}"))
            .collect();
        assert_eq!(from_file.len(), 10, "exactly 10 commands are approved");
        assert_eq!(
            S24_READS.to_vec(),
            from_file,
            "S24_READS and probes/readonly-s24.txt drifted"
        );
    }

    #[test]
    fn s24_answers_each_approved_probe_and_nothing_near_it() {
        for cmd in S24_READS {
            let words: Vec<&str> = cmd.split(' ').collect();
            let mut a = vec!["-s", S24_SERIAL];
            a.extend(words.iter());
            assert!(g(&a).unwrap().is_s24_read(), "{cmd}");
        }
        // full-list commands and variants of approved ones stay refused for the protected phone
        for cmd in [
            "dumpsys input",
            "getevent -pl",
            "settings list secure",
            "settings list global",
            "settings list system",
            "cmd display",
            "dumpsys display",
            "dumpsys deviceidle",
            "ime list -s",
            "cmd appops get com.android.shell",
            "wm size reset",
            "wm density 400",
            "dumpsys battery set level 5",
            "dumpsys battery ",
            "pm list features -u",
            "pm list packages",
            "dumpsys thermalservice;reboot",
            "getprop ro.product.brand",
            "getprop",
            "getprop ro.mi.os.version.name",
            "getprop ro.product.manufacturer",
        ] {
            let mut a = vec!["-s", S24_SERIAL, "shell"];
            a.push(cmd);
            assert!(
                g(&a).is_err(),
                "{cmd}: the guard refuses it for the protected phone outright"
            );
            let guarded = GuardedRunner::new(Spy {
                hw: S24_SERIAL.into(),
                calls: RefCell::new(vec![]),
            });
            let argv = args(&a);
            assert!(
                matches!(guarded.run(&argv, 1000), Err(RunError::Refused(_))),
                "{cmd}"
            );
        }
    }

    #[test]
    fn learnt_s24_address_is_refused() {
        let guard = Guard::new();
        assert!(guard.check(&args(&["connect", "192.0.2.99:40000"])).is_ok());
        guard.deny("192.0.2.99:40000");
        assert!(guard
            .check(&args(&["connect", "192.0.2.99:40000"]))
            .is_err());
        assert!(guard
            .check(&args(&["-s", "192.0.2.99:40000", "shell", "ls"]))
            .is_err());
    }

    /// Fake inner runner: records calls, answers getprop with a configured serial.
    struct Spy {
        hw: String,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl Runner for Spy {
        fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
            self.calls.borrow_mut().push(a.to_vec());
            let out = if a.last().map(String::as_str) == Some("getprop ro.serialno") {
                format!("{}\n", self.hw)
            } else {
                String::new()
            };
            Ok(RunOutput {
                code: Some(0),
                stdout: out,
                stderr: String::new(),
            })
        }
        fn run_bytes(&self, a: &[String], _t: u64) -> Result<crate::runner::BytesOutput, RunError> {
            self.calls.borrow_mut().push(a.to_vec());
            Ok(crate::runner::BytesOutput {
                code: Some(0),
                stdout: vec![0x89, b'P', b'N', b'G', 0, 255],
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn the_binary_path_is_guarded_like_the_text_path() {
        // the protected phone is refused for exec-out screencap, behind an address, and nothing is sent
        let s24 = GuardedRunner::new(Spy {
            hw: S24_SERIAL.into(),
            calls: RefCell::new(vec![]),
        });
        let argv = args(&["-s", "192.0.2.99:40000", "exec-out", "screencap", "-p"]);
        assert!(matches!(
            s24.run_bytes(&argv, 1000),
            Err(RunError::Refused(_))
        ));
        assert!(!s24
            .inner
            .calls
            .borrow()
            .iter()
            .any(|c| c.iter().any(|w| w == "screencap")));
        // an emulator passes, and the bytes come back untouched (a NUL and 0xFF survive)
        let emu = GuardedRunner::new(Spy {
            hw: "EMULATOR33X1".into(),
            calls: RefCell::new(vec![]),
        });
        let out = emu
            .run_bytes(
                &args(&["-s", "emulator-5554", "exec-out", "screencap", "-p"]),
                1000,
            )
            .unwrap();
        assert_eq!(out.stdout, vec![0x89, b'P', b'N', b'G', 0, 255]);
        // the argv guard applies too: no selector, no run
        assert!(emu
            .run_bytes(&args(&["exec-out", "screencap", "-p"]), 1000)
            .is_err());
    }

    #[test]
    fn guarded_runner_checks_hardware_serial_behind_an_ip() {
        let r = GuardedRunner::new(Spy {
            hw: S24_SERIAL.into(),
            calls: RefCell::new(vec![]),
        });
        let e = r
            .run(&args(&["-s", "192.0.2.99:40000", "shell", "ls"]), 1000)
            .unwrap_err();
        assert!(matches!(e, RunError::Refused(_)));
        // the real command never went out; and the address is now denied
        assert_eq!(r.inner.calls.borrow().len(), 1);
        assert!(r
            .run(&args(&["connect", "192.0.2.99:40000"]), 1000)
            .is_err());
        // the identity probe itself is allowed
        let ok = GuardedRunner::new(Spy {
            hw: S24_SERIAL.into(),
            calls: RefCell::new(vec![]),
        });
        assert!(ok
            .run(
                &args(&["-s", "192.0.2.99:40000", "shell", "getprop ro.serialno"]),
                1000
            )
            .is_ok());
    }

    #[test]
    fn guarded_runner_answers_the_s24_serial_only() {
        let r = GuardedRunner::new(Spy {
            hw: S24_SERIAL.into(),
            calls: RefCell::new(vec![]),
        });
        let ip = "192.0.2.99:40000";
        assert!(r
            .run(&args(&["-s", ip, "shell", "getprop ro.serialno"]), 1000)
            .is_ok());
        assert_eq!(r.inner.calls.borrow().len(), 1);
        assert!(r
            .run(&args(&["-s", ip, "shell", "input tap 1 1"]), 1000)
            .is_err());
        assert!(!r
            .inner
            .calls
            .borrow()
            .iter()
            .any(|c| c.iter().any(|w| w.contains("input tap"))));
        assert!(r.run(&args(&["connect", ip]), 1000).is_err()); // address now denied
                                                                // the serial read is still answered through the denied address
        assert!(r
            .run(&args(&["-s", ip, "shell", "getprop ro.serialno"]), 1000)
            .is_ok());
    }

    // ---- task 2.15: getevent shapes, full devices only, never the protected phone ----

    const POCO_HW: &str = "DEVICE0000EXAMPLE";

    #[test]
    fn getevent_exact_shapes_pass_for_a_full_device() {
        let guard = Guard::new();
        for shape in GETEVENT_SHAPES {
            let mut a = vec!["-s", POCO_HW];
            a.extend_from_slice(shape);
            let c = guard
                .check_getevent(&args(&a), true)
                .unwrap_or_else(|e| panic!("{shape:?}: {e}"));
            assert!(c.is_getevent_shape());
        }
        // also by address
        assert!(guard
            .check_getevent(
                &args(&["-s", "192.0.2.78:37123", "exec-out", "getevent", "-lt"]),
                true
            )
            .is_ok());
    }

    #[test]
    fn getevent_refused_for_helper_and_read_only_devices() {
        for shape in GETEVENT_SHAPES {
            let mut a = vec!["-s", POCO_HW];
            a.extend_from_slice(shape);
            let e = Guard::new().check_getevent(&args(&a), false).unwrap_err();
            assert!(e.0.contains("full devices only"), "{shape:?}: {e}");
        }
    }

    #[test]
    fn getevent_near_shapes_are_refused() {
        let near: [&[&str]; 14] = [
            &["exec-out", "getevent"],
            &["exec-out", "getevent", "-l"],
            &["exec-out", "getevent", "-lt", "/dev/input/event5"],
            &["exec-out", "getevent", "-ltq"],
            &["exec-out", "getevent", "-lt;", "reboot"],
            &["exec-out", "getevent -lt"],
            &["exec-out", "getevent", "-lt", "-c", "1"],
            &[
                "exec-out",
                "sendevent",
                "/dev/input/event5",
                "1",
                "330",
                "1",
            ],
            &["shell", "getevent", "-lt"],
            &["shell", "getevent -lt"],
            &["exec-out", "getevent", "-pl", "-lt"],
            &["exec-out", "getevent", "-PL"],
            &["exec-out", "getevent", ""],
            &["exec-out", "", "getevent", "-lt"],
        ];
        for n in near {
            let mut a = vec!["-s", POCO_HW];
            a.extend_from_slice(n);
            assert!(
                Guard::new().check_getevent(&args(&a), true).is_err(),
                "{n:?}"
            );
        }
        // no selector, or a selector that is not -s
        assert!(Guard::new()
            .check_getevent(&args(&["exec-out", "getevent", "-lt"]), true)
            .is_err());
        assert!(Guard::new()
            .check_getevent(&args(&["-d", "exec-out", "getevent", "-lt"]), true)
            .is_err());
        assert!(Guard::new()
            .check_getevent(&args(&["-t", "3", "exec-out", "getevent", "-lt"]), true)
            .is_err());
    }

    #[test]
    fn getevent_never_reaches_the_s24_in_any_spelling() {
        let spellings = [
            S24_SERIAL,
            "protected0000example",
            "adb-PROTECTED0000EXAMPLE-abc123 (2)._adb-tls-connect._tcp",
        ];
        for sp in spellings {
            for shape in GETEVENT_SHAPES {
                let mut a = vec!["-s", sp];
                a.extend_from_slice(shape);
                assert!(
                    Guard::new().check_getevent(&args(&a), true).is_err(),
                    "{sp} {shape:?}"
                );
                // and the plain guard refuses it too (getevent is not one of the 10 approved reads)
                assert!(g(&a).is_err(), "{sp} {shape:?}");
            }
        }
        for shape in GETEVENT_SHAPES {
            let line = shape[1..].join(" ");
            assert!(
                !S24_READS.iter().any(|r| r.ends_with(&line)),
                "{line} must never be an S24 read"
            );
        }
    }

    #[test]
    fn admit_getevent_checks_the_hardware_serial_behind_an_address() {
        let ip = "192.0.2.99:40000";
        let argv = args(&["-s", ip, "exec-out", "getevent", "-lt"]);
        // the protected phone behind an IP: refused, nothing but the identity probe went out, the address is denied
        let s24 = GuardedRunner::new(Spy {
            hw: S24_SERIAL.into(),
            calls: RefCell::new(vec![]),
        });
        assert!(matches!(
            s24.admit_getevent(&argv, true),
            Err(RunError::Refused(_))
        ));
        assert_eq!(s24.inner.calls.borrow().len(), 1);
        assert!(
            s24.inner.calls.borrow()[0].last().map(String::as_str) == Some("getprop ro.serialno")
        );
        assert!(s24.run(&args(&["connect", ip]), 1000).is_err());
        // the protected phone answering its serial in another case is still the protected phone
        let s24_lower = GuardedRunner::new(Spy {
            hw: S24_SERIAL.to_ascii_lowercase(),
            calls: RefCell::new(vec![]),
        });
        assert!(matches!(
            s24_lower.admit_getevent(&argv, true),
            Err(RunError::Refused(_))
        ));
        // a full POCO: admitted, and admission itself runs nothing but the identity probe
        let poco = GuardedRunner::new(Spy {
            hw: POCO_HW.into(),
            calls: RefCell::new(vec![]),
        });
        assert!(poco
            .admit_getevent(&argv, true)
            .unwrap()
            .is_getevent_shape());
        assert_eq!(poco.inner.calls.borrow().len(), 1);
        // a helper device: refused before anything goes out
        let helper = GuardedRunner::new(Spy {
            hw: POCO_HW.into(),
            calls: RefCell::new(vec![]),
        });
        assert!(helper.admit_getevent(&argv, false).is_err());
        assert!(helper.inner.calls.borrow().is_empty());
        // unidentifiable target: fails closed
        let bad = GuardedRunner::new(Spy {
            hw: "".into(),
            calls: RefCell::new(vec![]),
        });
        assert!(bad.admit_getevent(&argv, true).is_err());
    }

    #[test]
    fn guarded_runner_lets_other_phones_through_and_fails_closed() {
        let r = GuardedRunner::new(Spy {
            hw: "DEVICE0000EXAMPLE".into(),
            calls: RefCell::new(vec![]),
        });
        assert!(r
            .run(&args(&["-s", "192.0.2.78:37123", "shell", "ls"]), 1000)
            .is_ok());
        let bad = GuardedRunner::new(Spy {
            hw: "".into(),
            calls: RefCell::new(vec![]),
        });
        assert!(bad
            .run(&args(&["-s", "192.0.2.78:37123", "shell", "ls"]), 1000)
            .is_err());
        assert!(r.run(&args(&["kill-server"]), 1000).is_err());
        assert_eq!(bad.inner.calls.borrow().len(), 1); // only the identity probe
                                                       // the protected phone behind an address, answering its serial in lower case: still refused
        let s24_lower = GuardedRunner::new(Spy {
            hw: S24_SERIAL.to_ascii_lowercase(),
            calls: RefCell::new(vec![]),
        });
        assert!(s24_lower
            .run(&args(&["-s", "192.0.2.99:40000", "shell", "ls"]), 1000)
            .is_err());
        assert_eq!(s24_lower.inner.calls.borrow().len(), 1); // only the identity probe went out
    }

    // ---- task 2.12: the two link-pairing shapes

    const HUB_KEY: &str = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAERlQ5irFqp26nSBgVSVFj0Vh394elxmDCwS+evh9Xx8BCBHsxJEMMNc9gEvaHP7AefB46hnk8G3Ynj1icWWcF1g==";
    const POCO: &str = "DEVICE0000EXAMPLE";

    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    #[test]
    fn link_pairing_argv_are_exactly_the_documented_shapes() {
        assert_eq!(
            strs(&link_dump_argv(POCO)),
            [
                "-s",
                POCO,
                "shell",
                "dumpsys",
                "activity",
                "service",
                "dev.agentroid.helper/.link.LinkService"
            ]
        );
        assert_eq!(
            strs(&link_pair_argv(
                POCO,
                HUB_KEY,
                "100.101.102.103:8765",
                POCO,
                false
            )),
            [
                "-s",
                POCO,
                "shell",
                "am",
                "broadcast",
                "-a",
                "dev.agentroid.helper.PAIR",
                "-n",
                "dev.agentroid.helper/.link.ConfigReceiver",
                "--es",
                "hub_key",
                HUB_KEY,
                "--es",
                "hub",
                "100.101.102.103:8765",
                "--es",
                "serial",
                POCO,
            ]
        );
        let rekey = link_pair_argv(POCO, HUB_KEY, "100.64.0.1:1", POCO, true);
        assert_eq!(strs(&rekey[18..]), ["--ez", "rekey", "true"]);
    }

    #[test]
    fn link_pairing_shapes_are_allowed_for_full_devices() {
        let dump = g(&strs(&link_dump_argv(POCO))).unwrap();
        assert_eq!(dump.link_command(), Some(LinkCommand::DumpPublicKey));
        assert!(!dump.is_s24_read());
        for rekey in [false, true] {
            let pair = g(&strs(&link_pair_argv(
                POCO,
                HUB_KEY,
                "100.101.102.103:8765",
                POCO,
                rekey,
            )))
            .unwrap();
            assert_eq!(pair.link_command(), Some(LinkCommand::Pair));
            assert!(!pair.is_s24_read());
        }
        // through the guarded runner, behind an address, on a full device: the identity probe, then the command
        let r = GuardedRunner::new(Spy {
            hw: POCO.into(),
            calls: RefCell::new(vec![]),
        });
        let ip = "192.0.2.78:37123";
        r.run(&link_dump_argv(ip), 1000).unwrap();
        r.run(
            &link_pair_argv(ip, HUB_KEY, "100.101.102.103:8765", POCO, false),
            1000,
        )
        .unwrap();
        let calls = r.inner.calls.borrow();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[1], link_dump_argv(ip));
        assert_eq!(
            calls[3],
            link_pair_argv(ip, HUB_KEY, "100.101.102.103:8765", POCO, false)
        );
    }

    #[test]
    fn near_link_pairing_shapes_are_refused() {
        let pair =
            |key: &str, hub: &str, serial: &str| link_pair_argv(POCO, key, hub, serial, false);
        let private =
            "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg".to_string() + &"A".repeat(76);
        let mut cases: Vec<Vec<String>> = vec![
            pair(&private, "100.101.102.103:8765", POCO), // a private key
            pair(&HUB_KEY[..120], "100.101.102.103:8765", POCO), // truncated
            pair(&HUB_KEY.replace('+', " "), "100.101.102.103:8765", POCO),
            pair(HUB_KEY, "192.168.1.10:8765", POCO), // LAN hub
            pair(HUB_KEY, "127.0.0.1:8765", POCO),    // loopback (the phone refuses it too)
            pair(HUB_KEY, "100.100.100.100:8765", POCO), // Tailscale DNS
            pair(HUB_KEY, "100.128.0.1:8765", POCO),  // outside /10
            pair(HUB_KEY, "100.064.0.1:8765", POCO),  // leading zero
            pair(HUB_KEY, "100.101.102.103:0", POCO),
            pair(HUB_KEY, "100.101.102.103:65536", POCO),
            pair(HUB_KEY, "100.101.102.103", POCO),
            pair(HUB_KEY, "laptop.tail1.ts.net:8765", POCO),
            pair(HUB_KEY, "100.101.102.103:8765", "X;reboot"), // shell metacharacters
            pair(HUB_KEY, "100.101.102.103:8765", "has space"),
            pair(HUB_KEY, "100.101.102.103:8765;reboot", POCO),
        ];
        let mut extra = pair(HUB_KEY, "100.101.102.103:8765", POCO);
        extra.push("--es".into());
        extra.push("laptop".into());
        cases.push(extra);
        let mut wrong_flag = link_pair_argv(POCO, HUB_KEY, "100.101.102.103:8765", POCO, true);
        wrong_flag[20] = "false".into();
        cases.push(wrong_flag);
        let mut swapped = pair(HUB_KEY, "100.101.102.103:8765", POCO);
        swapped.swap(10, 13);
        cases.push(swapped);
        for words in [
            vec![
                "shell",
                "dumpsys activity service dev.agentroid.helper/.link.LinkService",
            ],
            vec![
                "shell",
                "dumpsys",
                "activity",
                "service",
                "dev.agentroid.helper/.link.LinkService",
                "--extra",
            ],
            vec![
                "exec-out",
                "dumpsys",
                "activity",
                "service",
                "dev.agentroid.helper/.link.LinkService",
            ],
            vec![
                "shell",
                "dumpsys",
                "activity",
                "service",
                "dev.agentroid.helper/.link.LinkService;reboot",
            ],
            vec![
                "shell",
                "am",
                "broadcast",
                "-a",
                "dev.agentroid.helper.PAIR",
            ],
            vec![
                "shell",
                "am",
                "broadcast",
                "-a",
                "DEV.AGENTROID.HELPER.PAIR",
                "-n",
                "dev.agentroid.helper/.link.ConfigReceiver",
            ],
            vec![
                "shell",
                "am",
                "startservice",
                "dev.agentroid.helper/.link.LinkService",
            ],
        ] {
            let mut a = vec!["-s".to_string(), POCO.to_string()];
            a.extend(words.iter().map(|s| s.to_string()));
            cases.push(a);
        }
        for c in cases {
            let e = Guard::new()
                .check(&c)
                .expect_err(&format!("should refuse {c:?}"));
            assert!(
                e.0.contains("link-pairing") || e.0.contains("S24"),
                "{c:?}: {}",
                e.0
            );
        }
        // without a selector the exact shapes are still refused (no default device)
        let mut bare = link_dump_argv(POCO);
        bare.drain(..2);
        assert!(Guard::new().check(&bare).is_err());
    }

    #[test]
    fn s24_link_pairing_is_refused() {
        // by serial, in every spelling: the guard refuses before anything runs
        for sp in [
            S24_SERIAL,
            "protected0000example",
            "adb-PROTECTED0000EXAMPLE-abc123 (2)._adb-tls-connect._tcp",
        ] {
            for argv in [
                link_dump_argv(sp),
                link_pair_argv(sp, HUB_KEY, "100.101.102.103:8765", S24_SERIAL, false),
            ] {
                let e = Guard::new().check(&argv).expect_err(&format!("{argv:?}"));
                assert!(e.0.contains("S24"), "{}", e.0);
            }
        }
        // behind an address: the runner reads ro.serialno, sees the protected phone and sends nothing else
        let ip = "192.0.2.99:40000";
        for argv in [
            link_dump_argv(ip),
            link_pair_argv(ip, HUB_KEY, "100.101.102.103:8765", S24_SERIAL, true),
        ] {
            let r = GuardedRunner::new(Spy {
                hw: S24_SERIAL.into(),
                calls: RefCell::new(vec![]),
            });
            assert!(
                matches!(r.run(&argv, 1000), Err(RunError::Refused(_))),
                "{argv:?}"
            );
            let calls = r.inner.calls.borrow();
            assert_eq!(calls.len(), 1, "only the identity probe reached adb");
            assert_eq!(
                calls[0].last().map(String::as_str),
                Some("getprop ro.serialno")
            );
        }
        // and the protected phone's approved list is untouched: exactly the 10 approved reads, none of them a link shape
        assert_eq!(S24_READS.len(), 10);
        assert!(S24_READS.iter().all(|r| !r.contains("agentroid")
            && !r.contains("broadcast")
            && !r.contains("activity service")));
    }
}
