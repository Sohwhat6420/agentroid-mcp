//! `bridged`: task 2.3 and 2.8.
//!
//! ```text
//! bridged                         run the daemon on \\.\pipe\agentroid
//! bridged --fake                  same, with core's fake device
//! bridged --adb PATH|auto         same, with the live adb transport (live.rs); PATH is adb.exe, `auto`
//!                                 looks for scrcpy's adb first, then ANDROID_HOME / ANDROID_SDK_ROOT, then
//!                                 %LOCALAPPDATA%\Android\Sdk\platform-tools, else plain `adb` on PATH
//! bridged --adb auto --allow-serial SERIAL [--allow-serial SERIAL2 ...] [--allow-file FILE]
//!                                 NOTHING is driven unless named: a hardware serial per --allow-serial,
//!                                 one per line in FILE (default <state dir>\allow.txt, `#` comments)
//! bridged --adb auto --allow-emulators
//!                                 also drive `emulator-*` serials
//! bridged --stop                  ask a running daemon to stop (writes the stop file)
//! bridged --print-schema [PATH]   write the IPC JSON Schema (default generated/ipc.schema.json)
//! options: --pipe NAME  --state-dir DIR
//!          --link-bind ADDR [--link-port N]   run the link hub (the tailnet address; never 0.0.0.0)
//!          --link-lan                         also allow a private home-LAN ADDR, served as wss://
//!                                             (TLS 1.3, the certificate the pairing QR pins)
//!          --view-dist DIR [--view-port N]    serve the job view (127.0.0.1; plus the link address
//!                                             when --link-bind is a tailnet address)
//!          --profiles DIR                     agent/human profiles for the take-over (2.15)
//!          --approval-topic-file PATH         D3b: the ntfy topic (a key file, never inline) that
//!                                             carries "asked" verdicts to a human; absent, every
//!                                             asked verdict stays refused
//!          --approval-inbound-file PATH       a second topic file for the answers (default: same topic)
//!          --ntfy-base URL                    the ntfy server (default https://ntfy.sh)
//!          --bench-approve                    BENCHMARKS ONLY: answer every "asked" verdict with allow,
//!                                             audited as `bench`, never as a human. Refused unless the
//!                                             allow list is exactly --allow-emulators, and the channel
//!                                             itself refuses any non-emulator hardware serial
//!          --audit-anchor PATH                D3b: a second copy of the audit chain head, written
//!                                             after every record (put it off this machine: a synced
//!                                             folder, a network share); best effort by default
//!                                             (by design): a failed write is
//!                                             counted and logged, the call goes ahead
//!          --audit-anchor-required            ... refuse the call when the anchor cannot be written
//!          --audit-anchor-best-effort         accepted for compatibility (it is the default)
//!          --ask-screen-state-dir DIR         where `agentroid ask-screen` keeps its leases (default:
//!                                             its own default); the screen lock reads them there
//! ```
//!
//! **The screen lock** (with `--adb`): every screen-changing call to a real phone is refused unless
//! `agentroid ask-screen` gave a lease that is still live, or the phone is not in use at the time
//! of the call (then the agent's own burst goes on until someone else touches it). Emulators are
//! not locked. See `agentroid_core::screenlock`.
//!
//! care (2.13) runs inside a job only when `JobsConfig.care` is set; `--adb` wires the transport and
//! `agentroid.shell` but not care yet (heat guard and battery floor are step 11, on the POCO), so
//! `--profiles` is still unused.
//!
//! On Windows this is a GUI-subsystem binary: no console window can ever appear, so there is no
//! stdout; progress goes to `bridged.log` in the state directory.

#![cfg_attr(windows, windows_subsystem = "windows")]

use agentroid_adb_link::guard::GuardedRunner;
use agentroid_adb_link::runner::{AdbRunner, SystemClock};
use agentroid_core::fake::{FakeClock, FakeDevice};
use agentroid_core::gate::{self, Call, DeviceMode};
use agentroid_core::transport::{Core, CoreError, DeviceInfo, Outcome, TransportError};
use bridged::server::{self, CoreBackend, Daemon, DeviceBackend};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tokio::sync::watch;

