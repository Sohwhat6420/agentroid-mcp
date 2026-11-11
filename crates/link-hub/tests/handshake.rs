//! The link.json handshake against the in-process Rust fake phone over a real WebSocket on
//! 127.0.0.1, and every needs_repair path with the exact fix text read from link.json.

mod common;

use std::time::Duration;

use common::*;
use link_hub::crypto::{hub_challenge_message, key_id, unb64, verify};
use link_hub::hub::{HubError, HubEvent};
use link_hub::proto::{validate, CLOSE_REPAIR};
use link_hub::store::StateDir;
use p256::ecdsa::SigningKey;
use serde_json::json;
use tokio::sync::{broadcast, watch};

fn drain(rx: &mut broadcast::Receiver<HubEvent>) -> Vec<HubEvent> {
    let mut out = vec![];
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

fn repairs(events: &[HubEvent]) -> Vec<(Option<String>, String, String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            HubEvent::NeedsRepair {
                serial,
                from,
                reason,
                fix,
                ..
            } => Some((serial.clone(), from.clone(), reason.clone(), fix.clone())),
            _ => None,
        })
        .collect()
}

async fn one_session(phone: &FakePhone, url: &str) -> End {
    let (_tx, mut rx) = watch::channel(false);
    tokio::time::timeout(Duration::from_secs(20), phone.session(url, &mut rx))
        .await
        .expect("session ends")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_handshake_over_a_real_websocket() {
    let dir = TempDir::new("hs-full");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    assert!(hub.url().starts_with("ws://127.0.0.1:"));
    assert!(hub.url().ends_with("/helper"));
    let phone = FakePhone::paired(11, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let mut events = hub.subscribe();

    let runner = PhoneRunner::start(phone.clone(), hub.url());
    let r = tokio::time::timeout(
        Duration::from_secs(20),
        hub.send_request(
            SERIAL,
            json!({"method": "ui.tap", "params": {"x": 1, "y": 2}, "id": "first-1"}),
        ),
    )
    .await
    .expect("answered")
    .expect("ok");
    assert_eq!(r.id, "first-1");
    assert_eq!(r.rpc["result"]["method"], "ui.tap");
    assert_eq!(r.rpc["result"]["run"], 1);
    assert!(!r.replayed && !r.from_hub_cache);

    // The frames the hub sent, in order: challenge (signed over the documented message), ready,
    // resume; every one valid against link.json.
    let frames = phone.st().hub_frames.clone();
    assert_eq!(frames[0]["t"], "challenge");
    assert_eq!(frames[1]["t"], "ready");
    assert_eq!(frames[2]["t"], "resume");
    for f in &frames {
        assert_eq!(validate(f), Ok(()), "{f}");
    }
    let ch = &frames[0];
    assert_eq!(ch["serial"], SERIAL);
    assert_eq!(ch["epoch"], 1);
    assert_eq!(ch["hub_key_id"], hub.hub_key().key_id());
    let msg = hub_challenge_message(
        ch["nonce"].as_str().unwrap(),
        ch["phone_nonce"].as_str().unwrap(),
        1,
        SERIAL,
        &phone.cfg.key.key_id,
    );
    assert!(verify(
        hub.hub_key().spki(),
        &msg,
        &unb64(ch["hub_sig"].as_str().unwrap()).unwrap()
    ));
    assert_eq!(frames[1]["key_id"], phone.cfg.key.key_id);

    let s = hub.status(SERIAL).unwrap();
    assert!(s.connected);
    assert_eq!(s.epoch, Some(1));
    assert_eq!(s.key_id, key_id(&phone.cfg.key.spki));
    assert_eq!(s.counters.handshakes, 1);
    assert_eq!(s.repair, None);
    let ev = drain(&mut events);
    assert!(
        ev.iter()
            .any(|e| matches!(e, HubEvent::Connected { serial, epoch: 1, .. } if serial == SERIAL)),
        "{ev:?}"
    );

    let ends = runner.stop().await;
    assert_eq!(ends.last(), Some(&End::Stopped));
    let addr = hub.local_addr();
    hub.shutdown().await;
    assert!(
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err(),
        "no listener is left running"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_phone_signature_gives_needs_repair_with_the_exact_text() {
    let dir = TempDir::new("hs-badsig");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let mut cfg_phone = FakePhone::paired(12, hub.hub_key());
    let phone = std::sync::Arc::get_mut(&mut cfg_phone).unwrap();
    phone.cfg.sign_with = Some(SigningKey::from_slice(&[99u8; 32]).unwrap()); // not the paired key
    pair_direct(dir.path(), SERIAL, &cfg_phone.cfg.key);
    let mut events = hub.subscribe();

    let end = one_session(&cfg_phone, &hub.url()).await;
    assert_eq!(end, End::Repair("bad_phone_signature".into()));
    {
        let st = cfg_phone.st();
        let f = st.hub_repair.clone().unwrap();
        assert_eq!(f["from"], "hub");
        assert_eq!(
            f["fix"],
            schema_fix("hub", "bad_phone_signature").as_str(),
            "the wire text is link.json's, byte for byte"
        );
        assert_eq!(f["epoch"], 1);
        assert_eq!(st.close_code, Some(CLOSE_REPAIR));
    }
    let shown = schema_fix("hub", "bad_phone_signature").replace("<serial>", SERIAL);
    let s = hub.status(SERIAL).unwrap();
    assert!(!s.connected);
    assert_eq!(
        s.repair
            .as_ref()
            .map(|r| (r.from.as_str(), r.reason.as_str(), r.fix.as_str())),
        Some(("hub", "bad_phone_signature", shown.as_str()))
    );
    assert_eq!(
        repairs(&drain(&mut events)),
        vec![(
            Some(SERIAL.into()),
            "hub".into(),
            "bad_phone_signature".into(),
            shown
        )]
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_device_key_gives_needs_repair_with_the_exact_text() {
    let dir = TempDir::new("hs-unknown");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let phone = FakePhone::paired(13, hub.hub_key());
    // a different key is paired for the serial; this phone's key is unknown
    pair_direct(dir.path(), SERIAL, &phone_key(14));
    let mut events = hub.subscribe();

    let end = one_session(&phone, &hub.url()).await;
    assert_eq!(end, End::Repair("unknown_device_key".into()));
    {
        let st = phone.st();
        let f = st.hub_repair.clone().unwrap();
        assert_eq!(f["from"], "hub");
        assert_eq!(f["fix"], schema_fix("hub", "unknown_device_key").as_str());
        assert_eq!(f["epoch"], 0);
        assert_eq!(st.close_code, Some(CLOSE_REPAIR));
        assert_eq!(
            st.hub_frames.len(),
            1,
            "no challenge is sent to an unknown key"
        );
    }
    let ev = drain(&mut events);
    let r = repairs(&ev);
    assert_eq!(
        r,
        vec![(
            None,
            "hub".into(),
            "unknown_device_key".into(),
            schema_fix("hub", "unknown_device_key")
        )]
    );
    match ev
        .iter()
        .find(|e| matches!(e, HubEvent::NeedsRepair { .. }))
    {
        Some(HubEvent::NeedsRepair { key_id, .. }) => assert_eq!(*key_id, phone.cfg.key.key_id),
        _ => unreachable!(),
    }
    hub.shutdown().await;
}

/// The phone already accepted a newer hub (epoch 5) and says so in hello: this older hub
/// (epoch 1) does not challenge, records `stale_epoch`, and stops owning the phone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_epoch_from_hello_gives_needs_repair_and_fences_the_old_hub() {
    let dir = TempDir::new("hs-stale-hello");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let phone = FakePhone::paired(15, hub.hub_key());
    phone.st().max_epoch = 5;
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let mut events = hub.subscribe();

    let end = one_session(&phone, &hub.url()).await;
    assert_eq!(end, End::Closed(Some(CLOSE_REPAIR)));
    assert!(
        phone.st().hub_frames.is_empty(),
        "no challenge from a stale hub"
    );
    let shown = schema_fix("phone", "stale_epoch");
    let s = hub.status(SERIAL).unwrap();
    assert!(s.superseded);
    assert_eq!(
        s.repair
            .as_ref()
            .map(|r| (r.reason.as_str(), r.fix.as_str())),
        Some(("stale_epoch", shown.as_str()))
    );
    let ev = drain(&mut events);
    assert_eq!(
        repairs(&ev),
        vec![(
            Some(SERIAL.into()),
            "phone".into(),
            "stale_epoch".into(),
            shown
        )]
    );
    assert!(ev
        .iter()
        .any(|e| matches!(e, HubEvent::Superseded { phone_epoch: 5, .. })));
    assert_eq!(
        hub.send_request(SERIAL, json!({"method": "ui.tap"})).await,
        Err(HubError::Superseded { phone_epoch: 5 })
    );
    assert_eq!(phone.st().executed_total, 0);
    hub.shutdown().await;
}

/// The phone answers the challenge itself with `needs_repair stale_epoch` (its hello under-reported
/// the epoch): the hub records the phone's exact fix text and stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_epoch_from_the_phone_gives_needs_repair_with_the_exact_text() {
    let dir = TempDir::new("hs-stale-phone");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let mut p = FakePhone::paired(16, hub.hub_key());
    std::sync::Arc::get_mut(&mut p).unwrap().cfg.lie_epoch_seen = Some(0);
    p.st().max_epoch = 5;
    pair_direct(dir.path(), SERIAL, &p.cfg.key);
    let mut events = hub.subscribe();

    let end = one_session(&p, &hub.url()).await;
    assert_eq!(end, End::Repair("stale_epoch".into()));
    let shown = schema_fix("phone", "stale_epoch");
    // the hub has processed the phone's frame once the event is out
    let mut got = vec![];
    for _ in 0..200 {
        got.extend(repairs(&drain(&mut events)));
        if !got.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        got,
        vec![(
            Some(SERIAL.into()),
            "phone".into(),
            "stale_epoch".into(),
            shown.clone()
        )]
    );
    let s = hub.status(SERIAL).unwrap();
    assert!(s.superseded && !s.connected);
    assert_eq!(s.repair.unwrap().fix, shown);
    assert!(matches!(
        hub.send_request(SERIAL, json!({"method": "ui.tap"})).await,
        Err(HubError::Superseded { .. })
    ));
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_key_gives_key_revoked_with_the_exact_text() {
    let dir = TempDir::new("hs-revoked");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let phone = FakePhone::paired(17, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    assert!(StateDir::open(dir.path()).unwrap().revoke(SERIAL).unwrap());

    let end = one_session(&phone, &hub.url()).await;
    assert_eq!(end, End::Repair("key_revoked".into()));
    let f = phone.st().hub_repair.clone().unwrap();
    assert_eq!(f["fix"], schema_fix("hub", "key_revoked").as_str());
    assert_eq!(
        hub.status(SERIAL).unwrap().repair.unwrap().fix,
        schema_fix("hub", "key_revoked").replace("<serial>", SERIAL)
    );
    hub.shutdown().await;
}

/// The phone checks the hub's signature too: a hub with another key gets the phone's
/// `bad_hub_signature` and records its exact text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_side_repair_is_recorded_with_the_exact_text() {
    let dir = TempDir::new("hs-badhub");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let other_hub = link_hub::crypto::HubKey::from_scalar(&[8u8; 32]).unwrap();
    let phone = FakePhone::paired(18, &other_hub);
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let end = one_session(&phone, &hub.url()).await;
    assert_eq!(end, End::Repair("bad_hub_signature".into()));
    let shown = schema_fix("phone", "bad_hub_signature").replace("<serial>", SERIAL);
    wait_until("the hub records the phone's repair", 10, || {
        hub.status(SERIAL).unwrap().repair.is_some()
    })
    .await;
    let r = hub.status(SERIAL).unwrap().repair.unwrap();
    assert_eq!(
        (r.from.as_str(), r.reason.as_str(), r.fix.as_str()),
        ("phone", "bad_hub_signature", shown.as_str())
    );
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_helper_path_is_served() {
    let dir = TempDir::new("hs-path");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let url = format!("ws://{}/other", hub.local_addr());
    assert!(tokio_tungstenite::connect_async(url).await.is_err());
    hub.shutdown().await;
}
