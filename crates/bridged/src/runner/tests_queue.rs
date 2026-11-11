//! PQ-1 tests: the scheduled queue, the one-wake-up rule and `resume(run_id)`. Offline: core's fake
//! device (the scripted `Phone` of slice a) and a fake clock. No phone, TV or emulator. Files go to a
//! fresh directory under the OS temp dir per test.

use super::popups::Rulebook;
use super::queue::{JobState, MemoryWakes, Queue, QueueError, Tick, When};
use super::tests::{app_nodes, app_screen, screen, with_overlay, Phone, APP};
use agentroid_core::fake::FakeClock;
use agentroid_core::gate::DeviceMode;
use agentroid_core::transport::{Core, POCO_SERIAL};
use serde_json::{json, Value};
use std::path::PathBuf;

const T0: u64 = 1_792_454_400_000;
const BT: [i32; 4] = [0, 720, 1220, 880];

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("agentroid-pq1-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn flow(steps: Value) -> Value {
    json!({"flow": "t", "version": 1, "app": {"package": APP, "version_range": "*"}, "steps": steps})
}

fn bt_open() -> String {
    let mut n = app_nodes();
    n.push((APP, "Connected devices", "", false, [0, 1100, 1220, 1200]));
    screen(&n)
}

fn core_with(xml: &str) -> Core<Phone, FakeClock> {
    let mut p = Phone::new(xml);
    p.on_tap.push((BT, bt_open()));
    Core::new(p, FakeClock::new())
}

fn at(t: u64) -> When {
    When {
        not_before_unix_ms: t,
        deadline_unix_ms: None,
        present: None,
    }
}

fn bt_flow() -> Value {
    flow(json!([
        {"id": "bt", "kind": "tap", "select": {"text": "Bluetooth"},
         "post": {"kind": "present", "select": {"text": "Connected devices"}}},
        {"id": "r", "kind": "read", "select": {"text": "Connected devices"}}
    ]))
}

fn enqueue(q: &mut Queue, id: &str, f: Value, when: When) {
    q.enqueue(id, f, None, when, Some("xiaomi"), Some(34), T0)
        .unwrap();
}

fn files(dir: &std::path::Path, run: &str, prefix: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir.join(run))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with(prefix))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn a_job_waits_for_its_time_then_runs_with_no_wake_up() {
    let dir = tmp("time");
    let mut q = Queue::open(&dir).unwrap();
    enqueue(&mut q, "night-1", bt_flow(), at(T0 + 60_000));
    let mut core = core_with(&app_screen());
    let mut wakes = MemoryWakes::default();
    let rules = Rulebook::seeded();

    let early = q.tick(&mut core, &rules, T0 + 59_999, &mut wakes).unwrap();
    assert_eq!(early, vec![Tick::NotYet("night-1".into())]);
    assert_eq!(
        core.transport.inputs(),
        0,
        "nothing touched before its time"
    );
    assert!(core.transport.dev.calls.is_empty(), "not even a read");

    let due = q.tick(&mut core, &rules, T0 + 60_000, &mut wakes).unwrap();
    assert_eq!(due, vec![Tick::Done("night-1".into())]);
    assert_eq!(core.transport.inputs(), 1);
    assert_eq!(q.job("night-1").unwrap().state, JobState::Done);
    assert!(wakes.sent.is_empty(), "a clean run wakes nobody");
    assert!(files(&dir, "night-1", "wakeup").is_empty());
    assert!(dir.join("night-1").join("journal.ndjson").exists());

    // a second tick finds nothing to do
    assert!(q
        .tick(&mut core, &rules, T0 + 120_000, &mut wakes)
        .unwrap()
        .is_empty());
}

