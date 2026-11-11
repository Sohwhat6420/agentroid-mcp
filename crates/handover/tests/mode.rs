//! The mode state machine on a fake clock with fake leases: decision within 300 ms, the human
//! profile restored exactly within 2 s, reclaim timing and its warning, media and calls, the pause
//! path, re-apply within 2 s, S1/SDK gating, merging, and the interrupted step.

mod common;

use common::{fixture, machine, ok_planner, raw_settings, FixedPlanner, KEEP};
use handover::clash::Clash;
use handover::mode::{
    Action, Caps, Injection, Mode, PauseReason, PhoneState, Resting, Source, TouchLog,
};
use handover::touch::{ClockMap, Detector};

// The ruled numbers, written here as literals on purpose: the
// tests must not read them from the crate, or a wrong constant there would also move the check.
const DECISION_BUDGET_MS: u64 = 300;
const RESTORE_BUDGET_MS: u64 = 2_000;
const REAPPLY_BUDGET_MS: u64 = 2_000;
const RECLAIM_IDLE_MS: u64 = 120_000;
const RECLAIM_TOLERANCE_MS: u64 = 5_000;
const WARN_MS: u64 = 10_000;
const PAUSE_RESUME_IDLE_MS: u64 = 300_000;
const MERGE_MS: u64 = 50;

const OFF: PhoneState = PhoneState {
    display0_on: false,
    locked: true,
    media_playing: false,
    in_call: false,
};
const ON_UNLOCKED: PhoneState = PhoneState {
    display0_on: true,
    locked: false,
    media_playing: false,
    in_call: false,
};
const ON_LOCKED: PhoneState = PhoneState {
    display0_on: true,
    locked: true,
    media_playing: false,
    in_call: false,
};

fn find<T>(acts: &[Action], f: impl Fn(&Action) -> Option<T>) -> Option<T> {
    acts.iter().find_map(f)
}

fn restored_at(acts: &[Action]) -> Option<u64> {
    find(acts, |a| {
        if let Action::HumanRestored { at } = a {
            Some(*at)
        } else {
            None
        }
    })
}

fn dot_at(acts: &[Action]) -> Option<u64> {
    find(acts, |a| {
        if let Action::ShowDot { until } = a {
            Some(*until - WARN_MS)
        } else {
            None
        }
    })
}

/// Tick every `step` ms until `until`, collecting (time, actions).
fn tick_until(
    m: &mut handover::mode::ModeMachine<common::FakeLeases, common::FakeClock>,
    clock: &common::FakeClock,
    until: u64,
    step: u64,
    p: PhoneState,
) -> Vec<(u64, Vec<Action>)> {
    let mut out = Vec::new();
    while clock.0.get() < until {
        clock.advance(step);
        let now = clock.0.get();
        let a = m.tick(p);
        if !a.is_empty() {
            out.push((now, a));
        }
    }
    out
}

/// A job in agent mode, then one touch: returns (machine, clock, store, touch time).
fn taken_over(
    caps: Caps,
) -> (
    handover::mode::ModeMachine<common::FakeLeases, common::FakeClock>,
    common::FakeClock,
    std::rc::Rc<std::cell::RefCell<common::Store>>,
    u64,
) {
    let (mut m, clock, store) = machine(1_000_000, 200, caps);
    m.start_job("job-1");
    clock.advance(5_000);
    let t = clock.0.get();
    m.human_touch(t, Source::Getevent, &mut ok_planner());
    (m, clock, store, t)
}

