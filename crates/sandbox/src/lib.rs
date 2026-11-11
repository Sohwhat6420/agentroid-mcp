//! `run_script`: an agent-supplied JavaScript snippet run in QuickJS (task 2.10, slice c).
//!
//! "`run_script` in QuickJS in a child process under a Job Object (64 MB, CPU
//! deadline, no `eval`, fs or net, every binding through `safety`)", and the How (line 960):
//! rquickjs **without** `rust-alloc` (so `set_memory_limit` works), `JOB_OBJECT_LIMIT_PROCESS_MEMORY`
//! through `windows-sys`, only `agentroid.*` bindings.
//!
//! Layers, outermost first:
//! 1. **Job Object** (parent, [`job`]): 64 MB process memory, kill-on-close, one active process,
//!    a per-process user-time limit, die-on-unhandled-exception, UI restrictions. The child is spawned
//!    with `CREATE_NO_WINDOW`; it is assigned to the job *before* it is sent the script, and the
//!    parent checks `IsProcessInJob(child, job)` before it sends anything.
//! 2. **Parent deadlines** ([`Sandbox::run`]): wall and CPU (job accounting) deadlines and the call
//!    cap; on expiry it terminates the job.
//! 3. **QuickJS** (child, [`child`]): memory limit, stack limit, interrupt handler for the CPU
//!    deadline, `eval` and every `Function` constructor removed, no fs/net/timers.
//! 4. **Bindings**: `agentroid.call` goes through [`agentroid_core::gate::check`];
//!    `agentroid.shell` through [`shell::classify`] (the 2.4 read-only list, everything else R2 and
//!    refused). Admitted requests go to the daemon's [`Bridge`]; refusals go back to the script as
//!    errors and into [`RunResult::refusals`] for the run journal (slice b).

pub mod calls;
pub mod child;
#[cfg(windows)]
pub mod job;
pub mod proto;
pub mod shell;

use agentroid_core::gate::{self, Call, DeviceMode, Subject};
use proto::{ChildLimits, FromChild, ToChild};
use serde::Serialize;
use serde_json::Value;
use shell::{RiskClass, ShellVerdict};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The name of the child binary (built from `src/bin/agentroid-sandbox.rs`).
pub const CHILD_BIN: &str = "agentroid-sandbox";

/// The daemon side of the bindings. Only requests that passed the gate reach it.
pub trait Bridge {
    /// An admitted `agentroid.call(tool, args)`. `call` already passed `gate::check` with the
    /// device's effective mode; `raw_args` is the script's argument object as sent. The returned
    /// JSON is handed back to the script. `Err` becomes a script error (a transport failure, not
    /// a refusal). The parent cannot pre-empt a blocked bridge call, so it must bound its own time.
    fn call(&mut self, call: &Call, raw_args: &Value) -> Result<Value, String>;
    /// An admitted `agentroid.shell(cmd)`: `argv` matched a 2.4 read-only pattern exactly (R0) and
    /// has no metacharacter. Build `adb shell` yourself and `sh_quote` every element.
    fn shell(&mut self, argv: &[String]) -> Result<String, String>;
    /// An `agentroid.shell(cmd)` this crate refused before it reached [`Bridge::shell`] (not R0,
    /// or a non-`full` device), so the daemon can write it to its audit log (D3b). Default: nothing.
    fn shell_refused(&mut self, _cmd: &str, _reason: &str) {}
}

/// The device the script drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    pub hw_serial: String,
    /// On the allowlist (2.2).
    pub allowed: bool,
    /// The configured mode; the protected phone is forced to `read_only` by [`gate::effective_mode`].
    pub mode: DeviceMode,
}

impl Device {
    pub fn effective_mode(&self) -> DeviceMode {
        gate::effective_mode(&[&self.serial, &self.hw_serial], self.mode)
    }
    fn subject(&self) -> Subject<'_> {
        Subject {
            serial: &self.serial,
            hw_serial: &self.hw_serial,
            allowed: self.allowed,
        }
    }
}

