//! The brief's Rust tests (2.14): sign/verify and tamper, R2 refuses the build, require_confirmation blocked in
//! Phase 2, stale epoch and expiry refused, duplicate step reports flagged, canonical fixtures match.

use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::canonical;
use crate::classes::{derive_safe_kind, Decision, Phase, Risk, SafeClasses, SAFE_CLASSES_JSON};
use crate::fixtures::{self, now, test_hub_key, DEVICE, EPOCH, SETTINGS};
use crate::segment::{
    self, build, build_and_sign, verify, BuildRefusal, HubKey, Post, Refusal, SegmentRequest,
    SignedSegment, Step, VerifyContext,
};
use crate::supervisor::{Finding, Report, StepReport, Supervisor};
use crate::time;

fn classes() -> SafeClasses {
    SafeClasses::bundled()
}

fn read(id: &str) -> Step {
    Step::new(id, "read", SETTINGS, Risk::R0, Post::none()).with_select(json!({"text": "Battery"}))
}

fn tap(id: &str) -> Step {
    Step::new(
        id,
        "tap",
        SETTINGS,
        Risk::R1,
        Post::present(json!({"text": "Done"})),
    )
    .with_select(json!({"id": "x:id/ok"}))
}

fn req(steps: Vec<Step>) -> SegmentRequest {
    SegmentRequest {
        segment_id: "seg-t".into(),
        run_id: "run-t".into(),
        device: DEVICE.into(),
        epoch: EPOCH,
        owned: false,
        expires: now() + 600,
        steps,
    }
}

fn ctx(spki: &[u8]) -> VerifyContext<'_> {
    VerifyContext {
        hub_spki: spki,
        device: DEVICE,
        current_epoch: EPOCH,
        now: now(),
    }
}

fn signed(steps: Vec<Step>, key: &HubKey) -> SignedSegment {
    build_and_sign(req(steps), &classes(), Phase::Two, now(), key).expect("builds")
}

// ------------------------------------------------------------------------------------------- sign / verify

#[test]
fn sign_with_an_outside_key_verifies_and_a_wrong_signer_is_refused() {
    use p256::ecdsa::signature::Signer;
    use p256::pkcs8::EncodePublicKey;
    // the link hub's key lives elsewhere (Credential Manager); only a signer closure reaches it
    let sk = p256::ecdsa::SigningKey::from_slice(&[9u8; 32]).unwrap();
    let spki = sk
        .verifying_key()
        .to_public_key_der()
        .unwrap()
        .as_bytes()
        .to_vec();
    let seg = build(
        req(vec![read("s1"), tap("s2")]),
        &classes(),
        Phase::Two,
        now(),
    )
    .unwrap();
    let der = |m: &[u8]| -> Vec<u8> {
        let s: p256::ecdsa::Signature = sk.sign(m);
        s.to_der().to_bytes().to_vec()
    };
    let s = segment::sign_with(seg.clone(), &spki, der).unwrap();
    assert_eq!(s.signature.key_id, segment::key_id(&spki));
    verify(&s, &ctx(&spki), &classes(), Phase::Two).unwrap();
    // a signer holding another key cannot mint a segment under this key id
    let other = p256::ecdsa::SigningKey::from_slice(&[10u8; 32]).unwrap();
    let wrong = |m: &[u8]| -> Vec<u8> {
        let s: p256::ecdsa::Signature = other.sign(m);
        s.to_der().to_bytes().to_vec()
    };
    assert!(segment::sign_with(seg.clone(), &spki, wrong).is_err());
    assert!(segment::sign_with(seg, &spki, |_| vec![1, 2, 3]).is_err());
}

#[test]
fn sign_and_verify_round_trip_with_a_fresh_getrandom_key() {
    let key = HubKey::generate().unwrap();
    let s = signed(vec![read("s1"), tap("s2")], &key);
    assert_eq!(s.segment.classification, Risk::R1);
    let spki = key.spki_der();
    verify(&s, &ctx(&spki), &classes(), Phase::Two).unwrap();
    // and after a JSON round trip, as the phone receives it
    let back = SignedSegment::from_json(&s.to_json()).unwrap();
    verify(&back, &ctx(&spki), &classes(), Phase::Two).unwrap();
    assert!(
        !format!("{key:?}").contains(&hex(&spki)),
        "Debug prints the key id only"
    );
}

