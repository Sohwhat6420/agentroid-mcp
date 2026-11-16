//! Registry v0 and the single-holder lease (task 2.8).
//!
//! * A lease is `{device, holder, epoch}` (`schemas/registry-lease.json`); one holder per device.
//! * Ownership is by connection id, not by name: two sessions that choose the same label are still
//!   two holders. The `holder` string is only what is shown.
//! * The lease dies with its connection ([`Registry::release_conn`], called from a drop guard).
//! * `epoch` is one monotonic counter, bumped on every new grant and written to the state file
//!   (temp file, `sync_all`, rename) *before* the grant is visible, so a crash never leaves a
//!   grant whose epoch a restart could reuse. A corrupt state file is an error, never a reset to 0.
//! * Idle lapse: a connection can stay open long after it stopped using a phone (an MCP server lives as
//!   long as its client's session). With an idle limit set ([`Registry::set_idle_ms`]), a lease whose
//!   holder has made no lease-checked call for that long is handed to the next connection that asks
//!   ([`Registry::acquire_lapsing`]), unless the caller says the device is busy (a live job). Every
//!   successful [`Registry::check_holder`] and re-acquire counts as use.

use crate::proto::Lease;
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

/// The default idle limit of the daemon: 10 minutes with no call from the holder.
pub const DEFAULT_IDLE_MS: u64 = 10 * 60 * 1000;

/// A grant from [`Registry::acquire_lapsing`]: the lease, and the idle one it replaced, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub lease: Lease,
    pub lapsed: Option<Lease>,
}

type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

fn monotonic_ms() -> Clock {
    let t0 = Instant::now();
    Box::new(move || u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX))
}

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
    /// Clock ms of the holder's last lease-checked call (or the grant).
    last: u64,
}

struct Inner {
    epoch: u64,
    leases: BTreeMap<String, Held>,
}

