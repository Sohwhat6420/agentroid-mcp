//! Fake heat.
//!
//! * [`inject_heat`]: `cmd thermalservice override-status` + `inject-temperature` (V4), for heat
//!   tests. A `thermal_override` lease record is written first; [`clear_heat`] resets and verifies.
//! * [`with_battery_temp`]: `dumpsys battery set temp` (V3), **guard-logic tests only**: compiled
//!   only under `cfg(test)` or the `guard-logic-tests` feature, which bridged never enables. Always
//!   leased (record written before the phone is touched), always followed by a verified
//!   `dumpsys battery reset`, even when the body errs or panics or `set` itself fails, and refused
//!   while the battery-floor check runs (any process).
//!
//! Kept apart from the 2.10c script shell tier (`sandbox::shell`, which names `dumpsys battery
//! set|reset` R2 for scripts): these functions take typed numbers, build the command line here,
//! and run it through adb-link's `GuardedRunner` directly. They are not reachable from
//! `run_script`, the `agentroid.shell` binding or any flow step; nothing a script writes is passed
//! through, and the script tier's classification is not changed (a test checks it still refuses).

use agentroid_adb_link::runner::Runner;
use agentroid_core::gate::{effective_mode, DeviceMode};
use serde_json::Map;

use crate::leases::{LeaseEntry, THERMAL_NS};
use crate::phone::{AdbPhone, ABSENT};
use crate::settings::{LeaseManager, Owner, RestoreStatus};
use crate::sink::CareSink;
use crate::CareError;

/// `android.os.Temperature` THROTTLING_NONE .. THROTTLING_SHUTDOWN, as `inject-temperature` spells them.
pub const STATUS_NAMES: [&str; 7] = [
    "NONE",
    "LIGHT",
    "MODERATE",
    "SEVERE",
    "CRITICAL",
    "EMERGENCY",
    "SHUTDOWN",
];