#[test]
fn a_tampered_segment_fails_verification() {
    let key = test_hub_key();
    let spki = key.spki_der();
    let s = signed(vec![read("s1"), tap("s2")], &key);
    type Edit = Box<dyn Fn(&mut SignedSegment)>;
    let edits: Vec<(&str, Edit)> = vec![
        ("epoch", Box::new(|s| s.segment.epoch += 1)),
        ("device", Box::new(|s| s.segment.device = "other".into())),
        (
            "step select",
            Box::new(|s| s.segment.steps[1].select = Some(json!({"id": "x:id/delete"}))),
        ),
        (
            "step label",
            Box::new(|s| s.segment.steps[0].risk = Risk::R1),
        ),
        (
            "expires",
            Box::new(|s| s.segment.expires = time::format_utc(now() + 900)),
        ),
        (
            "step dropped",
            Box::new(|s| {
                s.segment.steps.pop();
            }),
        ),
        (
            "signature byte",
            Box::new(|s| {
                let mut d = base64::engine::general_purpose::STANDARD
                    .decode(&s.signature.value)
                    .unwrap();
                let n = d.len() - 1;
                d[n] ^= 1;
                s.signature.value = base64::engine::general_purpose::STANDARD.encode(d);
            }),
        ),
    ];
    for (what, f) in edits {
        let mut t = s.clone();
        f(&mut t);
        let r = verify(
            &t,
            &VerifyContext {
                hub_spki: &spki,
                device: &t.segment.device.clone(),
                current_epoch: t.segment.epoch,
                now: now(),
            },
            &classes(),
            Phase::Two,
        );
        assert_eq!(
            r,
            Err(Refusal::BadSignature),
            "tampered {what} must fail the signature"
        );
    }
    // a different hub key
    let other = HubKey::generate().unwrap();
    assert_eq!(
        verify(&s, &ctx(&other.spki_der()), &classes(), Phase::Two),
        Err(Refusal::WrongKey)
    );
    // unsigned
    let mut u = s.clone();
    u.signature.value.clear();
    assert_eq!(
        verify(&u, &ctx(&spki), &classes(), Phase::Two),
        Err(Refusal::Unsigned)
    );
}

// ------------------------------------------------------------------------------------------- classification

#[test]
fn an_r2_step_makes_the_build_refuse() {
    let mut typed = Step::new(
        "s2",
        "type",
        SETTINGS,
        Risk::R1,
        Post::text_equals(json!({"id": "q"}), "x"),
    )
    .with_select(json!({"id": "q"}));
    typed.value = Some(json!("x"));
    let mut coord = Step::new(
        "s3",
        "tap",
        SETTINGS,
        Risk::R1,
        Post::present(json!({"text": "Done"})),
    );
    coord.select = Some(json!({"near_text": "OK"}));
    let mut perm = Step::new(
        "s4",
        "settings_set",
        SETTINGS,
        Risk::R2,
        Post::setting_equals(),
    );
    perm.namespace = Some("global".into());
    perm.key = Some("adb_enabled".into());
    perm.value = Some(json!("0"));
    perm.scope = Some("permanent".into());
    for bad in [
        typed,
        coord,
        perm,
        Step::new(
            "s5",
            "tap",
            "com.example.bank",
            Risk::R1,
            Post::present(json!({"text": "Paid"})),
        )
        .with_select(json!({"text": "Pay"})),
    ] {
        let id = bad.id.clone();
        match build(req(vec![read("s1"), bad]), &classes(), Phase::Two, now()) {
            Err(BuildRefusal::NotSafe(ds)) => {
                assert_eq!(ds.len(), 2, "every step gets a decision");
                assert_eq!(ds[0].final_decision, Decision::Allowed);
                assert_eq!(ds[1].step_id, id);
                assert_ne!(ds[1].final_decision, Decision::Allowed, "{id}");
                assert!(ds[1].effective >= Risk::R2, "{id}");
            }
            other => panic!("{id}: expected NotSafe, got {other:?}"),
        }
    }
    // A hub label of R3 is blocked outright even for a listed kind.
    let mut r3 = read("s9");
    r3.risk = Risk::R3;
    let Err(BuildRefusal::NotSafe(ds)) = build(req(vec![r3]), &classes(), Phase::Two, now()) else {
        panic!()
    };
    assert_eq!(ds[0].decision, Decision::Blocked);
}