/// Limits. The defaults are the design values where it gives one (64 MB) and this crate's
/// proposals elsewhere (see SANDBOX-REPORT.md, assumptions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `JOB_OBJECT_LIMIT_PROCESS_MEMORY` (committed memory of the child process).
    pub job_mem_bytes: usize,
    /// QuickJS `set_memory_limit`; below the job limit so QuickJS refuses first and reports.
    pub js_mem_bytes: usize,
    /// CPU deadline: QuickJS interrupt (child) and job CPU accounting (parent).
    pub cpu_ms: u64,
    /// Wall deadline (parent). Under the 110 s MCP call bound (V7).
    pub wall_ms: u64,
    /// `agentroid.call` + `agentroid.shell` requests per run; the next one kills the run.
    pub max_calls: u32,
    pub max_stdout_lines: usize,
    pub max_line_chars: usize,
    /// Cap of one `agentroid.sleep`.
    pub max_sleep_ms: u64,
    pub max_script_bytes: usize,
    pub max_value_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            job_mem_bytes: 64 * 1024 * 1024,
            js_mem_bytes: 32 * 1024 * 1024,
            cpu_ms: 1_000,
            wall_ms: 100_000,
            max_calls: 500,
            max_stdout_lines: 200,
            max_line_chars: 2_000,
            max_sleep_ms: 5_000,
            max_script_bytes: 256 * 1024,
            max_value_bytes: 64 * 1024,
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The script finished; `value` holds its completion value.
    Ok,
    /// The script threw (an uncaught refusal, a bad call, a syntax error, `eval` refused...).
    ScriptError,
    /// QuickJS refused an allocation, or the job's memory limit killed the child.
    MemoryLimit,
    /// The CPU deadline (QuickJS interrupt, job accounting or the job's user-time limit).
    CpuDeadline,
    WallDeadline,
    /// More than `max_calls` binding requests; the job was terminated.
    CallCap,
    /// The child died without a result.
    Crashed,
    /// The sandbox could not be set up (refused before any script ran), or the script was refused
    /// before spawning (too large).
    SandboxError,
}

/// One refusal, for slice b's run journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefusalRecord {
    /// 1-based number of the binding request in this run.
    pub seq: u32,
    /// `"call"` or `"shell"`.
    pub binding: &'static str,
    /// The tool name or the command line (truncated to 200 characters).
    pub request: String,
    /// `"gate"` (core::gate::check), `"read_only"`, `"mode"` (shell on a non-`full` device),
    /// `"shell_class"` (not R0) or `"invalid"` (unknown tool or bad arguments).
    pub kind: &'static str,
    /// Shell requests only.
    pub class: Option<RiskClass>,
    pub reason: String,
    pub at_ms: u64,
}

/// What the parent read back from the OS about the job, for tests and the receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct JobReport {
    /// `IsProcessInJob(child, job)`, checked by the parent before the script was sent.
    pub child_in_job: bool,
    /// The child's own `IsProcessInJob(self, NULL)`.
    pub child_reported_in_job: bool,
    pub memory_limit: usize,
    pub active_process_limit: u32,
    pub kill_on_close: bool,
    pub peak_process_memory: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunResult {
    pub outcome: Outcome,
    /// The completion value as JSON (`null` for `undefined` or on failure).
    pub value: Value,
    pub error: Option<String>,
    pub stdout: Vec<String>,
    pub stdout_truncated: bool,
    pub refusals: Vec<RefusalRecord>,
    /// Binding requests received (admitted or refused).
    pub calls: u32,
    pub wall_ms: u64,
    pub cpu_ms: u64,
    pub job: JobReport,
}

impl RunResult {
    fn new() -> Self {
        RunResult {
            outcome: Outcome::SandboxError,
            value: Value::Null,
            error: None,
            stdout: vec![],
            stdout_truncated: false,
            refusals: vec![],
            calls: 0,
            wall_ms: 0,
            cpu_ms: 0,
            job: JobReport::default(),
        }
    }
}

/// The child binary next to the running executable (how the daemon finds it once both are installed).
pub fn default_child_exe() -> std::io::Result<PathBuf> {
    let me = std::env::current_exe()?;
    let dir = me
        .parent()
        .ok_or_else(|| std::io::Error::other("no exe dir"))?;
    Ok(dir.join(format!("{CHILD_BIN}{}", std::env::consts::EXE_SUFFIX)))
}

pub struct Sandbox {
    child_exe: PathBuf,
    limits: Limits,
}

/// What the stdout reader thread sends the parent loop.
enum Line {
    Msg(String),
    TooLong,
    Eof,
}

const MAX_PROTO_LINE: u64 = 4 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(20);
/// Slack over `cpu_ms` before the parent kills on job CPU accounting (the child's own interrupt
/// should stop the script first and report).
const CPU_GRACE_MS: u64 = 300;

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

impl Sandbox {
    pub fn new(child_exe: impl Into<PathBuf>) -> Self {
        Sandbox {
            child_exe: child_exe.into(),
            limits: Limits::default(),
        }
    }
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
    pub fn child_exe(&self) -> &Path {
        &self.child_exe
    }

