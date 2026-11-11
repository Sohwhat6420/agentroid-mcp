//! The cross-language fixtures in `schemas/fixtures/segment/`, generated here and checked byte for byte.
//!
//! `SEGMENTS_WRITE_FIXTURES=1 cargo test --offline -p agentroid-segments fixtures` rewrites them; a normal run checks
//! that the files on disk are exactly what this generator produces (RFC 6979 signatures are deterministic, so the
//! signatures are stable too). `canonical.json` holds the test hub's public key and, per fixture, the canonical text
//! and the SHA-256 of the signed bytes; the Java suite (`SegmentFixturesTest`) recomputes both and verifies every
//! signature with that key.
//!
//! The test hub key is a FIXED, PUBLIC test scalar (SHA-256 of a label). It signs fixtures only; it is never a real
//! hub key.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::canonical;
use crate::classes::{Phase, Risk, SafeClasses};
use crate::segment::{self, HubKey, Post, Segment, SegmentRequest, SignedSegment, Step};
use crate::time;

pub const DEVICE: &str = "f1xture5erial";
pub const EPOCH: u64 = 7;
pub const NOW: &str = "2026-10-20T12:00:00Z";
pub const EXPIRES: &str = "2026-10-20T12:30:00Z";
pub const SETTINGS: &str = "com.android.settings";

pub fn test_hub_key() -> HubKey {
    let scalar = Sha256::digest(b"agentroid-segment fixtures: public test hub key v1");
    HubKey::from_scalar(&scalar).expect("test scalar is valid")
}

pub fn now() -> i64 {
    time::parse_utc(NOW).unwrap()
}

pub fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/fixtures/segment")
}

fn req(id: &str, owned: bool, steps: Vec<Step>) -> SegmentRequest {
    SegmentRequest {
        segment_id: id.into(),
        run_id: "run-fixture-1".into(),
        device: DEVICE.into(),
        epoch: EPOCH,
        owned,
        expires: time::parse_utc(EXPIRES).unwrap(),
        steps,
    }
}

fn built(r: SegmentRequest) -> SignedSegment {
    segment::build_and_sign(
        r,
        &SafeClasses::bundled(),
        Phase::Two,
        now(),
        &test_hub_key(),
    )
    .expect("fixture builds")
}

/// A hub that mislabels: signs without classifying (the phone must still refuse it).
fn unchecked(id: &str, owned: bool, class: Risk, steps: Vec<Step>) -> SignedSegment {
    let seg = Segment {
        v: 1,
        segment_id: id.into(),
        run_id: "run-fixture-1".into(),
        device: DEVICE.into(),
        epoch: EPOCH,
        owned,
        classification: class,
        expires: EXPIRES.into(),
        steps,
    };
    segment::sign_unchecked(seg, &test_hub_key())
}

fn wait(id: &str, ms: u64) -> Step {
    let mut s = Step::new(id, "wait", SETTINGS, Risk::R0, Post::none());
    s.value = Some(json!(ms));
    s
}