#[test]
fn require_confirmation_is_blocked_in_phase_2() {
    // R2 (not on the list) is require_confirmation before the phase rule, blocked after it.
    let mut typed = Step::new(
        "s1",
        "type",
        SETTINGS,
        Risk::R2,
        Post::text_equals(json!({"id": "q"}), "x"),
    )
    .with_select(json!({"id": "q"}));
    typed.value = Some(json!("x"));
    let d = classes().decide(&typed.view(), false, Phase::Two);
    assert_eq!(d.decision, Decision::RequireConfirmation);
    assert_eq!(d.final_decision, Decision::Blocked);
    assert_eq!(
        Phase::Two.apply(Decision::RequireConfirmation),
        Decision::Blocked
    );
    assert_eq!(Phase::Two.apply(Decision::Allowed), Decision::Allowed);
    assert_eq!(Phase::Two.apply(Decision::Blocked), Decision::Blocked);
    // and so the build refuses
    assert!(matches!(
        build(req(vec![typed]), &classes(), Phase::Two, now()),
        Err(BuildRefusal::NotSafe(_))
    ));
    // the decision serialises with Gemini's spelling
    assert_eq!(
        serde_json::to_value(Decision::RequireConfirmation).unwrap(),
        json!("require_confirmation")
    );
}

#[test]
fn safe_kinds_follow_the_documented_rules() {
    let c = classes();
    let class_of =
        |s: &Step, owned: bool| c.listed_class(&derive_safe_kind(&s.view(), owned), &s.package);
    // reads etc. are R0 in any package
    for k in ["read", "assert", "wait", "scroll", "back"] {
        let s = Step::new("a", k, "com.whatever", Risk::R0, Post::none());
        assert_eq!(class_of(&s, false), Risk::R0, "{k}");
    }
    // unique taps: R1 in listed packages only
    assert_eq!(class_of(&tap("a"), false), Risk::R1);
    let mut t = tap("a");
    t.package = "com.example.other".into();
    assert_eq!(class_of(&t, false), Risk::R2);
    // the Play Store Update button: R1 on an owned phone only, and only that button
    let upd = Step::new(
        "a",
        "tap",
        "com.android.vending",
        Risk::R1,
        Post::present(json!({"text": "Open"})),
    )
    .with_select(json!({"text": "Update"}));
    assert_eq!(derive_safe_kind(&upd.view(), true), "play_store_update");
    assert_eq!(class_of(&upd, true), Risk::R1);
    assert_eq!(class_of(&upd, false), Risk::R2);
    let buy = upd.clone().with_select(json!({"text": "Buy"}));
    assert_eq!(
        class_of(&buy, true),
        Risk::R2,
        "any other Play Store tap is not R1"
    );
    // settings toggles: leased, reversible scope, 0/1, in Settings
    let mut tog = Step::new(
        "a",
        "settings_set",
        SETTINGS,
        Risk::R1,
        Post::setting_equals(),
    );
    tog.namespace = Some("system".into());
    tog.key = Some("accelerometer_rotation".into());
    tog.value = Some(json!("1"));
    tog.scope = Some("job".into());
    tog.lease = Some("L1".into());
    assert_eq!(class_of(&tog, false), Risk::R1);
    for f in [
        &(|s: &mut Step| s.lease = None) as &dyn Fn(&mut Step),
        &|s: &mut Step| s.scope = Some("permanent".into()),
        &|s: &mut Step| s.value = Some(json!("120")),
        &|s: &mut Step| s.namespace = Some("vendor".into()),
        &|s: &mut Step| s.package = "com.android.chrome".into(),
    ] {
        let mut x = tog.clone();
        f(&mut x);
        assert_eq!(class_of(&x, false), Risk::R2);
    }
}

#[test]
fn the_bundled_list_obeys_schemas_safe_classes_json() {
    let c = SafeClasses::parse(SAFE_CLASSES_JSON).unwrap();
    assert_eq!(c, classes());
    // the schema's own invalid fixtures are refused by the same parser
    let fx = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas/fixtures/safe-classes");
    let mut n = 0;
    for e in std::fs::read_dir(fx).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let r = SafeClasses::parse(&std::fs::read_to_string(&p).unwrap());
        if name.starts_with("valid-") {
            assert!(r.is_ok(), "{name}: {r:?}");
            n += 1;
        } else if name.starts_with("invalid-") {
            assert!(r.is_err(), "{name} should be refused");
            n += 1;
        }
    }
    assert!(n >= 4);
}