#[test]
fn a_screen_condition_holds_the_job_without_waking_anyone() {
    let dir = tmp("cond");
    let mut q = Queue::open(&dir).unwrap();
    let mut when = at(T0);
    when.deadline_unix_ms = Some(T0 + 600_000);
    when.present = Some(json!({"text": "Display"}));
    // the home screen has no "Display" row yet
    let bare = screen(&[(APP, "Settings", "", false, [48, 200, 400, 280])]);
    enqueue(
        &mut q,
        "c-1",
        flow(json!([{"id": "r", "kind": "read", "select": {"text": "Settings"}}])),
        when,
    );
    let mut core = core_with(&bare);
    let mut wakes = MemoryWakes::default();
    let rules = Rulebook::seeded();

    let t = q.tick(&mut core, &rules, T0 + 1_000, &mut wakes).unwrap();
    assert_eq!(t, vec![Tick::Waiting("c-1".into())]);
    assert_eq!(q.job("c-1").unwrap().state, JobState::Queued);
    assert!(wakes.sent.is_empty());
    assert_eq!(core.transport.inputs(), 0);

    core.transport.dev.xml_override = Some(app_screen());
    let t = q.tick(&mut core, &rules, T0 + 2_000, &mut wakes).unwrap();
    assert_eq!(t, vec![Tick::Done("c-1".into())]);
    assert!(wakes.sent.is_empty());
}

#[test]
fn a_condition_that_never_holds_wakes_once_at_the_deadline() {
    let dir = tmp("cond-late");
    let mut q = Queue::open(&dir).unwrap();
    let mut when = at(T0);
    when.deadline_unix_ms = Some(T0 + 10_000);
    when.present = Some(json!({"text": "Nothing like this"}));
    enqueue(&mut q, "c-2", bt_flow(), when);
    let mut core = core_with(&app_screen());
    let mut wakes = MemoryWakes::default();
    let rules = Rulebook::seeded();

    assert_eq!(
        q.tick(&mut core, &rules, T0 + 5_000, &mut wakes).unwrap(),
        vec![Tick::Waiting("c-2".into())]
    );
    let t = q.tick(&mut core, &rules, T0 + 10_001, &mut wakes).unwrap();
    assert_eq!(
        t,
        vec![Tick::Woke {
            run_id: "c-2".into(),
            reason: "condition_not_met".into()
        }]
    );
    // later ticks stay quiet
    assert!(q
        .tick(&mut core, &rules, T0 + 20_000, &mut wakes)
        .unwrap()
        .is_empty());
    assert_eq!(wakes.sent.len(), 1);
    assert_eq!(wakes.sent[0].reason, "condition_not_met");
    assert_eq!(core.transport.inputs(), 0, "the flow never started");
}

#[test]
fn a_missed_window_wakes_once_instead_of_running_late() {
    let dir = tmp("late");
    let mut q = Queue::open(&dir).unwrap();
    let mut when = at(T0);
    when.deadline_unix_ms = Some(T0 + 1_000);
    enqueue(&mut q, "late-1", bt_flow(), when);
    let mut core = core_with(&app_screen());
    let mut wakes = MemoryWakes::default();
    let t = q
        .tick(&mut core, &Rulebook::seeded(), T0 + 90_000, &mut wakes)
        .unwrap();
    assert_eq!(
        t,
        vec![Tick::Woke {
            run_id: "late-1".into(),
            reason: "window_missed".into()
        }]
    );
    assert_eq!(wakes.sent.len(), 1);
    assert_eq!(core.transport.inputs(), 0);
}

