//! The battery floor: pause below 12 % only when the
//! phone is not `powered` (AC, USB, Wireless or Dock `powered: true`). The charging *status* is
//! never read: a phone held at the 80 % charge cap is powered but reports "not charging", and must
//! not trip the floor.
//!
//! Never combined with a battery temperature lease: while `dumpsys battery set` is in force the
//! level is fake, so the check refuses to judge (`Frozen`); and it holds the `floor` marker while it
//! reads, so a temperature lease in any process is refused meanwhile.

use agentroid_adb_link::runner::Runner;

use crate::health::BatteryInfo;
use crate::holds::{self, FLOOR, TEMP_LEASE};
use crate::leases::BATTERY_NS;
use crate::phone::AdbPhone;
use crate::settings::LeaseManager;
use crate::sink::CareSink;
use crate::thresholds::THRESHOLDS;
use crate::CareError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorVerdict {
    /// Powered, or at or above the floor.
    Ok { level: i32, powered: bool },
    /// Below the floor and not powered: pause (and poll again).
    Pause { level: i32 },
    /// A battery override is in force: the level is fake, no judgement.
    Frozen { level: i32, powered: bool },
}

/// The pure rule. Uses `powered`, never `status`.
pub fn battery_floor(b: &BatteryInfo) -> FloorVerdict {
    let level = b.level.unwrap_or(0);
    if b.frozen {
        return FloorVerdict::Frozen {
            level,
            powered: b.powered,
        };
    }
    if b.powered || level >= THRESHOLDS.battery_floor_pct {
        FloorVerdict::Ok {
            level,
            powered: b.powered,
        }
    } else {
        FloorVerdict::Pause { level }
    }
}

/// Read the phone and apply the floor, holding the `floor` marker while doing so.
pub fn check_floor<R: Runner, S: CareSink>(
    phone: &AdbPhone<'_, R>,
    mgr: &LeaseManager<S>,
) -> Result<FloorVerdict, CareError> {
    let ro = phone.ro_serial()?;
    let _hold = holds::hold(&mgr.store, FLOOR, Some(&ro))?;
    let b = phone.battery()?;
    let leased = mgr
        .store
        .entries()?
        .iter()
        .any(|e| e.namespace == BATTERY_NS && e.ident() == ro);
    let pids = mgr.pids();
    if leased || holds::held_by_other(&mgr.store, TEMP_LEASE, Some(&ro), pids.as_ref()) {
        return Ok(FloorVerdict::Frozen {
            level: b.level.unwrap_or(0),
            powered: b.powered,
        });
    }
    Ok(battery_floor(&b))
}