// ------------------------------------------------------------------------------------------- epoch / expiry

#[test]
fn a_stale_epoch_or_an_expired_segment_is_refused() {
    let key = test_hub_key();
    let spki = key.spki_der();
    let s = signed(vec![read("s1")], &key);
    let c = classes();
    let mut x = ctx(&spki);
    x.current_epoch = EPOCH + 1;
    assert_eq!(
        verify(&s, &x, &c, Phase::Two),
        Err(Refusal::StaleEpoch {
            segment: EPOCH,
            current: EPOCH + 1
        })
    );
    x.current_epoch = EPOCH - 1;
    assert_eq!(
        verify(&s, &x, &c, Phase::Two),
        Err(Refusal::EpochAhead {
            segment: EPOCH,
            current: EPOCH - 1
        })
    );
    let mut x = ctx(&spki);
    x.now = now() + 600;
    assert!(
        matches!(verify(&s, &x, &c, Phase::Two), Err(Refusal::Expired { .. })),
        "expires exactly now"
    );
    x.now = now() + 601;
    assert!(matches!(
        verify(&s, &x, &c, Phase::Two),
        Err(Refusal::Expired { .. })
    ));
    // the supervisor refuses both before sending
    let mut sup = Supervisor::new(EPOCH + 1, classes(), Phase::Two);
    assert_eq!(
        sup.admit(&s, &spki, DEVICE, now()).unwrap_err().code(),
        "stale_epoch"
    );
    let mut sup = Supervisor::new(EPOCH, classes(), Phase::Two);
    assert_eq!(
        sup.admit(&s, &spki, DEVICE, now() + 3600)
            .unwrap_err()
            .code(),
        "expired"
    );
    assert!(sup.admit(&s, &spki, DEVICE, now()).is_ok());
    // a build that expires in the past or too far ahead is refused
    let mut r = req(vec![read("s1")]);
    r.expires = now() - 1;
    assert!(matches!(
        build(r, &c, Phase::Two, now()),
        Err(BuildRefusal::Invalid(_))
    ));
    let mut r = req(vec![read("s1")]);
    r.expires = now() + segment::MAX_TTL_S + segment::CLOCK_SKEW_S + 1;
    assert!(matches!(
        build(r, &c, Phase::Two, now()),
        Err(BuildRefusal::Invalid(_))
    ));
    // wrong device
    let x = VerifyContext {
        device: "someone-else",
        ..ctx(&spki)
    };
    assert_eq!(
        verify(&s, &x, &c, Phase::Two).unwrap_err().code(),
        "wrong_device"
    );
}

#[test]
fn acting_steps_need_a_post_condition_and_selectors_are_unique_kinds_only() {
    let mut t = tap("s1");
    t.post = Post::none();
    assert!(matches!(
        build(req(vec![t]), &classes(), Phase::Two, now()),
        Err(BuildRefusal::Invalid(_))
    ));
    let mut t = tap("s1");
    t.select = Some(json!({"rel": {"x": 0, "y": 0}}));
    assert!(build(req(vec![t]), &classes(), Phase::Two, now()).is_err());
    assert!(
        build(
            req(vec![read("s1"), read("s1")]),
            &classes(),
            Phase::Two,
            now()
        )
        .is_err(),
        "duplicate ids"
    );
}

// ------------------------------------------------------------------------------------------- supervisor

fn report_for(s: &SignedSegment, steps: Vec<StepReport>, outcome: &str) -> Report {
    Report {
        v: 1,
        t: "segment_report".into(),
        segment_id: s.segment.segment_id.clone(),
        run_id: s.segment.run_id.clone(),
        device: DEVICE.into(),
        epoch: EPOCH,
        outcome: outcome.into(),
        steps,
        reason: None,
    }
}

fn done(id: &str, ts_ms: i64) -> StepReport {
    StepReport {
        id: id.into(),
        status: "done".into(),
        post_verified: true,
        decision: "allowed".into(),
        ts_ms,
        ms: Some(5),
        reason: None,
        value: None,
    }
}

