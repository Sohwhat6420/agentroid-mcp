//! The virtual-display handover plan and the clash rules over synthetic `am stack list`,
//! `dumpsys window` and `dumpsys activity` output; a clash pauses the job 5 of 5.

mod common;

use common::{fixture, machine, KEEP};
use handover::clash::{parse_resumed, parse_secure_windows, Clash, ClashPolicy};
use handover::mode::{Action, Mode, PauseReason, Planner, Source};
use handover::vdisplay::{check_ongoing, parse_stack_list, plan_handover, Move, Plan, Scene};

fn scene() -> Scene {
    Scene {
        tasks: parse_stack_list(&fixture("vdisplay/synthetic-stack-list.txt")),
        secure: parse_secure_windows(&fixture("vdisplay/synthetic-dumpsys-window.txt")),
        resumed: parse_resumed(&fixture("vdisplay/synthetic-activities.txt")),
    }
}

fn policy() -> ClashPolicy {
    ClashPolicy {
        banking: vec!["com.example.bank".into()],
        camera: vec!["com.example.camera".into()],
        refuses_secondary: vec!["com.example.fixedscreen".into()],
    }
}

/// The real planner: the scene at the moment of the touch.
struct ScenePlanner {
    pkg: String,
    task: Option<u32>,
    scene: Scene,
}

impl Planner for ScenePlanner {
    fn plan(&mut self) -> Result<Plan, Clash> {
        plan_handover(&self.pkg, self.task, 2, &self.scene, &policy())
    }
}

#[test]
fn parsers_read_the_synthetic_dumps() {
    let s = scene();
    assert_eq!(s.tasks.len(), 5);
    let job = s
        .tasks
        .iter()
        .find(|t| t.package == "com.example.jobapp")
        .unwrap();
    assert_eq!(
        (job.task_id, job.root_task_id, job.display, job.visible),
        (31, 1, 0, true)
    );
    assert_eq!(s.tasks.iter().find(|t| t.task_id == 50).unwrap().display, 2);
    assert_eq!(
        s.secure.len(),
        1,
        "only a real SECURE flag counts: {:?}",
        s.secure
    );
    assert_eq!(s.secure[0].package, "com.example.securepay");
    assert_eq!(
        s.resumed.get(&0).map(String::as_str),
        Some("com.example.jobapp")
    );
    assert_eq!(
        s.resumed.get(&2).map(String::as_str),
        Some("com.example.alreadyhidden")
    );
}

#[test]
fn a_compatible_app_moves_and_nothing_else_does() {
    let plan = plan_handover("com.example.jobapp", Some(31), 2, &scene(), &policy()).unwrap();
    assert_eq!(
        plan.moves,
        vec![Move {
            root_task_id: 1,
            package: "com.example.jobapp".into(),
            from: 0,
            to: 2
        }]
    );
    assert_eq!(
        plan.moves[0].command(),
        vec!["am", "display", "move-stack", "1", "2"]
    );
    // an app already on the virtual display needs no move
    assert!(
        plan_handover("com.example.alreadyhidden", None, 2, &scene(), &policy())
            .unwrap()
            .moves
            .is_empty()
    );
    // in the machine: the job keeps working out of sight
    let (mut m, clock, _s) = machine(0, 100, KEEP);
    m.start_job("job-1");
    clock.advance(1_000);
    let mut planner = ScenePlanner {
        pkg: "com.example.jobapp".into(),
        task: Some(31),
        scene: scene(),
    };
    let acts = m.human_touch(clock.0.get(), Source::Getevent, &mut planner);
    assert!(acts
        .iter()
        .any(|a| matches!(a, Action::MoveToVirtualDisplay(p) if p.moves.len() == 1)));
    assert_eq!(m.mode(), Mode::OutOfSight);
}

#[test]
fn a_clash_pauses_five_of_five() {
    let mut s = scene();
    // put the bank, camera and fixed-screen apps on display 0 as the job's own tasks
    for (id, pkg) in [
        (61u32, "com.example.bank"),
        (62, "com.example.camera"),
        (63, "com.example.fixedscreen"),
    ] {
        s.tasks.push(handover::vdisplay::Task {
            task_id: id,
            root_task_id: id,
            display: 0,
            package: pkg.into(),
            visible: false,
        });
    }
    type Case = (&'static str, Option<u32>, fn(&Clash) -> bool);
    let cases: [Case; 5] = [
        ("com.example.bank", Some(61), |c| {
            matches!(c, Clash::Banking { .. })
        }),
        ("com.example.camera", Some(62), |c| {
            matches!(c, Clash::Camera { .. })
        }),
        ("com.example.fixedscreen", Some(63), |c| {
            matches!(c, Clash::RefusesSecondaryDisplay { .. })
        }),
        ("com.example.securepay", None, |c| {
            matches!(c, Clash::SecureWindow { .. })
        }),
        // the job holds task 40; the human has the same app's task 41 on display 0
        ("com.example.twotask", Some(40), |c| {
            matches!(c, Clash::SameAppOnDisplay0 { .. })
        }),
    ];
    let mut paused = 0;
    for (pkg, task, want) in cases {
        let (mut m, clock, _s) = machine(0, 100, KEEP);
        m.start_job("job-1");
        clock.advance(1_000);
        let mut planner = ScenePlanner {
            pkg: pkg.into(),
            task,
            scene: s.clone(),
        };
        let acts = m.human_touch(clock.0.get(), Source::Getevent, &mut planner);
        let reason = acts.iter().find_map(|a| {
            if let Action::PauseJob(PauseReason::Clash(c)) = a {
                Some(c.clone())
            } else {
                None
            }
        });
        let reason = reason.unwrap_or_else(|| panic!("{pkg}: no clash pause in {acts:?}"));
        assert!(want(&reason), "{pkg}: {reason:?}");
        assert_eq!(m.mode(), Mode::Paused, "{pkg}");
        assert!(
            !acts
                .iter()
                .any(|a| matches!(a, Action::MoveToVirtualDisplay(_))),
            "{pkg}"
        );
        paused += 1;
    }
    assert_eq!(paused, 5);
}

#[test]
fn clashes_while_out_of_sight_are_found() {
    let mut s = scene();
    // after the move: the job's task is on display 2
    for t in s
        .tasks
        .iter_mut()
        .filter(|t| t.package == "com.example.jobapp")
    {
        t.display = 2;
    }
    s.resumed.insert(0, "com.example.launcher".into());
    assert_eq!(check_ongoing("com.example.jobapp", 2, &s), Ok(()));
    // the human opens the same app on display 0
    let mut same = s.clone();
    same.resumed.insert(0, "com.example.jobapp".into());
    assert_eq!(
        check_ongoing("com.example.jobapp", 2, &same),
        Err(Clash::SameAppOnDisplay0 {
            package: "com.example.jobapp".into()
        })
    );
    // the task left the hidden screen
    let mut left = s.clone();
    for t in left
        .tasks
        .iter_mut()
        .filter(|t| t.package == "com.example.jobapp")
    {
        t.display = 0;
    }
    assert!(matches!(
        check_ongoing("com.example.jobapp", 2, &left),
        Err(Clash::TaskLeftVirtualDisplay { .. })
    ));
    // a secure window appears in the job's app
    let mut sec = s.clone();
    sec.secure.push(handover::clash::SecureWindow {
        package: "com.example.jobapp".into(),
        display: 2,
    });
    assert!(matches!(
        check_ongoing("com.example.jobapp", 2, &sec),
        Err(Clash::SecureWindow { .. })
    ));
}