/// Where the protected-device lease keeps a Deny's hold, in the state directory.
const PROTECTED_HOLDS_FILE: &str = "protected-holds.json";

/// Backend used when neither --fake nor --adb is given: no devices, so nothing can be leased or driven.
struct NoDevices;

impl DeviceBackend for NoDevices {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        Ok(vec![])
    }
    fn mode_of(&self, d: &DeviceInfo) -> DeviceMode {
        gate::effective_mode(&[&d.serial, &d.hw_serial], DeviceMode::default())
    }
    fn call(&mut self, _serial: &str, _call: &Call) -> Result<Outcome, CoreError> {
        Err(CoreError::Invalid(
            "bridged was started without --adb (or --fake): no transport".into(),
        ))
    }
}

struct Args {
    fake: bool,
    adb: Option<String>,
    allow_emulators: bool,
    /// `--allow-serial`, repeatable: hardware serials the daemon may drive (nothing is allowed by default).
    allow_serials: Vec<String>,
    /// `--allow-file`: one serial per line; default `<state dir>/allow.txt` when it exists.
    allow_file: Option<PathBuf>,
    stop: bool,
    print_schema: Option<PathBuf>,
    pipe: String,
    state_dir: PathBuf,
    link_bind: Option<String>,
    link_lan: bool,
    link_port: u16,
    view_dist: Option<PathBuf>,
    view_port: u16,
    profiles: Option<PathBuf>,
    /// D3b `--approval-topic-file`: the ntfy topic key file; `None` keeps every `asked` refused.
    approval_topic_file: Option<PathBuf>,
    approval_inbound_file: Option<PathBuf>,
    ntfy_base: Option<String>,
    /// `--bench-approve`: the emulator-only stand-in approver (AndroidWorld "with approvals" score).
    bench_approve: bool,
    /// D3b `--audit-anchor`: the off-machine head copy.
    audit_anchor: Option<PathBuf>,
    /// Off by default: a failed anchor write is counted, not refused (by design).
    audit_anchor_required: bool,
    /// `--ask-screen-state-dir`: where the screen lock reads ask-screen's leases.
    ask_screen_state_dir: Option<PathBuf>,
    /// `--protected-lease`: the protected phone runs under a lease the owner allows (off by default).
    protected_lease: bool,
    protected_lease_min: Option<u64>,
    protected_ask_window_min: Option<u64>,
    /// `--presence-file` and `--presence-key-file`: the overnight rule's signed presence file.
    presence_file: Option<PathBuf>,
    presence_key_file: Option<PathBuf>,
    presence_ttl_min: Option<u64>,
}

fn minutes(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    it.next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("{flag} needs a number of minutes"))
}

