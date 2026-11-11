//! Generic credentials in Windows Credential Manager, for the MCP HTTP listener's bearer tokens
//! (task 2.6). Node has no API for this, so the TypeScript side spawns this helper hidden.
//!
//! Protocol (see main.rs): one JSON request on stdin, one JSON answer on stdout.
//! - `{"op":"read","target":T}` gives `{"ok":true,"secret":S}`
//! - `{"op":"write","target":T,"secret":S,"user":U}` gives `{"ok":true}` (`user` is optional)
//! - `{"op":"delete","target":T}` gives `{"ok":true,"deleted":bool}`
//! - `{"op":"list","prefix":P}` gives `{"ok":true,"items":[{"target":T,"secret":S}, ...]}`
//! - any failure gives `{"ok":false,"error":"..."}`; an error message never contains a secret.
//!
//! Only targets that start with `agentroid:` are read or written. Credentials are stored as
//! CRED_TYPE_GENERIC with CRED_PERSIST_LOCAL_MACHINE (this user, this machine, not roamed).

use serde_json::{json, Value};

/// Every target this helper touches starts with this.
pub const PREFIX: &str = "agentroid:";
/// CRED_MAX_CREDENTIAL_BLOB_SIZE.
pub const MAX_SECRET: usize = 5 * 512;
pub const MAX_TARGET: usize = 256;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NotFound,
    BadRequest(&'static str),
    BadTarget(&'static str),
    BadSecret(&'static str),
    Os(u32),
    Unsupported,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "not found"),
            Error::BadRequest(s) | Error::BadTarget(s) | Error::BadSecret(s) => write!(f, "{s}"),
            Error::Os(code) => write!(f, "Credential Manager error {code}"),
            Error::Unsupported => write!(f, "Credential Manager exists only on Windows"),
        }
    }
}

impl std::error::Error for Error {}

fn bad_chars(s: &str) -> bool {
    s.chars().any(|c| c.is_control() || c == '*')
}

/// A target must start with `agentroid:`, be at most 256 bytes, and hold no control character
/// or `*` (the CredEnumerateW wildcard).
pub fn check_target(t: &str) -> Result<(), Error> {
    if !t.starts_with(PREFIX) {
        return Err(Error::BadTarget("target must start with agentroid:"));
    }
    if t.len() > MAX_TARGET {
        return Err(Error::BadTarget("target is longer than 256 bytes"));
    }
    if bad_chars(t) {
        return Err(Error::BadTarget("target holds a control character or *"));
    }
    Ok(())
}

fn check_secret(s: &str) -> Result<(), Error> {
    if s.is_empty() {
        return Err(Error::BadSecret("secret is empty"));
    }
    if s.len() > MAX_SECRET {
        return Err(Error::BadSecret("secret is longer than 2560 bytes"));
    }
    Ok(())
}

