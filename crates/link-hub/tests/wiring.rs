//! What the daemon (bridged) needs from the hub, added when the library crates were wired into the daemon: the `segment`
//! frame end to end, one epoch source (an injected [`EpochSource`] and [`Hub::adopt_epoch`]), the
//! revoked flag in the status, and the response-cache prune at hub start.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use link_hub::clock::{Clock, FakeClock};
use link_hub::hub::{EpochSource, Hub, HubConfig, HubError};
use link_hub::store::{CachedResponse, StateDir, RESPONSE_MAX_AGE_MS};
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(30);

fn signed_segment() -> Value {
    let p = scaffold()
        .join("schemas")
        .join("fixtures")
        .join("segment")
        .join("valid-r1-tap.json");
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

async fn connected(hub: &Hub, handshakes: u64) {
    wait_until("the phone is connected", 20, || {
        hub.status(SERIAL)
            .is_some_and(|s| s.connected && s.counters.handshakes >= handshakes)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_segment_travels_in_a_segment_frame_and_is_answered_once() {
    let dir = TempDir::new("w-seg");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let p = FakePhone::paired(31, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &p.cfg.key);
    let runner = PhoneRunner::start(p.clone(), hub.url());
    connected(&hub, 1).await;
    let epoch = hub.epoch(SERIAL).unwrap();
    let seg = signed_segment();

    let r = tokio::time::timeout(
        T,
        hub.send_segment_fenced(SERIAL, epoch, "seg-1", seg.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r.rpc["result"]["method"], "runner.segment");
    assert_eq!(
        r.rpc["result"]["params"], seg,
        "the phone got the signed segment unchanged"
    );
    assert_eq!(r.rpc["result"]["run"], 1);
    // the same id again: answered from the hub cache, never re-sent
    let again = hub
        .send_segment_fenced(SERIAL, epoch, "seg-1", seg.clone())
        .await
        .unwrap();
    assert!(again.from_hub_cache);
    // the same id with another segment is refused
    let mut other = seg.clone();
    other["segment"]["segment_id"] = json!("seg-other");
    assert_eq!(
        hub.send_segment_fenced(SERIAL, epoch, "seg-1", other).await,
        Err(HubError::IdReused("seg-1".into()))
    );
    // a stale epoch sends nothing
    assert!(matches!(
        hub.send_segment_fenced(SERIAL, epoch + 1, "seg-2", seg.clone())
            .await,
        Err(HubError::StaleEpoch { .. })
    ));
    // not the segment.json envelope: refused before the hub
    assert!(matches!(
        hub.send_segment_fenced(SERIAL, epoch, "seg-3", json!({"steps": []}))
            .await,
        Err(HubError::BadPayload(_))
    ));
    {
        let st = p.st();
        assert_eq!(st.segments_seen, vec![seg.clone()]);
        let frames: Vec<&Value> = st
            .hub_frames
            .iter()
            .filter(|f| f["t"] == "segment")
            .collect();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["epoch"], epoch);
        assert_eq!(link_hub::proto::validate(frames[0]), Ok(()));
    }
    runner.stop().await;
    hub.shutdown().await;
}

/// The registry stand-in: a global counter that is always above the floor.
struct Counter(AtomicU64);

impl EpochSource for Counter {
    fn epoch_for(&self, _serial: &str, floor: u64) -> Result<u64, String> {
        let next = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(next.max(floor + 1))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn epochs_come_from_the_injected_source_and_adopt_epoch_moves_the_phone() {
    let dir = TempDir::new("w-epoch");
    StateDir::open(dir.path())
        .unwrap()
        .raise_epoch(SERIAL, 3)
        .unwrap();
    let mut cfg = HubConfig::new(dir.path(), "127.0.0.1", 0, hub_key());
    cfg.epochs = Some(Arc::new(Counter(AtomicU64::new(40))));
    let hub = Hub::start(cfg).await.unwrap();
    let p = FakePhone::paired(32, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &p.cfg.key);
    assert_eq!(
        hub.epoch(SERIAL).unwrap(),
        41,
        "from the source, not epochs.json"
    );
    assert_eq!(
        StateDir::open(dir.path())
            .unwrap()
            .current_epoch(SERIAL)
            .unwrap(),
        41,
        "persisted before use"
    );
    let runner = PhoneRunner::start(p.clone(), hub.url());
    connected(&hub, 1).await;
    wait_until("the phone took epoch 41", 10, || p.st().epoch == 41).await;
    let r = hub
        .send_request_fenced(SERIAL, 41, json!({"method": "a.b", "id": "e-1"}))
        .await
        .unwrap();
    assert_eq!(r.rpc["result"]["run"], 1);

    // a new lease epoch arrives: lower ones change nothing, a higher one re-handshakes the phone
    assert_eq!(hub.adopt_epoch(SERIAL, 41), Ok(false));
    assert_eq!(hub.adopt_epoch(SERIAL, 57), Ok(true));
    connected(&hub, 2).await;
    wait_until("the phone moved to 57", 10, || p.st().epoch == 57).await;
    assert_eq!(hub.epoch(SERIAL).unwrap(), 57);
    assert!(matches!(
        hub.send_request_fenced(SERIAL, 41, json!({"method": "a.b", "id": "e-2"}))
            .await,
        Err(HubError::StaleEpoch {
            requested: 41,
            current: 57
        })
    ));
    let r = tokio::time::timeout(
        T,
        hub.send_request_fenced(SERIAL, 57, json!({"method": "a.b", "id": "e-3"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r.rpc["result"]["run"], 1);
    assert_eq!(
        StateDir::open(dir.path())
            .unwrap()
            .current_epoch(SERIAL)
            .unwrap(),
        57
    );
    runner.stop().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_epoch_fails_queued_requests_as_stale_and_never_sends_them() {
    let dir = TempDir::new("w-adopt-queued");
    let hub = Arc::new(start_hub(dir.path(), hub_key(), None).await);
    let p = FakePhone::paired(33, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &p.cfg.key);
    let e = hub.epoch(SERIAL).unwrap();
    // the phone is away: the request queues
    let h = hub.clone();
    let waiting = tokio::spawn(async move {
        h.send_request_fenced(SERIAL, e, json!({"method": "a.b", "id": "q-1"}))
            .await
    });
    wait_until("queued", 10, || {
        hub.status(SERIAL).is_some_and(|s| s.pending_requests == 1)
    })
    .await;
    assert_eq!(hub.adopt_epoch(SERIAL, e + 5), Ok(true));
    let got = tokio::time::timeout(T, waiting).await.unwrap().unwrap();
    assert_eq!(
        got,
        Err(HubError::StaleEpoch {
            requested: e,
            current: e + 5
        })
    );
    let runner = PhoneRunner::start(p.clone(), hub.url());
    connected(&hub, 1).await;
    wait_until("the phone took the adopted epoch", 10, || {
        p.st().epoch == e + 5
    })
    .await;
    assert_eq!(p.st().requests_seen, 0, "the queued request never left");
    runner.stop().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_status_carries_the_revoked_flag() {
    let dir = TempDir::new("w-revoked");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let p = FakePhone::paired(34, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &p.cfg.key);
    assert!(!hub.status(SERIAL).unwrap().revoked);
    assert_eq!(hub.revoke(SERIAL), Ok(true));
    assert!(hub.status(SERIAL).unwrap().revoked);
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_start_prunes_cached_answers_older_than_7_days() {
    let dir = TempDir::new("w-prune");
    let clock = Arc::new(FakeClock::new(0));
    let now = clock.wall_ms();
    {
        let s = StateDir::open(dir.path()).unwrap();
        let mut c = s.response_cache(SERIAL).unwrap();
        let day = 24 * 60 * 60 * 1000;
        c.insert(
            "old",
            CachedResponse {
                request_hash: "h".into(),
                rpc: json!({}),
                replayed: false,
                ts_ms: now - 8 * day,
            },
        )
        .unwrap();
        c.insert(
            "young",
            CachedResponse {
                request_hash: "h".into(),
                rpc: json!({}),
                replayed: false,
                ts_ms: now - 6 * day,
            },
        )
        .unwrap();
    }
    const { assert!(RESPONSE_MAX_AGE_MS == 7 * 24 * 60 * 60 * 1000) };
    let hub = start_hub(dir.path(), hub_key(), Some(clock.clone() as Arc<dyn Clock>)).await;
    let c = StateDir::open(dir.path())
        .unwrap()
        .response_cache(SERIAL)
        .unwrap();
    assert!(c.get("old").is_none(), "8 days old: pruned at hub start");
    assert!(c.get("young").is_some(), "6 days old: kept");
    drop(c);
    hub.shutdown().await;
}
