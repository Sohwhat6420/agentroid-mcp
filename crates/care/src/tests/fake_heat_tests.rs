//! Fake-heat rows: `set temp` always paired with `reset`, never with the floor check, thermalservice
//! injection leased, full devices only, and the 2.10c script tier left as it was.

use std::panic::{catch_unwind, AssertUnwindSafe};

use agentroid_adb_link::guard::{GuardedRunner, S24_SERIAL};
use agentroid_core::gate::DeviceMode;

use super::Rig;
use crate::fake::FakePhone;
use crate::fake_heat::{clear_heat, inject_heat, with_battery_temp};
use crate::floor::{check_floor, FloorVerdict};
use crate::holds::{self, FLOOR};
use crate::leases::{BATTERY_NS, THERMAL_NS};
use crate::phone::AdbPhone;
use crate::settings::Owner;
use crate::sink::CareEventKind;
use crate::CareError;

const FULL: DeviceMode = DeviceMode::Full;

/// After `set temp` (attempted), a `reset` follows, the override is gone and so is the lease.
fn assert_paired(rig: &Rig) {
    let cmds = rig.fake.commands();
    let set = cmds
        .iter()
        .position(|c| c.starts_with("dumpsys battery set temp"))
        .expect("set temp was sent");
    assert!(
        cmds[set..].iter().any(|c| c == "dumpsys battery reset"),
        "{cmds:?}"
    );
    assert!(rig.fake.state().temp_override.is_none());
    assert!(!rig
        .store()
        .entries()
        .unwrap()
        .iter()
        .any(|e| e.namespace == BATTERY_NS));
}

#[test]
fn set_temp_is_always_followed_by_reset_even_on_error() {
    let owner = Owner::here("guard-logic", "s");
    // body succeeds
    let rig = Rig::new("pair-ok", "POCO0030");
    let phone = AdbPhone::new(&rig.runner, "POCO0030");
    let mut mgr = rig.manager();
    let seen = with_battery_temp(&phone, FULL, &mut mgr, &owner, 430, |_| {
        Ok(phone.battery().unwrap().temp_dc)
    })
    .unwrap();
    assert_eq!(seen, Some(430));
    assert_paired(&rig);
    assert_eq!(
        (
            mgr.sink.count(CareEventKind::OverrideSet),
            mgr.sink.count(CareEventKind::OverrideReset)
        ),
        (1, 1)
    );

    // body returns an error
    let rig = Rig::new("pair-err", "POCO0031");
    let phone = AdbPhone::new(&rig.runner, "POCO0031");
    let mut mgr = rig.manager();
    let r: Result<(), _> = with_battery_temp(&phone, FULL, &mut mgr, &owner, 430, |_| {
        Err(CareError::Adb("the guard test failed".into()))
    });
    assert_eq!(
        r.unwrap_err(),
        CareError::Adb("the guard test failed".into())
    );
    assert_paired(&rig);

    // body panics
    let rig = Rig::new("pair-panic", "POCO0032");
    let phone = AdbPhone::new(&rig.runner, "POCO0032");
    let mut mgr = rig.manager();
    let caught = catch_unwind(AssertUnwindSafe(|| {
        let _: Result<(), CareError> =
            with_battery_temp(&phone, FULL, &mut mgr, &owner, 430, |_| panic!("boom"));
    }));
    assert!(caught.is_err());
    assert_paired(&rig);

    // `set temp` itself fails
    let rig = Rig::new("pair-setfail", "POCO0033");
    let phone = AdbPhone::new(&rig.runner, "POCO0033");
    let mut mgr = rig.manager();
    rig.fake.state().fail_prefixes = vec!["dumpsys battery set".into()];
    let mut ran = false;
    let r = with_battery_temp(&phone, FULL, &mut mgr, &owner, 430, |_| {
        ran = true;
        Ok(())
    });
    assert!(r.is_err() && !ran);
    assert_paired(&rig);
}

#[test]
fn an_unverified_reset_keeps_the_lease_and_reconnect_finishes_it() {
    let rig = Rig::new("pair-unverified", "POCO0034");
    let phone = AdbPhone::new(&rig.runner, "POCO0034");
    let owner = Owner {
        pid: 9001,
        job_id: "guard-logic".into(),
        session: "s".into(),
    };
    {
        let mut mgr = rig.manager();
        rig.fake.state().reset_failures = 2; // both verified attempts fail
        let r = with_battery_temp(&phone, FULL, &mut mgr, &owner, 430, |_| Ok(()));
        assert!(matches!(r, Err(CareError::Adb(m)) if m.contains("reset could not be verified")));
        assert_eq!(
            rig.fake
                .commands()
                .iter()
                .filter(|c| *c == "dumpsys battery reset")
                .count(),
            2
        );
        assert_eq!(
            mgr.overrides_in_force("POCO0034").unwrap().len(),
            1,
            "kept for reconnect"
        );
    }
    rig.pids.kill(9001);
    let mut mgr = rig.manager();
    let rep = mgr.on_reconnect(&phone, FULL).unwrap();
    assert_eq!(rep.restored.len(), 1);
    assert!(rig.fake.state().temp_override.is_none());
    assert!(rig.store().entries().unwrap().is_empty());
}

