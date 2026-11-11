//! Is a lease owner's process still alive? (phone-guard `restorer.pid_alive`.) Injected, so a
//! test can "kill" an owner without killing anything.

use std::collections::HashSet;
use std::sync::Mutex;

pub trait PidProbe: Send + Sync {
    fn alive(&self, pid: u32) -> bool;
}

/// The operating system's answer. This process is always alive. On Windows: `OpenProcess` plus a
/// zero-wait `WaitForSingleObject`; access denied means "exists, not ours" (alive). Elsewhere:
/// `/proc/<pid>` when there is a `/proc`, else alive (never restore over a possibly live owner).
#[derive(Debug, Clone, Copy, Default)]
pub struct OsPids;

impl PidProbe for OsPids {
    fn alive(&self, pid: u32) -> bool {
        if pid == std::process::id() {
            return true;
        }
        if pid == 0 {
            return false;
        }
        os_alive(pid)
    }
}

#[cfg(windows)]
fn os_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };
    // SAFETY: plain Win32 calls on a handle we own and close.
    unsafe {
        let h = OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if h.is_null() {
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let r = WaitForSingleObject(h, 0);
        CloseHandle(h);
        r == WAIT_TIMEOUT
    }
}

#[cfg(not(windows))]
fn os_alive(pid: u32) -> bool {
    if std::path::Path::new("/proc/self").exists() {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

/// Test probe: every pid is alive until [`FakePids::kill`]ed.
#[derive(Debug, Default)]
pub struct FakePids {
    dead: Mutex<HashSet<u32>>,
}

impl FakePids {
    pub fn kill(&self, pid: u32) {
        self.dead
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pid);
    }
}

impl PidProbe for FakePids {
    fn alive(&self, pid: u32) -> bool {
        pid != 0
            && !self
                .dead
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_pid_zero_is_not() {
        assert!(OsPids.alive(std::process::id()));
        assert!(!OsPids.alive(0));
        let f = FakePids::default();
        assert!(f.alive(7));
        f.kill(7);
        assert!(!f.alive(7));
    }
}
