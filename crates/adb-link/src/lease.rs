//! The protected-device lease book (`docs/design/protected-device-lease.md`).
//!
//! The protected phone ([`crate::guard::S24_SERIAL`]) is a target only while a lease is open on it.
//! Core decides when one opens (an owner's Allow, or the overnight rule) and what it never allows;
//! this book is the one piece of state both core and the adb guard read, so it sits in the lowest
//! crate. It holds at most one lease, and nothing of it is written to disk: a daemon restart closes
//! it.
//!
//! * **Bound to its holder.** A lease names the daemon lease ([`Holder`]: the holder's name and the
//!   registry epoch) it was opened for. Core admits a call under it only for that same holder, and
//!   the daemon closes it when that holder's connection goes ([`LeaseBook::close_epoch`]).
//! * **Bound to the adb names core checked.** The guard lets a non-read command through to the
//!   protected phone only when its `-s` selector is one core has [`LeaseBook::bind`]ed under the open
//!   lease, so a phone core took for an ordinary device (its `ro.serialno` could not be read) is
//!   never let through on the strength of a lease core never applied to it.
//! * **Fail closed.** An unreadable wall clock, or a time past the end, is no lease.
//! * **Every close is told.** A lease the book drops itself (it ran out) or closes for the daemon
//!   (its holder released the phone or went) is queued with the reason, for core to write to the
//!   audit log ([`LeaseBook::take_ended`]).

use std::sync::{Arc, Mutex};

/// The default lease length (`--protected-lease-min`).
pub const DEFAULT_MINUTES: u64 = 30;
/// The longest lease (`--protected-lease-min` is clamped to `1..=MAX_MINUTES`).
pub const MAX_MINUTES: u64 = 120;

/// Clamp a lease length to `1..=MAX_MINUTES`.
pub fn clamp_minutes(m: u64) -> u64 {
    m.clamp(1, MAX_MINUTES)
}

/// Wall-clock time in Unix milliseconds (`None`: it could not be read). Leases use the wall clock
/// because the push channel's answers are dated on it.
pub trait WallClock: Send + Sync {
    fn now_ms(&self) -> Option<u64>;
}

/// The system's wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now_ms(&self) -> Option<u64> {
        let d = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        u64::try_from(d.as_millis()).ok()
    }
}

/// A wall clock tests set by hand. `None` stands for an unreadable clock.
#[derive(Debug, Default)]
pub struct ManualClock(Mutex<Option<u64>>);

impl ManualClock {
    pub fn new(ms: u64) -> Self {
        ManualClock(Mutex::new(Some(ms)))
    }
    pub fn set(&self, ms: Option<u64>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = ms;
    }
    pub fn advance(&self, ms: u64) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = g.as_mut() {
            *t = t.saturating_add(ms);
        }
    }
}

impl WallClock for ManualClock {
    fn now_ms(&self) -> Option<u64> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// How a lease came to be open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Granted {
    /// The owner tapped Allow on the lease push (request `request_id`).
    Approved { request_id: String },
    /// The overnight rule: a signed, fresh presence file said the owner is asleep.
    Overnight,
}

impl Granted {
    pub fn as_str(&self) -> &'static str {
        match self {
            Granted::Approved { .. } => "approved",
            Granted::Overnight => "overnight",
        }
    }
}

/// The daemon lease a protected-device lease belongs to: the holder's name and the registry epoch
/// of that grant. A new grant of the phone (another connection, or the same one after a release)
/// has a new epoch, so it never inherits the lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub name: String,
    pub epoch: u64,
}

/// One open lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The push request id behind it (an ask, or the overnight notice).
    pub id: String,
    pub holder: Holder,
    /// The job it was opened for, when a job asked (the push and the audit log name it).
    pub job: Option<String>,
    pub opened_at_ms: u64,
    pub expires_at_ms: u64,
    pub granted: Granted,
    /// The adb serials core checked to be the protected phone and admitted under this lease.
    pub bound: Vec<String>,
}

impl Lease {
    /// Whole seconds left at `now_ms`.
    pub fn seconds_left(&self, now_ms: u64) -> u64 {
        self.expires_at_ms.saturating_sub(now_ms) / 1000
    }
}