/// (file name, signed segment, what the phone must answer: "ok" or a refusal code).
pub fn valid_set() -> Vec<(&'static str, SignedSegment, &'static str)> {
    let mut scroll = Step::new(
        "s3",
        "scroll",
        SETTINGS,
        Risk::R0,
        Post::present(json!({"text": "About phone"})),
    );
    scroll.direction = Some("down".into());
    let r0 = built(req(
        "seg-r0",
        false,
        vec![
            Step::new("s1", "read", SETTINGS, Risk::R0, Post::none())
                .with_select(json!({"id": "com.android.settings:id/battery_percentage"})),
            wait("s2", 500),
            scroll,
            Step::new(
                "s4",
                "back",
                SETTINGS,
                Risk::R0,
                Post::absent(json!({"text": "About phone"})),
            ),
        ],
    ));

    let mut tap_timeout = Step::new(
        "s2",
        "tap",
        SETTINGS,
        Risk::R1,
        Post::present(json!({"text": "Internet"})),
    )
    .with_select(json!({"text": "Network & internet"}));
    tap_timeout.timeout_ms = Some(3000);
    let r1_tap = built(req(
        "seg-r1-tap",
        false,
        vec![
            Step::new("s1", "assert", SETTINGS, Risk::R0, Post::none())
                .with_select(json!({"desc": "Search settings"})),
            tap_timeout,
            Step::new(
                "s3",
                "tap",
                SETTINGS,
                Risk::R1,
                Post::text_equals(json!({"id": "android:id/title"}), "Internet"),
            )
            .with_select(json!({"id": "com.android.settings:id/internet"})),
        ],
    ));

    // Strings that exercise every canonical escape: quotes, backslash, slash, the five short escapes, a \u00xx
    // control, U+007F, Latin-1, a non-breaking hyphen, a BMP symbol and an astral (surrogate-pair) emoji.
    let edge = built(req("seg-edge \"q\" \\ /", false, vec![
        Step::new("é1", "read", "com.android.chrome", Risk::R0, Post::none()).with_select(json!({"text": "Caf\u{e9} \"Wi\u{2011}Fi\" \u{2615} \\ a/b \u{8}\t\n\u{c}\r \u{1}\u{1f}\u{7f} \u{1F600}"})),
        Step::new("s2", "tap", "com.android.chrome", Risk::R0, Post::present(json!({"near_text": "\u{1F600} done"}))).with_select(json!({"desc": "Zoom \u{2212}"})),
        wait("s3", 0),
    ]));

    let mut update = Step::new(
        "s2",
        "tap",
        "com.android.vending",
        Risk::R1,
        Post::present(json!({"text": "Open"})),
    )
    .with_select(json!({"text": "Update"}));
    update.timeout_ms = Some(300_000);
    let play_owned = built(req(
        "seg-play-update",
        true,
        vec![
            Step::new(
                "s1",
                "assert",
                "com.android.vending",
                Risk::R0,
                Post::none(),
            )
            .with_select(json!({"text": "Update"})),
            update.clone(),
        ],
    ));

    let mut toggle = Step::new(
        "s1",
        "settings_set",
        SETTINGS,
        Risk::R1,
        Post::setting_equals(),
    );
    toggle.namespace = Some("system".into());
    toggle.key = Some("accelerometer_rotation".into());
    toggle.value = Some(json!("0"));
    toggle.scope = Some("job".into());
    toggle.lease = Some("lease-run-fixture-1-rotation".into());
    let toggle_seg = built(req(
        "seg-toggle",
        false,
        vec![toggle.clone(), wait("s2", 250)],
    ));

    // A hub that labels a `type` step R1: `type` is not on the safe list, so the phone must refuse.
    let mut typed = Step::new(
        "s2",
        "type",
        SETTINGS,
        Risk::R1,
        Post::text_equals(json!({"id": "android:id/search_src_text"}), "wifi"),
    )
    .with_select(json!({"id": "android:id/search_src_text"}));
    typed.value = Some(json!("wifi"));
    let mislabelled = unchecked(
        "seg-mislabelled",
        false,
        Risk::R1,
        vec![
            Step::new("s1", "read", SETTINGS, Risk::R0, Post::none())
                .with_select(json!({"text": "Settings"})),
            typed,
        ],
    );

    // An honest R2 label on a permanent settings write: refused locally whatever the segment's classification says.
    let mut perm = toggle.clone();
    perm.id = "s2".into();
    perm.scope = Some("permanent".into());
    perm.risk = Risk::R2;
    let r2 = unchecked(
        "seg-r2",
        false,
        Risk::R1,
        vec![
            Step::new("s1", "read", SETTINGS, Risk::R0, Post::none())
                .with_select(json!({"text": "Display"})),
            perm,
        ],
    );

    // The Update button on a phone the hub does not own: a plain unique tap in the Play Store, which is not R1.
    let play_not_owned = unchecked("seg-play-not-owned", false, Risk::R1, vec![update]);

    vec![
        ("valid-r0.json", r0, "ok"),
        ("valid-r1-tap.json", r1_tap, "ok"),
        ("valid-r1-canonical-edge-cases.json", edge, "ok"),
        ("valid-r1-play-store-update-owned.json", play_owned, "ok"),
        ("valid-r1-settings-toggle-leased.json", toggle_seg, "ok"),
        (
            "valid-mislabelled-type-step.json",
            mislabelled,
            "step_not_safe",
        ),
        ("valid-r2-step-for-refusal-test.json", r2, "step_not_safe"),
        (
            "valid-play-store-update-not-owned.json",
            play_not_owned,
            "step_not_safe",
        ),
    ]
}