#[test]
fn the_supervisor_flags_a_duplicate_step_report() {
    let key = test_hub_key();
    let s = signed(vec![read("s1"), tap("s2"), read("s3")], &key);
    let t = now() * 1000;
    let mut sup = Supervisor::new(EPOCH, classes(), Phase::Two);
    sup.admit(&s, &key.spki_der(), DEVICE, now()).unwrap();
    let good = report_for(
        &s,
        vec![done("s1", t), done("s2", t + 10), done("s3", t + 20)],
        "completed",
    );
    assert!(
        sup.check_report(&good).ok(),
        "{:?}",
        sup.check_report(&good)
    );

    let mut sup = Supervisor::new(EPOCH, classes(), Phase::Two);
    sup.admit(&s, &key.spki_der(), DEVICE, now()).unwrap();
    let dup = report_for(
        &s,
        vec![
            done("s1", t),
            done("s2", t + 10),
            done("s2", t + 11),
            done("s3", t + 20),
        ],
        "completed",
    );
    let v = sup.check_report(&dup);
    assert!(
        v.findings.contains(&Finding::DuplicateStep("s2".into())),
        "{v:?}"
    );
    // a second report for the same segment is itself a duplicate
    assert!(sup
        .check_report(&good)
        .findings
        .contains(&Finding::DuplicateReport));
}

#[test]
fn the_supervisor_checks_post_conditions_order_expiry_and_epoch() {
    let key = test_hub_key();
    let s = signed(vec![read("s1"), tap("s2"), read("s3")], &key);
    let t = now() * 1000;
    let fresh = || {
        let mut sup = Supervisor::new(EPOCH, classes(), Phase::Two);
        sup.admit(&s, &key.spki_der(), DEVICE, now()).unwrap();
        sup
    };
    let mut unverified = done("s2", t);
    unverified.post_verified = false;
    let v = fresh().check_report(&report_for(
        &s,
        vec![done("s1", t), unverified, done("s3", t)],
        "completed",
    ));
    assert!(v.findings.contains(&Finding::PostNotVerified("s2".into())));
    let v = fresh().check_report(&report_for(
        &s,
        vec![done("s2", t), done("s1", t)],
        "completed",
    ));
    assert!(v.findings.contains(&Finding::OutOfOrder("s1".into())));
    assert!(v.findings.contains(&Finding::IncompleteButCompleted(1)));
    let v = fresh().check_report(&report_for(
        &s,
        vec![done("s1", t), done("zz", t)],
        "failed",
    ));
    assert!(v.findings.contains(&Finding::UnknownStep("zz".into())));
    let late = (now() + 601) * 1000;
    let v = fresh().check_report(&report_for(
        &s,
        vec![done("s1", t), done("s2", late)],
        "failed",
    ));
    assert!(v.findings.contains(&Finding::RanAfterExpiry("s2".into())));
    // stopped by a human: the interrupted step is fine, nothing claims completion
    let mut intr = done("s2", t);
    intr.status = "interrupted".into();
    intr.post_verified = false;
    assert!(fresh()
        .check_report(&report_for(&s, vec![done("s1", t), intr], "stopped_human"))
        .ok());
    // a report from an older epoch is refused
    let mut sup = Supervisor::new(EPOCH + 1, classes(), Phase::Two);
    let v = sup.check_report(&report_for(&s, vec![done("s1", t)], "completed"));
    assert_eq!(
        v.findings,
        vec![Finding::StaleEpoch {
            report: EPOCH,
            current: EPOCH + 1
        }]
    );
    // and one the hub never sent
    let v = Supervisor::new(EPOCH, classes(), Phase::Two).check_report(&report_for(
        &s,
        vec![],
        "completed",
    ));
    assert!(matches!(v.findings[0], Finding::UnknownSegment(_)));
}

#[test]
fn the_supervisor_reads_the_phone_report_json() {
    // The exact shape android/runner Report.java writes (copied from a JVM run's output).
    let text = r#"{"v":1,"t":"segment_report","segment_id":"seg-t","run_id":"run-t","device":"f1xture5erial","epoch":7,"outcome":"completed","steps":[{"id":"s1","status":"done","post_verified":true,"decision":"allowed","ts_ms":1792497600000,"ms":3}]}"#;
    let r: Report = serde_json::from_str(text).unwrap();
    assert_eq!(r.steps[0].id, "s1");
    assert_eq!(serde_json::to_string(&r).unwrap(), text);
}

