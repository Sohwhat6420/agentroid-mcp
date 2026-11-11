//! `care`: task 2.13 (Care), every piece that runs on the laptop without a phone.
//!
//! A port of claude-phone's `phone-guard` , sharing its `leases.json` store and format.
//!
//! * [`thresholds`]: the one const table (42/37 °C, 60 min rest cap, 12 % floor, pacing, ...).
//! * [`heat`]: the heat guard state machine (battery temperature pauses, skin and headroom pace).
//! * [`floor`]: the battery floor (12 % when not `powered`).
//! * [`leases`]: the `leases.json` store (atomic writes, lock file, phone-guard's record format).
//! * [`settings`]: job-scoped settings leases: scopes, compare-and-swap restore, `settings delete`
//!   for absent keys, re-assert, restore on reconnect, `changes_in_force`, journal entries.
//! * [`fake_heat`]: `cmd thermalservice` heat injection, and (guard-logic tests only) the leased
//!   `dumpsys battery set temp` that is always followed by `dumpsys battery reset`.
//! * [`migrate`]: picking a target phone from `profiles/<serial>.json` and the app-pack version range.
//! * [`fleet`]: one guard per phone and a memory gate on how many run at once.
//! * [`phone`]: the only way this crate reaches a phone: adb-link's [`GuardedRunner`].
//! * [`fake`]: a fake phone (an adb-link `Runner`) for tests; it never starts adb.
//!
//! Nothing here starts adb by itself: every device call goes through a `GuardedRunner` the caller
//! hands in.
//!
//! [`GuardedRunner`]: agentroid_adb_link::guard::GuardedRunner

pub mod clock;
pub mod fake;
pub mod fake_heat;
pub mod fleet;
pub mod floor;
pub mod health;
pub mod heat;
pub mod holds;
pub mod leases;
pub mod migrate;
pub mod phone;
pub mod pid;
pub mod settings;
pub mod sink;
pub mod thresholds;

#[cfg(test)]
mod tests;

use std::fmt;

/// Why a care call did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CareError {
    /// Refused before the phone was touched. `class` is `Some("R2")` for a risk-class refusal
    /// (a `permanent` lease), `Some("read_only")` for a read-only device, else `None`.
    Refused {
        class: Option<&'static str>,
        reason: String,
    },
    /// The phone answered with an error.
    Adb(String),
    /// The phone could not be reached (offline, timeout, unidentifiable).
    Unreachable(String),
    /// `leases.json` could not be read, locked or written.
    Store(String),
    /// A bad argument (namespace, key, value, range ...).
    Invalid(String),
    /// The journal sink refused an entry. The change it describes did happen and is leased.
    Journal(String),
}

impl CareError {
    pub fn refused(reason: impl Into<String>) -> Self {
        CareError::Refused {
            class: None,
            reason: reason.into(),
        }
    }
    /// The class of a refusal (`R2`, `read_only`), if any.
    pub fn class(&self) -> Option<&'static str> {
        match self {
            CareError::Refused { class, .. } => *class,
            _ => None,
        }
    }
}

impl fmt::Display for CareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CareError::Refused {
                class: Some(c),
                reason,
            } => write!(f, "refused ({c}): {reason}"),
            CareError::Refused {
                class: None,
                reason,
            } => write!(f, "refused: {reason}"),
            CareError::Adb(m) => write!(f, "adb: {m}"),
            CareError::Unreachable(m) => write!(f, "unreachable: {m}"),
            CareError::Store(m) => write!(f, "leases.json: {m}"),
            CareError::Invalid(m) => write!(f, "invalid: {m}"),
            CareError::Journal(m) => write!(f, "journal: {m}"),
        }
    }
}

impl std::error::Error for CareError {}
