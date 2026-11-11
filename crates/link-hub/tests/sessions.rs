//! Sessions: epochs, seq/ack/resume, request-id dedupe, fencing, heartbeats and liveness, all
//! against the Rust fake phone over a real WebSocket on 127.0.0.1.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use link_hub::clock::{Clock, FakeClock};
use link_hub::hub::{HubError, HubEvent, OFFLINE_AFTER_MS};
use link_hub::store::StateDir;
use serde_json::json;
use tokio::sync::broadcast;

const T: Duration = Duration::from_secs(60);

fn drain(rx: &mut broadcast::Receiver<HubEvent>) -> Vec<HubEvent> {
    let mut out = vec![];
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

async fn connected(hub: &link_hub::Hub, handshakes: u64) {
    wait_until("the phone is connected", 20, || {
        hub.status(SERIAL)
            .is_some_and(|s| s.connected && s.counters.handshakes >= handshakes)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_thousand_requests_with_twenty_drops_lose_and_duplicate_nothing() {
    const N: usize = 10_000;
    const DROPS: u64 = 20;
    let dir = TempDir::new("s-10k");
    let hub = Arc::new(start_hub(dir.path(), hub_key(), None).await);
    let mut p = FakePhone::paired(21, hub.hub_key());
    Arc::get_mut(&mut p).unwrap().cfg.record = false;
    // drop the connection after executions 490, 980, ... 9800; odd drops lose the response
    // (it stays in the phone's outbox), even drops lose whatever was in flight after it
    p.st().drop_at = (1..=DROPS).map(|k| k * 490).collect();
    pair_direct(dir.path(), SERIAL, &p.cfg.key);
    let runner = PhoneRunner::start(p.clone(), hub.url());

    let mut set = tokio::task::JoinSet::new();
    for i in 0..N {
        let hub = hub.clone();
        set.spawn(async move {
            let id = format!("bulk-{i:05}");
            let r = hub
                .send_request(
                    SERIAL,
                    json!({"method": "bulk.op", "params": {"i": i}, "id": id}),
                )
                .await;
            (id, i, r)
        });
    }
    let mut answered = std::collections::HashSet::new();
    let all = tokio::time::timeout(Duration::from_secs(300), async {
        while let Some(j) = set.join_next().await {
            let (id, i, r) = j.unwrap();
            let r = r.unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(r.id, id);
            assert_eq!(r.rpc["id"], id.as_str());
            assert_eq!(
                r.rpc["result"]["params"]["i"], i,
                "{id} got another request's answer"
            );
            assert_eq!(r.rpc["result"]["run"], 1, "{id} ran twice");
            assert!(answered.insert(id), "answered twice");
        }
    })
    .await;
    assert!(all.is_ok(), "timed out with {} answered", answered.len());

    let lost = N - answered.len();
    {
        let st = p.st();
        let duplicated = st.executed.values().filter(|n| **n > 1).count();
        let executed_once = st.executed.values().filter(|n| **n == 1).count();
        eprintln!(
            "10k: lost={lost} duplicated={duplicated} drops={} handshakes={} phone_replays={} phone_resent={}",
            st.drops, st.handshakes, st.replayed, st.resent
        );
        assert_eq!(lost, 0);
        assert_eq!(duplicated, 0);
        assert_eq!(executed_once, N);
        assert_eq!(st.executed_total, N as u64);
        assert_eq!(st.drops, DROPS as u32, "all 20 drops happened");
        assert_eq!(st.handshakes, DROPS as u32 + 1);
    }
    let s = hub.status(SERIAL).unwrap();
    eprintln!("10k hub counters: {:?}", s.counters);
    assert_eq!(
        s.counters.responses, N as u64,
        "each request delivered to its caller exactly once"
    );
    assert_eq!(s.pending_requests, 0);
    assert!(
        s.counters.frames_resent > 0 || p.st().resent > 0,
        "the drops forced resends"
    );
    runner.stop().await;
    hub.shutdown().await;
}

/// Epoch fencing: after a restart the device's epoch is 2; 1,000 commands fenced with the old
/// epoch 1 are refused by the hub and none reaches or runs on the phone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thousand_stale_epoch_commands_execute_none() {
    let dir = TempDir::new("s-fence");
    let phone;
    {
        let hub = start_hub(dir.path(), hub_key(), None).await;
        phone = FakePhone::paired(22, hub.hub_key());
        pair_direct(dir.path(), SERIAL, &phone.cfg.key);
        let runner = PhoneRunner::start(phone.clone(), hub.url());
        let r = hub
            .send_request_fenced(SERIAL, 1, json!({"method": "ui.tap", "id": "e1-only"}))
            .await
            .unwrap();
        assert_eq!(r.rpc["result"]["run"], 1);
        runner.stop().await;
        hub.shutdown().await;
    }
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    connected(&hub, 1).await;
    assert_eq!(hub.epoch(SERIAL).unwrap(), 2);
    let (seen, ran) = {
        let st = phone.st();
        (st.requests_seen, st.executed_total)
    };

    let mut refused = 0;
    for i in 0..1000 {
        match hub
            .send_request_fenced(
                SERIAL,
                1,
                json!({"method": "ui.tap", "params": {"i": i}, "id": format!("stale-{i}")}),
            )
            .await
        {
            Err(HubError::StaleEpoch {
                requested: 1,
                current: 2,
            }) => refused += 1,
            other => panic!("stale command {i} was not refused: {other:?}"),
        }
    }
    // let anything that slipped out reach the phone before counting
    let r = hub
        .send_request_fenced(SERIAL, 2, json!({"method": "ui.tap", "id": "e2-probe"}))
        .await
        .unwrap();
    assert_eq!(r.rpc["result"]["run"], 1);
    {
        let st = phone.st();
        let executed = st.executed_total - ran - 1;
        let reached = st.requests_seen - seen - 1;
        eprintln!("fencing: refused={refused} executed={executed} reached_phone={reached}");
        assert_eq!(refused, 1000);
        assert_eq!(executed, 0);
        assert_eq!(reached, 0);
        assert!(st.executed.keys().all(|k| !k.starts_with("stale-")));
    }
    assert_eq!(hub.status(SERIAL).unwrap().counters.fenced_by_hub, 1000);
    runner.stop().await;
    hub.shutdown().await;
}

/// A hub superseded by a newer one (the phone said so) refuses everything: 1,000 commands, 0 run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_hub_executes_none_of_a_thousand_commands() {
    let dir = TempDir::new("s-superseded");
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let phone = FakePhone::paired(23, hub.hub_key());
    phone.st().max_epoch = 9;
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    assert_eq!(
        phone.session(&hub.url(), &mut rx).await,
        End::Closed(Some(link_hub::proto::CLOSE_REPAIR))
    );
    for i in 0..1000 {
        let r = hub
            .send_request(
                SERIAL,
                json!({"method": "ui.tap", "id": format!("old-{i}")}),
            )
            .await;
        assert_eq!(r, Err(HubError::Superseded { phone_epoch: 9 }));
    }
    assert_eq!(phone.st().executed_total, 0);
    assert_eq!(phone.st().requests_seen, 0);
    hub.shutdown().await;
}

/// A request id answered before a hub restart is answered from the hub's persisted cache:
/// no request frame reaches the phone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replayed_request_id_after_a_hub_restart_is_answered_from_the_cache() {
    let dir = TempDir::new("s-replay");
    let payload = json!({"method": "ui.type_text", "params": {"text": "pay 5"}, "id": "act-0001"});
    let phone;
    let first;
    {
        let hub = start_hub(dir.path(), hub_key(), None).await;
        phone = FakePhone::paired(24, hub.hub_key());
        pair_direct(dir.path(), SERIAL, &phone.cfg.key);
        let runner = PhoneRunner::start(phone.clone(), hub.url());
        first = hub.send_request(SERIAL, payload.clone()).await.unwrap();
        assert!(!first.replayed && !first.from_hub_cache);
        runner.stop().await;
        hub.shutdown().await;
    }
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    connected(&hub, 1).await;
    wait_until("the stream is open", 10, || {
        phone
            .st()
            .hub_frames
            .iter()
            .filter(|f| f["t"] == "resume")
            .count()
            >= 2
    })
    .await;
    let seen = phone.st().requests_seen;

    let again = hub.send_request(SERIAL, payload.clone()).await.unwrap();
    assert!(again.from_hub_cache, "answered from the hub's cache");
    assert_eq!(again.rpc, first.rpc, "the same answer");
    // a probe after it proves the stream works and anything sent would have arrived first
    hub.send_request(SERIAL, json!({"method": "ui.tap", "id": "probe-1"}))
        .await
        .unwrap();
    {
        let st = phone.st();
        assert_eq!(
            st.requests_seen - seen,
            1,
            "only the probe reached the phone; the replay never did"
        );
        assert_eq!(st.executed.get("act-0001"), Some(&1), "ran exactly once");
    }
    assert_eq!(hub.status(SERIAL).unwrap().counters.hub_cache_hits, 1);
    // the same id with a different call is refused, not run
    let other = json!({"method": "ui.type_text", "params": {"text": "pay 500"}, "id": "act-0001"});
    assert_eq!(
        hub.send_request(SERIAL, other).await,
        Err(HubError::IdReused("act-0001".into()))
    );
    runner.stop().await;
    hub.shutdown().await;
}

/// The hub died after the phone ran a request but before the answer arrived: the retried id is
/// answered from the PHONE's cache (replayed=true) and does not run again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_after_a_lost_answer_is_replayed_by_the_phone_not_rerun() {
    let dir = TempDir::new("s-phonecache");
    let payload = json!({"method": "ui.tap", "params": {"x": 5}, "id": "act-lost-1"});
    let phone;
    {
        let hub = start_hub(dir.path(), hub_key(), None).await;
        phone = FakePhone::paired(25, hub.hub_key());
        phone.st().withhold.insert("act-lost-1".into());
        pair_direct(dir.path(), SERIAL, &phone.cfg.key);
        let runner = PhoneRunner::start(phone.clone(), hub.url());
        let hub = Arc::new(hub);
        let h = hub.clone();
        let waiting = tokio::spawn(async move { h.send_request(SERIAL, payload).await });
        wait_until("the phone ran it", 10, || {
            phone.st().executed.contains_key("act-lost-1")
        })
        .await;
        hub.shutdown().await;
        assert_eq!(waiting.await.unwrap(), Err(HubError::Shutdown));
        runner.stop().await;
    }
    phone.st().withhold.clear();
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    let r = tokio::time::timeout(
        T,
        hub.send_request(
            SERIAL,
            json!({"method": "ui.tap", "params": {"x": 5}, "id": "act-lost-1"}),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(r.replayed, "the phone answered from its request-id cache");
    assert!(!r.from_hub_cache);
    assert_eq!(r.rpc["result"]["run"], 1);
    assert_eq!(phone.st().executed.get("act-lost-1"), Some(&1));
    // and now the hub has it too: a third try never leaves the hub
    let r3 = hub
        .send_request(
            SERIAL,
            json!({"method": "ui.tap", "params": {"x": 5}, "id": "act-lost-1"}),
        )
        .await
        .unwrap();
    assert!(r3.from_hub_cache && r3.replayed);
    runner.stop().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_epoch_persists_across_restarts() {
    let dir = TempDir::new("s-epoch");
    let phone_key = phone_key(26);
    pair_direct(dir.path(), SERIAL, &phone_key);
    let mut epochs = vec![];
    for _ in 0..3 {
        let hub = start_hub(dir.path(), hub_key(), None).await;
        epochs.push(hub.epoch(SERIAL).unwrap());
        assert_eq!(
            hub.epoch(SERIAL).unwrap(),
            *epochs.last().unwrap(),
            "fixed for the life of the process"
        );
        hub.shutdown().await;
        drop(hub);
    }
    assert_eq!(epochs, vec![1, 2, 3]);
    assert_eq!(
        StateDir::open(dir.path())
            .unwrap()
            .current_epoch(SERIAL)
            .unwrap(),
        3
    );

    // and the phone sees it on the wire: the next hub challenges with epoch 4
    let hub = start_hub(dir.path(), hub_key(), None).await;
    let phone = FakePhone::new(PhoneCfg {
        serial: SERIAL.into(),
        key: phone_key,
        hub_spki: hub.hub_key().spki().to_vec(),
        sign_with: None,
        lie_epoch_seen: None,
        record: true,
        tls_fp: None,
    });
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    connected(&hub, 1).await;
    // the hub counts the link as up when the signature verifies; the phone after `ready`
    wait_until("the phone reached ready", 10, || phone.st().handshakes >= 1).await;
    assert_eq!(phone.st().hub_frames[0]["epoch"], 4);
    assert_eq!(phone.st().max_epoch, 4);
    runner.stop().await;
    hub.shutdown().await;
}

/// A new phone process (new instance) lost its outbox and cache: a request it had acked but not
/// answered fails as outcome-unknown, never replayed blindly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_restart_makes_unanswered_requests_outcome_unknown() {
    let dir = TempDir::new("s-instance");
    let hub = Arc::new(start_hub(dir.path(), hub_key(), None).await);
    let phone = FakePhone::paired(27, hub.hub_key());
    phone.st().withhold.insert("act-mid".into());
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let mut events = hub.subscribe();
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    let h = hub.clone();
    let waiting = tokio::spawn(async move {
        h.send_request(SERIAL, json!({"method": "ui.tap", "id": "act-mid"}))
            .await
    });
    wait_until("the phone ran it", 10, || {
        phone.st().executed.contains_key("act-mid")
    })
    .await;
    // a heartbeat acks the request, so it is out of the hub's outbox but still unanswered
    phone.cmd(PhoneCmd::Heartbeat { adb_port: None });
    wait_until("heartbeat seen", 10, || {
        hub.status(SERIAL).unwrap().last_heartbeat_ms.is_some()
    })
    .await;
    runner.stop().await;
    phone.restart_process();
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    let r = tokio::time::timeout(T, waiting).await.unwrap().unwrap();
    assert_eq!(r, Err(HubError::OutcomeUnknown("act-mid".into())));
    connected(&hub, 2).await;
    let ev = drain(&mut events);
    assert!(ev.iter().any(|e| matches!(e, HubEvent::PhoneRestarted { outcome_unknown, .. } if outcome_unknown == &vec!["act-mid".to_string()])), "{ev:?}");
    assert_eq!(
        phone.st().executed.get("act-mid"),
        Some(&1),
        "not re-run behind the caller's back"
    );
    // the stream works on the new instance
    let ok = hub
        .send_request(SERIAL, json!({"method": "ui.tap", "id": "after-restart"}))
        .await
        .unwrap();
    assert_eq!(ok.rpc["result"]["run"], 1);
    runner.stop().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeats_and_beacons_fill_the_device_status() {
    let dir = TempDir::new("s-status");
    let clock = Arc::new(FakeClock::new(1_000_000));
    let hub = start_hub(dir.path(), hub_key(), Some(clock.clone() as Arc<dyn Clock>)).await;
    let phone = FakePhone::paired(28, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let mut events = hub.subscribe();
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    connected(&hub, 1).await;
    let s = hub.status(SERIAL).unwrap();
    assert!(!s.online, "online needs a heartbeat");
    assert_eq!(s.last_seen_ms, Some(1_000_000));

    clock.advance(5_000);
    phone.cmd(PhoneCmd::Heartbeat {
        adb_port: Some(37123),
    });
    wait_until("heartbeat", 10, || {
        hub.status(SERIAL).unwrap().last_heartbeat_ms.is_some()
    })
    .await;
    phone.cmd(PhoneCmd::Untrusted {
        bssid: Some("aa:bb:cc:dd:ee:01".into()),
        reason: "not_in_trusted_list".into(),
    });
    phone.cmd(PhoneCmd::Untrusted {
        bssid: None,
        reason: "bssid_unreadable".into(),
    });
    wait_until("reports", 10, || {
        hub.status(SERIAL).unwrap().untrusted_bssid.len() == 2
    })
    .await;

    let s = hub.status(SERIAL).unwrap();
    assert!(s.online && s.connected);
    assert_eq!(s.last_heartbeat_ms, Some(1_005_000));
    assert_eq!(s.adb_port, Some(37123));
    let hb = s.last_heartbeat.unwrap();
    assert_eq!(hb.period_s, 15);
    assert_eq!(hb.keeper, "ok");
    assert_eq!(
        s.untrusted_bssid[0].bssid.as_deref(),
        Some("aa:bb:cc:dd:ee:01")
    );
    assert_eq!(s.untrusted_bssid[0].reason, "not_in_trusted_list");
    assert_eq!(s.untrusted_bssid[1].bssid, None);
    assert_eq!(s.untrusted_bssid[1].reason, "bssid_unreadable");
    // every heartbeat is acked
    wait_until("ack", 10, || {
        phone.st().hub_frames.iter().any(|f| f["t"] == "ack")
    })
    .await;

    let ev = drain(&mut events);
    assert!(ev.iter().any(|e| matches!(
        e,
        HubEvent::AdbPortChanged {
            port: Some(37123),
            ..
        }
    )));
    assert!(ev.iter().any(|e| matches!(e, HubEvent::Heartbeat { .. })));
    assert_eq!(
        ev.iter()
            .filter(|e| matches!(e, HubEvent::UntrustedBssid { .. }))
            .count(),
        2
    );

    // the same port again is not a change
    phone.cmd(PhoneCmd::Heartbeat {
        adb_port: Some(37123),
    });
    phone.cmd(PhoneCmd::Heartbeat {
        adb_port: Some(40001),
    });
    let mut seen = vec![];
    for _ in 0..1000 {
        seen.extend(drain(&mut events));
        if seen.iter().any(|e| {
            matches!(
                e,
                HubEvent::AdbPortChanged {
                    port: Some(40001),
                    ..
                }
            )
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let changes: Vec<_> = seen
        .iter()
        .filter(|e| matches!(e, HubEvent::AdbPortChanged { .. }))
        .collect();
    assert_eq!(
        changes.len(),
        1,
        "37123 again is not a change; 40001 is: {changes:?}"
    );
    assert_eq!(
        seen.iter()
            .filter(|e| matches!(e, HubEvent::Heartbeat { .. }))
            .count(),
        2
    );
    assert_eq!(hub.status(SERIAL).unwrap().adb_port, Some(40001));
    runner.stop().await;
    hub.shutdown().await;
}

/// The overlay's human_touch reaches the daemon as one HubEvent carrying both phone times, and the
/// device counter goes up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_human_touch_frame_becomes_one_hub_event() {
    let dir = TempDir::new("s-touch");
    let clock = Arc::new(FakeClock::new(1_000_000));
    let hub = start_hub(dir.path(), hub_key(), Some(clock.clone() as Arc<dyn Clock>)).await;
    let phone = FakePhone::paired(28, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let mut events = hub.subscribe();
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    connected(&hub, 1).await;

    phone.cmd(PhoneCmd::HumanTouch {
        touch_ms: 86_400_000,
        sent_ms: 86_400_020,
    });
    wait_until("touch counted", 10, || {
        hub.status(SERIAL).unwrap().counters.human_touches == 1
    })
    .await;
    let ev = drain(&mut events);
    let touches: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            HubEvent::HumanTouch {
                serial,
                touch_ms,
                sent_ms,
            } => Some((serial.clone(), *touch_ms, *sent_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(
        touches,
        vec![(SERIAL.to_string(), 86_400_000, 86_400_020)],
        "{ev:?}"
    );
    runner.stop().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_after_ten_minutes_without_a_heartbeat() {
    let dir = TempDir::new("s-offline");
    let clock = Arc::new(FakeClock::new(50_000_000));
    let hub = start_hub(dir.path(), hub_key(), Some(clock.clone() as Arc<dyn Clock>)).await;
    let phone = FakePhone::paired(29, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let mut events = hub.subscribe();
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    connected(&hub, 1).await;
    phone.cmd(PhoneCmd::Heartbeat { adb_port: None });
    wait_until("heartbeat", 10, || hub.status(SERIAL).unwrap().online).await;
    drain(&mut events);

    clock.advance(OFFLINE_AFTER_MS - 1);
    hub.sweep();
    assert!(
        hub.status(SERIAL).unwrap().online,
        "9:59.999 is still online"
    );
    assert!(!drain(&mut events)
        .iter()
        .any(|e| matches!(e, HubEvent::Offline { .. })));

    clock.advance(1);
    hub.sweep();
    assert!(
        !hub.status(SERIAL).unwrap().online,
        "10:00 without a heartbeat is offline"
    );
    hub.sweep();
    let offline: Vec<_> = drain(&mut events)
        .into_iter()
        .filter(|e| matches!(e, HubEvent::Offline { .. }))
        .collect();
    assert_eq!(
        offline,
        vec![HubEvent::Offline {
            serial: SERIAL.into()
        }],
        "reported once"
    );

    phone.cmd(PhoneCmd::Heartbeat { adb_port: None });
    wait_until("back online", 10, || hub.status(SERIAL).unwrap().online).await;
    runner.stop().await;
    hub.shutdown().await;
}