fn num(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<u16, String> {
    it.next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("{flag} needs a port number"))
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("agentroid"))
        .unwrap_or_else(|| PathBuf::from(".agentroid"))
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        fake: false,
        adb: None,
        allow_emulators: false,
        bench_approve: false,
        allow_serials: vec![],
        allow_file: None,
        stop: false,
        print_schema: None,
        pipe: bridged::PIPE_NAME.to_string(),
        state_dir: default_state_dir(),
        link_bind: None,
        link_lan: false,
        link_port: 8765,
        view_dist: None,
        view_port: 0,
        profiles: None,
        approval_topic_file: None,
        approval_inbound_file: None,
        ntfy_base: None,
        audit_anchor: None,
        audit_anchor_required: false,
        ask_screen_state_dir: None,
        protected_lease: false,
        protected_lease_min: None,
        protected_ask_window_min: None,
        presence_file: None,
        presence_key_file: None,
        presence_ttl_min: None,
    };
    let mut it = std::env::args().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--fake" => a.fake = true,
            "--adb" => a.adb = Some(it.next().ok_or("--adb needs a path to adb.exe, or auto")?),
            "--allow-emulators" => a.allow_emulators = true,
            "--bench-approve" => a.bench_approve = true,
            "--allow-serial" => a
                .allow_serials
                .push(it.next().ok_or("--allow-serial needs a hardware serial")?),
            "--allow-file" => {
                a.allow_file = Some(PathBuf::from(it.next().ok_or("--allow-file needs a path")?))
            }
            "--stop" => a.stop = true,
            "--print-schema" => {
                let path = match it.peek() {
                    Some(p) if !p.starts_with("--") => PathBuf::from(it.next().unwrap_or_default()),
                    _ => PathBuf::from("generated/ipc.schema.json"),
                };
                a.print_schema = Some(path);
            }
            "--pipe" => a.pipe = it.next().ok_or("--pipe needs a name")?,
            "--state-dir" => {
                a.state_dir = PathBuf::from(it.next().ok_or("--state-dir needs a directory")?)
            }
            "--link-bind" => a.link_bind = Some(it.next().ok_or("--link-bind needs an address")?),
            "--link-port" => a.link_port = num(&mut it, "--link-port")?,
            "--link-lan" => a.link_lan = true,
            "--view-dist" => {
                a.view_dist = Some(PathBuf::from(
                    it.next().ok_or("--view-dist needs a directory")?,
                ))
            }
            "--view-port" => a.view_port = num(&mut it, "--view-port")?,
            "--profiles" => {
                a.profiles = Some(PathBuf::from(
                    it.next().ok_or("--profiles needs a directory")?,
                ))
            }
            "--ask-screen-state-dir" => {
                a.ask_screen_state_dir = Some(PathBuf::from(
                    it.next()
                        .ok_or("--ask-screen-state-dir needs a directory")?,
                ))
            }
            "--approval-topic-file" => {
                a.approval_topic_file = Some(PathBuf::from(
                    it.next().ok_or("--approval-topic-file needs a path")?,
                ))
            }
            "--approval-inbound-file" => {
                a.approval_inbound_file = Some(PathBuf::from(
                    it.next().ok_or("--approval-inbound-file needs a path")?,
                ))
            }
            "--ntfy-base" => a.ntfy_base = Some(it.next().ok_or("--ntfy-base needs a URL")?),
            "--audit-anchor" => {
                a.audit_anchor = Some(PathBuf::from(
                    it.next().ok_or("--audit-anchor needs a path")?,
                ))
            }
            "--protected-lease" => a.protected_lease = true,
            "--protected-lease-min" => {
                a.protected_lease_min = Some(minutes(&mut it, "--protected-lease-min")?)
            }
            "--protected-ask-window-min" => {
                a.protected_ask_window_min = Some(minutes(&mut it, "--protected-ask-window-min")?)
            }
            "--presence-file" => {
                a.presence_file = Some(PathBuf::from(
                    it.next().ok_or("--presence-file needs a path")?,
                ))
            }
            "--presence-key-file" => {
                a.presence_key_file = Some(PathBuf::from(
                    it.next().ok_or("--presence-key-file needs a path")?,
                ))
            }
            "--presence-ttl-min" => {
                a.presence_ttl_min = Some(minutes(&mut it, "--presence-ttl-min")?)
            }
            "--audit-anchor-required" => a.audit_anchor_required = true,
            "--audit-anchor-best-effort" => a.audit_anchor_required = false,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

