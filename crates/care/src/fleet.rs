//! Parallel phones: each phone has its
//! own [`HeatGuard`]; the laptop memory gate caps how many run at once (measured check: commit
//! charge stays under 75 %).

use std::collections::BTreeMap;

use crate::heat::{HeatGuard, JobKind};
use crate::CareError;

/// May one more phone start, with `running` already running?
pub trait MemoryGate {
    fn admit(&self, running: usize) -> Result<(), String>;
}

/// A fixed cap.
#[derive(Debug, Clone, Copy)]
pub struct FixedGate(pub usize);

impl MemoryGate for FixedGate {
    fn admit(&self, running: usize) -> Result<(), String> {
        if running < self.0 {
            Ok(())
        } else {
            Err(format!(
                "memory gate: {running} phones running, the cap is {}",
                self.0
            ))
        }
    }
}

/// Admits while the laptop's commit charge is under `limit_pct`. The reading is injected
/// (`read_pct`); [`system_commit_pct`] is the real one on Windows.
pub struct CommitChargeGate<F: Fn() -> Option<u8>> {
    pub limit_pct: u8,
    pub read_pct: F,
}

impl<F: Fn() -> Option<u8>> MemoryGate for CommitChargeGate<F> {
    fn admit(&self, _running: usize) -> Result<(), String> {
        match (self.read_pct)() {
            Some(p) if p < self.limit_pct => Ok(()),
            Some(p) => Err(format!(
                "memory gate: commit charge {p} % is not under {} %",
                self.limit_pct
            )),
            None => Err("memory gate: commit charge unreadable; not starting another phone".into()),
        }
    }
}

/// Commit charge in percent: committed / commit limit (`GlobalMemoryStatusEx`, page-file totals).
#[cfg(windows)]
pub fn system_commit_pct() -> Option<u8> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: a zeroed MEMORYSTATUSEX with dwLength set, as the API requires.
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) == 0 || m.ullTotalPageFile == 0 {
            return None;
        }
        let used = m.ullTotalPageFile.saturating_sub(m.ullAvailPageFile);
        Some(
            ((used as f64 / m.ullTotalPageFile as f64) * 100.0)
                .round()
                .min(100.0) as u8,
        )
    }
}

#[cfg(not(windows))]
pub fn system_commit_pct() -> Option<u8> {
    None
}

/// The phones this daemon is caring for, one guard each.
pub struct Fleet<G: MemoryGate> {
    gate: G,
    guards: BTreeMap<String, HeatGuard>,
}

impl<G: MemoryGate> Fleet<G> {
    pub fn new(gate: G) -> Self {
        Fleet {
            gate,
            guards: BTreeMap::new(),
        }
    }

    pub fn running(&self) -> usize {
        self.guards.len()
    }

    /// Start caring for `serial` with a fresh guard of its own. One job per phone; the gate decides
    /// whether another phone may run.
    pub fn start(&mut self, serial: &str, kind: JobKind) -> Result<&mut HeatGuard, CareError> {
        if self.guards.contains_key(serial) {
            return Err(CareError::refused(format!("{serial} already runs a job")));
        }
        self.gate
            .admit(self.guards.len())
            .map_err(CareError::refused)?;
        Ok(self
            .guards
            .entry(serial.to_string())
            .or_insert_with(|| HeatGuard::new(serial, kind)))
    }

    pub fn guard(&mut self, serial: &str) -> Option<&mut HeatGuard> {
        self.guards.get_mut(serial)
    }

    /// The job on `serial` ended (done, stopped, migrated): its guard goes, its slot frees.
    pub fn finish(&mut self, serial: &str) -> Option<HeatGuard> {
        self.guards.remove(serial)
    }
}