#[test]
fn a_failed_step_raises_exactly_one_wake_up_with_screenshot_and_journal_tail() {
    let dir = tmp("fail");
    let mut q = Queue::open(&dir).unwrap();
    // "Wifi" is not on the screen, so the second step fails after the first succeeded
    enqueue(
        &mut q,
        "fail-1",
        flow(json!([
            {"id": "ok", "kind": "tap", "select": {"text": "Bluetooth"}},
            {"id": "bad", "kind": "tap", "select": {"text": "Wifi"}}
        ])),
        at(T0),
    );
    let mut core = core_with(&app_screen());
    let mut wakes = MemoryWakes::default();
    let rules = Rulebook::seeded();
    let t = q.tick(&mut core, &rules, T0, &mut wakes).unwrap();
    assert_eq!(
        t,
        vec![Tick::Woke {
            run_id: "fail-1".into(),
            reason: "failed".into()
        }]
    );

    assert_eq!(wakes.sent.len(), 1, "one stop, one wake-up");
    let w = &wakes.sent[0];
    assert_eq!(w.wake_id, "fail-1#1");
    assert_eq!(w.step_id.as_deref(), Some("bad"));
    assert_eq!(w.model_calls_before_wake, 0);
    let shot = w.screenshot.as_ref().expect("a screenshot path");
    assert!(std::fs::metadata(shot).unwrap().len() > 1000, "{shot}");
    assert!(!w.journal_tail.is_empty());
    assert!(w.journal_tail.iter().any(|r| r.step_id == "bad"));
    assert!(w.journal_tail.iter().any(|r| r.step_id == "ok"));
    assert_eq!(w.resume, json!({"tool": "resume", "run_id": "fail-1"}));
    assert_eq!(q.job("fail-1").unwrap().state, JobState::Woken);
    assert_eq!(
        files(&dir, "fail-1", "wakeup-"),
        vec!["wakeup-1.delivered", "wakeup-1.json"]
    );

    // ticking again, or reopening the queue after a restart, raises nothing more
    assert!(q
        .tick(&mut core, &rules, T0 + 1_000, &mut wakes)
        .unwrap()
        .is_empty());
    let mut q2 = Queue::open(&dir).unwrap();
    assert!(q2
        .tick(&mut core, &rules, T0 + 2_000, &mut wakes)
        .unwrap()
        .is_empty());
    assert!(q2
        .recover(&mut core, T0 + 3_000, &mut wakes)
        .unwrap()
        .is_empty());
    assert_eq!(wakes.sent.len(), 1);
}

#[test]
fn an_unexpected_screen_pauses_and_wakes_once_without_tapping_it() {
    let dir = tmp("overlay");
    let ad = with_overlay(&[
        (
            "com.unknown.ads",
            "Win a prize!",
            "",
            false,
            [100, 1000, 1100, 1200],
        ),
        ("com.unknown.ads", "Claim", "", true, [300, 1300, 900, 1450]),
    ]);
    let mut q = Queue::open(&dir).unwrap();
    enqueue(&mut q, "ad-1", bt_flow(), at(T0));
    let mut core = core_with(&ad);
    let mut wakes = MemoryWakes::default();
    let t = q
        .tick(&mut core, &Rulebook::seeded(), T0, &mut wakes)
        .unwrap();
    assert_eq!(
        t,
        vec![Tick::Woke {
            run_id: "ad-1".into(),
            reason: "paused".into()
        }]
    );
    assert_eq!(wakes.sent.len(), 1);
    assert_eq!(
        wakes.sent[0]
            .crop
            .as_deref()
            .map(|c| c.contains("[100,1000]")),
        Some(true)
    );
    assert_eq!(
        core.transport.inputs(),
        0,
        "an unknown overlay is never tapped"
    );
}

#[test]
fn resume_continues_a_paused_job_from_its_checkpoint_and_does_not_wake_again() {
    let dir = tmp("resume");
    let ad = with_overlay(&[
        (
            "com.unknown.ads",
            "Win a prize!",
            "",
            false,
            [100, 1000, 1100, 1200],
        ),
        ("com.unknown.ads", "Claim", "", true, [300, 1300, 900, 1450]),
    ]);
    let mut q = Queue::open(&dir).unwrap();
    enqueue(&mut q, "res-1", bt_flow(), at(T0));
    let mut core = core_with(&ad);
    let mut wakes = MemoryWakes::default();
    let rules = Rulebook::seeded();
    q.tick(&mut core, &rules, T0, &mut wakes).unwrap();
    assert_eq!(q.job("res-1").unwrap().state, JobState::Woken);

    // the agent (or a person) clears the overlay, then resumes by run id
    core.transport.dev.xml_override = Some(app_screen());
    let t = q
        .resume(&mut core, &rules, "res-1", false, T0 + 5_000, &mut wakes)
        .unwrap();
    assert_eq!(t, Tick::Done("res-1".into()));
    assert_eq!(q.job("res-1").unwrap().state, JobState::Done);
    assert_eq!(
        wakes.sent.len(),
        1,
        "the resume that finished raised nothing"
    );
    assert_eq!(core.transport.inputs(), 1);

    // the journal continued where it left off: seq counts 0, 1, 2 ... with no gap or repeat
    let text = std::fs::read_to_string(dir.join("res-1").join("journal.ndjson")).unwrap();
    for (i, line) in text.lines().enumerate() {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["seq"], i as u64);
    }
    // a finished job cannot be resumed
    assert!(matches!(
        q.resume(&mut core, &rules, "res-1", false, T0 + 6_000, &mut wakes),
        Err(QueueError::NotResumable(_))
    ));
}

