//! The seams the watcher needs from the outside world: running adb, probing TCP, telling time.
//! Tests inject fakes for all three; the real versions live here too.

use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Windows process creation flag: no console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Output of one adb run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl RunOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Output of one adb run whose stdout is binary (`exec-out screencap -p`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BytesOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl BytesOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Why a run did not produce output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The command guard refused it; adb was not started.
    Refused(String),
    /// Spawn failure, timeout or similar.
    Io(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Refused(m) => write!(f, "refused: {m}"),
            RunError::Io(m) => write!(f, "io: {m}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Runs adb with the given arguments (without the executable name).
pub trait Runner {
    fn run(&self, args: &[String], timeout_ms: u64) -> Result<RunOutput, RunError>;

    /// Like [`Runner::run`] with stdout kept as bytes. Only the real runner (and the guard in front
    /// of it) implements it; fakes refuse, so a screenshot is never faked by accident.
    fn run_bytes(&self, _args: &[String], _timeout_ms: u64) -> Result<BytesOutput, RunError> {
        Err(RunError::Io(
            "this runner cannot return binary output".into(),
        ))
    }

    /// Tell the runner a token (address, serial) must never be used as a target. Default: no-op.
    fn deny(&self, _token: &str) {}
}

/// Build an owned argument vector from literals.
pub fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// The real runner: spawns the adb executable with no console window on Windows.
#[derive(Debug, Clone)]
pub struct AdbRunner {
    pub adb_path: PathBuf,
}

impl AdbRunner {
    pub fn new(adb_path: impl Into<PathBuf>) -> Self {
        Self {
            adb_path: adb_path.into(),
        }
    }

    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.adb_path);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }
}

fn reader<T: Read + Send + 'static>(h: Option<T>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut h) = h {
            let _ = h.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

fn lossy(b: Vec<u8>) -> String {
    String::from_utf8_lossy(&b).into_owned()
}

/// One finished adb run before either stream is decoded.
struct Raw {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl AdbRunner {
    /// Spawn adb, wait up to `timeout_ms`, return the exit code and both streams as bytes.
    fn exec(&self, args: &[String], timeout_ms: u64) -> Result<Raw, RunError> {
        let mut child = self
            .command(args)
            .spawn()
            .map_err(|e| RunError::Io(format!("cannot start {}: {e}", self.adb_path.display())))?;
        let out_rx = reader(child.stdout.take());
        let err_rx = reader(child.stderr.take());
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunError::Io(format!("adb timed out after {timeout_ms} ms")));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(RunError::Io(e.to_string())),
            }
        };
        // A daemon that adb started may keep the pipes open: do not wait for it forever.
        let grace = Duration::from_secs(2);
        let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
        let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
        Ok(Raw {
            code: status.code(),
            stdout,
            stderr,
        })
    }
}

impl Runner for AdbRunner {
    fn run(&self, args: &[String], timeout_ms: u64) -> Result<RunOutput, RunError> {
        let raw = self.exec(args, timeout_ms)?;
        Ok(RunOutput {
            code: raw.code,
            stdout: lossy(raw.stdout),
            stderr: lossy(raw.stderr),
        })
    }

    fn run_bytes(&self, args: &[String], timeout_ms: u64) -> Result<BytesOutput, RunError> {
        let raw = self.exec(args, timeout_ms)?;
        Ok(BytesOutput {
            code: raw.code,
            stdout: raw.stdout,
            stderr: lossy(raw.stderr),
        })
    }
}

/// How reachable a `host:port` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// TCP connect succeeded.
    Open,
    /// The host answered but the port is closed (wireless debugging off, or the port changed).
    Refused,
    /// No answer (Wi-Fi off, wrong network, stale address).
    Unreachable,
}

/// TCP reachability probe.
pub trait Prober {
    fn probe(&self, addr: &str, timeout_ms: u64) -> Reach;
}

/// Real probe: `TcpStream::connect_timeout`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpProber;

impl Prober for TcpProber {
    fn probe(&self, addr: &str, timeout_ms: u64) -> Reach {
        let Ok(sa) = addr.parse::<SocketAddr>() else {
            return Reach::Unreachable;
        };
        match TcpStream::connect_timeout(&sa, Duration::from_millis(timeout_ms.max(1))) {
            Ok(_) => Reach::Open,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => Reach::Refused,
            Err(_) => Reach::Unreachable,
        }
    }
}

/// Time source, so tests can use a fake clock.
pub trait Clock {
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, ms: u64);
}

/// Real clock (monotonic since creation).
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_no_window_value() {
        assert_eq!(CREATE_NO_WINDOW, 0x08000000);
    }

    #[test]
    fn prober_rejects_garbage_without_network() {
        assert_eq!(TcpProber.probe("not an address", 10), Reach::Unreachable);
    }
}
