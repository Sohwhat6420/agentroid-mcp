//! `adb-link`: task 2.1 (adb link crate).
//!
//! * [`parse`]: `adb devices -l`, `adb mdns services`, phone id, `choose` (port of phone-guard).
//! * [`guard`]: deny-by-default command guard, S24 never a target.
//! * [`runner`]: the `Runner`, `Prober` and `Clock` seams, with real implementations.
//! * [`gone`]: `device_gone` cause and human fix.
//! * [`watcher`]: the 15 s watcher (mDNS first, then the last working address).
//!
//! Standard library only. Nothing here starts adb unless a caller hands it an [`runner::AdbRunner`].

pub mod gone;
pub mod guard;
pub mod parse;
pub mod runner;
pub mod watcher;

/// Crate name, used by tests and diagnostics.
pub fn name() -> &'static str {
    "adb-link"
}

#[cfg(test)]
mod tests {
    #[test]
    fn name_is_stable() {
        assert_eq!(super::name(), "adb-link");
    }
}
