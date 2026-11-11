//! Migration rows.

use agentroid_adb_link::guard::S24_SERIAL;
use agentroid_core::gate::{effective_mode, DeviceMode};

use super::temp_dir;
use crate::migrate::{
    choose_target, has_capability, load_profile, version_in_range, Candidate, MigrateError,
    MigrationNeed, ProfileCaps,
};

/// xorshift64*: deterministic, no crate needed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

fn profile(serial: &str, tier: &str) -> ProfileCaps {
    ProfileCaps {
        serial: serial.into(),
        maker: "Xiaomi".into(),
        model: "POCO X6 Pro".into(),
        sdk: 35,
        skin: "HyperOS".into(),
        settings_keys: vec!["peak_refresh_rate".into(), "min_refresh_rate".into()],
        refresh_modes: vec![60, 120],
        virtual_display: true,
        ime_switchable: true,
        fgs_types: vec!["connectedDevice".into()],
        appfunctions: false,
        wifi_adb: true,
        tier: tier.into(),
        probed: true,
    }
}

fn cand(serial: &str, mode: DeviceMode, tier: &str, version: &str, temp: i32) -> Candidate {
    Candidate {
        serial: serial.into(),
        ro_serial: Some(serial.into()),
        mode,
        owned: true,
        available: true,
        profile: Some(profile(serial, tier)),
        app_version: Some(version.into()),
        battery_dc: Some(temp),
    }
}

fn need(source: &str) -> MigrationNeed {
    MigrationNeed {
        job_id: "j".into(),
        source: source.into(),
        migratable: true,
        package: "com.fujifilm.xapp".into(),
        version_range: ">=4.2, <5".into(),
        needs: vec!["virtual_display".into(), "refresh:60".into()],
    }
}

#[test]
fn a_hundred_randomised_routings_never_pick_a_read_only_device() {
    let mut rng = Rng(0x2131_3c4e_5a1f_0001);
    let (mut picked, mut tempting) = (0, 0);
    for round in 0..100 {
        let n = 2 + rng.below(6) as usize;
        let mut cs = vec![];
        for i in 0..n {
            let ro = rng.chance(40);
            let s24 = !ro && rng.chance(10);
            let serial = if s24 {
                S24_SERIAL.to_string()
            } else {
                format!("PH{round:03}{i}")
            };
            // configured full, but the protected phone is read_only by identity
            let mode = if ro {
                DeviceMode::ReadOnly
            } else if rng.chance(10) {
                DeviceMode::Helper
            } else {
                DeviceMode::Full
            };
            // read_only phones get the most tempting profile: best tier, coolest, every capability
            let tier = if ro || s24 {
                "T0"
            } else {
                ["T1", "T2", "T3"][rng.below(3) as usize]
            };
            let temp = if ro || s24 {
                250
            } else {
                300 + rng.below(150) as i32
            };
            let ver = if rng.chance(85) { "4.3.1" } else { "3.9" };
            let mut c = cand(&serial, mode, tier, ver, temp);
            c.available = rng.chance(90);
            c.owned = rng.chance(95);
            cs.push(c);
        }
        let src = cs[rng.below(n as u64) as usize].serial.clone();
        if cs.iter().any(|c| {
            c.serial != src && effective_mode(&[&c.serial], c.mode) == DeviceMode::ReadOnly
        }) {
            tempting += 1;
        }
        match choose_target(&need(&src), &cs) {
            Ok(t) => {
                picked += 1;
                let c = cs.iter().find(|c| c.serial == t).unwrap();
                assert_ne!(
                    effective_mode(&[&c.serial], c.mode),
                    DeviceMode::ReadOnly,
                    "round {round}: picked read_only {t}"
                );
                assert_eq!(c.mode, DeviceMode::Full);
                assert_ne!(t, src);
            }
            Err(MigrateError::NoTarget(why)) => {
                for (s, reason) in &why {
                    let c = cs.iter().find(|c| &c.serial == s).unwrap();
                    if effective_mode(&[&c.serial], c.mode) == DeviceMode::ReadOnly {
                        assert!(reason.contains("read_only"), "{reason}");
                    }
                }
            }
            Err(e) => panic!("round {round}: {e:?}"),
        }
    }
    assert!(
        picked >= 30,
        "the routing picked something in {picked} of 100 rounds"
    );
    assert!(
        tempting >= 30,
        "read_only phones were on offer in {tempting} of 100 rounds"
    );
}

#[test]
fn migratable_false_never_moves() {
    let mut n = need("A");
    n.migratable = false;
    assert_eq!(
        choose_target(&n, &[cand("B", DeviceMode::Full, "T0", "4.3", 300)]),
        Err(MigrateError::NotMigratable)
    );
}