#[test]
fn the_temperature_lease_and_the_floor_check_never_combine() {
    let owner = Owner::here("guard-logic", "s");
    // (a) inside a temperature lease the floor refuses to judge
    let rig = Rig::new("excl-a", "POCO0035");
    let phone = AdbPhone::new(&rig.runner, "POCO0035");
    let mut mgr = rig.manager();
    rig.fake.state().level = 5;
    rig.fake.state().usb = false;
    let v = with_battery_temp(&phone, FULL, &mut mgr, &owner, 430, |m| {
        check_floor(&phone, m)
    })
    .unwrap();
    assert!(matches!(v, FloorVerdict::Frozen { .. }), "{v:?}");
    assert_eq!(
        check_floor(&phone, &mgr).unwrap(),
        FloorVerdict::Pause { level: 5 },
        "judged again after the reset"
    );

    // (b) while a floor check runs (any process), the temperature lease is refused, nothing sent
    let rig = Rig::new("excl-b", "POCO0036");
    let phone = AdbPhone::new(&rig.runner, "POCO0036");
    let mut mgr = rig.manager();
    let floor = holds::hold(&mgr.store, FLOOR, Some("POCO0036")).unwrap();
    let r = with_battery_temp(&phone, FULL, &mut mgr, &owner, 430, |_| Ok(()));
    assert!(matches!(r, Err(CareError::Refused { .. })), "{r:?}");
    assert!(!rig
        .fake
        .commands()
        .iter()
        .any(|c| c.starts_with("dumpsys battery set")));
    assert!(rig.store().entries().unwrap().is_empty());
    drop(floor);
    assert!(with_battery_temp(&phone, FULL, &mut mgr, &owner, 430, |_| Ok(())).is_ok());
    // a marker left by a dead process does not block
    let stale = holds::hold(&mgr.store, FLOOR, Some("POCO0036")).unwrap();
    rig.pids.kill(std::process::id());
    let blocked = holds::held_by_other(&mgr.store, FLOOR, Some("POCO0036"), rig.pids.as_ref());
    assert!(!blocked);
    drop(stale);
}

#[test]
fn thermalservice_heat_is_leased_and_cleared_with_verification() {
    let rig = Rig::new("thermal", "POCO0037");
    let phone = AdbPhone::new(&rig.runner, "POCO0037");
    let mut mgr = rig.manager();
    let owner = Owner::here("heat-test", "s");
    let lease = inject_heat(&phone, FULL, &mut mgr, &owner, 43.0, 3, "skin", "SKIN").unwrap();
    assert_eq!(lease.namespace, THERMAL_NS);
    let cmds = rig.fake.commands();
    assert_eq!(
        cmds,
        [
            "cmd thermalservice override-status 3",
            "cmd thermalservice inject-temperature SKIN SEVERE skin 43.0"
        ]
    );
    assert_eq!(rig.fake.state().status_override, Some(3));
    assert!(
        mgr.changes_in_force("POCO0037").unwrap().is_empty(),
        "overrides are listed separately"
    );
    assert_eq!(mgr.overrides_in_force("POCO0037").unwrap().len(), 1);
    clear_heat(&phone, FULL, &mut mgr, &lease).unwrap();
    assert_eq!(rig.fake.state().status_override, None);
    assert!(rig.store().entries().unwrap().is_empty());
    assert_eq!(
        (
            mgr.sink.count(CareEventKind::OverrideSet),
            mgr.sink.count(CareEventKind::OverrideReset)
        ),
        (1, 1)
    );
    // names never reach the shell unchecked
    assert!(inject_heat(
        &phone,
        FULL,
        &mut mgr,
        &owner,
        43.0,
        3,
        "skin;reboot",
        "SKIN"
    )
    .is_err());
    assert!(inject_heat(&phone, FULL, &mut mgr, &owner, 43.0, 9, "skin", "SKIN").is_err());
}

#[test]
fn fake_heat_runs_on_full_devices_only_through_the_guard() {
    let owner = Owner::here("heat-test", "s");
    let rig = Rig::new("heat-ro", "POCO0038");
    let phone = AdbPhone::new(&rig.runner, "POCO0038");
    let mut mgr = rig.manager();
    for mode in [DeviceMode::ReadOnly, DeviceMode::Helper] {
        assert!(inject_heat(&phone, mode, &mut mgr, &owner, 43.0, 3, "skin", "SKIN").is_err());
        assert!(with_battery_temp(&phone, mode, &mut mgr, &owner, 430, |_| Ok(())).is_err());
    }
    assert!(rig.fake.commands().is_empty());
    // the protected phone behind an address, configured full: only the identity probe goes out
    let s24 = FakePhone::new(S24_SERIAL);
    s24.state().tokens = vec!["198.51.100.9:5555".into()];
    let g = GuardedRunner::new(s24.clone());
    let p = AdbPhone::new(&g, "198.51.100.9:5555");
    assert!(with_battery_temp(&p, FULL, &mut mgr, &owner, 430, |_| Ok(())).is_err());
    assert!(inject_heat(&p, FULL, &mut mgr, &owner, 43.0, 3, "skin", "SKIN").is_err());
    assert!(s24.state().log.iter().all(|l| l == "getprop ro.serialno"));
    // and adb-link's guard itself still refuses it for the protected phone by name, with no allowlist change
    let argv: Vec<String> = ["-s", S24_SERIAL, "shell", "dumpsys battery set temp 430"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(g.guard().check(&argv).is_err());
}

#[test]
fn the_script_shell_tier_still_names_battery_set_and_reset_r2() {
    use sandbox::shell::{classify, RiskClass, ShellVerdict};
    for cmd in [
        "dumpsys battery set temp 430",
        "dumpsys battery reset",
        "cmd thermalservice inject-temperature SKIN SEVERE skin 43.0",
        "cmd thermalservice override-status 3",
    ] {
        let v = classify(cmd);
        assert!(
            matches!(
                v,
                ShellVerdict::Refused {
                    class: RiskClass::R2,
                    ..
                }
            ),
            "{cmd}: {v:?}"
        );
    }
}
