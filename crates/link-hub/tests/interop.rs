//! Interop with the Java phone half: the recorded LinkClient/FakeHub transcript verifies in Rust
//! (and fails when tampered with), every link.json fixture is judged as test_schemas.py judges it,
//! and the hub's fix texts are link.json's.

mod common;

use common::*;
use link_hub::crypto::{hub_challenge_message, key_id, phone_response_message, unb64, verify};
use link_hub::proto::{validate, FIX_TEXTS};
use serde_json::Value;

fn transcript() -> Value {
    let p = scaffold()
        .join("schemas")
        .join("fixtures")
        .join("link")
        .join("transcript.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// Both signatures of a transcript, checked as the hub and the phone check them.
fn verify_transcript(t: &Value) -> Result<(), String> {
    let s = |v: &Value| {
        v.as_str()
            .map(str::to_string)
            .ok_or("missing field".to_string())
    };
    let hub_spki = unb64(&s(&t["hub_public_key"])?).ok_or("hub key is not base64")?;
    let phone_spki = unb64(&s(&t["phone_public_key"])?).ok_or("phone key is not base64")?;
    let (hello, ch, resp) = (&t["hello"], &t["challenge"], &t["challenge_response"]);
    for f in [hello, ch, resp] {
        validate(f)?;
    }
    let serial = s(&t["serial"])?;
    let epoch = ch["epoch"].as_u64().ok_or("no epoch")?;
    if s(&hello["key_id"])? != key_id(&phone_spki) || s(&resp["key_id"])? != key_id(&phone_spki) {
        return Err("phone key id does not match its key".into());
    }
    if s(&ch["hub_key_id"])? != key_id(&hub_spki) {
        return Err("hub key id does not match its key".into());
    }
    if ch["phone_nonce"] != hello["phone_nonce"]
        || resp["nonce"] != ch["nonce"]
        || resp["epoch"] != ch["epoch"]
        || s(&ch["serial"])? != serial
    {
        return Err("the frames do not belong together".into());
    }
    let (nonce, phone_nonce) = (s(&ch["nonce"])?, s(&ch["phone_nonce"])?);
    let hub_msg = hub_challenge_message(&nonce, &phone_nonce, epoch, &serial, &key_id(&phone_spki));
    if !verify(
        &hub_spki,
        &hub_msg,
        &unb64(&s(&ch["hub_sig"])?).ok_or("hub_sig is not base64")?,
    ) {
        return Err("hub_sig does not verify".into());
    }
    let phone_msg =
        phone_response_message(&nonce, &phone_nonce, epoch, &serial, &key_id(&hub_spki));
    if !verify(
        &phone_spki,
        &phone_msg,
        &unb64(&s(&resp["signature"])?).ok_or("signature is not base64")?,
    ) {
        return Err("signature does not verify".into());
    }
    Ok(())
}

#[test]
fn the_recorded_java_transcript_verifies_in_rust() {
    let t = transcript();
    assert_eq!(verify_transcript(&t), Ok(()));
    assert_eq!(t["challenge"]["epoch"], 5);
    assert_eq!(t["serial"], "POCOX6TEST01");
}

#[test]
fn a_tampered_transcript_fails() {
    let t = transcript();
    let flip_b64 = |s: &str| -> String {
        let mut b = unb64(s).unwrap();
        let i = b.len() - 3;
        b[i] ^= 0x01;
        link_hub::crypto::b64(&b)
    };
    let mut cases: Vec<(&str, Value)> = vec![];
    let mut x = t.clone();
    x["challenge"]["epoch"] = 6.into();
    x["challenge_response"]["epoch"] = 6.into();
    cases.push(("epoch 5 -> 6 in both frames", x));
    let mut x = t.clone();
    x["serial"] = "POCOX6TEST02".into();
    x["challenge"]["serial"] = "POCOX6TEST02".into();
    cases.push(("another serial", x));
    let mut x = t.clone();
    x["challenge"]["hub_sig"] = flip_b64(t["challenge"]["hub_sig"].as_str().unwrap()).into();
    cases.push(("one bit of hub_sig", x));
    let mut x = t.clone();
    x["challenge_response"]["signature"] =
        flip_b64(t["challenge_response"]["signature"].as_str().unwrap()).into();
    cases.push(("one bit of the phone signature", x));
    let mut x = t.clone();
    let n = flip_b64(t["challenge"]["nonce"].as_str().unwrap());
    x["challenge"]["nonce"] = n.clone().into();
    x["challenge_response"]["nonce"] = n.into();
    cases.push(("another nonce", x));
    let mut x = t.clone();
    let pn = flip_b64(t["hello"]["phone_nonce"].as_str().unwrap());
    x["hello"]["phone_nonce"] = pn.clone().into();
    x["challenge"]["phone_nonce"] = pn.into();
    cases.push(("another phone nonce", x));
    let mut x = t.clone();
    x["challenge"]["hub_sig"] = t["challenge_response"]["signature"].clone();
    x["challenge_response"]["signature"] = t["challenge"]["hub_sig"].clone();
    cases.push(("the two signatures swapped", x));
    for (what, x) in cases {
        assert!(
            verify_transcript(&x).is_err(),
            "tampering not caught: {what}"
        );
    }
}

#[test]
fn every_link_fixture_is_judged_like_test_schemas_py() {
    let dir = scaffold().join("schemas").join("fixtures").join("link");
    let (mut valid, mut invalid) = (0, 0);
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if name == "transcript.json" {
            continue;
        }
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        if name.starts_with("valid-") {
            assert_eq!(validate(&v), Ok(()), "{name}");
            valid += 1;
        } else if name.starts_with("invalid-") {
            assert!(validate(&v).is_err(), "{name} was accepted");
            invalid += 1;
        }
    }
    assert!(
        valid >= 25 && invalid >= 30,
        "valid={valid} invalid={invalid}"
    );
}

#[test]
fn the_fix_texts_are_link_json_s() {
    for (from, reason, fix) in FIX_TEXTS {
        assert_eq!(schema_fix(from, reason), fix, "{from}/{reason}");
    }
    let s = link_json();
    assert_eq!(
        s["$defs"]["needs_repair"]["oneOf"]
            .as_array()
            .unwrap()
            .len(),
        FIX_TEXTS.len()
    );
}

/// "The hub never dials the phone": nothing in the library or the CLI opens an outbound
/// connection (the only client code is the fake phone under tests/).
#[test]
fn the_hub_never_dials_out() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![];
    let mut stack = vec![src];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                files.push(p);
            }
        }
    }
    assert!(files.len() >= 9);
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for needle in [
            "connect_async",
            "client_async",
            "TcpStream::connect",
            "TcpSocket",
            "UdpSocket",
            ".connect(",
            "::connect(",
        ] {
            assert!(!text.contains(needle), "{} contains {needle}", f.display());
        }
    }
}