#[test]
fn version_range_capabilities_and_ranking() {
    for (v, r, ok) in [
        ("4.3.1", ">=4.2, <5", true),
        ("5.0", ">=4.2, <5", false),
        ("4.2", ">=4.2 <5", true),
        ("4.1.9", ">=4.2", false),
        ("v4.2.0-beta", "=4.2", true),
        ("4.2.7", "4.2.*", true),
        ("4.3.0", "4.2.x", false),
        ("1.0", "*", true),
        ("1.0", "", true),
    ] {
        assert_eq!(version_in_range(v, r), Ok(ok), "{v} in {r}");
    }
    assert!(version_in_range("abc", ">=1").is_err());
    assert!(version_in_range("1.0", ">=x").is_err());

    let p = profile("B", "T1");
    assert_eq!(has_capability(&p, "refresh:120"), Ok(true));
    assert_eq!(has_capability(&p, "refresh:90"), Ok(false));
    assert_eq!(has_capability(&p, "sdk>=34"), Ok(true));
    assert_eq!(has_capability(&p, "tier<=T1"), Ok(true));
    assert_eq!(has_capability(&p, "tier<=T0"), Ok(false));
    assert_eq!(
        has_capability(&p, "settings_key:min_refresh_rate"),
        Ok(true)
    );
    assert_eq!(has_capability(&p, "fgs:connectedDevice"), Ok(true));
    assert_eq!(has_capability(&p, "appfunctions"), Ok(false));
    assert!(has_capability(&p, "teleport").is_err());

    // out of range and missing capability are refused; the best tier, then the coolest, wins
    let mut no_vd = cand("C", DeviceMode::Full, "T0", "4.3", 300);
    no_vd.profile.as_mut().unwrap().virtual_display = false;
    let cs = vec![
        cand("A", DeviceMode::Full, "T0", "4.3", 300), // the source
        cand("B", DeviceMode::Full, "T0", "5.1", 300), // out of range
        no_vd,
        cand("D", DeviceMode::Full, "T2", "4.3", 280),
        cand("E", DeviceMode::Full, "T1", "4.3", 390),
        cand("F", DeviceMode::Full, "T1", "4.3", 330),
    ];
    assert_eq!(choose_target(&need("A"), &cs), Ok("F".into()));
    let mut unknown = need("A");
    unknown.needs.push("teleport".into());
    assert!(matches!(
        choose_target(&unknown, &cs),
        Err(MigrateError::Invalid(_))
    ));
    // an unprobed profile, or one for another phone, does not qualify
    let mut unprobed = cand("G", DeviceMode::Full, "T0", "4.3", 200);
    unprobed.profile.as_mut().unwrap().probed = false;
    let mut wrong = cand("H", DeviceMode::Full, "T0", "4.3", 200);
    wrong.profile.as_mut().unwrap().serial = "OTHER".into();
    wrong.ro_serial = None;
    assert!(
        matches!(choose_target(&need("A"), &[unprobed, wrong]), Err(MigrateError::NoTarget(w)) if w.len() == 2)
    );
}

#[test]
fn profiles_load_from_profiles_dir_in_probe_format() {
    let dir = temp_dir("profiles");
    // the shape bridged's probe.rs writes (schemas/profile.json), including fields routing ignores
    let json = r#"{"serial":"POCO0040","maker":"Xiaomi","model":"23113RKC6G","overlay_box":{"left":0,"top":0,"right":10,"bottom":10,"rotation":0},"sdk":35,"skin":"HyperOS","skin_version":"OS2.0","touchscreen_node":"/dev/input/event5","settings_keys":["peak_refresh_rate"],"refresh_modes":[60,120],"virtual_display":true,"ime_switchable":true,"fgs_types":["connectedDevice","dataSync"],"appfunctions":false,"wifi_adb":true,"tier":"T1","oem_notes":[],"probed":true}"#;
    std::fs::write(dir.join("POCO0040.json"), json).unwrap();
    let p = load_profile(&dir, "POCO0040").unwrap();
    assert_eq!(
        (p.tier_rank(), p.sdk, p.refresh_modes.clone()),
        (2, 35, vec![60, 120])
    );
    std::fs::write(dir.join("POCO0041.json"), json).unwrap();
    assert!(
        load_profile(&dir, "POCO0041").is_err(),
        "the file must name its own serial"
    );
    assert!(load_profile(&dir, "../POCO0040").is_err());
    assert!(load_profile(&dir, "MISSING").is_err());
}