/// Schema-invalid variants of a real signed segment: (file name, JSON).
pub fn invalid_set(base: &SignedSegment) -> Vec<(&'static str, Value)> {
    let b = serde_json::to_value(base).unwrap();
    let edit = |f: &dyn Fn(&mut Value)| {
        let mut v = b.clone();
        f(&mut v);
        v
    };
    vec![
        (
            "invalid-empty-steps.json",
            edit(&|v| v["segment"]["steps"] = json!([])),
        ),
        (
            "invalid-missing-signature.json",
            edit(&|v| {
                v.as_object_mut().unwrap().remove("signature");
            }),
        ),
        (
            "invalid-step-missing-risk.json",
            edit(&|v| {
                v["segment"]["steps"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("risk");
            }),
        ),
        (
            "invalid-wrong-signature-alg.json",
            edit(&|v| v["signature"]["alg"] = json!("HS256")),
        ),
        (
            "invalid-classification-r2.json",
            edit(&|v| v["segment"]["classification"] = json!("R2")),
        ),
        (
            "invalid-missing-device.json",
            edit(&|v| {
                v["segment"].as_object_mut().unwrap().remove("device");
            }),
        ),
        (
            "invalid-step-missing-post.json",
            edit(&|v| {
                v["segment"]["steps"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("post");
            }),
        ),
        (
            "invalid-bad-key-id.json",
            edit(&|v| v["signature"]["key_id"] = json!("not-hex")),
        ),
        (
            "invalid-expires-with-offset.json",
            edit(&|v| v["segment"]["expires"] = json!("2026-10-20T12:30:00+00:00")),
        ),
        (
            "invalid-rel-selector.json",
            edit(&|v| v["segment"]["steps"][0]["select"] = json!({"rel": {"x": 1, "y": 0}})),
        ),
        (
            "invalid-unknown-step-field.json",
            edit(&|v| v["segment"]["steps"][0]["coords"] = json!([1, 2])),
        ),
    ]
}

pub fn canonical_doc(valid: &[(&'static str, SignedSegment, &'static str)]) -> Value {
    let key = test_hub_key();
    use base64::Engine as _;
    let cases: Vec<Value> = valid
        .iter()
        .map(|(name, s, expect)| {
            let seg = serde_json::to_value(&s.segment).unwrap();
            let text = canonical::to_canonical(&seg).unwrap();
            let bytes = canonical::signed_bytes(&seg).unwrap();
            let sha: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
            json!({"fixture": name, "canonical": text, "signed_bytes_sha256": sha, "expect": expect})
        })
        .collect();
    json!({
        "about": "Cross-language vectors for schemas/segment.json. Generated by crates/segments/src/fixtures.rs; checked by its tests and by android/runner SegmentFixturesTest. hub_spki is a PUBLIC TEST key (fixtures only).",
        "domain": canonical::DOMAIN,
        "hub_spki": base64::engine::general_purpose::STANDARD.encode(key.spki_der()),
        "hub_key_id": key.key_id(),
        "device": DEVICE,
        "epoch": EPOCH,
        "now": NOW,
        "cases": cases,
    })
}

/// Every file the generator owns, name to exact contents.
pub fn all_files() -> Vec<(String, String)> {
    let valid = valid_set();
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, s, _) in &valid {
        out.push((name.to_string(), s.to_json() + "\n"));
    }
    for (name, v) in invalid_set(&valid[1].1) {
        out.push((name.to_string(), serde_json::to_string(&v).unwrap() + "\n"));
    }
    out.push((
        "canonical.json".into(),
        serde_json::to_string_pretty(&canonical_doc(&valid)).unwrap() + "\n",
    ));
    out
}

#[test]
fn fixtures_on_disk_are_exactly_the_generated_ones() {
    let files = all_files();
    let d = dir();
    if std::env::var("SEGMENTS_WRITE_FIXTURES").as_deref() == Ok("1") {
        std::fs::create_dir_all(&d).unwrap();
        for (name, text) in &files {
            std::fs::write(d.join(name), text).unwrap();
        }
    }
    for (name, text) in &files {
        let on_disk = std::fs::read_to_string(d.join(name))
            .unwrap_or_else(|e| panic!("{name}: {e} (run with SEGMENTS_WRITE_FIXTURES=1)"));
        assert_eq!(
            &on_disk.replace("\r\n", "\n"),
            text,
            "{name} differs from the generator"
        );
    }
    let mut on_disk: Vec<String> = std::fs::read_dir(&d)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let mut expected: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
    expected.sort();
    assert_eq!(
        on_disk, expected,
        "stale or extra files in schemas/fixtures/segment"
    );
}