    /// Run `script` against `device`. Never panics on script behaviour; every failure is an [`Outcome`].
    pub fn run(&self, script: &str, device: &Device, bridge: &mut dyn Bridge) -> RunResult {
        let started = Instant::now();
        let mut res = RunResult::new();
        if script.len() > self.limits.max_script_bytes {
            res.error = Some(format!(
                "script is {} bytes; the limit is {}",
                script.len(),
                self.limits.max_script_bytes
            ));
            return res;
        }
        #[cfg(windows)]
        {
            self.run_windows(script, device, bridge, started, &mut res);
        }
        #[cfg(not(windows))]
        {
            let _ = (device, bridge);
            res.error =
                Some("run_script needs a Windows Job Object; refusing to run unsandboxed".into());
        }
        res.wall_ms = started.elapsed().as_millis() as u64;
        res
    }

    #[cfg(windows)]
    fn run_windows(
        &self,
        script: &str,
        device: &Device,
        bridge: &mut dyn Bridge,
        started: Instant,
        res: &mut RunResult,
    ) {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        let lim = &self.limits;
        let job = match job::Job::new(lim.job_mem_bytes, lim.cpu_ms + CPU_GRACE_MS * 2) {
            Ok(j) => j,
            Err(e) => {
                res.error = Some(format!("job object: {e}"));
                return;
            }
        };
        if let Some(r) = job.limits() {
            res.job.memory_limit = r.memory_limit;
            res.job.active_process_limit = r.active_process_limit;
            res.job.kill_on_close = r.kill_on_close;
        }
        let mut cmd = Command::new(&self.child_exe);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .env_clear();
        if let Ok(root) = std::env::var("SystemRoot") {
            cmd.env("SystemRoot", root);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                res.error = Some(format!("spawn {}: {e}", self.child_exe.display()));
                return;
            }
        };
        let handle = child.as_raw_handle();
        // The child blocks on stdin until `start`, so nothing of the script runs before this.
        if let Err(e) = job.assign(handle) {
            kill(&mut child);
            res.error = Some(format!("AssignProcessToJobObject: {e}"));
            return;
        }
        res.job.child_in_job = job.contains(handle);
        if !res.job.child_in_job {
            kill(&mut child);
            res.error = Some("the child is not in the job object; refusing to run".into());
            return;
        }

        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            kill(&mut child);
            res.error = Some("child pipes missing".into());
            return;
        };
        let rx = spawn_reader(stdout);
        let err_tail = spawn_stderr(stderr);

        let start = ToChild::Start {
            script: script.to_string(),
            limits: ChildLimits {
                js_mem_bytes: lim.js_mem_bytes,
                cpu_ms: lim.cpu_ms,
                max_sleep_ms: lim.max_sleep_ms,
                max_stdout_lines: lim.max_stdout_lines,
                max_line_chars: lim.max_line_chars,
                max_value_bytes: lim.max_value_bytes,
            },
        };
        if send(&mut stdin, &start).is_err() {
            job.terminate();
            let _ = child.wait();
            res.outcome = Outcome::Crashed;
            res.error = Some("could not send the script to the child".into());
            return;
        }

        let mode = device.effective_mode();
        let mut finished: Option<(Outcome, Option<String>)> = None;
        let mut killed: Option<(Outcome, String)> = None;
        loop {
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_millis(lim.wall_ms) {
                killed = Some((
                    Outcome::WallDeadline,
                    format!("wall deadline of {} ms", lim.wall_ms),
                ));
                break;
            }
            let cpu = job.cpu_ms();
            if cpu > lim.cpu_ms + CPU_GRACE_MS {
                killed = Some((
                    Outcome::CpuDeadline,
                    format!(
                        "CPU deadline of {} ms (job accounting {cpu} ms)",
                        lim.cpu_ms
                    ),
                ));
                break;
            }
            let wait = POLL.min(Duration::from_millis(lim.wall_ms).saturating_sub(elapsed));
            let line = match rx.recv_timeout(wait) {
                Ok(Line::Msg(l)) => l,
                Ok(Line::TooLong) => {
                    killed = Some((Outcome::Crashed, "protocol line too long".into()));
                    break;
                }
                Ok(Line::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            };
            let msg: FromChild = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(e) => {
                    killed = Some((Outcome::Crashed, format!("protocol error: {e}")));
                    break;
                }
            };
            match msg {
                FromChild::Hello { in_job } => res.job.child_reported_in_job = in_job,
                FromChild::Log { line } => {
                    if res.stdout.len() < lim.max_stdout_lines {
                        res.stdout.push(truncate(&line, lim.max_line_chars));
                    } else {
                        res.stdout_truncated = true;
                    }
                }
                FromChild::Call { id, tool, args } => {
                    res.calls += 1;
                    if res.calls > lim.max_calls {
                        killed = Some((
                            Outcome::CallCap,
                            format!("call cap of {} binding requests reached", lim.max_calls),
                        ));
                        break;
                    }
                    let at_ms = started.elapsed().as_millis() as u64;
                    let reply = self.answer_call(res, at_ms, device, mode, &tool, &args, bridge);
                    let _ = send(&mut stdin, &reply.into_msg(id));
                }
                FromChild::Shell { id, cmd } => {
                    res.calls += 1;
                    if res.calls > lim.max_calls {
                        killed = Some((
                            Outcome::CallCap,
                            format!("call cap of {} binding requests reached", lim.max_calls),
                        ));
                        break;
                    }
                    let at_ms = started.elapsed().as_millis() as u64;
                    let reply = answer_shell(res, at_ms, device, mode, &cmd, bridge);
                    let _ = send(&mut stdin, &reply.into_msg(id));
                }
                FromChild::Done {
                    ok,
                    kind,
                    value,
                    error,
                } => {
                    if ok {
                        match value.as_deref().map(serde_json::from_str::<Value>) {
                            None => finished = Some((Outcome::Ok, None)),
                            Some(Ok(v)) => {
                                res.value = v;
                                finished = Some((Outcome::Ok, None));
                            }
                            Some(Err(e)) => {
                                finished = Some((
                                    Outcome::ScriptError,
                                    Some(format!("bad return value: {e}")),
                                ))
                            }
                        }
                    } else {
                        let o = match kind.as_deref() {
                            Some("memory") => Outcome::MemoryLimit,
                            Some("cpu_deadline") => Outcome::CpuDeadline,
                            Some("sandbox") => Outcome::SandboxError,
                            _ => Outcome::ScriptError,
                        };
                        finished = Some((o, error));
                    }
                    break;
                }
            }
        }
        drop(stdin);

        if let Some((o, why)) = killed {
            job.terminate();
            res.outcome = o;
            res.error = Some(why);
        } else if let Some((o, err)) = finished {
            res.outcome = o;
            res.error = err;
            // the child exits on its own after `done`; give it a moment, then the job goes
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(500) {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            job.terminate();
        } else {
            // EOF without `done`: classify from what the job saw
            let _ = child.wait();
            let code = child.try_wait().ok().flatten().and_then(|s| s.code());
            let peak = job.limits().map(|l| l.peak_process_memory).unwrap_or(0);
            let cpu = job.cpu_ms();
            let tail = err_tail.join().unwrap_or_default();
            if peak + 2 * 1024 * 1024 >= lim.job_mem_bytes {
                res.outcome = Outcome::MemoryLimit;
                res.error = Some(format!(
                    "the child hit the job memory limit ({} bytes) and died (exit {code:?})",
                    lim.job_mem_bytes
                ));
            } else if cpu >= lim.cpu_ms {
                res.outcome = Outcome::CpuDeadline;
                res.error = Some(format!("the job's CPU limit ended the child ({cpu} ms)"));
            } else {
                res.outcome = Outcome::Crashed;
                res.error = Some(format!(
                    "the child exited without a result (exit {code:?}) {}",
                    truncate(tail.trim(), 400)
                ));
            }
        }
        let _ = child.wait();
        res.cpu_ms = job.cpu_ms();
        if let Some(l) = job.limits() {
            res.job.peak_process_memory = l.peak_process_memory;
        }
        // A commit refused by the job's memory limit surfaces in QuickJS as a failed malloc, i.e. a
        // script error (often a bare `null`); the job's peak says which limit it was.
        if res.outcome == Outcome::ScriptError
            && res.job.peak_process_memory + 4 * 1024 * 1024 >= lim.job_mem_bytes
        {
            res.outcome = Outcome::MemoryLimit;
            res.error = Some(format!(
                "job memory limit of {} bytes reached ({})",
                lim.job_mem_bytes,
                res.error.take().unwrap_or_default()
            ));
        }
        // `job` drops here: CloseHandle, and KILL_ON_JOB_CLOSE ends anything left.
    }

    #[allow(clippy::too_many_arguments)]
    fn answer_call(
        &self,
        res: &mut RunResult,
        at_ms: u64,
        device: &Device,
        mode: DeviceMode,
        tool: &str,
        args: &Value,
        bridge: &mut dyn Bridge,
    ) -> Reply {
        let seq = res.calls;
        let mut refuse = |kind: &'static str, reason: String| {
            res.refusals.push(RefusalRecord {
                seq,
                binding: "call",
                request: truncate(tool, 200),
                kind,
                class: None,
                reason: reason.clone(),
                at_ms,
            });
            Reply::Err(format!("refused: {reason}"))
        };
        let call = match calls::parse_call(tool, args) {
            Ok(c) => c,
            Err(e) => return refuse("invalid", e),
        };
        if let Err(r) = gate::check(&device.subject(), mode, &call) {
            return refuse("gate", r.to_string());
        }
        // "`read_only` refuses every call" (the job tools are `full` devices only),
        // so a read_only device's script gets no reads either.
        if mode == DeviceMode::ReadOnly {
            return refuse(
                "read_only",
                format!(
                    "{} refused: run_script bindings are refused on read_only devices",
                    call.name()
                ),
            );
        }
        match bridge.call(&call, args) {
            Ok(v) => Reply::Ok(v),
            Err(e) => Reply::Err(e),
        }
    }
}

