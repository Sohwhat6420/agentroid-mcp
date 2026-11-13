//! The daemon arm of three MCP tools: `doctor` (`diagnose`), `push` and `pull_new`.
//!
//! They arrive as `device.call` with `call.tool` set to one of the three; `server.rs` hands those
//! (and only those) to [`Daemon::devtools_call`] through one match arm. Every call holds the lease,
//! passes this module's own gate (the protected phone, the allowlist, the device mode) before anything reaches
//! the transport, and is journalled in the daemon log ([`devtools_log`], and stderr).
//!
//! * `doctor` with `diagnose`: the read-only allowlist of [`crate::diagnose`]. `full` and `read_only`
//!   devices; the protected phone only its 11 approved reads; `helper` devices have no adb shell, so refused.
//! * `push`: `full` devices only (the plan's 3.2 matrix row). SHA-256 of the laptop file, then of
//!   the phone file by `sha256sum` (the read-back); a mismatch is an error. Paths are quoted here.
//! * `pull_new`: `full` devices only too (it writes the laptop). The newest file of a phone
//!   directory, its size read twice until two polls agree, pulled, sized and hashed on the laptop.
//! * The laptop side is fenced too (11 Nov 2026): `push` reads only files inside `<files>/outbox`
//!   and `pull_new` writes only inside `<files>/inbox`, where `<files>` is [`Daemon::set_files_dir`]
//!   (`bridged` passes `<state dir>/files`) or [`default_files_dir`]. A relative path is taken inside
//!   that folder; an absolute one must resolve (links followed) to a place inside it. Anything else
//!   is `REFUSED` with class `outbox` or `inbox` before the phone is touched, so a `full` token
//!   cannot copy an SSH key off the laptop or drop a file into its Startup folder.

use crate::diagnose;
use crate::proto::*;
use crate::server::{device_key, Daemon};
use crate::sha256::file_sha256;
use agentroid_core::fileguard::FileOp;
use agentroid_core::gate::{self, DeviceMode, Refusal};
use agentroid_core::transport::CoreError;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// Tools this module serves.
pub const TOOLS: [&str; 3] = ["doctor", "push", "pull_new"];
/// Most polls `pull_new` makes while waiting for a stable size.
pub const MAX_POLLS: u32 = 20;
/// Longest `doctor` answer kept, in bytes.
const MAX_TEXT: usize = 256 * 1024;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn log(line: String) {
    eprintln!("bridged: devtools: {line}");
    let mut l = LOG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if l.len() >= 2000 {
        l.drain(..1000);
    }
    l.push(line);
}

/// The journal of these calls, oldest first: `<tool> <device> <allowed|refused:class|failed> <detail>`.
pub fn devtools_log() -> Vec<String> {
    LOG.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Is this `device.call` one of ours?
pub fn claims(params: &Value) -> bool {
    params
        .pointer("/call/tool")
        .and_then(Value::as_str)
        .is_some_and(|t| TOOLS.contains(&t))
}

fn refused(r: &Refusal, tool: &str, class: &str) -> RpcError {
    RpcError::new(REFUSED, r.to_string()).with_data(json!({ "tool": tool, "class": class }))
}

fn arg<'a>(call: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    call.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                format!("invalid params: {name} is required (a non-empty string)"),
            )
        })
}

/// A phone path: absolute, no control character, no `..` segment.
pub fn check_phone_path(p: &str) -> Result<(), RpcError> {
    if !p.starts_with('/') || p.chars().any(char::is_control) || p.split('/').any(|s| s == "..") {
        return Err(RpcError::new(INVALID_PARAMS, format!("invalid params: phone path {p:?} must be absolute, with no `..` and no control character")));
    }
    Ok(())
}

/// The first 64-hex word of a `sha256sum` answer.
fn parse_sha(out: &str) -> Option<String> {
    let w = out.split_whitespace().next()?;
    (w.len() == 64 && w.chars().all(|c| c.is_ascii_hexdigit())).then(|| w.to_ascii_lowercase())
}

/// A file name from the phone, made safe to use as a laptop file name.
fn local_name(n: &str) -> String {
    let s: String = n
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if s.is_empty() || s == "." || s == ".." {
        "pulled_file".into()
    } else {
        s
    }
}

/// The laptop folder for `push` and `pull_new` when the daemon was given none:
/// `%LOCALAPPDATA%/agentroid/files`, or `<temp>/agentroid-files` without LOCALAPPDATA.
pub fn default_files_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("agentroid").join("files"))
        .unwrap_or_else(|| std::env::temp_dir().join("agentroid-files"))
}

/// A relative laptop path made only of plain names (no `..`, no root, no drive), or `None`.
fn plain_relative(p: &Path) -> Option<&Path> {
    use std::path::Component;
    (!p.as_os_str().is_empty() && p.components().all(|c| matches!(c, Component::Normal(_))))
        .then_some(p)
}