#[test]
fn twenty_fixture_touches_stop_injection_within_300ms_and_restore_within_2s() {
    let mut det = Detector::from_map(&fixture("getevent/synthetic-map.txt"));
    let stream = fixture("getevent/synthetic-human-20.txt");
    let hub_epoch = 5_000_000u64; // hub ms when the phone clock read 0 (+ per-line delay)
    let mut map = ClockMap::default();
    let mut caught = 0;
    for (i, line) in stream.lines().enumerate() {
        // each line reaches the hub 15..=245 ms after the phone stamped it (deterministic spread)
        let delay = 15 + (i as u64 * 37) % 231;
        let Some(phone_us) = line
            .strip_prefix('[')
            .and_then(|r| r.split_once(']'))
            .and_then(|(ts, _)| {
                let (s, f) = ts.trim().split_once('.')?;
                Some(s.parse::<u64>().ok()? * 1_000_000 + f.parse::<u64>().ok()?)
            })
        else {
            continue;
        };
        let arrive = hub_epoch + phone_us / 1000 + delay;
        map.observe(phone_us, arrive);
        let Some(touch) = det.feed(line) else {
            continue;
        };
        let event_at = map.to_hub_ms(touch.at_us);
        // a fresh job for each touch; its leases take 400 ms per call on the fake clock
        let (mut m, clock, store) = machine(event_at - 60_000, 400, KEEP);
        m.start_job(&format!("job-{i}"));
        clock.set(arrive);
        let acts = m.human_touch(event_at, Source::Getevent, &mut ok_planner());
        let (ev, decided) = find(&acts, |a| {
            if let Action::StopInjection {
                event_at,
                decided_at,
                ..
            } = a
            {
                Some((*event_at, *decided_at))
            } else {
                None
            }
        })
        .expect("injection stopped");
        assert!(
            decided - ev <= DECISION_BUDGET_MS,
            "touch {caught}: decided {} ms after the event",
            decided - ev
        );
        assert!(
            matches!(acts.first(), Some(Action::StopInjection { .. })),
            "stop injection comes before anything slow"
        );
        assert_eq!(m.injection(), Injection::VirtualDisplayOnly);
        let r = restored_at(&acts).expect("human profile restored");
        assert!(
            r - ev <= RESTORE_BUDGET_MS,
            "restored {} ms after the event",
            r - ev
        );
        let human = raw_settings("human");
        let s = store.borrow();
        for (k, v) in &human {
            assert_eq!(s.values.get(k), Some(v), "{k}");
        }
        caught += 1;
    }
    assert_eq!(caught, 20);
}

#[test]
fn agent_profile_is_leased_exactly_while_the_job_holds_the_phone() {
    let (mut m, _clock, store) = machine(0, 100, KEEP);
    assert_eq!(m.mode(), Mode::Idle);
    assert_eq!(m.injection(), Injection::Stopped);
    let acts = m.start_job("job-1");
    assert!(matches!(acts[0], Action::AgentApplied { .. }));
    assert_eq!(m.injection(), Injection::Display0);
    let agent = raw_settings("agent");
    {
        let s = store.borrow();
        for (k, v) in &agent {
            assert_eq!(s.values.get(k), Some(v), "{k}");
        }
        assert!(s.dnd_rule, "agent DND rule on");
    }
    m.end_job();
    assert_eq!(m.mode(), Mode::Idle);
    let s = store.borrow();
    assert!(s.leased.is_empty(), "every lease released");
    assert!(!s.dnd_rule);
}

#[test]
fn the_human_profile_is_restored_exactly() {
    let (m, _clock, store, _t) = taken_over(KEEP);
    assert_eq!(m.mode(), Mode::OutOfSight);
    let human = raw_settings("human");
    let agent = raw_settings("agent");
    let s = store.borrow();
    // every human-mode key: exactly the value in profiles/human.json
    for (k, v) in &human {
        assert_eq!(
            s.values.get(k),
            Some(v),
            "{k} must be exactly human.json's value"
        );
    }
    // every other agent key: back to the phone's own value (compare-and-swap), or absent again
    for k in agent.keys().filter(|k| !human.contains_key(*k)) {
        let before = common::FakeLeases::new(common::FakeClock::default(), 0)
            .store
            .borrow()
            .values
            .get(k)
            .cloned();
        assert_eq!(
            s.values.get(k).cloned(),
            before,
            "{k} restored by compare-and-swap"
        );
    }
    assert!(!s.dnd_rule, "DND off");
    assert!(s.leased.is_empty());
}

