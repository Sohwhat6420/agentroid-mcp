//! The task 2.7 tests: doctor from recorded fixtures, setup, --dry-run, --undo, prompts, the protected phone.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use agentroid_adb_link::guard::{GuardedRunner, S24_READS, S24_SERIAL};
use agentroid_core::gate::DeviceMode;

use crate::device::{self, HELPER_SERVICE, WRITE_WORDS};
use crate::doctor::{self, *};
use crate::journal::{self, tree_hash};
use crate::quirks::Quirks;
use crate::setup::{self, Budget, Options, Prompter, Question, SetupError, MAX_PROMPTS};
use crate::testkit::{line_of, FakePhone, Recorded, Shared};

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("../fixtures/doctor/", $name, ".txt"))
    };
}

const ALL_GOOD: &str = fixture!("all-good");
const FRESH: &str = fixture!("fresh-poco");
const IP: &str = "192.0.2.78:37123";

fn q() -> Quirks {
    Quirks::builtin()
}

fn run_doctor(fixture: &str) -> Report {
    doctor::doctor(&Recorded::new(fixture), IP, DeviceMode::Full, &q()).unwrap()
}

/// A scripted human: answers in order (empty means "take the default"); records every question.
struct Script {
    answers: VecDeque<String>,
    asked: Vec<Question>,
}

impl Script {
    fn new(answers: &[&str]) -> Self {
        Script {
            answers: answers.iter().map(|s| s.to_string()).collect(),
            asked: vec![],
        }
    }
}

