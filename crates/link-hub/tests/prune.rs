//! Wiring 2 (25 Oct), item 3: the response cache (every answer to a request or a segment, by id)
//! is pruned while the hub runs, by age AND by count, on a cache write, on the sweeper's tick and
//! on demand, with the hub's injectable clock; an answer the daemon says is still running is never
//! pruned. (`store.rs` unit tests cover the cache itself; these run it through a live hub.)

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use link_hub::clock::{Clock, FakeClock};
use link_hub::hub::{CachePolicy, Hub, HubConfig, KeepCached};
use link_hub::store::file_stem;
use serde_json::json;

const DAY: u64 = 24 * 60 * 60 * 1000;

fn cache_ids(dir: &TempDir) -> Vec<String> {
    let p = dir
        .path()
        .join("hub-state")
        .join("responses")
        .join(format!("{}.jsonl", file_stem(SERIAL)));
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect()
}

/// The hub answers a waiter before it writes (and prunes) the cache, so wait for the file to settle.
async fn settles(dir: &TempDir, want: &[&str]) {
    let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    wait_until(&format!("the cache file holds {want:?}"), 10, || {
        cache_ids(dir) == want
    })
    .await;
}

async fn ask(hub: &Hub, id: &str) {
    hub.send_request(SERIAL, json!({"method": "ui.tap", "id": id}))
        .await
        .unwrap();
}

struct Rig {
    dir: TempDir,
    hub: Hub,
    clock: Arc<FakeClock>,
    runner: PhoneRunner,
}

async fn rig(name: &str, policy: CachePolicy, keep: Option<KeepCached>) -> Rig {
    let dir = TempDir::new(name);
    let clock = Arc::new(FakeClock::new(1_000));
    let mut cfg = HubConfig::new(dir.path(), "127.0.0.1", 0, hub_key());
    cfg.clock = clock.clone() as Arc<dyn Clock>;
    cfg.cache = policy;
    cfg.keep_cached = keep;
    let hub = Hub::start(cfg).await.unwrap();
    let phone = FakePhone::paired(31, hub.hub_key());
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let runner = PhoneRunner::start(phone, hub.url());
    wait_until("the phone is connected", 20, || {
        hub.status(SERIAL).is_some_and(|s| s.connected)
    })
    .await;
    Rig {
        dir,
        hub,
        clock,
        runner,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cache_write_prunes_the_too_old_keeps_the_fresh_and_never_a_running_segment() {
    let running: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec!["seg-running".to_string()]));
    let r2 = running.clone();
    let keep: KeepCached =
        Arc::new(move |serial, id| serial == SERIAL && r2.lock().unwrap().iter().any(|x| x == id));
    let r = rig("prune-age", CachePolicy::default(), Some(keep)).await;
    for id in ["old-1", "old-2", "seg-running"] {
        ask(&r.hub, id).await;
    }
    settles(&r.dir, &["old-1", "old-2", "seg-running"]).await;
    // eight days later a fresh answer is written: the write prunes
    r.clock.advance(8 * DAY);
    ask(&r.hub, "fresh-1").await;
    settles(&r.dir, &["seg-running", "fresh-1"]).await;
    assert_eq!(r.hub.status(SERIAL).unwrap().counters.cache_pruned, 2);
    // the segment finishes; the next due prune takes it with the rest that are old enough
    running.lock().unwrap().clear();
    r.clock.advance(8 * DAY);
    assert_eq!(
        r.hub.prune_caches(),
        2,
        "seg-running and fresh-1 are now both 8 days old"
    );
    settles(&r.dir, &[]).await;
    r.runner.stop().await;
    r.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_interval_throttles_the_write_time_prune_but_not_an_explicit_one() {
    let r = rig(
        "prune-interval",
        CachePolicy {
            max_age_ms: DAY,
            max_count: 4096,
            every_ms: 60 * 60 * 1000,
        },
        None,
    )
    .await;
    // first write: a prune runs (nothing to drop) and starts the interval
    ask(&r.hub, "a").await;
    // The hub answers the waiter before it stamps and writes the answer and starts the interval. If the
    // clock moved first, `a` would be stamped two days on: fresh, with the interval starting there, so
    // the write of `b` below would find nothing to prune and nothing due. Wait for the write.
    settles(&r.dir, &["a"]).await;
    r.clock.advance(2 * DAY); // `a` is now too old, but...
    r.clock.advance(0);
    // ...a prune ran at t=1_000 and the next is due an hour later: this write is 2 days later, so it is due
    ask(&r.hub, "b").await;
    settles(&r.dir, &["b"]).await;
    // 10 minutes on, `b` is not too old and the interval has not run out: nothing to do either way
    r.clock.advance(10 * 60 * 1000);
    ask(&r.hub, "c").await;
    settles(&r.dir, &["b", "c"]).await;
    // 25 hours on: `b` and `c` are older than a day. A write inside the interval since the last prune
    // would not look; here the interval is long past, so it does
    r.clock.advance(25 * 60 * 60 * 1000);
    ask(&r.hub, "d").await;
    settles(&r.dir, &["d"]).await;
    // an explicit prune ignores the interval: `d` ages out 2 days on, with no write at all
    r.clock.advance(2 * DAY);
    assert_eq!(r.hub.prune_caches(), 1);
    settles(&r.dir, &[]).await;
    r.runner.stop().await;
    r.hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_count_cap_holds_while_the_hub_runs() {
    // every_ms 0: every write may prune; five answers, a cap of three
    let r = rig(
        "prune-count",
        CachePolicy {
            max_age_ms: 7 * DAY,
            max_count: 3,
            every_ms: 0,
        },
        None,
    )
    .await;
    for i in 0..5 {
        r.clock.advance(1_000);
        ask(&r.hub, &format!("q{i}")).await;
    }
    settles(&r.dir, &["q2", "q3", "q4"]).await;
    assert_eq!(r.hub.status(SERIAL).unwrap().counters.cache_pruned, 2);
    r.runner.stop().await;
    r.hub.shutdown().await;
}