#[test]
fn a_resumed_job_that_stops_again_raises_wake_up_two() {
    let dir = tmp("resume-twice");
    let mut q = Queue::open(&dir).unwrap();
    enqueue(
        &mut q,
        "res-2",
        flow(json!([{"id": "bad", "kind": "tap", "select": {"text": "Wifi"}}])),
        at(T0),
    );
    let mut core = core_with(&app_screen());
    let mut wakes = MemoryWakes::default();
    let rules = Rulebook::seeded();
    q.tick(&mut core, &rules, T0, &mut wakes).unwrap();
    let t = q
        .resume(&mut core, &rules, "res-2", false, T0 + 1_000, &mut wakes)
        .unwrap();
    assert!(matches!(t, Tick::Woke { .. }), "{t:?}");
    assert_eq!(wakes.sent.len(), 2);
    assert_eq!(wakes.sent[1].wake_id, "res-2#2");
    assert_eq!(files(&dir, "res-2", "wakeup-").len(), 4);
    assert_eq!(core.transport.inputs(), 0, "nothing was sent blindly");
}

#[test]
fn resume_is_refused_for_a_queued_job_and_an_unknown_one() {
    let dir = tmp("resume-refused");
    let mut q = Queue::open(&dir).unwrap();
    enqueue(&mut q, "q-1", bt_flow(), at(T0 + 999_999));
    let mut core = core_with(&app_screen());
    let mut wakes = MemoryWakes::default();
    let rules = Rulebook::seeded();
    assert!(matches!(
        q.resume(&mut core, &rules, "q-1", false, T0, &mut wakes),
        Err(QueueError::NotResumable(_))
    ));
    assert!(matches!(
        q.resume(&mut core, &rules, "nope", false, T0, &mut wakes),
        Err(QueueError::Unknown(_))
    ));
}

#[test]
fn every_step_goes_through_the_gate_read_only_refuses_and_wakes() {
    let dir = tmp("gate");
    let mut q = Queue::open(&dir).unwrap();
    enqueue(&mut q, "ro-1", bt_flow(), at(T0));
    let mut core = core_with(&app_screen());
    core.set_mode(POCO_SERIAL, DeviceMode::ReadOnly);
    let mut wakes = MemoryWakes::default();
    let t = q
        .tick(&mut core, &Rulebook::seeded(), T0, &mut wakes)
        .unwrap();
    assert_eq!(
        t,
        vec![Tick::Woke {
            run_id: "ro-1".into(),
            reason: "refused".into()
        }]
    );
    assert!(
        core.transport.dev.acted.is_empty(),
        "the gate stopped it: {:?}",
        core.transport.dev.acted
    );
    assert_eq!(wakes.sent.len(), 1);
    // the wake-up screenshot is a read, and read_only allows reads
    assert!(wakes.sent[0].screenshot.is_some());
}

