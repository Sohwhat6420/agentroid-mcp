//! Laptop power while a job runs (V13): `PowerCreateRequest` +
//! `PowerSetRequest(PowerRequestSystemRequired)`, held by a guard that clears the request and
//! closes the handle when it is dropped. The system may still turn the display off; it will not
//! sleep while the guard lives. No process is spawned and no window is opened.

use std::io;
use std::sync::Arc;

/// The four Win32 calls, behind a seam so a test can watch them.
pub trait PowerApi: Send + Sync {
    /// `PowerCreateRequest` with a simple reason string; returns the handle.
    fn create(&self, reason: &str) -> io::Result<usize>;
    /// `PowerSetRequest(handle, PowerRequestSystemRequired)`.
    fn set(&self, handle: usize) -> io::Result<()>;
    /// `PowerClearRequest(handle, PowerRequestSystemRequired)`.
    fn clear(&self, handle: usize) -> io::Result<()>;
    /// `CloseHandle(handle)`.
    fn close(&self, handle: usize);
}

/// The real Win32 power-request API.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsPower;

#[cfg(windows)]
mod win {
    use std::io;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Power::{
        PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest,
    };
    use windows_sys::Win32::System::Threading::{
        POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0,
    };

    /// DIAGNOSTIC_REASON_VERSION / POWER_REQUEST_CONTEXT_VERSION.
    const POWER_REQUEST_CONTEXT_VERSION: u32 = 0;

    pub fn create(reason: &str) -> io::Result<usize> {
        let mut wide: Vec<u16> = reason.encode_utf16().chain(std::iter::once(0)).collect();
        let ctx = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: wide.as_mut_ptr(),
            },
        };
        // SAFETY: ctx and the string it points at are valid for the call; the API copies the string.
        let h: HANDLE = unsafe { PowerCreateRequest(&ctx) };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(h as usize)
    }

    pub fn set(h: usize) -> io::Result<()> {
        // SAFETY: h came from PowerCreateRequest and is still open
        if unsafe { PowerSetRequest(h as HANDLE, PowerRequestSystemRequired) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn clear(h: usize) -> io::Result<()> {
        // SAFETY: as above
        if unsafe { PowerClearRequest(h as HANDLE, PowerRequestSystemRequired) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn close(h: usize) {
        // SAFETY: h came from PowerCreateRequest; it is closed exactly once (by KeepAwake::drop)
        unsafe { CloseHandle(h as HANDLE) };
    }
}

#[cfg(windows)]
impl PowerApi for WindowsPower {
    fn create(&self, reason: &str) -> io::Result<usize> {
        win::create(reason)
    }
    fn set(&self, handle: usize) -> io::Result<()> {
        win::set(handle)
    }
    fn clear(&self, handle: usize) -> io::Result<()> {
        win::clear(handle)
    }
    fn close(&self, handle: usize) {
        win::close(handle)
    }
}

#[cfg(not(windows))]
impl PowerApi for WindowsPower {
    fn create(&self, _: &str) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "power requests exist only on Windows",
        ))
    }
    fn set(&self, _: usize) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "power requests exist only on Windows",
        ))
    }
    fn clear(&self, _: usize) -> io::Result<()> {
        Ok(())
    }
    fn close(&self, _: usize) {}
}

/// Keeps the laptop from sleeping while it lives (one per running job).
pub struct KeepAwake {
    api: Arc<dyn PowerApi>,
    handle: usize,
    cleared: bool,
}

impl std::fmt::Debug for KeepAwake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeepAwake(cleared={})", self.cleared)
    }
}

impl KeepAwake {
    /// Sets a system-required power request with the real Win32 API.
    pub fn begin(reason: &str) -> io::Result<Self> {
        Self::begin_with(Arc::new(WindowsPower), reason)
    }

    /// Same, through `api` (tests).
    pub fn begin_with(api: Arc<dyn PowerApi>, reason: &str) -> io::Result<Self> {
        let handle = api.create(reason)?;
        if let Err(e) = api.set(handle) {
            api.close(handle);
            return Err(e);
        }
        Ok(Self {
            api,
            handle,
            cleared: false,
        })
    }

    /// Clears the request now and reports whether Windows accepted the clear (drop does the same
    /// and ignores the result).
    pub fn end(mut self) -> io::Result<()> {
        self.cleared = true;
        self.api.clear(self.handle)
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if !self.cleared {
            self.cleared = true;
            let _ = self.api.clear(self.handle);
        }
        self.api.close(self.handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Spy(Mutex<Vec<String>>);

    impl PowerApi for Spy {
        fn create(&self, reason: &str) -> io::Result<usize> {
            self.0.lock().unwrap().push(format!("create {reason}"));
            Ok(42)
        }
        fn set(&self, h: usize) -> io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("set {h} SystemRequired"));
            Ok(())
        }
        fn clear(&self, h: usize) -> io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("clear {h} SystemRequired"));
            Ok(())
        }
        fn close(&self, h: usize) {
            self.0.lock().unwrap().push(format!("close {h}"));
        }
    }

    #[test]
    fn guard_sets_while_alive_and_clears_on_drop() {
        let spy = Arc::new(Spy::default());
        {
            let g = KeepAwake::begin_with(spy.clone(), "job").unwrap();
            assert_eq!(
                *spy.0.lock().unwrap(),
                ["create job", "set 42 SystemRequired"]
            );
            drop(g);
        }
        assert_eq!(
            *spy.0.lock().unwrap(),
            [
                "create job",
                "set 42 SystemRequired",
                "clear 42 SystemRequired",
                "close 42"
            ]
        );
        // end() clears once; drop then only closes
        let spy2 = Arc::new(Spy::default());
        KeepAwake::begin_with(spy2.clone(), "j2")
            .unwrap()
            .end()
            .unwrap();
        assert_eq!(
            *spy2.0.lock().unwrap(),
            [
                "create j2",
                "set 42 SystemRequired",
                "clear 42 SystemRequired",
                "close 42"
            ]
        );
    }

    #[test]
    fn a_failed_set_closes_the_handle() {
        struct NoSet(Mutex<Vec<&'static str>>);
        impl PowerApi for NoSet {
            fn create(&self, _: &str) -> io::Result<usize> {
                Ok(7)
            }
            fn set(&self, _: usize) -> io::Result<()> {
                Err(io::Error::other("denied"))
            }
            fn clear(&self, _: usize) -> io::Result<()> {
                self.0.lock().unwrap().push("clear");
                Ok(())
            }
            fn close(&self, _: usize) {
                self.0.lock().unwrap().push("close");
            }
        }
        let api = Arc::new(NoSet(Mutex::new(vec![])));
        assert!(KeepAwake::begin_with(api.clone(), "x").is_err());
        assert_eq!(*api.0.lock().unwrap(), ["close"]);
    }

    /// The real Win32 calls: the request is set (PowerSetRequest returned TRUE) and cleared
    /// (PowerClearRequest returned TRUE). Nothing is spawned; no window opens.
    #[cfg(windows)]
    #[test]
    fn real_power_request_is_set_and_cleared() {
        let g = KeepAwake::begin("Agentroid link-hub test: power request")
            .expect("PowerCreateRequest + PowerSetRequest");
        g.end().expect("PowerClearRequest");
        // and the drop path on a second request
        let g2 = KeepAwake::begin("Agentroid link-hub test: drop path").unwrap();
        drop(g2);
    }
}
