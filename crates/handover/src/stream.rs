//! Running Detector 1 against a phone: the admitted `getevent` commands, the line stream, and the
//! supervisor that restarts on EOF and re-maps on every `add device`.
//!
//! Every command goes through adb-link's guard first ([`GuardedRunner::admit_getevent`]): exact
//! argv shapes, `-s` target, full devices only, the protected phone refused behind any spelling. The stream is
//! spawned only after that, with `CREATE_NO_WINDOW` on Windows. Nothing here runs in the tests:
//! they drive [`Supervisor`] with fake sources.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;

use agentroid_adb_link::guard::{GuardedRunner, GETEVENT_SHAPES};
use agentroid_adb_link::runner::{RunError, Runner, CREATE_NO_WINDOW};

use crate::touch::{Detector, HumanTouch};

/// `-s <serial>` + one of [`GETEVENT_SHAPES`].
fn argv(serial: &str, shape: &[&str]) -> Vec<String> {
    let mut a = vec!["-s".to_string(), serial.to_string()];
    a.extend(shape.iter().map(|s| s.to_string()));
    a
}

/// argv for the device map with bus ids (`exec-out getevent -pli`).
pub fn map_argv(serial: &str) -> Vec<String> {
    argv(serial, GETEVENT_SHAPES[1])
}

/// argv for the event stream (`exec-out getevent -lt`).
pub fn stream_argv(serial: &str) -> Vec<String> {
    argv(serial, GETEVENT_SHAPES[2])
}

/// Read the device map through the guarded runner (admitted first; `getevent -p` exits by itself).
pub fn read_map<R: Runner>(
    guarded: &GuardedRunner<R>,
    serial: &str,
    device_is_full: bool,
) -> Result<String, RunError> {
    let a = map_argv(serial);
    guarded.admit_getevent(&a, device_is_full)?;
    let out = guarded.run(&a, 5_000)?;
    if !out.ok() {
        return Err(RunError::Io(format!(
            "getevent -pli failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(out.stdout)
}

/// A source of `getevent -lt` lines; `None` is EOF.
pub trait LineSource {
    fn next_line(&mut self) -> Option<String>;
}

/// The real stream: an admitted `adb -s <serial> exec-out getevent -lt` child with no window.
pub struct AdbLineStream {
    child: Child,
    rx: mpsc::Receiver<String>,
}

impl AdbLineStream {
    /// Admit, then spawn. Refused commands never start a process.
    pub fn spawn<R: Runner>(
        guarded: &GuardedRunner<R>,
        adb: &Path,
        serial: &str,
        device_is_full: bool,
    ) -> Result<Self, RunError> {
        let a = stream_argv(serial);
        guarded.admit_getevent(&a, device_is_full)?;
        let mut cmd = Command::new(adb);
        cmd.args(&a)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        #[cfg(not(windows))]
        let _ = CREATE_NO_WINDOW;
        let mut child = cmd
            .spawn()
            .map_err(|e| RunError::Io(format!("cannot start {}: {e}", adb.display())))?;
        let out = child
            .stdout
            .take()
            .ok_or_else(|| RunError::Io("no stdout".into()))?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        Ok(Self { child, rx })
    }
}

impl LineSource for AdbLineStream {
    fn next_line(&mut self) -> Option<String> {
        self.rx.recv().ok()
    }
}

impl Drop for AdbLineStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Opens the map and the stream; the supervisor calls it again after every EOF.
pub trait Phone {
    type Source: LineSource;
    fn read_map(&mut self) -> Result<String, RunError>;
    fn open_stream(&mut self) -> Result<Self::Source, RunError>;
}

/// Restarts on EOF (with a fresh map; devices hot-plugged in an earlier stream stay excluded) and
/// re-maps after every `add device`. Stops after `max_restarts` restarts or on a refusal.
pub struct Supervisor<P: Phone> {
    pub phone: P,
    pub restarts: u32,
    pub max_restarts: u32,
}

impl<P: Phone> Supervisor<P> {
    pub fn new(phone: P, max_restarts: u32) -> Self {
        Self {
            phone,
            restarts: 0,
            max_restarts,
        }
    }

    /// Run until the restart budget is used; `on_touch` gets every human touch.
    pub fn run(&mut self, mut on_touch: impl FnMut(HumanTouch)) -> Result<(), RunError> {
        let mut hot: Vec<(String, String)> = Vec::new();
        loop {
            let mut det = Detector::from_map_after(&self.phone.read_map()?, &hot);
            let mut src = self.phone.open_stream()?;
            while let Some(line) = src.next_line() {
                if let Some(t) = det.feed(&line) {
                    on_touch(t);
                }
                if det.needs_remap() {
                    det.remap(&self.phone.read_map()?);
                }
            }
            hot = det.hot_devices();
            if self.restarts >= self.max_restarts {
                return Ok(());
            }
            self.restarts += 1;
        }
    }
}