fn answer_shell(
    res: &mut RunResult,
    at_ms: u64,
    device: &Device,
    mode: DeviceMode,
    cmd: &str,
    bridge: &mut dyn Bridge,
) -> Reply {
    let seq = res.calls;
    let mut refuse = |kind: &'static str, class: Option<RiskClass>, reason: String| {
        res.refusals.push(RefusalRecord {
            seq,
            binding: "shell",
            request: truncate(cmd, 200),
            kind,
            class,
            reason: reason.clone(),
            at_ms,
        });
        Reply::Err(format!("refused: {reason}"))
    };
    // the gate's subject rules first (S24, allowlist), with the same messages as for calls
    if gate::is_s24(&device.serial) || gate::is_s24(&device.hw_serial) {
        return refuse(
            "gate",
            None,
            gate::Refusal::S24 { tool: "shell" }.to_string(),
        );
    }
    if !device.allowed {
        return refuse(
            "gate",
            None,
            gate::Refusal::NotAllowed {
                serial: device.serial.clone(),
            }
            .to_string(),
        );
    }
    if mode != DeviceMode::Full {
        let reason = format!(
            "agentroid.shell is for full devices only; this device is {}",
            mode.as_str()
        );
        bridge.shell_refused(cmd, &reason);
        return refuse("mode", None, reason);
    }
    match shell::classify(cmd) {
        ShellVerdict::Refused { class, reason } => {
            let reason = format!("{} ({reason}); no exception, no chat route", class.as_str());
            bridge.shell_refused(cmd, &reason);
            refuse("shell_class", Some(class), reason)
        }
        ShellVerdict::Allowed { argv } => match bridge.shell(&argv) {
            Ok(out) => Reply::Ok(Value::String(truncate(&out, 64 * 1024))),
            Err(e) => Reply::Err(e),
        },
    }
}

