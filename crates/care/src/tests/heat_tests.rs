//! Heat guard and battery floor rows.

use std::cell::{Cell, RefCell};

use agentroid_adb_link::runner::Runner;
use agentroid_core::gate::DeviceMode;

use super::Rig;
use crate::clock::{CareClock, FakeClock};
use crate::fake_heat::with_battery_temp;
use crate::floor::{battery_floor, check_floor, FloorVerdict};
use crate::health::BatteryInfo;
use crate::heat::{
    pace_ms, GuardAction, HeatGuard, HeatState, JobKind, Notifier, Screen, StopReason,
};
use crate::phone::{AdbPhone, Reading};
use crate::settings::Owner;
use crate::thresholds::THRESHOLDS;
use crate::CareError;

/// Records screen commands.
#[derive(Default)]
pub(crate) struct ScreenLog {
    pub calls: RefCell<Vec<&'static str>>,
    pub awake: Cell<Option<bool>>,
}

impl Screen for ScreenLog {
    fn sleep(&self) -> Result<(), CareError> {
        self.calls.borrow_mut().push("sleep");
        self.awake.set(Some(false));
        Ok(())
    }
    fn wake(&self) -> Result<bool, CareError> {
        self.calls.borrow_mut().push("wake");
        self.awake.set(Some(true));
        Ok(false)
    }
    fn awake(&self) -> Option<bool> {
        self.awake.get()
    }
}

/// Records pushes.
#[derive(Default)]
pub(crate) struct Pushes(pub RefCell<Vec<(String, String)>>);

impl Notifier for Pushes {
    fn push(&self, serial: &str, title: &str, _body: &str) -> Result<(), String> {
        self.0
            .borrow_mut()
            .push((serial.to_string(), title.to_string()));
        Ok(())
    }
}

/// Drive a guard on the fake clock, one second per tick, polling whenever it is due.
pub(crate) fn drive<R: Runner>(
    guard: &mut HeatGuard,
    phone: &AdbPhone<'_, R>,
    clock: &FakeClock,
    notifier: &dyn Notifier,
    for_ms: u64,
    mut stop: impl FnMut(&GuardAction) -> bool,
) -> Vec<(u64, GuardAction)> {
    let end = clock.mono_ms() + for_ms;
    let mut seen = vec![];
    while clock.mono_ms() <= end {
        let now = clock.mono_ms();
        if guard.due(now) {
            let r = phone.reading().expect("fake phone reads");
            let a = guard.poll(now, &r, phone, notifier);
            let done = stop(&a);
            seen.push((now, a));
            if done {
                break;
            }
        }
        clock.advance(1_000);
    }
    seen
}

#[test]
fn injected_43c_pauses_within_30s_and_resumes_only_below_37c() {
    let rig = Rig::new("heat43", "POCO0001");
    let phone = AdbPhone::new(&rig.runner, "POCO0001");
    let mut mgr = rig.manager();
    let owner = Owner::here("guard-logic-test", "s1");
    let pushes = Pushes::default();
    let mut guard = HeatGuard::new("POCO0001", JobKind::Normal);

    // Working at 30.0 °C for 17 s: no pause, no screen command.
    let warm = drive(&mut guard, &phone, &rig.clock, &pushes, 17_000, |_| false);
    assert!(warm
        .iter()
        .all(|(_, a)| matches!(a, GuardAction::Work { .. })));
    assert!(
        !rig.fake
            .commands()
            .iter()
            .any(|c| c.starts_with("input keyevent")),
        "never panel-off while working"
    );

    let injected_at = rig.clock.mono_ms();
    let clock = rig.clock.clone();
    let fake = rig.fake.clone();
    with_battery_temp(&phone, DeviceMode::Full, &mut mgr, &owner, 430, |_mgr| {
        let seen = drive(&mut guard, &phone, &clock, &pushes, 120_000, |a| {
            matches!(a, GuardAction::Paused { .. })
        });
        let (at, a) = seen.last().unwrap().clone();
        assert_eq!(a, GuardAction::Paused { temp_dc: 430 });
        assert!(
            at - injected_at <= THRESHOLDS.pause_deadline_ms,
            "paused {} ms after the injection",
            at - injected_at
        );
        assert!(!fake.state().awake, "the screen sleeps while resting");

        // Cooling: 37.1 and exactly 37.0 keep resting; 36.9 resumes.
        for (t, expect_resume) in [(371, false), (370, false), (370, false), (369, true)] {
            fake.state().temp_override = Some(t);
            let seen = drive(&mut guard, &phone, &clock, &pushes, 61_000, |_| true);
            let a = &seen.last().unwrap().1;
            assert_eq!(
                matches!(a, GuardAction::Resumed { .. }),
                expect_resume,
                "{t}: {a:?}"
            );
            if !expect_resume {
                assert!(
                    matches!(a, GuardAction::Resting { temp_dc, .. } if *temp_dc == t),
                    "{t}: {a:?}"
                );
            }
        }
        assert!(guard.is_working());
        assert!(fake.state().awake, "woken on resume");
        Ok(())
    })
    .unwrap();
    // The lease ended with a verified reset.
    let cmds = rig.fake.commands();
    let set = cmds
        .iter()
        .position(|c| c == "dumpsys battery set temp 430")
        .unwrap();
    assert!(cmds[set..].iter().any(|c| c == "dumpsys battery reset"));
    assert!(rig.fake.state().temp_override.is_none());
    assert!(rig.store().entries().unwrap().is_empty());
    assert!(pushes.0.borrow().is_empty());
}

