//! profiles/human.json and profiles/agent.json against schemas/lease-bundle.json, the ruled
//! values, and "vendor keys live only in the profile files".

mod common;

use common::{crate_dir, profiles, profiles_dir, raw_settings};
use handover::profile::{validate, Bundle, BundleKind, NAMESPACES};

fn schema() -> serde_json::Value {
    let p = crate_dir().join("../../schemas/lease-bundle.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn both_profiles_load_and_validate() {
    let p = profiles();
    assert_eq!(p.human.kind, BundleKind::Human);
    assert_eq!(p.agent.kind, BundleKind::Agent);
    for name in ["human", "agent"] {
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(profiles_dir().join(format!("{name}.json"))).unwrap(),
        )
        .unwrap();
        validate(&v).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn the_validator_matches_the_schema_and_its_fixtures() {
    let s = schema();
    assert_eq!(
        s["properties"]["bundle"]["enum"],
        serde_json::json!(["human", "agent"])
    );
    assert_eq!(
        s["properties"]["settings"]["propertyNames"]["pattern"],
        format!("^({}):[A-Za-z0-9_.]+$", NAMESPACES.join("|"))
    );
    assert_eq!(s["required"], serde_json::json!(["bundle", "settings"]));
    let dir = crate_dir().join("../../schemas/fixtures/lease-bundle");
    let mut seen = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        let ok = validate(&v).is_ok();
        assert_eq!(ok, name.starts_with("valid-"), "{name}");
        seen += 1;
    }
    assert!(seen >= 5);
}

#[test]
fn the_human_profile_holds_the_ruled_values() {
    // adaptive brightness, 120 Hz, animations 1x, Gboard, DND off
    let h = raw_settings("human");
    assert_eq!(h["system:screen_brightness_mode"], "1");
    assert_eq!(h["system:peak_refresh_rate"], "120");
    for k in [
        "global:window_animation_scale",
        "global:transition_animation_scale",
        "global:animator_duration_scale",
    ] {
        assert_eq!(h[k], "1.0", "{k}");
    }
    assert!(h["secure:default_input_method"].starts_with("com.google.android.inputmethod.latin/"));
    assert_eq!(profiles().human.dnd_zen_rule, Some(false));
    // round 36: dim to minimum + 60 Hz, animations off, agent keyboard, DND
    let a = raw_settings("agent");
    assert_eq!(a["system:screen_brightness_mode"], "0");
    assert_eq!(a["system:screen_brightness"], "1");
    assert_eq!(a["system:peak_refresh_rate"], "60");
    assert!(a["secure:default_input_method"].starts_with("dev.agentroid.helper/"));
    assert_eq!(profiles().agent.dnd_zen_rule, Some(true));
    // every human key is also an agent key, so the hand-back always rewrites what the agent changed
    for k in h.keys() {
        assert!(a.contains_key(k), "{k}");
    }
}

#[test]
fn a_key_the_device_lacks_is_skipped() {
    let b = Bundle::parse(&std::fs::read_to_string(profiles_dir().join("human.json")).unwrap())
        .unwrap();
    let all = b.writes(None);
    assert_eq!(all.len(), raw_settings("human").len());
    // a device whose 2.16 profile lists every key except the vendor refresh key
    let present: Vec<String> = all
        .iter()
        .map(|w| w.key.clone())
        .filter(|k| k != "miui_refresh_rate")
        .collect();
    let some = b.writes(Some(&present));
    assert_eq!(some.len(), all.len() - 1);
    assert!(!some.iter().any(|w| w.key == "miui_refresh_rate"));
}

#[test]
fn no_setting_key_or_vendor_name_is_written_in_the_code() {
    let mut keys: Vec<String> = raw_settings("human")
        .keys()
        .chain(raw_settings("agent").keys())
        .map(|k| k.split_once(':').unwrap().1.to_string())
        .collect();
    keys.sort();
    keys.dedup();
    for e in std::fs::read_dir(crate_dir().join("src")).unwrap() {
        let p = e.unwrap().path();
        let src = std::fs::read_to_string(&p).unwrap();
        for k in &keys {
            assert!(
                !src.contains(k.as_str()),
                "{} names the setting key {k}: keys live only in profiles/",
                p.display()
            );
        }
        for vendor in ["miui", "xiaomi", "poco", "samsung", "oneui", "hyperos"] {
            assert!(
                !src.to_ascii_lowercase().contains(vendor),
                "{} names the vendor {vendor}",
                p.display()
            );
        }
    }
}