/// Cross-language, end to end: the JVM link-down test (android/runner LinkDownTest) dumps the segment its Java test
/// hub signed and the report its runner produced. The Rust supervisor admits that Java-signed segment (so Java
/// signing + canonical form verify in Rust too) and accepts the 50-step report with no findings. Skipped when the
/// JVM suite has not run in this tree.
#[test]
fn the_supervisor_accepts_the_real_jvm_report_when_present() {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../android/runner/out/runner-sample.json");
    let Ok(text) = std::fs::read_to_string(&p) else {
        eprintln!(
            "skipped: {} not found (run android/runner/run-runner-tests.sh first)",
            p.display()
        );
        return;
    };
    let d: Value = serde_json::from_str(&text).unwrap();
    let spki = base64::engine::general_purpose::STANDARD
        .decode(d["hub_spki"].as_str().unwrap())
        .unwrap();
    let signed = SignedSegment::from_json(d["signed"].as_str().unwrap()).unwrap();
    let report: Report = serde_json::from_str(d["report"].as_str().unwrap()).unwrap();
    let mut sup = Supervisor::new(d["epoch"].as_u64().unwrap(), classes(), Phase::Two);
    sup.admit(
        &signed,
        &spki,
        d["device"].as_str().unwrap(),
        d["now_s"].as_i64().unwrap(),
    )
    .expect("Rust verifies the Java-signed segment");
    assert_eq!(report.steps.len(), 50);
    let v = sup.check_report(&report);
    assert!(v.ok(), "{:?}", v.findings);
    assert_eq!(v.outcome, "completed");
    // and the supervisor would catch a phone that reported a step twice
    let mut dup = report.clone();
    dup.steps.insert(5, dup.steps[4].clone());
    let mut sup2 = Supervisor::new(d["epoch"].as_u64().unwrap(), classes(), Phase::Two);
    sup2.admit(
        &signed,
        &spki,
        d["device"].as_str().unwrap(),
        d["now_s"].as_i64().unwrap(),
    )
    .unwrap();
    assert!(sup2
        .check_report(&dup)
        .findings
        .contains(&Finding::DuplicateStep(dup.steps[4].id.clone())));
}

// ------------------------------------------------------------------------------------------- fixtures

#[test]
fn the_canonical_bytes_fixtures_match() {
    let dir = fixtures::dir();
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("canonical.json")).unwrap())
            .unwrap();
    let spki = base64::engine::general_purpose::STANDARD
        .decode(doc["hub_spki"].as_str().unwrap())
        .unwrap();
    assert_eq!(segment::key_id(&spki), doc["hub_key_id"].as_str().unwrap());
    assert_eq!(doc["domain"].as_str().unwrap(), canonical::DOMAIN);
    let cases = doc["cases"].as_array().unwrap();
    assert!(cases.len() >= 8);
    let ctx = VerifyContext {
        hub_spki: &spki,
        device: doc["device"].as_str().unwrap(),
        current_epoch: doc["epoch"].as_u64().unwrap(),
        now: time::parse_utc(doc["now"].as_str().unwrap()).unwrap(),
    };
    for c in cases {
        let name = c["fixture"].as_str().unwrap();
        let text = std::fs::read_to_string(dir.join(name)).unwrap();
        // canonical form of the parsed segment object, from the raw JSON (not via the typed struct)
        let raw: Value = serde_json::from_str(&text).unwrap();
        let canon = canonical::to_canonical(&raw["segment"]).unwrap();
        assert_eq!(
            canon,
            c["canonical"].as_str().unwrap(),
            "{name}: canonical text"
        );
        let sha: String = Sha256::digest(canonical::signed_bytes(&raw["segment"]).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            sha,
            c["signed_bytes_sha256"].as_str().unwrap(),
            "{name}: signed bytes"
        );
        // and the typed path gives the same bytes
        let s = SignedSegment::from_json(&text).unwrap();
        assert_eq!(
            segment::signed_bytes(&s.segment).unwrap(),
            canonical::signed_bytes(&raw["segment"]).unwrap(),
            "{name}"
        );
        let got = match verify(&s, &ctx, &classes(), Phase::Two) {
            Ok(()) => "ok",
            Err(r) => r.code(),
        };
        assert_eq!(got, c["expect"].as_str().unwrap(), "{name}");
    }
    // The edge-case fixture really contains every escape class.
    let edge = cases
        .iter()
        .find(|c| c["fixture"] == "valid-r1-canonical-edge-cases.json")
        .unwrap()["canonical"]
        .as_str()
        .unwrap()
        .to_string();
    for needle in [
        "\\\"",
        "\\\\",
        "\\b",
        "\\t",
        "\\n",
        "\\f",
        "\\r",
        "\\u0001",
        "\\u001f",
        "\u{7f}",
        "\u{2615}",
        "\u{1F600}",
        "/",
    ] {
        assert!(edge.contains(needle), "edge case lacks {needle:?}");
    }
}