#[test]
fn resume_boundary_is_strictly_below_37_0() {
    let (screen, pushes) = (ScreenLog::default(), Pushes::default());
    let mut g = HeatGuard::new("A", JobKind::Normal);
    assert_eq!(
        g.poll(0, &Reading::battery(419), &screen, &pushes),
        GuardAction::Work { pace_ms: 0 }
    );
    assert_eq!(
        g.poll(10_000, &Reading::battery(420), &screen, &pushes),
        GuardAction::Paused { temp_dc: 420 }
    );
    let mut t = 10_000;
    for temp in [400, 371, 370, 370, 370] {
        t += 60_000;
        assert!(
            matches!(
                g.poll(t, &Reading::battery(temp), &screen, &pushes),
                GuardAction::Resting { .. }
            ),
            "{temp} must not resume"
        );
    }
    t += 60_000;
    assert!(matches!(
        g.poll(t, &Reading::battery(369), &screen, &pushes),
        GuardAction::Resumed {
            temp_dc: 369,
            peak_dc: 420,
            ..
        }
    ));
    assert_eq!(*screen.calls.borrow(), ["sleep", "wake"]);
}

#[test]
fn only_battery_temperature_pauses_skin_and_headroom_only_pace() {
    let (screen, pushes) = (ScreenLog::default(), Pushes::default());
    let mut g = HeatGuard::new("A", JobKind::Normal);
    let r = Reading {
        battery_dc: 350,
        skin_dc: Some(480),
        headroom: Some(1.5),
        level: 50,
        powered: true,
        frozen: false,
    };
    assert_eq!(
        g.poll(0, &r, &screen, &pushes),
        GuardAction::Work {
            pace_ms: THRESHOLDS.max_pace_ms
        }
    );
    assert!(
        screen.calls.borrow().is_empty(),
        "a hot skin never sleeps the panel while working"
    );
    // proportional, capped at one call a second
    assert_eq!(pace_ms(None, None), 0);
    assert_eq!(pace_ms(Some(390), None), 0);
    assert_eq!(pace_ms(Some(405), None), 500);
    assert_eq!(pace_ms(Some(420), None), 1_000);
    assert_eq!(pace_ms(Some(600), None), 1_000);
    assert_eq!(pace_ms(None, Some(0.85)), 500);
    assert_eq!(pace_ms(None, Some(9.0)), 1_000);
    assert_eq!(pace_ms(None, Some(f32::NAN)), 0);
    assert_eq!(pace_ms(Some(399), Some(0.79)), 300);
}

#[test]
fn bench_jobs_pause_at_45c() {
    let (screen, pushes) = (ScreenLog::default(), Pushes::default());
    let mut g = HeatGuard::new("A", JobKind::Bench);
    assert!(matches!(
        g.poll(0, &Reading::battery(449), &screen, &pushes),
        GuardAction::Work { .. }
    ));
    assert!(matches!(
        g.poll(10_000, &Reading::battery(450), &screen, &pushes),
        GuardAction::Paused { .. }
    ));
}