#[test]
fn a_human_change_during_the_job_is_not_overwritten_by_compare_and_swap() {
    let (mut m, clock, store) = machine(0, 100, KEEP);
    m.start_job("job-1");
    store
        .borrow_mut()
        .values
        .insert("system:screen_off_timeout".into(), "15000".into());
    clock.advance(1_000);
    m.human_touch(clock.0.get(), Source::Overlay, &mut ok_planner());
    assert_eq!(
        store
            .borrow()
            .values
            .get("system:screen_off_timeout")
            .map(String::as_str),
        Some("15000")
    );
}

#[test]
fn reclaim_after_two_minutes_then_the_ten_second_warning() {
    for p in [OFF, ON_LOCKED] {
        let (mut m, clock, _store, t) = taken_over(KEEP);
        let log = tick_until(&mut m, &clock, t + 200_000, 250, p);
        let warn = log
            .iter()
            .find_map(|(_, a)| dot_at(a))
            .expect("warning shown");
        let idle = warn - t;
        assert!(
            (RECLAIM_IDLE_MS - RECLAIM_TOLERANCE_MS..=RECLAIM_IDLE_MS + RECLAIM_TOLERANCE_MS)
                .contains(&idle),
            "warning after {idle} ms of no touch"
        );
        let reclaimed = log
            .iter()
            .find_map(|(_, a)| {
                find(a, |x| {
                    if let Action::Reclaimed { at } = x {
                        Some(*at)
                    } else {
                        None
                    }
                })
            })
            .expect("reclaimed");
        assert!(
            (WARN_MS..=WARN_MS + 250).contains(&(reclaimed - warn)),
            "reclaim {} ms after the warning",
            reclaimed - warn
        );
        assert_eq!(m.mode(), Mode::Agent);
    }
}

#[test]
fn never_reclaims_while_the_screen_is_on_and_unlocked() {
    let (mut m, clock, _store, t) = taken_over(KEEP);
    let log = tick_until(&mut m, &clock, t + 900_000, 1_000, ON_UNLOCKED);
    assert!(log.is_empty(), "{log:?}");
    assert_eq!(m.mode(), Mode::OutOfSight);
}

#[test]
fn a_touch_during_the_warning_cancels_it_ten_of_ten() {
    for i in 0..10u64 {
        let (mut m, clock, _store, t) = taken_over(KEEP);
        let log = tick_until(&mut m, &clock, t + RECLAIM_IDLE_MS + 1_000, 500, OFF);
        let warn = log
            .iter()
            .find_map(|(_, a)| dot_at(a))
            .expect("warning shown");
        assert!(matches!(m.mode(), Mode::ReclaimWarn { .. }));
        // touch somewhere inside the 10 s
        clock.set(warn + 500 + i * 900);
        let acts = m.human_touch(
            clock.0.get(),
            if i % 2 == 0 {
                Source::Getevent
            } else {
                Source::Overlay
            },
            &mut ok_planner(),
        );
        assert!(acts.contains(&Action::HideDot), "run {i}: {acts:?}");
        assert_eq!(m.mode(), Mode::OutOfSight, "run {i}");
        // no reclaim for the rest of the warning, nor before another 2 min of no touch
        let touch = clock.0.get();
        let after = tick_until(
            &mut m,
            &clock,
            touch + RECLAIM_IDLE_MS - RECLAIM_TOLERANCE_MS,
            500,
            OFF,
        );
        assert!(
            after.iter().all(|(_, a)| !a
                .iter()
                .any(|x| matches!(x, Action::Reclaimed { .. } | Action::ShowDot { .. }))),
            "run {i}: {after:?}"
        );
        assert_eq!(m.mode(), Mode::OutOfSight);
    }
}