#[test]
fn enqueue_refuses_r2_duplicates_bad_ids_bad_flows_and_bad_windows() {
    let dir = tmp("enqueue");
    let mut q = Queue::open(&dir).unwrap();
    let r2 = flow(json!([{"id": "buy", "kind": "tap", "select": {"text": "Buy"}, "risk": "R2"}]));
    assert!(matches!(
        q.enqueue("r2", r2, None, at(T0), None, None, T0),
        Err(QueueError::Invalid(m)) if m.contains("R2")
    ));
    enqueue(&mut q, "dup", bt_flow(), at(T0));
    assert!(matches!(
        q.enqueue("dup", bt_flow(), None, at(T0), None, None, T0),
        Err(QueueError::Duplicate(_))
    ));
    assert!(matches!(
        q.enqueue("../evil", bt_flow(), None, at(T0), None, None, T0),
        Err(QueueError::Invalid(_))
    ));
    assert!(matches!(
        q.enqueue(
            "bad-flow",
            json!({"flow": "x"}),
            None,
            at(T0),
            None,
            None,
            T0
        ),
        Err(QueueError::Invalid(_))
    ));
    let mut w = at(T0);
    w.deadline_unix_ms = Some(T0 - 1);
    assert!(matches!(
        q.enqueue("bad-window", bt_flow(), None, w, None, None, T0),
        Err(QueueError::Invalid(_))
    ));
    let mut w = at(T0);
    w.present = Some(json!({"colour": "red"}));
    assert!(matches!(
        q.enqueue("bad-sel", bt_flow(), None, w, None, None, T0),
        Err(QueueError::Invalid(_))
    ));
    assert_eq!(q.jobs().len(), 1, "nothing refused was stored");
    // cancel only works while queued
    q.cancel("dup", T0).unwrap();
    assert!(q.cancel("dup", T0).is_err());
}

#[test]
fn the_queue_survives_a_restart_and_a_dead_runner_wakes_once() {
    let dir = tmp("recover");
    {
        let mut q = Queue::open(&dir).unwrap();
        enqueue(&mut q, "die-1", bt_flow(), at(T0));
        enqueue(&mut q, "later-1", bt_flow(), at(T0 + 1_000_000));
    }
    // simulate the runner dying mid-run: the job is marked running and never finished
    let qp = dir.join("queue.json");
    let text = std::fs::read_to_string(&qp)
        .unwrap()
        .replacen("\"queued\"", "\"running\"", 1);
    std::fs::write(&qp, text).unwrap();

    let mut q = Queue::open(&dir).unwrap();
    assert_eq!(q.jobs().len(), 2);
    let mut core = core_with(&app_screen());
    let mut wakes = MemoryWakes::default();
    let t = q.recover(&mut core, T0 + 10, &mut wakes).unwrap();
    assert_eq!(
        t,
        vec![Tick::Woke {
            run_id: "die-1".into(),
            reason: "runner_died".into()
        }]
    );
    assert!(
        q.recover(&mut core, T0 + 20, &mut wakes)
            .unwrap()
            .is_empty(),
        "once only"
    );
    assert_eq!(wakes.sent.len(), 1);
    assert_eq!(q.job("later-1").unwrap().state, JobState::Queued);
}

#[test]
fn a_failed_delivery_is_retried_without_a_second_record() {
    let dir = tmp("redeliver");
    let mut q = Queue::open(&dir).unwrap();
    enqueue(
        &mut q,
        "rd-1",
        flow(json!([{"id": "bad", "kind": "tap", "select": {"text": "Wifi"}}])),
        at(T0),
    );
    let mut core = core_with(&app_screen());
    let mut wakes = MemoryWakes {
        fail: 1,
        ..Default::default()
    };
    q.tick(&mut core, &Rulebook::seeded(), T0, &mut wakes)
        .unwrap();
    assert!(wakes.sent.is_empty(), "the first delivery failed");
    assert_eq!(files(&dir, "rd-1", "wakeup-"), vec!["wakeup-1.json"]);
    assert_eq!(q.redeliver(&mut wakes), 1);
    assert_eq!(
        q.redeliver(&mut wakes),
        0,
        "delivered once, then the marker holds"
    );
    assert_eq!(wakes.sent.len(), 1);
    assert_eq!(wakes.sent[0].wake_id, "rd-1#1");
}

#[test]
fn jobs_run_in_time_order_one_at_a_time() {
    let dir = tmp("order");
    let mut q = Queue::open(&dir).unwrap();
    enqueue(&mut q, "b-second", bt_flow(), at(T0 + 2_000));
    enqueue(&mut q, "a-first", bt_flow(), at(T0 + 1_000));
    let mut core = core_with(&app_screen());
    let mut wakes = MemoryWakes::default();
    let t = q
        .tick(&mut core, &Rulebook::seeded(), T0 + 5_000, &mut wakes)
        .unwrap();
    assert_eq!(
        t,
        vec![Tick::Done("a-first".into()), Tick::Done("b-second".into())]
    );
}
