//! The Windows named-pipe listener.
//!
//! * The DACL admits only the current user's SID (`D:P(A;;GA;;;<sid>)`: protected, no inherited
//!   entries, no SYSTEM, no Administrators, no Everyone).
//! * Remote clients are rejected (`reject_remote_clients`), so the pipe never answers over SMB.
//! * The first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`: if anything already owns
//!   the name, [`PipeListener::bind`] fails instead of serving next to a squatter.
//! * The next instance is created before a connected one is handed out, so the name never has zero
//!   instances while the daemon runs (that gap is what a squatter would need).

use crate::listener::Listener;
use std::ffi::c_void;
use std::io;
use std::ptr::null_mut;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The current user's SID as text (`S-1-5-21-...`).
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: plain Win32 calls with correctly sized buffers; the token handle is closed on every path.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len); // fails by design: asks for the size
            if len == 0 {
                return Err(io::Error::last_os_error());
            }
            // u64 elements: TOKEN_USER needs pointer alignment.
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = buf.as_ptr().cast::<TOKEN_USER>();
            let mut wide: *mut u16 = null_mut();
            if ConvertSidToStringSidW((*user).User.Sid, &mut wide) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut n = 0;
            while *wide.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(wide, n));
            LocalFree(wide.cast());
            Ok(s)
        })();
        CloseHandle(token);
        result
    }
}

/// The SDDL string for "this user and nobody else".
pub fn sddl_for_current_user() -> io::Result<String> {
    Ok(format!("D:P(A;;GA;;;{})", current_user_sid()?))
}

/// An owned security descriptor (freed with `LocalFree`).
pub struct PipeSecurity {
    sd: *mut c_void,
}

// SAFETY: the descriptor is immutable after creation and only read by the kernel on pipe creation.
unsafe impl Send for PipeSecurity {}

impl PipeSecurity {
    pub fn current_user_only() -> io::Result<Self> {
        let sddl: Vec<u16> = sddl_for_current_user()?
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut sd: *mut c_void = null_mut();
        // SAFETY: `sddl` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor that Drop frees.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PipeSecurity { sd })
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        // SAFETY: `sd` came from ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            LocalFree(self.sd);
        }
    }
}

fn create_instance(name: &str, sec: &PipeSecurity, first: bool) -> io::Result<NamedPipeServer> {
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sec.sd,
        bInheritHandle: 0,
    };
    let mut opts = ServerOptions::new();
    opts.first_pipe_instance(first).reject_remote_clients(true);
    // SAFETY: `attrs` and the descriptor it points to outlive the call.
    unsafe {
        opts.create_with_security_attributes_raw(
            name,
            (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
}

pub struct PipeListener {
    name: String,
    sec: PipeSecurity,
    next: Option<NamedPipeServer>,
}

impl PipeListener {
    /// Claim `name` (must run inside a tokio runtime). Fails if any instance of it already exists.
    pub fn bind(name: &str) -> io::Result<Self> {
        let sec = PipeSecurity::current_user_only()?;
        let first = create_instance(name, &sec, true)?;
        Ok(PipeListener {
            name: name.to_string(),
            sec,
            next: Some(first),
        })
    }
}

impl Listener for PipeListener {
    type Conn = NamedPipeServer;

    async fn accept(&mut self) -> io::Result<NamedPipeServer> {
        if self.next.is_none() {
            self.next = Some(create_instance(&self.name, &self.sec, false)?);
        }
        if let Some(server) = self.next.as_ref() {
            server.connect().await?;
        }
        match create_instance(&self.name, &self.sec, false) {
            Ok(fresh) => self
                .next
                .replace(fresh)
                .ok_or_else(|| io::Error::other("pipe instance missing")),
            Err(e) => {
                self.next = None; // the connected client is dropped rather than handed out twice
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sddl_names_only_the_current_user() {
        let s = sddl_for_current_user().unwrap();
        assert!(s.starts_with("D:P(A;;GA;;;S-1-"), "{s}");
        assert_eq!(s.matches("(A;").count(), 1, "exactly one ACE");
        assert!(
            !s.contains(";WD)") && !s.contains(";BA)") && !s.contains(";SY)"),
            "no Everyone, Administrators or SYSTEM"
        );
    }
}