#[test]
fn never_reclaims_during_media_or_a_call() {
    for busy in [
        PhoneState {
            media_playing: true,
            ..OFF
        },
        PhoneState {
            in_call: true,
            ..OFF
        },
    ] {
        let (mut m, clock, _store, t) = taken_over(KEEP);
        let log = tick_until(&mut m, &clock, t + 900_000, 1_000, busy);
        assert!(log.is_empty(), "{busy:?}: {log:?}");
        // media or a call that starts during the warning cancels it
        let w = tick_until(&mut m, &clock, clock.0.get() + 2_000, 500, OFF);
        assert!(w.iter().any(|(_, a)| dot_at(a).is_some()));
        let c = m.tick(busy);
        assert!(c.contains(&Action::HideDot));
        assert_eq!(m.mode(), Mode::OutOfSight);
        let rest = tick_until(&mut m, &clock, clock.0.get() + 60_000, 1_000, busy);
        assert!(rest.is_empty());
    }
}

#[test]
fn the_pause_path_resumes_only_after_five_minutes_with_the_screen_off() {
    let caps = Caps {
        s1_passed: false,
        sdk: 35,
    };
    let (mut m, clock, store) = machine(1_000_000, 200, caps);
    m.start_job("job-1");
    m.step_started("step-7");
    clock.advance(3_000);
    let t = clock.0.get();
    let acts = m.human_touch(t, Source::Getevent, &mut ok_planner());
    assert!(acts.contains(&Action::PauseJob(PauseReason::NoKeepWorking)));
    assert!(acts.contains(&Action::InterruptedStep {
        step: "step-7".into()
    }));
    assert_eq!(m.mode(), Mode::Paused);
    assert_eq!(m.injection(), Injection::Stopped);
    // locked but screen on: never resumes
    assert!(tick_until(&mut m, &clock, t + 900_000, 1_000, ON_LOCKED).is_empty());
    // a touch resets the five minutes
    let t2 = clock.0.get();
    m.human_touch(t2, Source::Overlay, &mut ok_planner());
    let log = tick_until(&mut m, &clock, t2 + 400_000, 500, OFF);
    let warn = log.iter().find_map(|(_, a)| dot_at(a)).expect("warning");
    assert!(
        (PAUSE_RESUME_IDLE_MS..=PAUSE_RESUME_IDLE_MS + 500).contains(&(warn - t2)),
        "{}",
        warn - t2
    );
    let resumed = log
        .iter()
        .find(|(_, a)| a.iter().any(|x| matches!(x, Action::ResumeJob { .. })))
        .expect("resumed");
    assert!(
        resumed.1.contains(&Action::ResumeJob {
            check_first: Some("step-7".into())
        }),
        "the interrupted step is checked first, never re-run"
    );
    assert!(resumed.0 - warn >= WARN_MS);
    assert_eq!(m.mode(), Mode::Agent);
    let agent = raw_settings("agent");
    for (k, v) in &agent {
        assert_eq!(store.borrow().values.get(k), Some(v));
    }
}

#[test]
fn the_agent_profile_is_reapplied_within_two_seconds_of_a_reclaim() {
    let (mut m, clock, store, t) = taken_over(KEEP);
    // slow leases: 700 ms per call
    m.leases.latency_ms = 700;
    let log = tick_until(&mut m, &clock, t + 200_000, 1_000, OFF);
    let (_, acts) = log
        .iter()
        .find(|(_, a)| a.iter().any(|x| matches!(x, Action::Reclaimed { .. })))
        .expect("reclaimed");
    let rec = find(acts, |a| {
        if let Action::Reclaimed { at } = a {
            Some(*at)
        } else {
            None
        }
    })
    .unwrap();
    let applied = find(acts, |a| {
        if let Action::AgentApplied { at } = a {
            Some(*at)
        } else {
            None
        }
    })
    .expect("agent re-applied");
    assert!(applied - rec <= REAPPLY_BUDGET_MS, "{} ms", applied - rec);
    for (k, v) in &raw_settings("agent") {
        assert_eq!(store.borrow().values.get(k), Some(v), "{k}");
    }
    assert!(store.borrow().dnd_rule);
}