impl Prompter for Script {
    fn ask(&mut self, q: &Question) -> String {
        self.asked.push(q.clone());
        self.answers.pop_front().unwrap_or_default()
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("agentroid-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A config dir that already holds another phone's entry and an unrelated file.
fn seeded(name: &str) -> TempDir {
    let t = TempDir::new(name);
    std::fs::create_dir_all(&t.0).unwrap();
    std::fs::write(
        t.0.join("devices.toml"),
        "# hand-written\n[devices.\"PLACEHOLDER-F5-SERIAL\"]\nname = \"F5 Pro\"\nmode = \"full\"\r\n\n# keep me\n",
    )
    .unwrap();
    std::fs::write(t.0.join("notes.txt"), "unrelated bytes \u{e9}\n").unwrap();
    t
}

fn opts(dir: &Path) -> Options {
    Options {
        config_dir: dir.to_path_buf(),
        target: Some(IP.to_string()),
        ..Default::default()
    }
}

// ---------------------------------------------------------------- doctor

#[test]
fn doctor_all_good_fixture_reports_nothing() {
    let r = run_doctor(ALL_GOOD);
    assert_eq!(r.ids(), Vec::<&str>::new(), "{}", r.render());
    assert!(r.unchecked.is_empty(), "{:?}", r.unchecked);
    assert_eq!(
        (
            r.profile.maker.as_str(),
            r.profile.skin.as_str(),
            r.profile.sdk
        ),
        ("Xiaomi", "HyperOS", 35)
    );
    assert_eq!(r.serial, "PLACEHOLDER-POCO-SERIAL");
    // the quirk "note" is a one-off step, not a finding
    assert_eq!(r.notes.len(), 1);
    assert!(r.render().contains("nothing wrong found"));
}

#[test]
fn doctor_names_each_switch_from_its_fixture() {
    let cases: [(&str, &str, &str); 9] = [
        (fixture!("helper-missing"), HELPER_MISSING, "install"),
        (fixture!("wireless-debugging-off"), WIRELESS_DEBUGGING_OFF, "shell \"settings put global adb_wifi_enabled '1'\""),
        (
            fixture!("accessibility-off"),
            ACCESSIBILITY_OFF,
            "settings put secure enabled_accessibility_services 'com.example.synthetic.artemis/com.example.synthetic.artemis.ArtemisService:dev.agentroid.helper/dev.agentroid.helper.a11y.HelperAccessibilityService'",
        ),
        (fixture!("battery-optimisation-on"), BATTERY_OPTIMISATION_ON, "shell \"dumpsys deviceidle whitelist +dev.agentroid.helper\""),
        (fixture!("background-restricted"), BACKGROUND_RESTRICTED, "cmd appops set dev.agentroid.helper RUN_ANY_IN_BACKGROUND allow"),
        (fixture!("wss-missing"), WSS_MISSING, "pm grant dev.agentroid.helper android.permission.WRITE_SECURE_SETTINGS"),
        (fixture!("autostart-off"), "hyperos_autostart", "cmd appops set dev.agentroid.helper 10008 allow"),
        (fixture!("no-restrictions-missing"), "hyperos_no_restrictions", "Battery saver > No restrictions"),
        (fixture!("millet-key-absent"), "hyperos_no_restrictions:key_absent", "MILLET_NO_RESTRICT_APP is not in settings"),
    ];
    for (text, id, fix) in cases {
        let r = run_doctor(text);
        assert_eq!(r.ids(), vec![id], "{id}: {}", r.render());
        let f = &r.findings[0];
        let fix_text = f.fix_text(IP);
        assert!(fix_text.contains(fix), "{id}: fix was {fix_text}");
        assert!(r.render().contains(&format!("WRONG {id}")), "{id}");
    }
    // autostart: only the wrong op is fixed
    let r = run_doctor(fixture!("autostart-off"));
    assert_eq!(r.findings[0].changes.len(), 1);
    assert!(r.findings[0].detail.contains("op 10008 is deny"));
    // the accessibility fix keeps the service already there (append, never replace)
    let r = run_doctor(fixture!("accessibility-off"));
    assert_eq!(
        r.findings[0].changes.len(),
        1,
        "accessibility_enabled is already 1"
    );
    // a missing helper leaves the helper checks "not checked", never "fine"
    let r = run_doctor(fixture!("helper-missing"));
    assert!(r
        .unchecked
        .iter()
        .any(|u| u.contains("helper is not installed")));
    // and WRITE_SECURE_SETTINGS is a full-mode switch only
    let helper_mode = doctor::doctor(
        &Recorded::new(fixture!("wss-missing")),
        IP,
        DeviceMode::Helper,
        &q(),
    )
    .unwrap();
    assert!(helper_mode.findings.is_empty());
}

#[test]
fn doctor_flags_each_uiautomation_owner() {
    for (text, name, running) in [
        (fixture!("owner-uiautomator2"), "uiautomator2", true),
        (fixture!("owner-appium"), "Appium", false),
        (fixture!("owner-mobilecli"), "mobilecli", true),
    ] {
        let r = run_doctor(text);
        let id = format!("{OWNER_PREFIX}{name}");
        assert_eq!(r.ids(), vec![id.as_str()], "{name}: {}", r.render());
        let f = &r.findings[0];
        assert_eq!(
            f.detail.starts_with("running now"),
            running,
            "{name}: {}",
            f.detail
        );
        let fix = f.fix_text(IP);
        assert!(fix.contains("uninstall"), "{name}: {fix}");
        assert_eq!(fix.contains("am force-stop"), running, "{name}: {fix}");
        assert!(
            f.changes.is_empty(),
            "setup never removes another tool by itself"
        );
    }
}

#[test]
fn doctor_reads_quirks_by_skin() {
    // the same wrong Autostart op on a phone the probe does not see as HyperOS: no quirk applies
    let text = fixture!("autostart-off").replace(
        "### getprop ro.mi.os.version.name\nOS2.0",
        "### getprop ro.mi.os.version.name\n",
    );
    let r = run_doctor(&text);
    assert!(r.findings.is_empty(), "{}", r.render());
    assert!(r.notes.is_empty());
    // and an SDK outside the range drops it too
    let text = fixture!("autostart-off").replace(
        "### getprop ro.build.version.sdk\n35",
        "### getprop ro.build.version.sdk\n33",
    );
    assert!(run_doctor(&text).findings.is_empty());
}

#[test]
fn doctor_reports_a_failed_read_as_not_checked() {
    let text = fixture!("wireless-debugging-off").replace(
        "### settings list global",
        "### settings list global (unanswered)",
    );
    let r = run_doctor(&text);
    assert!(!r.ids().contains(&WIRELESS_DEBUGGING_OFF));
    assert!(
        r.unchecked.iter().any(|u| u.contains("wireless debugging")),
        "{:?}",
        r.unchecked
    );
    let text = ALL_GOOD.replace("### pm list packages", "### pm list packages (unanswered)");
    let r = run_doctor(&text);
    assert!(r.findings.is_empty());
    assert!(r
        .unchecked
        .iter()
        .any(|u| u.contains("pm list packages failed")));
}

#[test]
fn doctor_sends_only_read_commands() {
    let list = doctor::reads_for(DeviceMode::Full);
    let probe = doctor::read_list(device::PROBE_FULL);
    let own = [
        "cmd appops get dev.agentroid.helper",
        "dumpsys package dev.agentroid.helper",
        "pm list packages",
        "ps -A -o NAME",
    ];
    for l in &list {
        assert!(
            probe.contains(l) || own.contains(&l.as_str()),
            "{l} is neither a probe read nor one of doctor's four"
        );
        for w in l.split(' ') {
            assert!(!WRITE_WORDS.contains(&w), "{l} holds the write word {w}");
        }
    }
    assert!(
        list.iter().filter(|l| probe.contains(l)).count() >= 10,
        "doctor reuses the probe's reads"
    );
    let fake = Recorded::new(ALL_GOOD);
    doctor::doctor(&fake, IP, DeviceMode::Full, &q()).unwrap();
    let sent = fake.lines();
    assert_eq!(sent, list, "exactly the list, in order, serial first");
    for a in fake.calls.borrow().iter() {
        assert_eq!(
            (a.len(), a[0].as_str(), a[1].as_str(), a[2].as_str()),
            (4, "-s", IP, "shell")
        );
    }
}

fn approved(line: &str) -> bool {
    S24_READS.contains(&format!("shell {line}").as_str())
}

#[test]
fn doctor_on_the_s24_sends_only_approved_reads() {
    // a phone that could answer every read, but whose hardware serial is the protected phone's
    for configured in [DeviceMode::Full, DeviceMode::Helper, DeviceMode::ReadOnly] {
        // through the guard (production) ...
        let mut inner = Recorded::new(ALL_GOOD);
        inner.serial = Some(S24_SERIAL.to_string());
        let guarded = GuardedRunner::new(inner);
        let r = doctor::doctor(&guarded, IP, configured, &q()).unwrap();
        assert_eq!(r.mode, DeviceMode::ReadOnly);
        assert!(r.findings.is_empty() && !r.unchecked.is_empty());
        assert!(r.sent.iter().all(|l| approved(l)), "{:?}", r.sent);
        assert_eq!(r.sent.len(), 11, "the 11 approved reads, each once");
        // ... and without it: doctor itself sends nothing else
        let mut bare = Recorded::new(ALL_GOOD);
        bare.serial = Some(S24_SERIAL.to_string());
        doctor::doctor(&bare, IP, configured, &q()).unwrap();
        let lines = bare.lines();
        assert!(lines.iter().all(|l| approved(l)), "{lines:?}");
        assert_eq!(lines.len(), 11);
    }
    // spelt as the protected phone: the same
    let bare = Recorded::new(ALL_GOOD);
    doctor::doctor(
        &bare,
        &format!("adb-{S24_SERIAL}-x._adb-tls-connect._tcp"),
        DeviceMode::Full,
        &q(),
    )
    .unwrap();
    assert!(bare.lines().iter().all(|l| approved(l)));
}

// ---------------------------------------------------------------- setup

#[test]
fn dry_run_leaves_config_hashes_unchanged() {
    let t = seeded("dryrun");
    let before = tree_hash(&t.0);
    let phone = Rc::new(FakePhone::new(FRESH));
    let state_before = phone.snapshot();
    let guarded = GuardedRunner::new(Shared(phone.clone()));
    let mut o = opts(&t.0);
    o.dry_run = true;
    let out = setup::setup(&guarded, &q(), &mut Script::new(&[]), &o).unwrap();
    assert!(!out.applied);
    assert_eq!(out.hash_before, before);
    assert_eq!(out.hash_after, before, "the outcome's own hashes");
    assert_eq!(
        tree_hash(&t.0),
        before,
        "config hash unchanged after --dry-run"
    );
    // it did plan real work: two files and the phone fixes
    assert!(out.plan.len() >= 8, "{:#?}", out.plan);
    assert!(out.plan.iter().any(|s| matches!(s, setup::Step::Device(_))));
    // and wrote nothing to the phone
    assert_eq!(phone.writes(), Vec::<String>::new());
    assert_eq!(phone.snapshot(), state_before);
    // a config dir that does not exist yet stays absent
    let fresh = TempDir::new("dryrun-absent");
    let absent = tree_hash(&fresh.0);
    let mut o = opts(&fresh.0);
    o.dry_run = true;
    o.yes = true;
    setup::setup(
        &GuardedRunner::new(FakePhone::new(FRESH)),
        &q(),
        &mut Script::new(&[]),
        &o,
    )
    .unwrap();
    assert!(!fresh.0.exists());
    assert_eq!(tree_hash(&fresh.0), absent);
}

#[test]
fn dry_run_sends_no_write_to_the_phone() {
    let t = seeded("dryrun-writes");
    let phone = Rc::new(FakePhone::new(FRESH));
    let guarded = GuardedRunner::new(Shared(phone.clone()));
    let mut o = opts(&t.0);
    o.dry_run = true;
    setup::setup(&guarded, &q(), &mut Script::new(&[]), &o).unwrap();
    assert_eq!(phone.writes(), Vec::<String>::new());
    assert!(!t.0.join(journal::UNDO_DIR).exists(), "no journal either");
}

fn devices_toml(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("devices.toml")).unwrap()
}

#[test]
fn setup_then_undo_is_byte_identical() {
    for seeded_dir in [true, false] {
        let t = if seeded_dir {
            seeded("undo")
        } else {
            TempDir::new("undo-absent")
        };
        let hash0 = tree_hash(&t.0);
        let toml0 = std::fs::read(t.0.join("devices.toml")).ok();
        let phone = Rc::new(FakePhone::new(FRESH));
        let state0 = phone.snapshot();
        let guarded = GuardedRunner::new(Shared(phone.clone()));
        // mode, name, confirm, human steps (skipped): 4 prompts
        let mut human = Script::new(&["", "", "y", "skip"]);
        let out = setup::setup(&guarded, &q(), &mut human, &opts(&t.0)).unwrap();
        assert!(out.applied, "{}", out.render(&t.0));
        assert_eq!(out.prompts, 4);
        assert_eq!(out.run.as_deref(), Some("0001"));
        // it really changed things
        assert_ne!(tree_hash(&t.0), hash0);
        assert_ne!(phone.snapshot(), state0);
        let toml = devices_toml(&t.0);
        assert!(
            toml.contains(
                "[devices.\"PLACEHOLDER-POCO-SERIAL\"]\nname = \"POCO X6 Pro\"\nmode = \"full\""
            ),
            "{toml}"
        );
        if seeded_dir {
            assert!(
                toml.contains("PLACEHOLDER-F5-SERIAL") && toml.contains("# keep me"),
                "the other phone is kept"
            );
        }
        let profile =
            std::fs::read_to_string(t.0.join("profiles").join("PLACEHOLDER-POCO-SERIAL.json"))
                .unwrap();
        assert!(profile.contains("\"skin\": \"HyperOS\""));
        // read back: only the human-only step is left (it was skipped)
        let left: Vec<&str> = out.remaining.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(left, ["hyperos_no_restrictions"], "{}", out.render(&t.0));
        // Artemis was kept
        let st = phone.snapshot();
        assert_eq!(
            st.settings["secure"]["enabled_accessibility_services"],
            format!("com.example.synthetic.artemis/com.example.synthetic.artemis.ArtemisService:{HELPER_SERVICE}")
        );

        let rep = journal::undo(&t.0, Some(&guarded), None).unwrap();
        assert_eq!(
            tree_hash(&t.0),
            hash0,
            "config tree byte-identical after --undo"
        );
        assert_eq!(
            std::fs::read(t.0.join("devices.toml")).ok(),
            toml0,
            "devices.toml byte for byte"
        );
        assert_eq!(
            phone.snapshot(),
            state0,
            "every phone key back, absent ones deleted"
        );
        assert_eq!(rep.device_reverted, 9, "{rep:?}");
        assert_eq!(t.0.exists(), seeded_dir);
        // nothing left to undo
        assert!(journal::undo(&t.0, Some(&guarded), None).is_err());
    }
}

#[test]
fn undo_refuses_a_file_changed_since_setup() {
    let t = seeded("undo-conflict");
    let phone = Rc::new(FakePhone::new(FRESH));
    let guarded = GuardedRunner::new(Shared(phone.clone()));
    let mut o = opts(&t.0);
    o.yes = true;
    setup::setup(&guarded, &q(), &mut Script::new(&[]), &o).unwrap();
    std::fs::write(t.0.join("devices.toml"), "# edited by hand after setup\n").unwrap();
    let hash = tree_hash(&t.0);
    let state = phone.snapshot();
    let e = journal::undo(&t.0, Some(&guarded), None).unwrap_err();
    assert!(e.0.contains("changed since setup"), "{e}");
    assert_eq!(tree_hash(&t.0), hash, "nothing touched");
    assert_eq!(phone.snapshot(), state, "phone untouched too");
}

#[test]
fn undo_refuses_another_phone() {
    let t = seeded("undo-other");
    let phone = Rc::new(FakePhone::new(FRESH));
    let guarded = GuardedRunner::new(Shared(phone.clone()));
    let mut o = opts(&t.0);
    o.yes = true;
    setup::setup(&guarded, &q(), &mut Script::new(&[]), &o).unwrap();
    let other = Rc::new(FakePhone::new(
        &FRESH.replace("PLACEHOLDER-POCO-SERIAL", "PLACEHOLDER-OTHER-SERIAL"),
    ));
    let e =
        journal::undo(&t.0, Some(&GuardedRunner::new(Shared(other.clone()))), None).unwrap_err();
    assert!(e.0.contains("not touching anything"), "{e}");
    assert!(other.writes().is_empty());
}

#[test]
fn setup_asks_at_most_six_prompts() {
    // the worst case: two phones and no --device, no --mode/--name, a human-only step, and an
    // accessibility service that does not stay on (Android 13+ restricted settings)
    let phone = Rc::new(FakePhone::new(FRESH));
    let mut ph = Rc::try_unwrap(phone).ok().unwrap();
    ph.sections.insert(
        "devices -l".into(),
        format!("List of devices attached\n{IP}  device product:x model:POCO_X6_Pro device:duchamp transport_id:3\nPLACEHOLDER-F5-SERIAL  device usb:1-1 product:y model:F5 device:z transport_id:4\n"),
    );
    ph.drop_keys
        .borrow_mut()
        .insert("enabled_accessibility_services".into());
    let phone = Rc::new(ph);
    let guarded = GuardedRunner::new(Shared(phone.clone()));
    let t = TempDir::new("prompts");
    let mut human = Script::new(&["1", "", "", "y", "done", "retry"]);
    let o = Options {
        config_dir: t.0.clone(),
        ..Default::default()
    };
    let out = setup::setup(&guarded, &q(), &mut human, &o).unwrap();
    let ids: Vec<&str> = human.asked.iter().map(|q| q.id).collect();
    assert_eq!(
        ids,
        [
            "device",
            "mode",
            "name",
            "confirm",
            "human_steps",
            "restricted_settings"
        ]
    );
    assert_eq!(out.prompts, 6);
    assert!(out.prompts <= MAX_PROMPTS);
    assert!(
        out.remaining.iter().any(|f| f.id == ACCESSIBILITY_OFF),
        "the phone kept dropping it"
    );
    // --yes: no prompt at all, the human-only step is listed instead
    let t2 = TempDir::new("prompts-yes");
    let mut silent = Script::new(&[]);
    let o = Options {
        config_dir: t2.0.clone(),
        target: Some(IP.into()),
        yes: true,
        ..Default::default()
    };
    let out = setup::setup(
        &GuardedRunner::new(FakePhone::new(FRESH)),
        &q(),
        &mut silent,
        &o,
    )
    .unwrap();
    assert!(silent.asked.is_empty());
    assert_eq!(out.prompts, 0);
    assert!(out.notes.iter().any(|n| n.contains("No restrictions")));
}

#[test]
fn the_prompt_budget_refuses_a_seventh() {
    let mut s = Script::new(&[]);
    let mut b = Budget {
        inner: &mut s,
        asked: 0,
    };
    for _ in 0..MAX_PROMPTS {
        b.ask("x", "q".into(), "d").unwrap();
    }
    assert_eq!(
        b.ask("seventh", "q".into(), "d"),
        Err(SetupError::TooManyPrompts("seventh"))
    );
    assert_eq!(s.asked.len(), MAX_PROMPTS);
}

#[test]
fn the_s24_is_refused_by_setup() {
    // spelt as the protected phone: refused before any command
    for spelling in [
        S24_SERIAL.to_string(),
        S24_SERIAL.to_lowercase(),
        format!("adb-{S24_SERIAL}-abc._adb-tls-connect._tcp"),
    ] {
        let t = TempDir::new("s24-name");
        let fake = Recorded::new(ALL_GOOD);
        let o = Options {
            config_dir: t.0.clone(),
            target: Some(spelling.clone()),
            yes: true,
            ..Default::default()
        };
        let e = setup::setup(&fake, &q(), &mut Script::new(&[]), &o).unwrap_err();
        assert!(matches!(e, SetupError::Refused(_)), "{spelling}: {e}");
        assert!(fake.calls.borrow().is_empty(), "{spelling}: nothing sent");
        assert!(!t.0.exists());
    }
    // behind an IP, found by hardware serial: only the approved serial read went out
    for guarded in [true, false] {
        let t = seeded("s24-ip");
        let hash = tree_hash(&t.0);
        let mut inner = Recorded::new(ALL_GOOD);
        inner.serial = Some(S24_SERIAL.to_string());
        let inner = Rc::new(inner);
        struct R(Rc<Recorded>);
        impl agentroid_adb_link::runner::Runner for R {
            fn run(
                &self,
                a: &[String],
                t: u64,
            ) -> Result<agentroid_adb_link::runner::RunOutput, agentroid_adb_link::runner::RunError>
            {
                self.0.run(a, t)
            }
        }
        let mut human = Script::new(&["", "", "y", "done", "retry"]);
        let e = if guarded {
            setup::setup(
                &GuardedRunner::new(R(inner.clone())),
                &q(),
                &mut human,
                &opts(&t.0),
            )
            .unwrap_err()
        } else {
            setup::setup(&R(inner.clone()), &q(), &mut human, &opts(&t.0)).unwrap_err()
        };
        assert!(
            matches!(e, SetupError::Refused(ref m) if m.contains("S24") || m.contains("protected phone")),
            "{e}"
        );
        assert_eq!(
            inner.lines(),
            ["getprop ro.serialno"],
            "only the approved serial read"
        );
        assert!(human.asked.is_empty(), "not even a question");
        assert_eq!(tree_hash(&t.0), hash);
    }
    // when it is the only phone listed, it is not offered at all
    let mut lone = Recorded::new(ALL_GOOD);
    lone.sections.insert(
        "devices -l".into(),
        format!(
            "List of devices attached\n{S24_SERIAL}  device usb:1-1 model:SM_X transport_id:9\n"
        ),
    );
    let t = TempDir::new("s24-list");
    let o = Options {
        config_dir: t.0.clone(),
        yes: true,
        ..Default::default()
    };
    let e = setup::setup(&lone, &q(), &mut Script::new(&[]), &o).unwrap_err();
    assert!(matches!(e, SetupError::NoDevice(_)), "{e}");
    assert_eq!(lone.lines(), ["devices -l"]);
}

#[test]
fn read_only_setup_changes_no_phone_setting() {
    let t = TempDir::new("readonly");
    let phone = Rc::new(FakePhone::new(FRESH));
    let guarded = GuardedRunner::new(Shared(phone.clone()));
    let o = Options {
        config_dir: t.0.clone(),
        target: Some(IP.into()),
        mode: Some(DeviceMode::ReadOnly),
        yes: true,
        ..Default::default()
    };
    let out = setup::setup(&guarded, &q(), &mut Script::new(&[]), &o).unwrap();
    assert!(out.applied);
    assert!(phone.writes().is_empty());
    let read_lines: Vec<String> = phone
        .lines()
        .into_iter()
        .filter(|l| l != "getprop ro.serialno")
        .collect();
    assert!(
        read_lines.iter().all(|l| approved(l)),
        "a read_only device gets the approved subset only: {read_lines:?}"
    );
    assert!(t
        .0
        .join("profiles")
        .join("local")
        .join("PLACEHOLDER-POCO-SERIAL.json")
        .is_file());
    assert!(devices_toml(&t.0).contains("mode = \"read_only\""));
    journal::undo(&t.0, Some(&guarded), None).unwrap();
    assert!(!t.0.exists());
}

#[test]
fn every_write_is_journalled_before_it_is_made() {
    let t = TempDir::new("journal");
    let phone = Rc::new(FakePhone::new(FRESH));
    let guarded = GuardedRunner::new(Shared(phone.clone()));
    let o = Options {
        config_dir: t.0.clone(),
        target: Some(IP.into()),
        yes: true,
        ..Default::default()
    };
    setup::setup(&guarded, &q(), &mut Script::new(&[]), &o).unwrap();
    let j: journal::Journal = serde_json::from_str(
        &std::fs::read_to_string(t.0.join("undo").join("0001").join("journal.json")).unwrap(),
    )
    .unwrap();
    let device_entries = j.entries.iter().filter(|e| e.is_device()).count();
    assert_eq!(
        device_entries,
        phone.writes().len(),
        "one entry per phone write"
    );
    let files = j
        .entries
        .iter()
        .filter(|e| {
            matches!(
                e,
                journal::Entry::FileCreated { .. } | journal::Entry::FileModified { .. }
            )
        })
        .count();
    assert_eq!(files, 2, "devices.toml and the profile");
    assert!(j.entries.iter().any(|e| matches!(e, journal::Entry::Setting { key, before: None, .. } if key == "accessibility_enabled")));
    assert!(j
        .entries
        .iter()
        .any(|e| matches!(e, journal::Entry::AppOp { op, before: None, .. } if op == "10007")));
}

#[test]
fn devices_toml_replaces_only_its_own_section() {
    let old = "# head\n[devices.\"A\"]\nname = \"a\"\n\n[devices.\"B\"]\nname = \"old\"\n\n[devices.\"C\"]\nname = \"c\"\n";
    let new = setup::devices_toml(Some(old), "B", "[devices.\"B\"]\nname = \"new\"\n").unwrap();
    assert_eq!(new, "# head\n[devices.\"A\"]\nname = \"a\"\n\n[devices.\"C\"]\nname = \"c\"\n\n[devices.\"B\"]\nname = \"new\"\n");
    assert!(
        setup::devices_toml(Some("not = [toml"), "B", "x").is_err(),
        "an unreadable file is never edited"
    );
}

// ---------------------------------------------------------------- fixtures

fn has_mac(text: &str) -> bool {
    text.as_bytes().windows(17).any(|w| {
        let sep = w[2];
        (sep == b':' || sep == b'-')
            && w.iter().enumerate().all(|(i, c)| {
                if i % 3 == 2 {
                    *c == sep
                } else {
                    c.is_ascii_hexdigit()
                }
            })
    })
}

#[test]
fn fixtures_hold_no_real_identifier() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("doctor");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# SYNTHETIC"), "{}", p.display());
        assert!(
            !text.to_ascii_uppercase().contains(S24_SERIAL),
            "{}",
            p.display()
        );
        assert!(!has_mac(&text), "{}", p.display());
        assert!(
            !text.to_ascii_lowercase().contains("android_id"),
            "{}",
            p.display()
        );
        let sections = crate::testkit::parse_fixture(&text);
        let serial = sections["getprop ro.serialno"].trim().to_string();
        assert!(
            serial.starts_with("PLACEHOLDER-"),
            "{}: {serial}",
            p.display()
        );
        n += 1;
    }
    assert_eq!(n, 14, "all-good, 9 switches, 3 owners, fresh-poco");
    let _ = line_of(&[]);
}