/// At most one lease, shared (`Arc`) by core and the adb guard.
pub struct LeaseBook {
    clock: Arc<dyn WallClock>,
    open: Mutex<Option<Lease>>,
    /// Leases closed without core (run out, released, connection gone), with why, not yet audited.
    ended: Mutex<Vec<(Lease, &'static str)>>,
}

/// How many closes the book keeps for the audit log before it drops the oldest.
pub const ENDED_KEPT: usize = 64;

impl std::fmt::Debug for LeaseBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseBook")
            .field("open", &self.lock().as_ref().map(|l| l.id.clone()))
            .finish()
    }
}

impl LeaseBook {
    pub fn new(clock: Arc<dyn WallClock>) -> Self {
        LeaseBook {
            clock,
            open: Mutex::new(None),
            ended: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Lease>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The wall clock, `None` when it cannot be read.
    pub fn now_ms(&self) -> Option<u64> {
        self.clock.now_ms()
    }

    /// Open `lease`, replacing any other. Returns it.
    pub fn open(&self, lease: Lease) -> Lease {
        *self.lock() = Some(lease.clone());
        lease
    }

    /// The open lease, if the clock can be read and it has not run out. A lease that has run out is
    /// dropped here.
    pub fn active(&self) -> Option<Lease> {
        let now = self.now_ms();
        let mut g = self.lock();
        match (now, g.as_ref()) {
            (Some(t), Some(l)) if t < l.expires_at_ms => Some(l.clone()),
            (Some(_), Some(_)) => {
                if let Some(l) = g.take() {
                    self.ended_push(l, "expired");
                }
                None
            }
            _ => None,
        }
    }

    /// Close the lease at once. Returns the one that was open.
    pub fn close(&self) -> Option<Lease> {
        self.lock().take()
    }

    /// Close the lease if it belongs to the daemon lease of `epoch` (its holder's connection went,
    /// or it released the phone; `why` says which, for the audit log). Returns the one closed.
    pub fn close_epoch(&self, epoch: u64, why: &'static str) -> Option<Lease> {
        let mut g = self.lock();
        if g.as_ref().is_some_and(|l| l.holder.epoch == epoch) {
            let l = g.take();
            drop(g);
            if let Some(l) = &l {
                self.ended_push(l.clone(), why);
            }
            return l;
        }
        None
    }

    fn ended_push(&self, l: Lease, why: &'static str) {
        let mut e = self.ended.lock().unwrap_or_else(|e| e.into_inner());
        if e.len() >= ENDED_KEPT {
            e.remove(0);
        }
        e.push((l, why));
    }

    /// The leases closed without core since the last call (oldest first), with why: core writes
    /// each to the audit log.
    pub fn take_ended(&self) -> Vec<(Lease, &'static str)> {
        std::mem::take(&mut *self.ended.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Put closes that could not be written back at the front of the queue, oldest first, ahead
    /// of any that ended meanwhile (core writes them on its next try). None is dropped: while one
    /// waits, core opens no lease, so the queue cannot grow without bound.
    pub fn requeue_ended(&self, unwritten: Vec<(Lease, &'static str)>) {
        if unwritten.is_empty() {
            return;
        }
        let mut e = self.ended.lock().unwrap_or_else(|e| e.into_inner());
        let later = std::mem::take(&mut *e);
        e.extend(unwritten);
        e.extend(later);
    }

    /// Is a close waiting to be written to the audit log?
    pub fn has_ended(&self) -> bool {
        !self
            .ended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Name the job now running under the daemon lease of `epoch` (`None`: it ended) on the open
    /// lease, when that lease is the epoch's.
    pub fn set_job(&self, epoch: u64, job: Option<String>) {
        let mut g = self.lock();
        if let Some(l) = g.as_mut().filter(|l| l.holder.epoch == epoch) {
            l.job = job;
        }
    }

    /// Record that core admitted `adb_serial` (checked to be the protected phone) under the open
    /// lease. Nothing happens without an active lease.
    pub fn bind(&self, adb_serial: &str) {
        if self.active().is_none() {
            return;
        }
        let mut g = self.lock();
        if let Some(l) = g.as_mut() {
            let s = adb_serial.trim();
            if !s.is_empty() && !l.bound.iter().any(|b| b.eq_ignore_ascii_case(s)) {
                l.bound.push(s.to_string());
            }
        }
    }

    /// May the guard let a command for selector `adb_serial` through to the protected phone? Only
    /// while a lease is active and core bound that very name under it.
    pub fn admits(&self, adb_serial: &str) -> bool {
        self.active().is_some_and(|l| {
            l.bound
                .iter()
                .any(|b| b.eq_ignore_ascii_case(adb_serial.trim()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_700_000_000_000;

    fn lease(epoch: u64) -> Lease {
        Lease {
            id: "abcdefghijklmnopqrst".into(),
            holder: Holder {
                name: "lane".into(),
                epoch,
            },
            job: None,
            opened_at_ms: T0,
            expires_at_ms: T0 + 60_000,
            granted: Granted::Overnight,
            bound: vec![],
        }
    }

    #[test]
    fn a_lease_lives_until_its_end_on_a_readable_clock() {
        let clock = Arc::new(ManualClock::new(T0));
        let book = LeaseBook::new(clock.clone());
        assert!(book.active().is_none());
        book.open(lease(7));
        assert!(book.active().is_some());
        clock.advance(59_999);
        assert_eq!(book.active().map(|l| l.seconds_left(T0 + 59_999)), Some(0));
        clock.set(None);
        assert!(book.active().is_none(), "an unreadable clock is no lease");
        clock.set(Some(T0 + 60_000));
        assert!(book.active().is_none(), "the end is exclusive");
        clock.set(Some(T0));
        assert!(book.active().is_none(), "a lease that ran out was dropped");
        let ended = book.take_ended();
        assert_eq!(
            ended.len(),
            1,
            "the run-out lease is queued once for the audit log"
        );
        assert_eq!(ended[0].1, "expired");
        assert!(book.take_ended().is_empty());
    }

    /// Review round 3, item 10: closes that could not be written go back in front, in order, and
    /// none is dropped.
    #[test]
    fn unwritten_closes_go_back_in_front_of_later_ones() {
        let clock = Arc::new(ManualClock::new(T0));
        let book = LeaseBook::new(clock.clone());
        assert!(!book.has_ended());
        book.requeue_ended(vec![]);
        assert!(!book.has_ended());
        for e in 1..=2 {
            book.open(lease(e));
            book.close_epoch(e, "released");
        }
        let first = book.take_ended();
        assert!(!book.has_ended());
        book.open(lease(3));
        book.close_epoch(3, "released");
        book.requeue_ended(first);
        assert!(book.has_ended());
        let all: Vec<u64> = book
            .take_ended()
            .iter()
            .map(|(l, _)| l.holder.epoch)
            .collect();
        assert_eq!(all, [1, 2, 3]);
        let many: Vec<(Lease, &'static str)> = (0..ENDED_KEPT as u64 + 5)
            .map(|e| (lease(e), "x"))
            .collect();
        book.requeue_ended(many);
        assert_eq!(book.take_ended().len(), ENDED_KEPT + 5);
    }

    #[test]
    fn the_guard_admits_only_names_core_bound_under_the_lease() {
        let clock = Arc::new(ManualClock::new(T0));
        let book = LeaseBook::new(clock.clone());
        book.bind("192.0.2.60:5555");
        assert!(!book.admits("192.0.2.60:5555"), "no lease, nothing bound");
        book.open(lease(7));
        assert!(!book.admits("192.0.2.60:5555"), "open, but not bound");
        book.bind("192.0.2.60:5555");
        assert!(book.admits("192.0.2.60:5555"));
        assert!(!book.admits("192.0.2.61:5555"));
        assert!(
            book.close_epoch(8, "released").is_none(),
            "another holder's epoch"
        );
        assert!(book.take_ended().is_empty());
        assert!(book.close_epoch(7, "released").is_some());
        assert_eq!(book.take_ended()[0].1, "released");
        assert!(!book.admits("192.0.2.60:5555"));
        assert_eq!(clamp_minutes(0), 1);
        assert_eq!(clamp_minutes(u64::MAX), MAX_MINUTES);
    }
}
