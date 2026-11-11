//! The 2.13 test rows, on fakes and an injected clock. No adb, no device, no window.

mod fake_heat_tests;
mod fleet_tests;
mod heat_tests;
mod lease_tests;
mod migrate_tests;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agentroid_adb_link::guard::GuardedRunner;

use crate::clock::{FakeClock, TEST_EPOCH_MS};
use crate::fake::FakePhone;
use crate::leases::LeaseStore;
use crate::pid::FakePids;
use crate::settings::LeaseManager;
use crate::sink::MemoryCareSink;

/// A fresh, empty directory under the system temp dir, unique per test.
pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "care-test-{}-{tag}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One test rig: a fake phone behind adb-link's real guard, a lease manager over a temp
/// `leases.json`, a fake clock and fake pids.
pub(crate) struct Rig {
    pub fake: FakePhone,
    pub runner: GuardedRunner<FakePhone>,
    pub clock: Arc<FakeClock>,
    pub pids: Arc<FakePids>,
    pub dir: PathBuf,
}

impl Rig {
    pub fn new(tag: &str, serial: &str) -> Rig {
        let fake = FakePhone::new(serial);
        Rig {
            runner: GuardedRunner::new(fake.clone()),
            fake,
            clock: Arc::new(FakeClock::new(TEST_EPOCH_MS)),
            pids: Arc::new(FakePids::default()),
            dir: temp_dir(tag),
        }
    }

    pub fn store(&self) -> LeaseStore {
        LeaseStore::new(self.dir.join("leases.json"))
    }

    /// A manager as a (new) daemon process would build it.
    pub fn manager(&self) -> LeaseManager<MemoryCareSink> {
        LeaseManager::new(
            self.store(),
            MemoryCareSink::default(),
            self.clock.clone(),
            self.pids.clone(),
        )
    }
}