#[test]
fn lease_failures_are_retried_inside_the_budget_and_then_on_the_next_tick() {
    let (mut m, clock, store) = machine(0, 100, KEEP);
    m.start_job("job-1");
    m.leases.fail_next.set(2);
    clock.advance(1_000);
    let t = clock.0.get();
    let acts = m.human_touch(t, Source::Getevent, &mut ok_planner());
    let r = restored_at(&acts).expect("third attempt lands");
    assert!(r - t <= RESTORE_BUDGET_MS);
    assert_eq!(
        acts.iter()
            .filter(|a| matches!(a, Action::LeaseTrouble { .. }))
            .count(),
        2
    );
    // every attempt fails: injection is still stopped, and the next tick retries
    let (mut m2, clock2, store2) = machine(0, 100, KEEP);
    m2.start_job("job-2");
    m2.leases.fail_next.set(3);
    clock2.advance(1_000);
    let acts = m2.human_touch(clock2.0.get(), Source::Getevent, &mut ok_planner());
    assert!(restored_at(&acts).is_none());
    assert_ne!(m2.injection(), Injection::Display0);
    let next = m2.tick(ON_UNLOCKED);
    assert!(restored_at(&next).is_some());
    for (k, v) in &raw_settings("human") {
        assert_eq!(store2.borrow().values.get(k), Some(v));
    }
    drop(store);
}

#[test]
fn sdk_below_30_or_s1_failed_means_pause_on_touch() {
    for caps in [
        Caps {
            s1_passed: false,
            sdk: 35,
        },
        Caps {
            s1_passed: true,
            sdk: 29,
        },
        Caps {
            s1_passed: false,
            sdk: 29,
        },
    ] {
        let (m, _c, _s, _t) = taken_over(caps);
        assert_eq!(m.mode(), Mode::Paused, "{caps:?}");
    }
    let (m, _c, _s, _t) = taken_over(Caps {
        s1_passed: true,
        sdk: 30,
    });
    assert_eq!(m.mode(), Mode::OutOfSight);
}

#[test]
fn a_clash_at_handover_or_while_out_of_sight_pauses() {
    let (mut m, clock, _store) = machine(0, 100, KEEP);
    m.start_job("job-1");
    clock.advance(1_000);
    let clash = Clash::SecureWindow {
        package: "com.example.securepay".into(),
    };
    let acts = m.human_touch(
        clock.0.get(),
        Source::Getevent,
        &mut FixedPlanner(Err(clash.clone())),
    );
    assert!(acts.contains(&Action::PauseJob(PauseReason::Clash(clash))));
    assert_eq!(m.mode(), Mode::Paused);
    // while out of sight, and during a warning
    let (mut m, clock, _store, t) = taken_over(KEEP);
    let acts = m.clash(Clash::SameAppOnDisplay0 {
        package: "com.example.jobapp".into(),
    });
    assert!(matches!(acts[0], Action::PauseJob(PauseReason::Clash(_))));
    assert_eq!(m.mode(), Mode::Paused);
    let _ = (clock, t);
    let (mut m, clock, _store, t) = taken_over(KEEP);
    tick_until(&mut m, &clock, t + RECLAIM_IDLE_MS + 1_000, 500, OFF);
    assert!(matches!(
        m.mode(),
        Mode::ReclaimWarn {
            from: Resting::OutOfSight,
            ..
        }
    ));
    let acts = m.clash(Clash::TaskLeftVirtualDisplay {
        package: "com.example.jobapp".into(),
    });
    assert_eq!(acts[0], Action::HideDot);
    assert_eq!(m.mode(), Mode::Paused);
}