#[test]
fn every_invalid_fixture_is_refused_by_the_typed_parser_or_verifier() {
    let dir = fixtures::dir();
    let key = test_hub_key();
    let spki = key.spki_der();
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with("invalid-") {
            continue;
        }
        n += 1;
        let text = std::fs::read_to_string(&p).unwrap();
        let r = SignedSegment::from_json(&text)
            .and_then(|s| verify(&s, &ctx(&spki), &classes(), Phase::Two));
        assert!(r.is_err(), "{name} must be refused");
    }
    assert!(n >= 10);
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ------------------------------------------------------------------------------------------- supervisor pruning

/// A segment admitted at `at` (epoch s) that expires 600 s later.
fn admitted(sup: &mut Supervisor, key: &HubKey, id: &str, at: i64) -> SignedSegment {
    let mut r = req(vec![read("s1")]);
    r.segment_id = id.into();
    r.expires = at + 600;
    let s = build_and_sign(r, &classes(), Phase::Two, at, key).expect("builds");
    sup.admit(&s, &key.spki_der(), DEVICE, at)
        .expect("admitted");
    s
}

#[test]
fn the_supervisor_forgets_finished_segments_by_age_and_by_count_and_never_a_running_one() {
    let key = test_hub_key();
    let t0 = now();
    let day = 86_400;
    let mut sup = Supervisor::new(EPOCH, classes(), Phase::Two);
    let old_expired = admitted(&mut sup, &key, "old-expired", t0);
    let old_reported = admitted(&mut sup, &key, "old-reported", t0 + 10);
    let _ = old_expired;
    assert!(sup
        .check_report(&report_for(
            &old_reported,
            vec![done("s1", (t0 + 11) * 1000)],
            "completed"
        ))
        .ok());
    let fresh = admitted(&mut sup, &key, "fresh", t0 + 8 * day);
    let later = t0 + 8 * day + 30; // `fresh` is running, the two old ones are 8 days old and finished
    assert!(
        sup.is_running("fresh", later)
            && !sup.is_running("old-expired", later)
            && !sup.is_running("old-reported", t0)
    );
    assert_eq!(sup.prune(later, 7 * day, 100), 2, "both old ones, by age");
    assert_eq!(sup.remembered(), 1);
    assert!(
        sup.is_running("fresh", later),
        "the running one is untouched"
    );
    // a late report on a forgotten segment is now unknown (it was a duplicate while remembered)
    assert!(sup
        .check_report(&report_for(&old_reported, vec![], "completed"))
        .findings
        .contains(&Finding::UnknownSegment("old-reported".into())));

    // by count: five finished segments, one running, a cap of 2: the three oldest finished go
    let mut sup = Supervisor::new(EPOCH, classes(), Phase::Two);
    for k in 0..5 {
        admitted(&mut sup, &key, &format!("done-{k}"), t0 + k * 10);
    }
    admitted(&mut sup, &key, "running", t0 + 3_000);
    let at = t0 + 3_100; // the five are long expired (never reported), "running" has 500 s left
    assert_eq!(
        sup.prune(at, 7 * day, 2),
        4,
        "5 finished + 1 running remembered; cap 2 leaves the running one and one finished"
    );
    assert_eq!(sup.remembered(), 2);
    assert!(sup.is_running("running", at));
    assert!(!sup.is_running("done-4", at) && sup.remembered() == 2);
    // a cap below the number of running segments never drops a running one
    let mut sup = Supervisor::new(EPOCH, classes(), Phase::Two);
    for k in 0..3 {
        admitted(&mut sup, &key, &format!("live-{k}"), t0 + k);
    }
    assert_eq!(
        sup.prune(t0 + 10, 0, 0),
        0,
        "age 0 and count 0, but all three are running"
    );
    assert_eq!(sup.remembered(), 3);
    let _ = fresh;
}