#[test]
fn a_rest_of_60_min_pushes_once_and_stops() {
    let rig = Rig::new("restcap", "POCO0002");
    let phone = AdbPhone::new(&rig.runner, "POCO0002");
    rig.fake.state().battery_dc = 440;
    let pushes = Pushes::default();
    let mut g = HeatGuard::new("POCO0002", JobKind::Normal);
    let seen = drive(&mut g, &phone, &rig.clock, &pushes, 2 * 3_600_000, |a| {
        matches!(a, GuardAction::Stop { .. })
    });
    let paused_at = seen
        .iter()
        .find(|(_, a)| matches!(a, GuardAction::Paused { .. }))
        .unwrap()
        .0;
    let (stop_at, stop) = seen.last().unwrap().clone();
    assert!(
        matches!(
            stop,
            GuardAction::Stop {
                reason: StopReason::RestCap,
                pushed: true,
                ..
            }
        ),
        "{stop:?}"
    );
    assert_eq!(
        stop_at - paused_at,
        THRESHOLDS.rest_cap_ms,
        "stops exactly at the 60 min cap"
    );
    // every poll before the cap was a rest, not a stop
    assert!(seen
        .iter()
        .filter(|(t, _)| *t > paused_at && *t < stop_at)
        .all(|(_, a)| matches!(a, GuardAction::Resting { .. })));
    assert_eq!(
        *pushes.0.borrow(),
        [(
            "POCO0002".to_string(),
            "Phone too hot: job stopped".to_string()
        )]
    );
    assert_eq!(
        g.state(),
        HeatState::Stopped {
            reason: StopReason::RestCap
        }
    );
    assert!(!g.due(stop_at + 3_600_000), "a stopped guard polls no more");
    assert!(!rig.fake.state().awake, "the screen is left asleep");
    assert!(!rig
        .fake
        .commands()
        .iter()
        .any(|c| c == "input keyevent KEYCODE_WAKEUP"));
    // a stray extra poll neither pushes again nor wakes the phone
    let again = g.poll(stop_at + 1, &Reading::battery(440), &phone, &pushes);
    assert!(matches!(again, GuardAction::Stop { pushed: false, .. }));
    assert_eq!(pushes.0.borrow().len(), 1);
}

#[test]
fn a_woken_phone_is_put_back_to_sleep_while_resting() {
    let rig = Rig::new("rewake", "POCO0003");
    let phone = AdbPhone::new(&rig.runner, "POCO0003");
    rig.fake.state().battery_dc = 430;
    let pushes = Pushes::default();
    let mut g = HeatGuard::new("POCO0003", JobKind::Normal);
    drive(&mut g, &phone, &rig.clock, &pushes, 1_000, |a| {
        matches!(a, GuardAction::Paused { .. })
    });
    rig.fake.state().awake = true; // someone pressed power
    drive(&mut g, &phone, &rig.clock, &pushes, 61_000, |a| {
        matches!(a, GuardAction::Resting { .. })
    });
    assert!(!rig.fake.state().awake);
    assert_eq!(
        rig.fake
            .commands()
            .iter()
            .filter(|c| *c == "input keyevent KEYCODE_SLEEP")
            .count(),
        2
    );
}

#[test]
fn the_battery_floor_uses_powered_not_charging() {
    let b = |level, powered, status| BatteryInfo {
        level: Some(level),
        temp_dc: Some(300),
        status: Some(status),
        powered,
        ..Default::default()
    };
    // Held at the 80 % cap: powered, status 4 "not charging" → fine.
    assert_eq!(
        battery_floor(&b(80, true, 4)),
        FloorVerdict::Ok {
            level: 80,
            powered: true
        }
    );
    // Powered but "not charging" at 5 % (cap or a weak charger) → fine: powered counts, status does not.
    assert_eq!(
        battery_floor(&b(5, true, 4)),
        FloorVerdict::Ok {
            level: 5,
            powered: true
        }
    );
    // Status says "charging" (2) but nothing is powered → below the floor pauses.
    assert_eq!(
        battery_floor(&b(11, false, 2)),
        FloorVerdict::Pause { level: 11 }
    );
    assert_eq!(
        battery_floor(&b(12, false, 3)),
        FloorVerdict::Ok {
            level: 12,
            powered: false
        }
    );
    assert_eq!(
        battery_floor(&b(11, false, 3)),
        FloorVerdict::Pause { level: 11 }
    );
    // A frozen (overridden) battery is never judged.
    let mut f = b(1, false, 3);
    f.frozen = true;
    assert!(matches!(battery_floor(&f), FloorVerdict::Frozen { .. }));

    // Through the phone: USB powered, status "not charging", 10 %.
    let rig = Rig::new("floor", "POCO0004");
    let phone = AdbPhone::new(&rig.runner, "POCO0004");
    let mgr = rig.manager();
    {
        let mut s = rig.fake.state();
        s.level = 10;
        s.status = 4;
        s.usb = true;
    }
    assert_eq!(
        check_floor(&phone, &mgr).unwrap(),
        FloorVerdict::Ok {
            level: 10,
            powered: true
        }
    );
    {
        let mut s = rig.fake.state();
        s.usb = false;
        s.status = 2;
    }
    assert_eq!(
        check_floor(&phone, &mgr).unwrap(),
        FloorVerdict::Pause { level: 10 }
    );
    // the floor marker is gone after the check
    assert!(!std::fs::read_dir(&rig.dir)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".hold")));
}
