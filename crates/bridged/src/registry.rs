//! Registry v0 and the single-holder lease (task 2.8).
//!
//! * A lease is `{device, holder, epoch}` (`schemas/registry-lease.json`); one holder per device.
//! * Ownership is by connection id, not by name: two sessions that choose the same label are still
//!   two holders. The `holder` string is only what is shown.
//! * The lease dies with its connection ([`Registry::release_conn`], called from a drop guard).
//! * `epoch` is one monotonic counter, bumped on every new grant and written to the state file
//!   (temp file, `sync_all`, rename) *before* the grant is visible, so a crash never leaves a
//!   grant whose epoch a restart could reuse. A corrupt state file is an error, never a reset to 0.

use crate::proto::Lease;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    /// Someone else holds it.
    Held(Lease),
    /// The epoch could not be persisted; nothing was granted.
    Persist(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    Released,
    NotLeased,
    /// Leased, but by another connection.
    NotHolder(Lease),
}

struct Held {
    conn: u64,
    lease: Lease,
}

struct Inner {
    epoch: u64,
    leases: BTreeMap<String, Held>,
}

pub struct Registry {
    inner: Mutex<Inner>,
    state: Option<PathBuf>,
}

fn read_epoch(path: &Path) -> io::Result<u64> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            })?;
            v.get("epoch")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: no integer \"epoch\"", path.display()),
                    )
                })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