/// Is `p` (which must exist) inside `root` once both are resolved? Links and `..` are followed first.
fn resolves_inside(root: &Path, p: &Path) -> bool {
    match (std::fs::canonicalize(root), std::fs::canonicalize(p)) {
        (Ok(r), Ok(q)) => q.starts_with(&r),
        _ => false,
    }
}

fn fenced(tool: &str, class: &str, msg: String) -> RpcError {
    RpcError::new(REFUSED, msg).with_data(json!({ "tool": tool, "class": class }))
}

fn cap(mut s: String) -> String {
    if s.len() > MAX_TEXT {
        let mut n = MAX_TEXT;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
        s.push_str("\n[truncated]");
    }
    s
}

struct Target {
    name: String,
    serial: String,
    mode: DeviceMode,
    allowed: bool,
    s24: bool,
}

impl Daemon {
    /// Serve `doctor`, `push` or `pull_new` (see the module docs).
    pub(crate) fn devtools_call(&self, conn: u64, params: &Value) -> Result<Value, RpcError> {
        let name = params
            .get("device")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(INVALID_PARAMS, "invalid params: device is required"))?;
        let call = params.get("call").cloned().unwrap_or(Value::Null);
        let tool = call
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let info = self.resolve(name)?;
        let key = device_key(&info);
        self.require_holder(&key, conn)?;
        self.keep_awake(conn, &key, &info);
        let (mode, allowed) = {
            let b = self.backend();
            (b.mode_of(&info), b.allowed(&info))
        };
        let t = Target {
            name: key,
            serial: info.serial.clone(),
            mode,
            allowed,
            s24: gate::is_s24(&info.serial) || gate::is_s24(&info.hw_serial),
        };
        let r = match tool.as_str() {
            "doctor" => self.doctor(&t, &call),
            "push" => self.push(&t, &call),
            "pull_new" => self.pull_new(&t, &call),
            other => Err(RpcError::new(
                INVALID_PARAMS,
                format!("invalid params: {other} is not a devtools tool"),
            )),
        };
        match &r {
            Ok(_) => log(format!("{tool} {} allowed", t.name)),
            Err(e) if e.code == REFUSED => log(format!("{tool} {} refused: {}", t.name, e.message)),
            Err(e) => log(format!("{tool} {} failed: {}", t.name, e.message)),
        }
        // Same as `device_call`: every answer past the lease check carries the health member (no phone traffic).
        let health = self.health_member(&info);
        match r {
            Ok(mut v) => {
                if let Value::Object(m) = &mut v {
                    m.insert("health".into(), health);
                }
                Ok(v)
            }
            Err(mut e) => {
                let mut data = match e.data.take() {
                    Some(Value::Object(m)) => m,
                    _ => serde_json::Map::new(),
                };
                data.insert("health".into(), health);
                e.data = Some(Value::Object(data));
                Err(e)
            }
        }
    }

    fn sh(&self, t: &Target, line: &str) -> Result<String, RpcError> {
        self.backend()
            .raw_shell(&t.serial, line)
            .map_err(|e| RpcError::new(DEVICE_ERROR, e.to_string()))
    }

    /// Gate for a mutating tool that needs adb and a `full` device (`push`, `pull_new`).
    fn full_only(t: &Target, tool: &'static str) -> Result<(), RpcError> {
        if t.s24 {
            return Err(refused(&Refusal::S24 { tool }, tool, "s24"));
        }
        if !t.allowed {
            return Err(refused(
                &Refusal::NotAllowed {
                    serial: t.serial.clone(),
                },
                tool,
                "not_allowed",
            ));
        }
        match t.mode {
            DeviceMode::Full => Ok(()),
            DeviceMode::ReadOnly => Err(refused(&Refusal::ReadOnly { tool }, tool, "read_only")),
            DeviceMode::Helper => Err(refused(&Refusal::NeedsAdbShell { tool }, tool, "helper")),
        }
    }

    fn doctor(&self, t: &Target, call: &Value) -> Result<Value, RpcError> {
        let raw = match call.get("diagnose") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(a)) => Some(
                a.iter()
                    .map(|v| v.as_str().unwrap_or("\u{0}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            Some(_) => {
                return Err(RpcError::new(
                    INVALID_PARAMS,
                    "invalid params: diagnose must be a string",
                ))
            }
        };
        let Some(raw) = raw else {
            // no command: what the daemon knows without asking the phone
            let text = format!(
                "doctor: {} mode={} adb={}{}",
                t.name,
                t.mode.as_str(),
                t.serial,
                if t.s24 {
                    " (deny-by-default: only getprop ro.serialno and the approved probes via diagnose)"
                } else {
                    ""
                }
            );
            return Ok(json!({ "tool": "doctor", "text": text, "image_bytes": null }));
        };
        let argv = if t.s24 {
            diagnose::classify_s24(&raw).map_err(|d| {
                RpcError::new(REFUSED, d.message)
                    .with_data(json!({ "tool": "doctor", "class": d.class }))
            })?
        } else {
            if !t.allowed {
                return Err(refused(
                    &Refusal::NotAllowed {
                        serial: t.serial.clone(),
                    },
                    "doctor",
                    "not_allowed",
                ));
            }
            if t.mode == DeviceMode::Helper {
                return Err(refused(
                    &Refusal::NeedsAdbShell { tool: "doctor" },
                    "doctor",
                    "helper",
                ));
            }
            diagnose::classify(&raw).map_err(|d| {
                RpcError::new(REFUSED, d.message)
                    .with_data(json!({ "tool": "doctor", "class": d.class }))
            })?
        };
        let out = self.sh(t, &diagnose::shell_line(&argv))?;
        Ok(
            json!({ "tool": "doctor", "text": cap(out.trim_end().to_string()), "image_bytes": null }),
        )
    }

    /// The file guard (D3b): core's device gate, `fileguard::classify_file`, the approval channel
    /// for an `asked` path and the audit log. A refusal is `REFUSED` with class `file_guard`.
    fn file_gate(&self, t: &Target, op: FileOp, phone_path: &str) -> Result<(), RpcError> {
        let tool = op.as_str();
        match self.backend().file_gate(&t.serial, op, phone_path) {
            Ok(()) => Ok(()),
            Err(CoreError::Refused(r)) => Err(refused(&r, tool, "file_guard")),
            Err(CoreError::Invalid(why)) => Err(RpcError::new(INVALID_PARAMS, why)),
            Err(e) => Err(RpcError::new(DEVICE_ERROR, format!("{tool}: {e}"))),
        }
    }

    /// `<files>` (see the module docs).
    fn files_root(&self) -> PathBuf {
        self.files_dir.get_or_init(default_files_dir).clone()
    }

    fn push(&self, t: &Target, call: &Value) -> Result<Value, RpcError> {
        Self::full_only(t, "push")?;
        let local = arg(call, "local")?;
        let remote = arg(call, "phone_path")?;
        check_phone_path(remote)?;
        // the laptop side first: nothing outside the outbox is read, asked about or sent
        let outbox = self.files_root().join("outbox");
        let given = PathBuf::from(local);
        let local_path = if given.is_absolute() {
            given
        } else if let Some(rel) = plain_relative(&given) {
            outbox.join(rel)
        } else {
            return Err(fenced(
                "push",
                "outbox",
                format!(
                    "push reads only from the outbox ({}): {local:?} is not a plain name inside it",
                    outbox.display()
                ),
            ));
        };
        if !local_path.is_file() {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!("invalid params: local file {local:?} does not exist or is not a file (push reads from {})", outbox.display()),
            ));
        }
        if !resolves_inside(&outbox, &local_path) {
            return Err(fenced(
                "push",
                "outbox",
                format!(
                    "push reads only from the outbox ({}): {local:?} is outside it",
                    outbox.display()
                ),
            ));
        }
        self.file_gate(t, FileOp::Push, remote)?;
        let (want, size) = file_sha256(&local_path).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                format!("invalid params: cannot read {local:?}: {e}"),
            )
        })?;
        self.backend()
            .raw_push(&t.serial, &local_path, remote)
            .map_err(|e| RpcError::new(DEVICE_ERROR, format!("push failed: {e}")))?;
        let back = self.sh(t, &format!("sha256sum {}", diagnose::quote(remote)))?;
        match parse_sha(&back) {
            Some(got) if got == want => {}
            Some(got) => return Err(RpcError::new(DEVICE_ERROR, format!("push read-back mismatch: the phone's {remote} has sha256 {got}, the laptop file has {want}"))),
            None => return Err(RpcError::new(DEVICE_ERROR, format!("push could not be read back: sha256sum {remote} answered {:?}", back.trim()))),
        }
        Ok(
            json!({ "tool": "push", "text": format!("pushed {size} bytes to {remote}, sha256 {want} read back from the phone"), "image_bytes": null, "path": remote, "size": size, "sha256": want }),
        )
    }

    fn pull_new(&self, t: &Target, call: &Value) -> Result<Value, RpcError> {
        Self::full_only(t, "pull_new")?;
        let dir = arg(call, "dir")?;
        check_phone_path(dir)?;
        // the directory first (a refused directory is never listed), the file itself below
        self.file_gate(t, FileOp::Pull, &format!("{}/", dir.trim_end_matches('/')))?;
        let poll_ms = call
            .get("poll_ms")
            .and_then(Value::as_u64)
            .unwrap_or(500)
            .min(5000);
        // the laptop side before the phone is listed: pulls land only inside the inbox
        let inbox = self.files_root().join("inbox");
        let local_dir = match call
            .get("local_dir")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            None => inbox.clone(),
            Some(d) => {
                let given = PathBuf::from(d);
                if given.is_absolute() {
                    if !given.is_dir() || !resolves_inside(&inbox, &given) {
                        return Err(fenced("pull_new", "inbox", format!("pull_new writes only inside the inbox ({}): {d:?} is not an existing folder inside it", inbox.display())));
                    }
                    given
                } else if let Some(rel) = plain_relative(&given) {
                    inbox.join(rel)
                } else {
                    return Err(fenced("pull_new", "inbox", format!("pull_new writes only inside the inbox ({}): {d:?} is not a plain name inside it", inbox.display())));
                }
            }
        };
        let listing = self.sh(t, &format!("ls -1tp {}", diagnose::quote(dir)))?;
        let newest = listing
            .lines()
            .map(str::trim)
            .find(|n| {
                !n.is_empty()
                    && !n.ends_with('/')
                    && !n.starts_with('.')
                    && !n.ends_with(".pending")
                    && !n.ends_with(".tmp")
                    && !n.ends_with(".part")
                    && !n.ends_with(".crdownload")
            })
            .ok_or_else(|| RpcError::new(DEVICE_ERROR, format!("pull_new: no file in {dir}")))?
            .to_string();
        if newest.contains('/') || newest.chars().any(char::is_control) {
            return Err(RpcError::new(
                DEVICE_ERROR,
                format!("pull_new: refusing the file name {newest:?}"),
            ));
        }
        let remote = format!("{}/{newest}", dir.trim_end_matches('/'));
        self.file_gate(t, FileOp::Pull, &remote)?;
        let rq = diagnose::quote(&remote);
        // stable across two polls: the same size twice in a row
        let mut prev: Option<u64> = None;
        let mut stable: Option<u64> = None;
        for _ in 0..MAX_POLLS {
            let n: u64 = self
                .sh(t, &format!("stat -c %s {rq}"))?
                .trim()
                .parse()
                .map_err(|_| {
                    RpcError::new(
                        DEVICE_ERROR,
                        format!("pull_new: cannot read the size of {remote}"),
                    )
                })?;
            if prev == Some(n) {
                stable = Some(n);
                break;
            }
            prev = Some(n);
            if poll_ms > 0 {
                std::thread::sleep(Duration::from_millis(poll_ms));
            }
        }
        let size = stable.ok_or_else(|| {
            RpcError::new(
                DEVICE_ERROR,
                format!("pull_new: {remote} was still changing after {MAX_POLLS} polls"),
            )
        })?;
        std::fs::create_dir_all(&local_dir).map_err(|e| {
            RpcError::new(
                STATE_ERROR,
                format!("pull_new: cannot create {}: {e}", local_dir.display()),
            )
        })?;
        // a folder inside the inbox that is a link to somewhere else is still outside it
        if !resolves_inside(&inbox, &local_dir) {
            return Err(fenced(
                "pull_new",
                "inbox",
                format!(
                    "pull_new writes only inside the inbox ({}): {} resolves outside it",
                    inbox.display(),
                    local_dir.display()
                ),
            ));
        }
        let local: PathBuf = local_dir.join(local_name(&newest));
        self.backend()
            .raw_pull(&t.serial, &remote, &local)
            .map_err(|e| RpcError::new(DEVICE_ERROR, format!("pull failed: {e}")))?;
        let (sha, got) = file_sha256(&local).map_err(|e| {
            RpcError::new(
                DEVICE_ERROR,
                format!("pull_new: cannot read {}: {e}", local.display()),
            )
        })?;
        if got != size {
            return Err(RpcError::new(
                DEVICE_ERROR,
                format!("pull_new: {remote} is {size} bytes on the phone but {got} on the laptop"),
            ));
        }
        if let Some(theirs) = parse_sha(&self.sh(t, &format!("sha256sum {rq}"))?) {
            if theirs != sha {
                return Err(RpcError::new(
                    DEVICE_ERROR,
                    format!("pull_new: sha256 mismatch for {remote}: phone {theirs}, laptop {sha}"),
                ));
            }
        }
        let path = local_path_string(&local);
        Ok(
            json!({ "tool": "pull_new", "text": format!("pulled {remote} ({size} bytes, size stable across two polls) to {path}"), "image_bytes": null, "path": path, "size": size, "sha256": sha, "phone_path": remote }),
        )
    }
}

fn local_path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