#[cfg(windows)]
mod win {
    use super::Error;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND, FILETIME};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW, CREDENTIALW,
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// # Safety
    /// `p` is null or points at a NUL-terminated UTF-16 string.
    unsafe fn from_wide(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut n = 0;
        // SAFETY: the caller promises a NUL-terminated string
        while unsafe { *p.add(n) } != 0 {
            n += 1;
        }
        // SAFETY: n u16s were just read from p
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
    }

    /// # Safety
    /// `c` came from CredReadW or CredEnumerateW and has not been freed.
    unsafe fn blob(c: &CREDENTIALW) -> Result<String, Error> {
        if c.CredentialBlob.is_null() || c.CredentialBlobSize == 0 {
            return Ok(String::new());
        }
        // SAFETY: the API gives CredentialBlobSize bytes at CredentialBlob
        let bytes =
            unsafe { std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize) };
        String::from_utf8(bytes.to_vec())
            .map_err(|_| Error::BadSecret("the stored secret is not UTF-8"))
    }

    fn last_error() -> Error {
        // SAFETY: no preconditions
        let e = unsafe { GetLastError() };
        if e == ERROR_NOT_FOUND {
            Error::NotFound
        } else {
            Error::Os(e)
        }
    }

    pub fn read(target: &str) -> Result<String, Error> {
        let t = wide(target);
        let mut p: *mut CREDENTIALW = null_mut();
        // SAFETY: t is NUL-terminated; p receives a buffer we free below
        if unsafe { CredReadW(t.as_ptr(), CRED_TYPE_GENERIC, 0, &mut p) } == 0 {
            return Err(last_error());
        }
        // SAFETY: CredReadW succeeded, so p points at one credential
        let r = unsafe { blob(&*p) };
        // SAFETY: p came from CredReadW
        unsafe { CredFree(p as *const _) };
        r
    }

    pub fn write(target: &str, secret: &str, user: &str) -> Result<(), Error> {
        let mut t = wide(target);
        let mut u = wide(user);
        let mut bytes = secret.as_bytes().to_vec();
        let cred = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: t.as_mut_ptr(),
            Comment: null_mut(),
            LastWritten: FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: null_mut(),
            TargetAlias: null_mut(),
            UserName: u.as_mut_ptr(),
        };
        // SAFETY: every pointer in cred is valid for the call
        let ok = unsafe { CredWriteW(&cred, 0) };
        bytes.fill(0);
        if ok == 0 {
            Err(last_error())
        } else {
            Ok(())
        }
    }

    pub fn delete(target: &str) -> Result<bool, Error> {
        let t = wide(target);
        // SAFETY: t is NUL-terminated
        if unsafe { CredDeleteW(t.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0 {
            return match last_error() {
                Error::NotFound => Ok(false),
                e => Err(e),
            };
        }
        Ok(true)
    }

    pub fn list(prefix: &str) -> Result<Vec<(String, String)>, Error> {
        let filter = wide(&format!("{prefix}*"));
        let mut count: u32 = 0;
        let mut arr: *mut *mut CREDENTIALW = null_mut();
        // SAFETY: filter is NUL-terminated; arr receives a buffer we free below
        if unsafe { CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut arr) } == 0 {
            return match last_error() {
                Error::NotFound => Ok(Vec::new()),
                e => Err(e),
            };
        }
        let mut out = Vec::new();
        let mut failure = None;
        for i in 0..count as usize {
            // SAFETY: CredEnumerateW gave `count` valid credential pointers
            let c = unsafe { &**arr.add(i) };
            if c.Type != CRED_TYPE_GENERIC {
                continue;
            }
            // SAFETY: TargetName is a NUL-terminated string owned by the buffer
            let target = unsafe { from_wide(c.TargetName) };
            if !target.starts_with(prefix) {
                continue;
            }
            // SAFETY: c is valid until CredFree below
            match unsafe { blob(c) } {
                Ok(secret) => out.push((target, secret)),
                Err(e) => failure = Some(e),
            }
        }
        // SAFETY: arr came from CredEnumerateW
        unsafe { CredFree(arr as *const _) };
        match failure {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }
}

#[cfg(not(windows))]
mod win {
    use super::Error;
    pub fn read(_: &str) -> Result<String, Error> {
        Err(Error::Unsupported)
    }
    pub fn write(_: &str, _: &str, _: &str) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
    pub fn delete(_: &str) -> Result<bool, Error> {
        Err(Error::Unsupported)
    }
    pub fn list(_: &str) -> Result<Vec<(String, String)>, Error> {
        Err(Error::Unsupported)
    }
}

/// The secret stored under `target`.
pub fn read(target: &str) -> Result<String, Error> {
    check_target(target)?;
    win::read(target)
}

/// Stores `secret` under `target`, replacing any old value.
pub fn write(target: &str, secret: &str, user: &str) -> Result<(), Error> {
    check_target(target)?;
    check_secret(secret)?;
    if user.len() > 256 || bad_chars(user) {
        return Err(Error::BadRequest(
            "user must be at most 256 bytes with no control character or *",
        ));
    }
    win::write(target, secret, user)
}

/// Deletes `target`; false if there was nothing to delete.
pub fn delete(target: &str) -> Result<bool, Error> {
    check_target(target)?;
    win::delete(target)
}

/// Every (target, secret) whose target starts with `prefix`.
pub fn list(prefix: &str) -> Result<Vec<(String, String)>, Error> {
    check_target(prefix)?;
    win::list(prefix)
}

/// One protocol request in, one answer out. Never panics on bad input.
pub fn handle(input: &str) -> Value {
    match dispatch(input) {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

fn dispatch(input: &str) -> Result<Value, Error> {
    let req: Value =
        serde_json::from_str(input).map_err(|_| Error::BadRequest("the request is not JSON"))?;
    let field = |k: &str| req.get(k).and_then(Value::as_str);
    let target = || field("target").ok_or(Error::BadRequest("missing target"));
    match field("op").ok_or(Error::BadRequest("missing op"))? {
        "read" => Ok(json!({ "ok": true, "secret": read(target()?)? })),
        "write" => {
            let secret = field("secret").ok_or(Error::BadRequest("missing secret"))?;
            write(target()?, secret, field("user").unwrap_or("agentroid"))?;
            Ok(json!({ "ok": true }))
        }
        "delete" => Ok(json!({ "ok": true, "deleted": delete(target()?)? })),
        "list" => {
            let prefix = field("prefix").ok_or(Error::BadRequest("missing prefix"))?;
            let items: Vec<Value> = list(prefix)?
                .into_iter()
                .map(|(t, s)| json!({ "target": t, "secret": s }))
                .collect();
            Ok(json!({ "ok": true, "items": items }))
        }
        _ => Err(Error::BadRequest("unknown op")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_outside_the_prefix_and_wildcards_are_refused() {
        assert!(check_target("agentroid:mcp-token:ABC:full").is_ok());
        assert_eq!(
            check_target("windows:other-app"),
            Err(Error::BadTarget("target must start with agentroid:"))
        );
        assert!(check_target("agentroid:*").is_err());
        assert!(check_target("agentroid:a\nb").is_err());
        assert!(check_target(&format!("agentroid:{}", "x".repeat(300))).is_err());
        assert!(read("not-ours").is_err());
        assert!(delete("not-ours").is_err());
        assert!(list("").is_err());
    }

    #[test]
    fn bad_requests_answer_ok_false_and_never_echo_the_secret() {
        let secret = "agr_SECRET_MUST_NOT_ECHO_0123456789abcdef";
        let cases = [
            "not json".to_string(),
            "{}".to_string(),
            json!({ "op": "nope" }).to_string(),
            json!({ "op": "write", "target": "agentroid:x" }).to_string(),
            json!({ "op": "write", "target": "elsewhere:x", "secret": secret }).to_string(),
            json!({ "op": "write", "target": "agentroid:x", "secret": "" }).to_string(),
            json!({ "op": "write", "target": "agentroid:x", "secret": "s".repeat(MAX_SECRET + 1) })
                .to_string(),
            json!({ "op": "write", "target": "agentroid:x", "secret": secret, "user": "a\u{7}b" })
                .to_string(),
            json!({ "op": "list" }).to_string(),
            json!({ "op": "list", "prefix": "agentroid:*" }).to_string(),
        ];
        for c in cases {
            let out = handle(&c);
            assert_eq!(out["ok"], json!(false), "{c} -> {out}");
            assert!(out["error"].is_string());
            assert!(!out.to_string().contains(secret), "secret echoed for {c}");
        }
    }

    /// Deletes the throwaway target even if an assertion fails.
    #[cfg(windows)]
    struct Throwaway(String);
    #[cfg(windows)]
    impl Drop for Throwaway {
        fn drop(&mut self) {
            let _ = delete(&self.0);
        }
    }

    #[cfg(windows)]
    #[test]
    fn round_trip_on_a_throwaway_target_then_deleted() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let target = format!("agentroid:test:credstore-{}-{nanos}", std::process::id());
        let guard = Throwaway(target.clone());
        assert_eq!(read(&target), Err(Error::NotFound));

        write(&target, "first-secret-value", "test").unwrap();
        assert_eq!(read(&target).unwrap(), "first-secret-value");
        write(&target, "second-secret-value", "test").unwrap();
        assert_eq!(read(&target).unwrap(), "second-secret-value");

        let found = list(&target).unwrap();
        assert_eq!(
            found,
            vec![(target.clone(), "second-secret-value".to_string())]
        );

        // the same through the JSON protocol
        let out = handle(&json!({ "op": "read", "target": target }).to_string());
        assert_eq!(out, json!({ "ok": true, "secret": "second-secret-value" }));

        assert!(delete(&target).unwrap());
        assert_eq!(read(&target), Err(Error::NotFound));
        assert!(!delete(&target).unwrap());
        assert_eq!(
            handle(&json!({ "op": "read", "target": target }).to_string()),
            json!({ "ok": false, "error": "not found" })
        );
        assert!(list(&target).unwrap().is_empty());
        drop(guard);
        assert_eq!(read(&target), Err(Error::NotFound));
    }
}