#[test]
fn both_detectors_feed_one_last_touch_with_merging() {
    let mut log = TouchLog::default();
    assert!(log.record(10_000));
    assert!(
        !log.record(10_000 + MERGE_MS),
        "the other detector's copy merges"
    );
    assert!(
        !log.record(10_000 + 10),
        "an earlier copy (within 50 ms of the latest) merges too"
    );
    assert_eq!(log.last(), Some(10_050));
    assert!(log.record(10_200));
    assert!(log.record(9_000), "a distinct older touch is recorded");
    assert_eq!(log.last(), Some(10_200), "the latest timestamp wins");
    // in the machine: getevent then the overlay 30 ms later is one takeover
    let (mut m, clock, store) = machine(0, 100, KEEP);
    m.start_job("job-1");
    clock.advance(1_000);
    let t = clock.0.get();
    let a1 = m.human_touch(t, Source::Getevent, &mut ok_planner());
    let a2 = m.human_touch(t + 30, Source::Overlay, &mut ok_planner());
    assert!(a1.iter().any(|a| matches!(a, Action::StopInjection { .. })));
    assert!(a2.is_empty());
    assert_eq!(
        store
            .borrow()
            .calls
            .iter()
            .filter(|c| c.starts_with("hand_back"))
            .count(),
        1
    );
    assert_eq!(m.last_human_touch(), Some(t + 30));
    assert_eq!(m.touch_log().merged, 1);
}

#[test]
fn a_touch_older_than_the_agents_hold_never_takes_over() {
    let (mut m, clock, _store, t) = taken_over(KEEP);
    tick_until(&mut m, &clock, t + 200_000, 1_000, OFF);
    assert_eq!(m.mode(), Mode::Agent);
    // a late-delivered copy of a touch from before the reclaim
    let acts = m.human_touch(t + 100_000, Source::Overlay, &mut ok_planner());
    assert!(acts.is_empty());
    assert_eq!(m.mode(), Mode::Agent);
    // a new touch does
    let acts = m.human_touch(clock.0.get(), Source::Getevent, &mut ok_planner());
    assert!(acts
        .iter()
        .any(|a| matches!(a, Action::StopInjection { .. })));
}

#[test]
fn the_interrupted_step_is_reported_once_and_never_rerun() {
    let (mut m, clock, _store) = machine(0, 100, KEEP);
    m.start_job("job-1");
    m.step_started("s1");
    m.step_finished("s1");
    m.step_started("s2");
    clock.advance(1_000);
    let acts = m.human_touch(clock.0.get(), Source::Getevent, &mut ok_planner());
    let steps: Vec<_> = acts
        .iter()
        .filter_map(|a| {
            if let Action::InterruptedStep { step } = a {
                Some(step.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(steps, vec!["s2".to_string()]);
    // the machine itself never asks for the step to run again
    assert!(!acts.iter().any(|a| matches!(a, Action::ResumeJob { .. })));
}

#[test]
fn every_transition_is_journalled() {
    let (mut m, clock, _store, t) = taken_over(KEEP);
    tick_until(&mut m, &clock, t + 200_000, 1_000, OFF);
    m.end_job();
    let path: Vec<Mode> = m.journal.iter().map(|j| j.to).collect();
    assert_eq!(path[0], Mode::Agent);
    assert_eq!(path[1], Mode::HumanSeen);
    assert_eq!(path[2], Mode::OutOfSight);
    assert!(matches!(
        path[3],
        Mode::ReclaimWarn {
            from: Resting::OutOfSight,
            ..
        }
    ));
    assert_eq!(path[4], Mode::Agent);
    assert_eq!(path[5], Mode::Idle);
    assert!(m.journal.windows(2).all(|w| w[0].to == w[1].from));
}