enum Reply {
    Ok(Value),
    Err(String),
}

impl Reply {
    fn into_msg(self, id: u64) -> ToChild {
        match self {
            Reply::Ok(v) => ToChild::Reply {
                id,
                ok: true,
                value: Some(v),
                error: None,
            },
            Reply::Err(e) => ToChild::Reply {
                id,
                ok: false,
                value: None,
                error: Some(e),
            },
        }
    }
}

fn send(stdin: &mut ChildStdin, msg: &ToChild) -> std::io::Result<()> {
    let mut line = serde_json::to_string(msg).map_err(std::io::Error::other)?;
    line.push('\n');
    stdin.write_all(line.as_bytes())?;
    stdin.flush()
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_reader(stdout: impl Read + Send + 'static) -> mpsc::Receiver<Line> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        loop {
            let mut buf = Vec::new();
            match (&mut r).take(MAX_PROTO_LINE).read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => {
                    let _ = tx.send(Line::Eof);
                    return;
                }
                Ok(_) if buf.last() != Some(&b'\n') && buf.len() as u64 >= MAX_PROTO_LINE => {
                    let _ = tx.send(Line::TooLong);
                    return;
                }
                Ok(_) => {
                    let s = String::from_utf8_lossy(&buf).trim_end().to_string();
                    if tx.send(Line::Msg(s)).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

fn spawn_stderr(stderr: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut keep = Vec::new();
        let mut buf = [0u8; 4096];
        let mut r = stderr;
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 {
                break;
            }
            if keep.len() < 8192 {
                keep.extend_from_slice(&buf[..n]);
            }
        }
        String::from_utf8_lossy(&keep).into_owned()
    })
}