/// Atomic write: temp file in the same directory, flushed to disk, then renamed over the target.
fn write_epoch(path: &Path, epoch: u64) -> io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(format!("{{\"epoch\":{epoch}}}\n").as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

impl Registry {
    /// No persistence (tests of pure lease logic).
    pub fn in_memory() -> Self {
        Registry {
            inner: Mutex::new(Inner {
                epoch: 0,
                leases: BTreeMap::new(),
            }),
            state: None,
        }
    }

    /// Load the epoch from `path` (0 when the file does not exist yet). Creates the parent directory.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let epoch = read_epoch(&path)?;
        Ok(Registry {
            inner: Mutex::new(Inner {
                epoch,
                leases: BTreeMap::new(),
            }),
            state: Some(path),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock cannot leave the maps half-updated (each update is one
        // insert/remove after all fallible work), so a poisoned lock is still consistent.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The highest epoch handed out so far.
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Grant `device` to connection `conn`. Re-acquiring your own lease returns it unchanged.
    pub fn acquire(&self, device: &str, conn: u64, holder: &str) -> Result<Lease, AcquireError> {
        let mut g = self.lock();
        if let Some(h) = g.leases.get(device) {
            return if h.conn == conn {
                Ok(h.lease.clone())
            } else {
                Err(AcquireError::Held(h.lease.clone()))
            };
        }
        let next = g
            .epoch
            .checked_add(1)
            .ok_or_else(|| AcquireError::Persist("epoch counter exhausted".into()))?;
        if let Some(path) = &self.state {
            write_epoch(path, next)
                .map_err(|e| AcquireError::Persist(format!("{}: {e}", path.display())))?;
        }
        g.epoch = next;
        let lease = Lease {
            device: device.to_string(),
            holder: holder.to_string(),
            epoch: next,
        };
        g.leases.insert(
            device.to_string(),
            Held {
                conn,
                lease: lease.clone(),
            },
        );
        Ok(lease)
    }

    /// Hands out one more epoch without granting a lease (the link hub's epoch for a phone no
    /// lease covers yet): strictly above both the counter and `floor`, persisted before it is
    /// returned, so it is never reused and every later grant is above it (one epoch source).
    pub fn allocate_epoch_above(&self, floor: u64) -> Result<u64, AcquireError> {
        let mut g = self.lock();
        let next = g
            .epoch
            .max(floor)
            .checked_add(1)
            .ok_or_else(|| AcquireError::Persist("epoch counter exhausted".into()))?;
        if let Some(path) = &self.state {
            write_epoch(path, next)
                .map_err(|e| AcquireError::Persist(format!("{}: {e}", path.display())))?;
        }
        g.epoch = next;
        Ok(next)
    }

    /// Raises the counter to at least `floor` (persisted), so every later grant is above it.
    /// The daemon calls this with the link hub's highest stored epoch when the hub starts.
    pub fn raise_floor(&self, floor: u64) -> Result<u64, AcquireError> {
        let mut g = self.lock();
        if floor > g.epoch {
            if let Some(path) = &self.state {
                write_epoch(path, floor)
                    .map_err(|e| AcquireError::Persist(format!("{}: {e}", path.display())))?;
            }
            g.epoch = floor;
        }
        Ok(g.epoch)
    }

    pub fn release(&self, device: &str, conn: u64) -> ReleaseOutcome {
        let mut g = self.lock();
        match g.leases.get(device) {
            None => ReleaseOutcome::NotLeased,
            Some(h) if h.conn != conn => ReleaseOutcome::NotHolder(h.lease.clone()),
            Some(_) => {
                g.leases.remove(device);
                ReleaseOutcome::Released
            }
        }
    }

    /// Drop every lease of a closed connection; returns them.
    pub fn release_conn(&self, conn: u64) -> Vec<Lease> {
        let mut g = self.lock();
        let devices: Vec<String> = g
            .leases
            .iter()
            .filter(|(_, h)| h.conn == conn)
            .map(|(d, _)| d.clone())
            .collect();
        devices
            .iter()
            .filter_map(|d| g.leases.remove(d))
            .map(|h| h.lease)
            .collect()
    }

    pub fn lease_of(&self, device: &str) -> Option<Lease> {
        self.lock().leases.get(device).map(|h| h.lease.clone())
    }

    /// The lease on `device` when connection `conn` holds it (`None`: nobody, or someone else).
    pub fn lease_held_by(&self, device: &str, conn: u64) -> Option<Lease> {
        self.lock()
            .leases
            .get(device)
            .filter(|h| h.conn == conn)
            .map(|h| h.lease.clone())
    }

    /// `Ok` when `conn` holds `device`; otherwise the current lease, if any.
    pub fn check_holder(&self, device: &str, conn: u64) -> Result<(), Option<Lease>> {
        match self.lock().leases.get(device) {
            Some(h) if h.conn == conn => Ok(()),
            Some(h) => Err(Some(h.lease.clone())),
            None => Err(None),
        }
    }

    pub fn status(&self, device: Option<&str>) -> Vec<Lease> {
        self.lock()
            .leases
            .values()
            .filter(|h| device.is_none_or(|d| h.lease.device == d))
            .map(|h| h.lease.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_per_device_and_release_frees_it() {
        let r = Registry::in_memory();
        let a = r.acquire("D", 1, "a").unwrap();
        assert_eq!(
            a,
            Lease {
                device: "D".into(),
                holder: "a".into(),
                epoch: 1
            }
        );
        assert_eq!(r.acquire("D", 2, "b"), Err(AcquireError::Held(a.clone())));
        assert_eq!(
            r.acquire("D", 1, "a"),
            Ok(a),
            "own lease is idempotent, epoch unchanged"
        );
        assert_eq!(
            r.release("D", 2),
            ReleaseOutcome::NotHolder(r.lease_of("D").unwrap())
        );
        assert_eq!(r.release("D", 1), ReleaseOutcome::Released);
        assert_eq!(r.release("D", 1), ReleaseOutcome::NotLeased);
        assert_eq!(r.acquire("D", 2, "b").unwrap().epoch, 2);
    }

    #[test]
    fn same_label_is_still_a_different_holder() {
        let r = Registry::in_memory();
        r.acquire("D", 1, "same").unwrap();
        assert!(matches!(
            r.acquire("D", 2, "same"),
            Err(AcquireError::Held(_))
        ));
    }

    #[test]
    fn closing_a_connection_frees_all_its_leases() {
        let r = Registry::in_memory();
        r.acquire("D1", 1, "a").unwrap();
        r.acquire("D2", 1, "a").unwrap();
        r.acquire("D3", 2, "b").unwrap();
        assert_eq!(r.release_conn(1).len(), 2);
        assert_eq!(r.status(None).len(), 1);
        assert!(r.release_conn(1).is_empty());
    }

    #[test]
    fn link_epochs_and_lease_epochs_share_one_counter() {
        let r = Registry::in_memory();
        assert_eq!(r.allocate_epoch_above(0), Ok(1));
        assert_eq!(
            r.allocate_epoch_above(10),
            Ok(11),
            "above the hub's stored floor"
        );
        assert_eq!(
            r.acquire("D", 1, "a").unwrap().epoch,
            12,
            "a grant is above every link epoch"
        );
        assert_eq!(r.raise_floor(5), Ok(12), "never lowers");
        assert_eq!(r.raise_floor(30), Ok(30));
        assert_eq!(r.acquire("E", 1, "a").unwrap().epoch, 31);
    }
}