/// The adb that ships with scrcpy (installed by WinGet). Preferring it keeps every adb client on one
/// build: a client of a different build kills and restarts the adb server, which drops every
/// wireless connection.
fn scrcpy_adb() -> Option<PathBuf> {
    let pkgs = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("Microsoft")
        .join("WinGet")
        .join("Packages");
    for pkg in std::fs::read_dir(pkgs).ok()?.flatten() {
        if !pkg
            .file_name()
            .to_string_lossy()
            .starts_with("Genymobile.scrcpy")
        {
            continue;
        }
        for sub in std::fs::read_dir(pkg.path()).ok()?.flatten() {
            let exe = sub.path().join("adb.exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// `--adb auto`: scrcpy's adb (see [`scrcpy_adb`]), then ANDROID_HOME / ANDROID_SDK_ROOT, then the
/// default SDK folder, else plain `adb` on PATH.
fn resolve_adb(value: &str) -> PathBuf {
    if value != "auto" {
        return PathBuf::from(value);
    }
    if let Some(p) = scrcpy_adb() {
        return p;
    }
    let sdk_roots = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from);
    let default =
        std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("Android").join("Sdk"));
    for root in sdk_roots.chain(default) {
        let exe = root
            .join("platform-tools")
            .join(if cfg!(windows) { "adb.exe" } else { "adb" });
        if exe.exists() {
            return exe;
        }
    }
    PathBuf::from("adb")
}

fn log(dir: &Path, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("bridged.log"))
    {
        let _ = writeln!(f, "{msg}");
    }
}

fn write_schema(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    std::fs::write(path, bridged::proto::ipc_schema())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("bridged: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(path) = &args.print_schema {
        return match write_schema(path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("bridged: cannot write {}: {e}", path.display());
                ExitCode::FAILURE
            }
        };
    }
    let stop_file = args.state_dir.join("stop");
    if args.stop {
        let _ = std::fs::create_dir_all(&args.state_dir);
        return if std::fs::write(&stop_file, b"stop\n").is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let _ = std::fs::create_dir_all(&args.state_dir);
    match run(&args, &stop_file) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log(&args.state_dir, &format!("bridged failed: {e}"));
            eprintln!("bridged: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(any(windows, unix)))]
fn run(_args: &Args, _stop_file: &Path) -> Result<(), String> {
    Err("the daemon needs a named pipe (Windows) or a Unix socket".into())
}

#[cfg(windows)]
fn bind_listener(name: &str) -> std::io::Result<bridged::pipe::PipeListener> {
    bridged::pipe::PipeListener::bind(name)
}

#[cfg(unix)]
fn bind_listener(name: &str) -> std::io::Result<bridged::unix_socket::UnixSocketListener> {
    bridged::unix_socket::UnixSocketListener::bind(name)
}

/// Why `--bench-approve` must not start, or `None`. It needs the live transport, no ntfy topic, and
/// an allow list that is exactly the emulator prefix (so no phone can be driven at all).
fn bench_approve_refusal(args: &Args, allow: &[String]) -> Option<String> {
    if !args.bench_approve {
        return None;
    }
    if args.approval_topic_file.is_some() {
        return Some("--bench-approve and --approval-topic-file are alternatives".into());
    }
    let only_emulators =
        !allow.is_empty() && allow.iter().all(|e| e == bridged::allow::EMULATOR_PREFIX);
    if !args.allow_emulators || !only_emulators {
        return Some(
            "--bench-approve drives emulators only: pass --allow-emulators and no --allow-serial, --allow-file or allow.txt entries".into(),
        );
    }
    None
}

/// Why `--protected-lease` (or one of its flags) must not start, or `None`. The lease needs the
/// live transport and a real approval channel whose answers come on a topic of their own (so a
/// process that reads the outbound push cannot answer it); the presence file and its key come as
/// a pair.
fn protected_lease_refusal(args: &Args) -> Option<String> {
    let extras = args.protected_lease_min.is_some()
        || args.protected_ask_window_min.is_some()
        || args.presence_file.is_some()
        || args.presence_key_file.is_some()
        || args.presence_ttl_min.is_some();
    if !args.protected_lease {
        return extras.then(|| {
            "--protected-lease-min, --protected-ask-window-min and the --presence-* flags need --protected-lease".into()
        });
    }
    if args.adb.is_none() {
        return Some("--protected-lease needs --adb (the live transport)".into());
    }
    if args.bench_approve {
        return Some("--protected-lease never runs with --bench-approve".into());
    }
    let (Some(out), Some(inb)) = (&args.approval_topic_file, &args.approval_inbound_file) else {
        return Some("--protected-lease needs an approval channel: --approval-topic-file and --approval-inbound-file (the answers on a topic of their own)".into());
    };
    let topic = |p: &Path| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
    };
    match (topic(out), topic(inb)) {
        (Some(a), Some(b)) if !a.is_empty() && a != b => {}
        (Some(_), Some(_)) => {
            return Some("--protected-lease needs the inbound topic to differ from the outbound one (otherwise anything that reads the push could answer it)".into())
        }
        _ => return Some("--protected-lease: the approval topic files cannot be read".into()),
    }
    if args.presence_file.is_some() != args.presence_key_file.is_some() {
        return Some("--presence-file and --presence-key-file come together".into());
    }
    None
}

#[cfg(any(windows, unix))]
fn run(args: &Args, stop_file: &Path) -> Result<(), String> {
    let registry = bridged::registry::Registry::open(args.state_dir.join("state.json"))
        .map_err(|e| format!("state file: {e}"))?;
    if args.fake && args.adb.is_some() {
        return Err("--fake and --adb are alternatives".into());
    }
    if args.adb.is_none()
        && (!args.allow_serials.is_empty() || args.allow_file.is_some() || args.allow_emulators)
    {
        return Err("--allow-serial, --allow-file and --allow-emulators need --adb (the fake device has a fixed allowlist)".into());
    }
    let adb_path = args.adb.as_deref().map(resolve_adb);
    if let Some(why) = protected_lease_refusal(args) {
        return Err(why);
    }
    // the protected-device lease book, shared by core and the adb guard (one Allow, one lease)
    let protected_book = args.protected_lease.then(|| {
        std::sync::Arc::new(agentroid_adb_link::lease::LeaseBook::new(
            std::sync::Arc::new(agentroid_adb_link::lease::SystemWallClock),
        ))
    });
    let backend: Box<dyn DeviceBackend> = if args.fake {
        Box::new(CoreBackend::new(Core::new(
            FakeDevice::new(),
            FakeClock::new(),
        )))
    } else if let Some(path) = &adb_path {
        log(
            &args.state_dir,
            &format!("live adb transport via {}", path.display()),
        );
        let runner = match &protected_book {
            Some(book) => GuardedRunner::with_leases(AdbRunner::new(path), book.clone()),
            None => GuardedRunner::new(AdbRunner::new(path)),
        };
        let mut transport = bridged::live::AdbTransport::new(runner);
        if protected_book.is_some() {
            // the protected phone's heads-up check, through its own single-purpose runner
            transport = transport.with_heads_up_runner(Box::new(GuardedRunner::with_s24_heads_up(
                AdbRunner::new(path),
            )));
        }
        let mut core = Core::new(transport, SystemClock::new());
        // Nothing is allowed unless named: --allow-serial, the allow file, or --allow-emulators
        // (allowed() matches a hardware serial or an adb serial exactly, or the `adb-<serial>-` prefix of a wireless name; an emulator is `emulator-<port>`).
        let allow = bridged::allow::load(
            &args.allow_serials,
            args.allow_file.as_deref(),
            &args.state_dir,
            args.allow_emulators,
        )
        .map_err(|e| format!("allowlist: {e}"))?;
        log(
            &args.state_dir,
            &format!(
                "allowlist: {} entries{}",
                allow.len(),
                if allow.is_empty() {
                    " (no device can be driven: pass --allow-serial <hardware serial>, list it in allow.txt, or use --allow-emulators)"
                } else {
                    ""
                }
            ),
        );
        if let Some(why) = bench_approve_refusal(args, &allow) {
            return Err(why);
        }
        core.set_allow(&allow.iter().map(String::as_str).collect::<Vec<_>>());
        // D3: every acting call and every refusal is chained into <state dir>\audit.jsonl before it
        // is sent; `agentroid audit verify` checks it. A broken log is never appended to.
        // D3b: the file rotates (10 000 records or 7 days; the next file's first record carries
        // the previous file's final hash) and the chain head is copied to --audit-anchor.
        let audit_path = args.state_dir.join("audit.jsonl");
        let opts = agentroid_core::audit::Options {
            rotation: Some(agentroid_core::audit::Rotation::default()),
            anchor: args
                .audit_anchor
                .clone()
                .map(|path| agentroid_core::audit::Anchor {
                    path,
                    required: args.audit_anchor_required,
                }),
        };
        let audit = agentroid_core::audit::AuditLog::open_with(&audit_path, opts)
            .map_err(|e| format!("audit log: {e}"))?;
        log(
            &args.state_dir,
            &format!(
                "audit log {} (seq {}, rotation on, anchor {})",
                audit_path.display(),
                audit.head().0,
                match &args.audit_anchor {
                    Some(p) => format!(
                        "{} ({})",
                        p.display(),
                        if args.audit_anchor_required {
                            "required"
                        } else {
                            "best effort"
                        }
                    ),
                    None => "none".to_string(),
                }
            ),
        );
        core.set_audit(audit);
        // D3b: the approval channel. The topic is a key file named here, never inline; without the
        // flag every `asked` verdict stays refused.
        match &args.approval_topic_file {
            _ if args.bench_approve => {
                core.set_approver(agentroid_core::approval::Approver::new(Box::new(
                    agentroid_core::approval::BenchChannel::default(),
                )));
                log(
                    &args.state_dir,
                    "approval channel: BENCH (every asked verdict on an emulator is allowed and audited as bench; benchmarks only)",
                );
            }
            Some(topic_file) => {
                let mut cfg = agentroid_core::approval::NtfyConfig::new(topic_file.clone());
                cfg.inbound_topic_file = args.approval_inbound_file.clone();
                // the lease's answers must come on a topic of their own, checked on every send and poll
                cfg.require_separate_inbound = args.protected_lease;
                if let Some(b) = &args.ntfy_base {
                    cfg.base = b.trim_end_matches('/').to_string();
                }
                let channel = agentroid_core::approval::NtfyChannel::new(
                    cfg,
                    agentroid_core::approval::CurlHttp::default(),
                );
                core.set_approver(agentroid_core::approval::Approver::new(Box::new(channel)));
                log(
                    &args.state_dir,
                    &format!(
                        "approval channel: ntfy, topic file {} ({})",
                        topic_file.display(),
                        if topic_file.is_file() { "present" } else { "MISSING: asked verdicts will be refused" }
                    ),
                );
            }
            None => log(
                &args.state_dir,
                "approval channel: none (every `asked` verdict is refused; pass --approval-topic-file)",
            ),
        }
        // The screen lock (13 Nov 2026): no screen-changing call to a real phone unless
        // ask-screen gave a live lease or the phone is not in use at the time of the call. There
        // is no switch to turn it off.
        let lock_dir = args
            .ask_screen_state_dir
            .clone()
            .unwrap_or_else(agentroid_core::askscreen::default_state_dir);
        log(
            &args.state_dir,
            &format!(
                "screen lock: on (leases from {}; emulators that say they are one are not locked)",
                lock_dir.display()
            ),
        );
        bridged::live::install_screen_lock(&mut core, lock_dir, path);
        if let Some(book) = &protected_book {
            use agentroid_core::protected::{PresenceConfig, Protected, ProtectedConfig};
            let presence = match (&args.presence_file, &args.presence_key_file) {
                (Some(f), Some(k)) => Some(PresenceConfig::new(
                    f,
                    k,
                    args.presence_ttl_min
                        .unwrap_or(agentroid_core::protected::DEFAULT_PRESENCE_TTL_MIN),
                )),
                _ => None,
            };
            let cfg = ProtectedConfig::new(
                args.protected_lease_min
                    .unwrap_or(agentroid_adb_link::lease::DEFAULT_MINUTES),
                args.protected_ask_window_min
                    .unwrap_or(agentroid_core::protected::DEFAULT_ASK_WINDOW_MIN),
                presence,
            )
            // a Deny's hold outlives a restart
            .with_holds_file(args.state_dir.join(PROTECTED_HOLDS_FILE));
            log(
                &args.state_dir,
                &format!(
                    "protected-device lease: on ({} min leases, {} min to answer an ask, overnight rule {})",
                    cfg.lease_ms / 60_000,
                    cfg.ask_window_ms / 60_000,
                    if cfg.presence.is_some() { "on (signed presence file)" } else { "off" }
                ),
            );
            core.set_protected(Protected::new(cfg, book.clone()));
        }
        Box::new(CoreBackend::new(core))
    } else {
        Box::new(NoDevices)
    };
    // jobs (2.10b): one directory per job under <state dir>\runs. agentroid.shell gets the same adb
    // (behind the guard) when --adb is given; without it the call answers "no adb runner", unsent.
    let mut jobs = bridged::jobs::JobsConfig::new(args.state_dir.join("runs"));
    if let Some(path) = &adb_path {
        jobs = jobs.with_shell_runner(std::sync::Arc::new(AdbRunner::new(path)));
    }
    let _ = &args.profiles; // the take-over needs care, which needs the live adb runner (not wired)
    let daemon = Daemon::with_jobs(registry, backend, Some(jobs));
    if let Some(book) = protected_book {
        daemon.attach_protected(book)?;
    }
    // session stay-awake (bridged::awake): live adb only; read_only devices are refused by care,
    // except the protected phone's one exception (the two screen keys, nothing else) unless AGENTROID_NO_S24_AWAKE=1
    if let Some(path) = &adb_path {
        let (on, idle_ms) = bridged::awake::from_env();
        if on {
            let s24 = bridged::awake::s24_from_env();
            let cfg = bridged::awake::AwakeConfig {
                adb: bridged::jobs::DynRunner(std::sync::Arc::new(AdbRunner::new(path))),
                s24,
                // phone-guard's store when PHONE_GUARD_LEASES names it, as care's LeaseStore::from_env_or does
                leases: std::env::var_os("PHONE_GUARD_LEASES")
                    .filter(|p| !p.is_empty())
                    .map_or_else(|| args.state_dir.join("leases.json"), PathBuf::from),
                clock: std::sync::Arc::new(care::clock::SystemClock::new()),
                pids: std::sync::Arc::new(care::pid::OsPids),
                idle_ms,
            };
            daemon.attach_awake(cfg, Duration::from_secs(30))?;
            log(
                &args.state_dir,
                &format!(
                    "stay-awake on: idle restore after {} s; S24 screen keys {}",
                    idle_ms / 1000,
                    if s24 {
                        "on"
                    } else {
                        "off (AGENTROID_NO_S24_AWAKE=1)"
                    }
                ),
            );
        } else {
            log(
                &args.state_dir,
                "stay-awake off (AGENTROID_NO_STAY_AWAKE=1)",
            );
        }
    }
    // push reads only <state dir>\files\outbox; pull_new writes only <state dir>\files\inbox
    daemon.set_files_dir(args.state_dir.join("files"));
    // the link hub (2.12): only when an address is given; link-hub refuses 0.0.0.0 and the LAN
    let mut hub_ip: Option<std::net::IpAddr> = None;
    if let Some(bind) = &args.link_bind {
        let key = link_hub::crypto::HubKey::load_or_create(link_hub::crypto::HUB_KEY_TARGET)
            .map_err(|e| e.0)?;
        let tls = if args.link_lan {
            let (t, created) = link_hub::tls::HubTls::load_or_create(
                link_hub::tls::TLS_KEY_TARGET,
                &args.state_dir.join("hub-state"),
            )
            .map_err(|e| e.to_string())?;
            if created {
                log(
                    &args.state_dir,
                    "new link TLS certificate: LAN-paired phones must re-pair",
                );
            }
            log(
                &args.state_dir,
                &format!("link TLS fingerprint {}", t.fingerprint_hex()),
            );
            Some(std::sync::Arc::new(t))
        } else {
            None
        };
        let cfg = bridged::link::LinkConfig {
            state_dir: args.state_dir.clone(),
            bind: bind.clone(),
            port: args.link_port,
            key,
            clock: None,
            tls,
            lan: args.link_lan,
        };
        let link = daemon.attach_link(cfg)?;
        log(
            &args.state_dir,
            &format!("link hub on {}", link.local_addr()),
        );
        hub_ip = bind
            .parse()
            .ok()
            .filter(|ip| bridged::jobview::is_tailnet(*ip));
    }
    // the job view (2.11b): 127.0.0.1, plus the tailnet address in hub mode
    let _view = match &args.view_dist {
        Some(dist) => {
            let mut cfg = bridged::jobview::JobViewConfig::localhost(dist);
            cfg.port = args.view_port;
            cfg.state_dir = Some(args.state_dir.clone());
            if let Some(ip) = hub_ip {
                cfg.bind.push(ip);
                cfg.hub_mode = true;
            }
            let v = bridged::jobview::JobView::start(daemon.clone(), cfg)?;
            log(&args.state_dir, &format!("job view on {}", v.url()));
            Some(v)
        }
        None => None,
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let listener = bind_listener(&args.pipe).map_err(|e| {
            format!(
                "cannot claim {}: {e} (is another bridged running?)",
                args.pipe
            )
        })?;
        let _ = std::fs::remove_file(stop_file); // a stale stop file must not stop a fresh start
        let (tx, rx) = watch::channel(false);
        let tx_c = tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = tx_c.send(true);
            }
        });
        let stop = stop_file.to_path_buf();
        let tx_s = tx.clone();
        tokio::spawn(async move {
            loop {
                if stop.exists() {
                    let _ = std::fs::remove_file(&stop);
                    let _ = tx_s.send(true);
                    return;
                }
                if tx_s.is_closed() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
        log(
            &args.state_dir,
            &format!(
                "bridged {} listening on {}",
                env!("CARGO_PKG_VERSION"),
                args.pipe
            ),
        );
        let awake = daemon.awake().cloned();
        if let Some(a) = awake.clone() {
            let dir = args.state_dir.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    for n in a.take_notes() {
                        log(&dir, &n);
                    }
                }
            });
        }
        let r = server::serve(daemon, listener, rx)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(true);
        if let Some(a) = awake {
            a.shutdown();
            for n in a.take_notes() {
                log(&args.state_dir, &n);
            }
        }
        log(&args.state_dir, "bridged stopped");
        r
    })
}

