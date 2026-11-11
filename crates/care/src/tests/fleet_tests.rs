//! Parallel phones: one guard each, acting independently; the memory gate caps the count.

use agentroid_adb_link::guard::GuardedRunner;

use super::heat_tests::Pushes;
use crate::clock::{CareClock, FakeClock, TEST_EPOCH_MS};
use crate::fake::FakePhone;
use crate::fleet::{system_commit_pct, CommitChargeGate, FixedGate, Fleet, MemoryGate};
use crate::heat::{GuardAction, HeatState, JobKind};
use crate::phone::AdbPhone;

#[test]
fn two_guards_act_independently() {
    let (fa, fb) = (FakePhone::new("PHONEA01"), FakePhone::new("PHONEB01"));
    let (ga, gb) = (
        GuardedRunner::new(fa.clone()),
        GuardedRunner::new(fb.clone()),
    );
    let (pa, pb) = (
        AdbPhone::new(&ga, "PHONEA01"),
        AdbPhone::new(&gb, "PHONEB01"),
    );
    let clock = FakeClock::new(TEST_EPOCH_MS);
    let pushes = Pushes::default();
    let mut fleet = Fleet::new(FixedGate(2));
    fleet.start("PHONEA01", JobKind::Normal).unwrap();
    fleet.start("PHONEB01", JobKind::Normal).unwrap();
    fa.state().battery_dc = 430; // A is hot, B is not
    fb.state().battery_dc = 300;
    fb.state().skin_dc = Some(405); // B's warm skin only paces it

    let (mut acts_a, mut acts_b) = (vec![], vec![]);
    for _ in 0..(20 * 60) {
        let now = clock.mono_ms();
        if now == 10 * 60_000 {
            // A has cooled; B heats up
            fa.state().battery_dc = 360;
            fb.state().battery_dc = 425;
        }
        for (serial, phone, acts) in [
            ("PHONEA01", &pa, &mut acts_a),
            ("PHONEB01", &pb, &mut acts_b),
        ] {
            let g = fleet.guard(serial).unwrap();
            if g.due(now) {
                let r = phone.reading().unwrap();
                acts.push((now, g.poll(now, &r, phone, &pushes)));
            }
        }
        clock.advance(1_000);
    }
    let first = |acts: &Vec<(u64, GuardAction)>, f: fn(&GuardAction) -> bool| {
        acts.iter().find(|(_, a)| f(a)).map(|(t, _)| *t)
    };
    let paused = |a: &GuardAction| matches!(a, GuardAction::Paused { .. });
    let resumed = |a: &GuardAction| matches!(a, GuardAction::Resumed { .. });
    assert_eq!(first(&acts_a, paused), Some(0), "A pauses at once");
    assert!(
        acts_b
            .iter()
            .filter(|(t, _)| *t < 10 * 60_000)
            .all(|(_, a)| *a == GuardAction::Work { pace_ms: 500 }),
        "B keeps working, paced by its own skin"
    );
    assert!(first(&acts_a, resumed).unwrap() >= 10 * 60_000);
    assert!(first(&acts_b, paused).unwrap() >= 10 * 60_000);
    assert_eq!(fleet.guard("PHONEA01").unwrap().state(), HeatState::Working);
    assert!(matches!(
        fleet.guard("PHONEB01").unwrap().state(),
        HeatState::Resting { .. }
    ));
    // each phone's screen commands went to that phone only
    let sleeps = |f: &FakePhone| {
        f.commands()
            .iter()
            .filter(|c| *c == "input keyevent KEYCODE_SLEEP")
            .count()
    };
    assert_eq!((sleeps(&fa), sleeps(&fb)), (1, 1));
    assert!(fa.state().awake && !fb.state().awake);
    assert!(pushes.0.borrow().is_empty());
}

#[test]
fn the_memory_gate_caps_how_many_run() {
    let mut fleet = Fleet::new(FixedGate(1));
    fleet.start("A", JobKind::Normal).unwrap();
    assert!(
        fleet.start("A", JobKind::Normal).is_err(),
        "one job per phone"
    );
    assert!(
        fleet.start("B", JobKind::Normal).is_err(),
        "the gate holds at 1"
    );
    assert!(fleet.finish("A").is_some());
    assert!(fleet.start("B", JobKind::Bench).is_ok());
    assert_eq!(fleet.running(), 1);
    assert_eq!(fleet.guard("B").unwrap().pause_at_dc(), 450);

    assert!(CommitChargeGate {
        limit_pct: 75,
        read_pct: || Some(74)
    }
    .admit(5)
    .is_ok());
    assert!(CommitChargeGate {
        limit_pct: 75,
        read_pct: || Some(75)
    }
    .admit(0)
    .is_err());
    assert!(
        CommitChargeGate {
            limit_pct: 75,
            read_pct: || None
        }
        .admit(0)
        .is_err(),
        "unreadable fails closed"
    );
    if cfg!(windows) {
        let p = system_commit_pct().expect("GlobalMemoryStatusEx answers");
        assert!(p <= 100);
    }
}