/// phone-guard `THERMAL_NAME_RE` `^[A-Za-z0-9_]{1,32}$`.
pub fn thermal_name_ok(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Full devices only: fake heat changes device state.
fn full_only<R: Runner>(phone: &AdbPhone<'_, R>, mode: DeviceMode) -> Result<String, CareError> {
    if effective_mode(&[phone.serial()], mode) != DeviceMode::Full {
        return Err(CareError::Refused {
            class: Some(mode.as_str()),
            reason: "fake heat runs on full devices only".into(),
        });
    }
    let ro = phone.ro_serial()?;
    if effective_mode(&[phone.serial(), &ro], mode) != DeviceMode::Full {
        return Err(CareError::Refused {
            class: Some("read_only"),
            reason: "fake heat runs on full devices only".into(),
        });
    }
    Ok(ro)
}

fn override_entry<S: CareSink>(
    mgr: &LeaseManager<S>,
    phone_serial: &str,
    ro: &str,
    ns: &str,
    key: &str,
    applied: &str,
    owner: &Owner,
) -> LeaseEntry {
    LeaseEntry {
        key: key.into(),
        namespace: ns.into(),
        before: ABSENT.into(),
        applied: applied.into(),
        owner_pid: owner.pid,
        job_id: owner.job_id.clone(),
        ts: mgr.now_ts(),
        serial: phone_serial.into(),
        ro_serial: ro.into(),
        scope: None,
        expires: None,
        session: (!owner.session.is_empty()).then(|| owner.session.clone()),
        extra: Map::new(),
    }
}

/// Feed thermalservice a reading and a status. Returns the lease; end it with [`clear_heat`].
/// Command shapes are AOSP's `inject-temperature TYPE STATUS NAME [VALUE]` (unverified on the POCO).
#[allow(clippy::too_many_arguments)]
pub fn inject_heat<R: Runner, S: CareSink>(
    phone: &AdbPhone<'_, R>,
    mode: DeviceMode,
    mgr: &mut LeaseManager<S>,
    owner: &Owner,
    temp_c: f32,
    status: usize,
    name: &str,
    sensor_type: &str,
) -> Result<LeaseEntry, CareError> {
    if !(thermal_name_ok(name) && thermal_name_ok(sensor_type))
        || status >= STATUS_NAMES.len()
        || !temp_c.is_finite()
        || !(-40.0..=150.0).contains(&temp_c)
    {
        return Err(CareError::Invalid(
            "thermal name/type must be [A-Za-z0-9_]{1,32}, status 0..6, temperature -40..150 °C"
                .into(),
        ));
    }
    let ro = full_only(phone, mode)?;
    let entry = mgr.store.upsert(override_entry(
        mgr,
        phone.serial(),
        &ro,
        THERMAL_NS,
        name,
        sensor_type,
        owner,
    ))?;
    mgr.note_override(&entry)?;
    let sent = phone
        .shell_ok(&format!("cmd thermalservice override-status {status}"))
        .and_then(|_| {
            phone.shell_ok(&format!(
                "cmd thermalservice inject-temperature {sensor_type} {} {name} {temp_c:.1}",
                STATUS_NAMES[status]
            ))
        });
    if let Err(e) = sent {
        let _ = mgr.restore_entry(phone, mode, &entry); // undo what half-happened; the lease stays if that fails
        return Err(e);
    }
    Ok(entry)
}

/// Undo [`inject_heat`]: the lease is dropped only after a verified reset.
pub fn clear_heat<R: Runner, S: CareSink>(
    phone: &AdbPhone<'_, R>,
    mode: DeviceMode,
    mgr: &mut LeaseManager<S>,
    lease: &LeaseEntry,
) -> Result<(), CareError> {
    match mgr.restore_entry(phone, mode, lease) {
        RestoreStatus::Reset => Ok(()),
        other => Err(CareError::Adb(format!("cmd thermalservice reset could not be verified ({other:?}); the lease is kept for reconnect"))),
    }
}

/// Freeze the battery temperature at `tenths` deci-°C for the body (guard-logic tests only).
#[cfg(any(test, feature = "guard-logic-tests"))]
pub fn with_battery_temp<R: Runner, S: CareSink, T>(
    phone: &AdbPhone<'_, R>,
    mode: DeviceMode,
    mgr: &mut LeaseManager<S>,
    owner: &Owner,
    tenths: i32,
    body: impl FnOnce(&mut LeaseManager<S>) -> Result<T, CareError>,
) -> Result<T, CareError> {
    use crate::holds::{self, FLOOR, TEMP_LEASE};
    use crate::leases::BATTERY_NS;
    use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

    if !(-200..=1000).contains(&tenths) {
        return Err(CareError::Invalid(
            "battery temperature must be -20.0 .. 100.0 °C".into(),
        ));
    }
    let ro = full_only(phone, mode)?;
    let _hold = holds::hold(&mgr.store, TEMP_LEASE, Some(&ro))?;
    let pids = mgr.pids();
    if holds::held_by_other(&mgr.store, FLOOR, Some(&ro), pids.as_ref()) {
        return Err(CareError::refused("the battery-floor check is active; a battery temperature lease is never combined with it"));
    }
    // The crash record, before the phone is touched.
    let entry = mgr.store.upsert(override_entry(
        mgr,
        phone.serial(),
        &ro,
        BATTERY_NS,
        "temp",
        "set",
        owner,
    ))?;
    let noted = mgr.note_override(&entry);
    let outcome =
        match noted.and_then(|_| phone.shell_ok(&format!("dumpsys battery set temp {tenths}"))) {
            Ok(_) => catch_unwind(AssertUnwindSafe(|| body(mgr))),
            Err(e) => Ok(Err(e)),
        };
    // Always: the verified reset. The record leaves leases.json only if it worked.
    let reset = mgr.restore_entry(phone, mode, &entry);
    let result = match outcome {
        Err(panic) => resume_unwind(panic),
        Ok(r) => r,
    };
    if reset != RestoreStatus::Reset {
        return Err(CareError::Adb(format!("dumpsys battery reset could not be verified ({reset:?}); the lease is kept for reconnect")));
    }
    result
}
