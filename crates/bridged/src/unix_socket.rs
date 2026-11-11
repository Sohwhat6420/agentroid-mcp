//! The daemon's transport on Linux and macOS: a Unix-domain socket that only the daemon's own user can use.
//!
//! Why (11 Nov 2026): the AndroidWorld benchmark moves to GitHub Actions, where hardware-accelerated emulators run on
//! Linux runners only. On Windows the daemon listens on a named pipe whose security descriptor names only the current
//! user ([`crate::pipe`]); this is the same promise on Unix, kept twice:
//! - the socket file is made 0600 as soon as it is bound, and
//! - every client's peer credentials are checked on accept; a client running as another user is dropped.
//!
//! There is still deliberately no TCP listener.

use crate::listener::Listener;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

pub struct UnixSocketListener {
    path: PathBuf,
    uid: u32,
    inner: UnixListener,
}

impl UnixSocketListener {
    /// Claim `path` (must run inside a tokio runtime). Fails if another daemon is answering on it; a stale socket
    /// file left by a daemon that died is removed first.
    pub fn bind(path: &str) -> io::Result<Self> {
        let path = PathBuf::from(path);
        if path.exists() {
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} already has a listener", path.display()),
                ));
            }
            let meta = std::fs::symlink_metadata(&path)?;
            if !std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type()) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", path.display()),
                ));
            }
            std::fs::remove_file(&path)?;
        }
        let inner = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        // The socket file was made by this process, so its owner is this process's user.
        let uid = std::fs::metadata(&path)?.uid();
        Ok(UnixSocketListener { path, uid, inner })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UnixSocketListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Listener for UnixSocketListener {
    type Conn = UnixStream;

    async fn accept(&mut self) -> io::Result<UnixStream> {
        loop {
            let (stream, _) = self.inner.accept().await?;
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == self.uid => return Ok(stream),
                // Another user, or credentials we cannot read: drop the client and keep listening.
                _ => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn sock(name: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("agentroid-sock-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join("bridged.sock").to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn the_socket_is_owner_only_and_a_same_user_client_is_accepted() {
        let p = sock("ok");
        let mut l = UnixSocketListener::bind(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let client = tokio::spawn({
            let p = p.clone();
            async move {
                let mut c = UnixStream::connect(&p).await.unwrap();
                c.write_all(b"ping").await.unwrap();
            }
        });
        let mut s = l.accept().await.unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        client.await.unwrap();
    }

    #[tokio::test]
    async fn a_live_listener_is_not_stolen_and_a_stale_file_is_replaced() {
        let p = sock("busy");
        let first = UnixSocketListener::bind(&p).unwrap();
        let err = UnixSocketListener::bind(&p)
            .err()
            .expect("second bind must fail");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        drop(first);
        // A crash leaves the socket file behind with nobody listening.
        let p2 = sock("stale");
        std::fs::create_dir_all(Path::new(&p2).parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&p2).unwrap()); // leaves the file, closes the listener
        assert!(Path::new(&p2).exists());
        let l = UnixSocketListener::bind(&p2).unwrap();
        drop(l);
        assert!(
            !Path::new(&p2).exists(),
            "dropping the listener removes its file"
        );
    }

    #[test]
    fn a_regular_file_at_the_path_is_never_removed() {
        let p = sock("file");
        std::fs::write(&p, b"not a socket").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async { UnixSocketListener::bind(&p).err().unwrap() });
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&p).unwrap(), b"not a socket");
    }
}