#[cfg(test)]
mod protected_flag_tests {
    use super::*;

    fn args() -> Args {
        let mut a = Args {
            fake: false,
            adb: Some("adb".into()),
            allow_emulators: false,
            allow_serials: vec![],
            allow_file: None,
            stop: false,
            print_schema: None,
            pipe: String::new(),
            state_dir: PathBuf::new(),
            link_bind: None,
            link_lan: false,
            link_port: 0,
            view_dist: None,
            view_port: 0,
            profiles: None,
            approval_topic_file: None,
            approval_inbound_file: None,
            ntfy_base: None,
            bench_approve: false,
            audit_anchor: None,
            audit_anchor_required: false,
            ask_screen_state_dir: None,
            protected_lease: true,
            protected_lease_min: None,
            protected_ask_window_min: None,
            presence_file: None,
            presence_key_file: None,
            presence_ttl_min: None,
        };
        a.protected_lease = true;
        a
    }

    #[test]
    fn the_protected_lease_needs_a_channel_whose_answers_come_on_their_own_topic() {
        let d = std::env::temp_dir().join(format!("bridged-plp-flags-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (out, inb) = (d.join("out.topic"), d.join("in.topic"));
        std::fs::write(&out, "agentroid-out-0123456789abcdef\n").unwrap();
        std::fs::write(&inb, "agentroid-out-0123456789abcdef\n").unwrap();
        let mut a = args();
        assert!(protected_lease_refusal(&a)
            .unwrap()
            .contains("approval channel"));
        a.approval_topic_file = Some(out.clone());
        assert!(protected_lease_refusal(&a).is_some(), "no inbound file");
        a.approval_inbound_file = Some(inb.clone());
        assert!(protected_lease_refusal(&a).unwrap().contains("differ"));
        std::fs::write(&inb, "agentroid-in-fedcba9876543210\n").unwrap();
        assert_eq!(protected_lease_refusal(&a), None);
        a.bench_approve = true;
        assert!(protected_lease_refusal(&a).is_some());
        a.bench_approve = false;
        a.presence_file = Some(d.join("presence.json"));
        assert!(protected_lease_refusal(&a).unwrap().contains("together"));
        a.presence_key_file = Some(d.join("presence.key"));
        assert_eq!(protected_lease_refusal(&a), None);
        a.adb = None;
        assert!(protected_lease_refusal(&a).is_some());
        // off by default, and its flags alone do not turn it on
        let mut off = args();
        off.protected_lease = false;
        assert_eq!(protected_lease_refusal(&off), None);
        off.protected_lease_min = Some(5);
        assert!(protected_lease_refusal(&off).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}