/// Review defect 5: a segment is admitted BEFORE it is sent, so one the phone refuses must stop counting as running.
#[test]
fn a_refused_segment_is_not_running_is_pruned_as_finished_and_runs_again_when_it_is_sent_again() {
    let key = test_hub_key();
    let t0 = now();
    let mut sup = Supervisor::new(EPOCH, classes(), Phase::Two);
    let s = admitted(&mut sup, &key, "seg-no", t0);
    admitted(&mut sup, &key, "seg-ok", t0 + 1);
    assert!(
        sup.is_running("seg-no", t0 + 2) && sup.is_running("seg-ok", t0 + 2),
        "both are running until the phone answers"
    );
    sup.refuse("seg-no");
    assert!(
        !sup.is_running("seg-no", t0 + 2),
        "refused: nothing ran, so it is not running (its cached answer is no longer pinned)"
    );
    assert!(
        sup.is_running("seg-ok", t0 + 2),
        "the other one is untouched"
    );
    sup.refuse("never-sent"); // an id the supervisor does not know is ignored
    assert_eq!(sup.remembered(), 2);
    // the refused one is finished at once for pruning, though it has 10 minutes left; the running one stays
    assert_eq!(
        sup.prune(t0 + 3, 0, 100),
        0,
        "by age only: a refused segment that has not yet expired is not old"
    );
    assert_eq!(
        sup.prune(t0 + 3, i64::MAX / 4, 1),
        1,
        "by count: the refused segment goes, the running one stays"
    );
    assert_eq!(sup.remembered(), 1);
    assert!(sup.is_running("seg-ok", t0 + 3));
    // sending the same id again (after a busy refusal, say) makes it running again
    let _ = s;
    admitted(&mut sup, &key, "seg-no", t0 + 4);
    assert!(sup.is_running("seg-no", t0 + 5));
    sup.refuse("seg-no");
    let again = admitted(&mut sup, &key, "seg-no", t0 + 6);
    assert!(
        sup.is_running("seg-no", t0 + 7),
        "re-admitting clears the refusal"
    );
    // a refused segment's report (the phone queues one for most refusals) is still checked
    let v = sup.check_report(&report_for(&again, vec![], "refused"));
    assert!(v.ok(), "{:?}", v.findings);
}

/// Review round 2, defect B: a segment id runs once. A resend of one the supervisor already sent (running, finished, or
/// of unknown outcome after a runner timeout) is refused at the hub; only a refused one may go again.
#[test]
fn the_supervisor_refuses_a_segment_id_it_already_sent_unless_that_one_was_refused() {
    let key = test_hub_key();
    let t0 = now();
    let mut sup = Supervisor::new(EPOCH, classes(), Phase::Two);
    let s = admitted(&mut sup, &key, "seg-1", t0);
    let again = |sup: &mut Supervisor, at: i64| {
        let mut r = req(vec![read("s1")]);
        r.segment_id = "seg-1".into();
        r.expires = at + 600;
        let s = build_and_sign(r, &classes(), Phase::Two, at, &key).expect("builds");
        sup.admit(&s, &key.spki_der(), DEVICE, at)
    };
    // running (or of unknown outcome: nothing marks it, a timeout leaves it exactly like this)
    assert!(matches!(again(&mut sup, t0 + 1), Err(Refusal::Duplicate(ref id)) if id == "seg-1"));
    assert_eq!(
        again(&mut sup, t0 + 1).unwrap_err().code(),
        "duplicate_segment"
    );
    assert!(
        sup.is_running("seg-1", t0 + 2),
        "a refused duplicate changes nothing about the first"
    );
    // finished
    assert!(sup
        .check_report(&report_for(
            &s,
            vec![done("s1", (t0 + 2) * 1000)],
            "completed"
        ))
        .ok());
    assert!(
        matches!(again(&mut sup, t0 + 3), Err(Refusal::Duplicate(_))),
        "a finished segment id is not run again either"
    );
    // only a refusal (nothing ran) frees the id
    admitted(&mut sup, &key, "seg-2", t0);
    sup.refuse("seg-2");
    admitted(&mut sup, &key, "seg-2", t0 + 1);
    assert!(sup.is_running("seg-2", t0 + 2));
}