pub struct Registry {
    inner: Mutex<Inner>,
    state: Option<PathBuf>,
    clock: Clock,
    /// `None`: a lease never lapses (the behaviour before the idle limit).
    idle_ms: Mutex<Option<u64>>,
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
            clock: monotonic_ms(),
            idle_ms: Mutex::new(None),
        }
    }

    /// Replace the clock (tests): milliseconds from any fixed origin.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    /// Set the idle limit: a lease unused for `ms` may lapse to the next asker. `None` or `Some(0)`: never.
    pub fn set_idle_ms(&self, ms: Option<u64>) {
        *self
            .idle_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ms.filter(|m| *m > 0);
    }

    pub fn idle_ms(&self) -> Option<u64> {
        *self
            .idle_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// How long the holder of `device` has been idle, in ms (`None`: not leased).
    pub fn idle_for(&self, device: &str) -> Option<u64> {
        let now = (self.clock)();
        self.lock()
            .leases
            .get(device)
            .map(|h| now.saturating_sub(h.last))
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
            clock: monotonic_ms(),
            idle_ms: Mutex::new(None),
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
    /// Never takes a lease from another holder, however idle (see [`Registry::acquire_lapsing`]).
    pub fn acquire(&self, device: &str, conn: u64, holder: &str) -> Result<Lease, AcquireError> {
        self.acquire_lapsing(device, conn, holder, false)
            .map(|g| g.lease)
    }

    /// As [`Registry::acquire`], but a lease held by another connection that has been idle for the idle
    /// limit or longer lapses and is granted to `conn` with a new epoch, but only when `may_lapse` (the
    /// caller passes `false` while the device has a live job, which keeps its lease however quiet it is).
    /// With no idle limit set, or `may_lapse` false, this is exactly `acquire`.
    pub fn acquire_lapsing(
        &self,
        device: &str,
        conn: u64,
        holder: &str,
        may_lapse: bool,
    ) -> Result<Grant, AcquireError> {
        let now = (self.clock)();
        let idle = self.idle_ms();
        let mut g = self.lock();
        let mut lapsed = None;
        if let Some(h) = g.leases.get_mut(device) {
            if h.conn == conn {
                h.last = now;
                return Ok(Grant {
                    lease: h.lease.clone(),
                    lapsed: None,
                });
            }
            let stale = idle.is_some_and(|ms| now.saturating_sub(h.last) >= ms);
            if !(may_lapse && stale) {
                return Err(AcquireError::Held(h.lease.clone()));
            }
            lapsed = Some(h.lease.clone());
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
                last: now,
            },
        );
        Ok(Grant { lease, lapsed })
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

    /// `Ok` when `conn` holds `device` (which counts as use, for the idle limit); otherwise the current
    /// lease, if any.
    pub fn check_holder(&self, device: &str, conn: u64) -> Result<(), Option<Lease>> {
        let now = (self.clock)();
        match self.lock().leases.get_mut(device) {
            Some(h) if h.conn == conn => {
                h.last = now;
                Ok(())
            }
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

    fn clocked() -> (Registry, std::sync::Arc<std::sync::atomic::AtomicU64>) {
        let t = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let c = t.clone();
        let r =
            Registry::in_memory().with_clock(move || c.load(std::sync::atomic::Ordering::SeqCst));
        (r, t)
    }
    fn at(t: &std::sync::atomic::AtomicU64, ms: u64) {
        t.store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    #[test]
    fn an_idle_lease_lapses_to_the_next_asker_with_a_new_epoch() {
        let (r, t) = clocked();
        r.set_idle_ms(Some(600_000));
        let a = r.acquire("D", 1, "a").unwrap();
        at(&t, 599_999);
        assert!(
            matches!(
                r.acquire_lapsing("D", 2, "b", true),
                Err(AcquireError::Held(_))
            ),
            "not idle long enough"
        );
        at(&t, 600_000);
        let g = r.acquire_lapsing("D", 2, "b", true).unwrap();
        assert_eq!(g.lapsed, Some(a));
        assert_eq!(g.lease.holder, "b");
        assert_eq!(g.lease.epoch, 2, "a lapse is a new grant: new epoch");
        assert_eq!(
            r.check_holder("D", 1),
            Err(Some(g.lease.clone())),
            "the old holder is told who has it now"
        );
    }

    #[test]
    fn use_keeps_a_lease_from_lapsing() {
        let (r, t) = clocked();
        r.set_idle_ms(Some(1_000));
        r.acquire("D", 1, "a").unwrap();
        at(&t, 900);
        assert_eq!(r.check_holder("D", 1), Ok(()), "a call by the holder");
        at(&t, 1_800);
        assert!(matches!(
            r.acquire_lapsing("D", 2, "b", true),
            Err(AcquireError::Held(_))
        ));
        assert_eq!(r.idle_for("D"), Some(900));
        at(&t, 2_500);
        r.acquire("D", 1, "a").unwrap(); // re-acquire is use too
        at(&t, 3_400);
        assert!(matches!(
            r.acquire_lapsing("D", 2, "b", true),
            Err(AcquireError::Held(_))
        ));
    }

    #[test]
    fn a_busy_device_or_no_idle_limit_never_lapses() {
        let (r, t) = clocked();
        r.acquire("D", 1, "a").unwrap();
        at(&t, 10_000_000);
        assert!(
            matches!(
                r.acquire_lapsing("D", 2, "b", true),
                Err(AcquireError::Held(_))
            ),
            "no idle limit: never"
        );
        r.set_idle_ms(Some(1_000));
        assert!(
            matches!(
                r.acquire_lapsing("D", 2, "b", false),
                Err(AcquireError::Held(_))
            ),
            "a live job keeps it"
        );
        assert!(
            matches!(r.acquire("D", 2, "b"), Err(AcquireError::Held(_))),
            "plain acquire never takes a lease"
        );
        r.set_idle_ms(Some(0));
        assert_eq!(r.idle_ms(), None, "0 means never");
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
