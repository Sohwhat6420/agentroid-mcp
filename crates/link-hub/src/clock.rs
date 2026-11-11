//! Time for the status bookkeeping (last seen, the 10-minute offline rule). Network timeouts
//! (handshake 15 s) use tokio's timer; everything a test needs to fast-forward goes through here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    /// Monotonic milliseconds.
    fn now_ms(&self) -> u64;
    /// Unix milliseconds, for display and persisted timestamps.
    fn wall_ms(&self) -> u64;
}

/// The real clock.
#[derive(Debug)]
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
    fn wall_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A clock that moves only when told to.
#[derive(Debug, Default)]
pub struct FakeClock {
    ms: AtomicU64,
}

impl FakeClock {
    pub fn new(start_ms: u64) -> Self {
        Self {
            ms: AtomicU64::new(start_ms),
        }
    }
    pub fn advance(&self, ms: u64) {
        self.ms.fetch_add(ms, Ordering::SeqCst);
    }
    pub fn set(&self, ms: u64) {
        self.ms.store(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.ms.load(Ordering::SeqCst)
    }
    fn wall_ms(&self) -> u64 {
        1_760_000_000_000 + self.now_ms()
    }
}
