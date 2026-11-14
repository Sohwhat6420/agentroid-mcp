//! The protected-device lease, core and approver together (`docs/design/protected-device-lease.md`).
//! Every test names the review item it holds to; nothing here reaches a phone or adb.

use agentroid_adb_link::guard::S24_SERIAL;
use agentroid_adb_link::lease::{Granted, Holder, LeaseBook, ManualClock};
use agentroid_core::approval::{Approver, Channel, Inbound, Request, Style};
use agentroid_core::fake::FakeClock;
use agentroid_core::fileguard::FileOp;
use agentroid_core::frame::Shot;
use agentroid_core::gate::{Call, DeviceMode, Refusal, Target};
use agentroid_core::protected::{sign_presence, PresenceConfig, Protected, ProtectedConfig};
use agentroid_core::settle::SettleOpts;
use agentroid_core::transport::{
    Backend, Core, CoreError, DeviceInfo, Input, Screenshot, Transport, TransportError,
};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const PROT: &str = "192.0.2.60:5555";
const POCO: &str = "192.0.2.50:37123";
const T0: u64 = 1_800_000_000_000;
const MIN: u64 = 60_000;
const KEY: &str = "presence-key-0123456789abcdef";

// ---------------------------------------------------------------------------------------------
// a scripted phone pair
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Phone {
    /// Trees served in turn; the last one repeats.
    screens: VecDeque<String>,
    calls: Vec<String>,
    tree_fails: bool,
}

impl Phone {
    fn show(&mut self, xml: String) {
        self.screens = VecDeque::from([xml]);
    }
    fn then(&mut self, xml: String) {
        self.screens.push_back(xml);
    }
    /// What reached the protected phone besides reads of its screen.
    fn sent_to_protected(&self) -> Vec<&String> {
        self.calls
            .iter()
            .filter(|c| {
                c.contains(PROT)
                    && (c.starts_with("input")
                        || c.starts_with("shell")
                        || c.starts_with("push")
                        || c.starts_with("pull"))
            })
            .collect()
    }
    fn shots(&self) -> usize {
        self.calls
            .iter()
            .filter(|c| c.starts_with("screenshot"))
            .count()
    }
}

impl Transport for Phone {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        Ok(vec![
            DeviceInfo {
                serial: POCO.into(),
                model: "poco".into(),
                hw_serial: agentroid_core::transport::POCO_SERIAL.into(),
            },
            DeviceInfo {
                serial: PROT.into(),
                model: "protected".into(),
                hw_serial: S24_SERIAL.into(),
            },
        ])
    }
    fn screen_size(&mut self, _: &str) -> Result<(i32, i32), TransportError> {
        Ok((1080, 2400))
    }
    fn screen_state(&mut self, _: &str) -> Result<String, TransportError> {
        Ok("Awake".into())
    }
    fn screenshot(&mut self, serial: &str, _: i32) -> Result<Screenshot, TransportError> {
        self.calls.push(format!("screenshot {serial}"));
        Ok(Screenshot {
            jpeg: vec![0xff; 16],
            shot: Shot {
                dev_w: 1080,
                dev_h: 2400,
                img_w: 1080,
                img_h: 2400,
            },
        })
    }
    fn tree_xml(&mut self, serial: &str, _: Backend) -> Result<String, TransportError> {
        self.calls.push(format!("tree {serial}"));
        if self.tree_fails {
            return Err(TransportError("uiautomator dump failed".into()));
        }
        let x = if self.screens.len() > 1 {
            self.screens.pop_front()
        } else {
            self.screens.front().cloned()
        };
        x.ok_or_else(|| TransportError("no screen".into()))
    }
    fn input(&mut self, serial: &str, input: &Input) -> Result<(), TransportError> {
        self.calls.push(format!("input {serial} {input:?}"));
        Ok(())
    }
    fn packages(&mut self, _: &str) -> Result<Vec<String>, TransportError> {
        Ok(vec![
            "com.android.settings".into(),
            "com.whatsapp".into(),
            "com.google.android.gm.lite".into(),
        ])
    }
    fn shell(&mut self, serial: &str, line: &str) -> Result<String, TransportError> {
        self.calls.push(format!("shell {serial} {line}"));
        Ok("Events injected: 1\n".into())
    }
    fn helper_status(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// screens
// ---------------------------------------------------------------------------------------------

fn n(pkg: &str, rid: &str, text: &str, desc: &str, b: &str) -> String {
    format!("<node index=\"0\" text=\"{text}\" resource-id=\"{rid}\" class=\"android.widget.TextView\" package=\"{pkg}\" content-desc=\"{desc}\" clickable=\"true\" bounds=\"{b}\" />")
}

fn page(nodes: &[String]) -> String {
    format!(
        "<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\">{}{}</hierarchy>",
        n("com.android.systemui", "com.android.systemui:id/clock", "12:00", "", "[0,0][200,60]"),
        nodes.join("")
    )
}

fn home() -> String {
    page(&[
        n(
            "com.example.launcher",
            "",
            "Settings",
            "",
            "[0,200][200,400]",
        ),
        n(
            "com.example.launcher",
            "",
            "WhatsApp",
            "",
            "[0,1000][200,1200]",
        ),
    ])
}

fn settings() -> String {
    page(&[
        n(
            "com.android.settings",
            "android:id/title",
            "Wi-Fi",
            "",
            "[0,200][1080,300]",
        ),
        n(
            "com.android.settings",
            "com.android.settings:id/delete",
            "Delete",
            "",
            "[0,500][1080,600]",
        ),
    ])
}

fn keyguard() -> String {
    page(&[n(
        "com.android.systemui",
        "com.android.systemui:id/keyguard_status_view",
        "Fri 14 Nov",
        "",
        "[0,200][1080,600]",
    )])
}

fn shade() -> String {
    page(&[n(
        "com.android.systemui",
        "com.android.systemui:id/expandableNotificationRow",
        "Dinner at 8?",
        "",
        "[0,200][1080,400]",
    )])
}

fn recents() -> String {
    page(&[n(
        "com.example.launcher",
        "com.example.launcher:id/task_thumbnail",
        "",
        "",
        "[0,200][1080,1800]",
    )])
}

fn gmail_go() -> String {
    page(&[n(
        "com.google.android.gm.lite",
        "",
        "Inbox",
        "",
        "[0,200][1080,300]",
    )])
}

fn pin_prompt() -> String {
    page(&[
        n(
            "com.example.bank",
            "com.example.bank:id/pinEntry",
            "",
            "",
            "[0,200][1080,300]",
        ),
        n(
            "com.example.bank",
            "com.example.bank:id/key1",
            "1",
            "",
            "[0,1000][300,1200]",
        ),
    ])
}

fn at(x: f64, y: f64) -> Call {
    Call::Tap {
        target: Target::Point {
            x,
            y,
            space: Some("device".into()),
            image_width: None,
        },
    }
}

fn ui_tree() -> Call {
    Call::UiTree {
        interactive_only: false,
        require: vec![],
        timeout_ms: None,
    }
}

// ---------------------------------------------------------------------------------------------
// the rig
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Inbox {
    sent: Vec<Request>,
    events: Vec<Inbound>,
    poll_fails: bool,
    send_fails: bool,
    next_event: u32,
}

struct Shared(Arc<Mutex<Inbox>>);

impl Channel for Shared {
    fn name(&self) -> &'static str {
        "test"
    }
    fn send(&mut self, req: &Request) -> Result<(), String> {
        let mut g = self.0.lock().unwrap();
        if g.send_fails {
            return Err("push server unreachable".into());
        }
        g.sent.push(req.clone());
        Ok(())
    }
    fn poll(&mut self, _: &Request) -> Result<Vec<Inbound>, String> {
        let g = self.0.lock().unwrap();
        if g.poll_fails {
            return Err("poll failed".into());
        }
        Ok(g.events.clone())
    }
}

struct Rig {
    core: Core<Phone, FakeClock>,
    wall: Arc<ManualClock>,
    inbox: Arc<Mutex<Inbox>>,
    holder: Arc<Mutex<Option<Holder>>>,
    book: Arc<LeaseBook>,
    presence_file: PathBuf,
    dir: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(name: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("agentroid-plp-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let key_file = dir.join("presence.key");
    std::fs::write(&key_file, KEY).unwrap();
    let presence_file = dir.join("presence.json");
    let wall = Arc::new(ManualClock::new(T0));
    let book = Arc::new(LeaseBook::new(wall.clone()));
    let cfg = ProtectedConfig::new(
        30,
        10,
        Some(PresenceConfig::new(&presence_file, &key_file, 15)),
    );
    let mut prot = Protected::new(cfg, book.clone());
    let holder = Arc::new(Mutex::new(Some(Holder {
        name: "lane-a".into(),
        epoch: 1,
    })));
    let h = holder.clone();
    prot.set_holders(Box::new(move |_| h.lock().unwrap().clone()));
    let inbox = Arc::new(Mutex::new(Inbox::default()));
    let mut core = Core::new(Phone::default(), FakeClock::new());
    core.set_approver(Approver::new(Box::new(Shared(inbox.clone()))));
    core.set_protected(prot);
    core.transport.show(home());
    Rig {
        core,
        wall,
        inbox,
        holder,
        book,
        presence_file,
        dir,
    }
}

impl Rig {
    fn call(&mut self, c: &Call) -> Result<String, CoreError> {
        self.core.call(Some(PROT), c).map(|o| o.text)
    }
    fn sent(&self) -> Vec<Request> {
        self.inbox.lock().unwrap().sent.clone()
    }
    fn answer(&self, verb: &str, id: &str, at_ms: u64) {
        let mut g = self.inbox.lock().unwrap();
        g.next_event += 1;
        let event_id = format!("ev{}", g.next_event);
        g.events.push(Inbound {
            event_id,
            time_ms: at_ms,
            message: format!("{verb} {id}"),
        });
    }
    fn now(&self) -> u64 {
        self.book.now_ms().unwrap()
    }
    fn presence(&self, state: &str, written_at: u64) {
        std::fs::write(
            &self.presence_file,
            sign_presence(KEY.as_bytes(), state, written_at),
        )
        .unwrap();
    }
    /// Ask, then Allow: an open lease for the current holder.
    fn open_lease(&mut self) -> Request {
        let r = self.call(&at(100.0, 300.0));
        assert!(
            matches!(r, Err(CoreError::Refused(Refusal::LeasePending { .. }))),
            "{r:?}"
        );
        let ask = self.sent().last().cloned().unwrap();
        self.answer("allow", &ask.id, self.now());
        self.call(&at(100.0, 300.0))
            .expect("the lease opens on Allow");
        ask
    }
}

fn refused(r: Result<String, CoreError>) -> Refusal {
    match r {
        Err(CoreError::Refused(x)) => x,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn protected_rule(r: Result<String, CoreError>) -> &'static str {
    match refused(r) {
        Refusal::Protected { rule, .. } => rule,
        other => panic!("expected a protected refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// one Allow, a burst, both leases
// ---------------------------------------------------------------------------------------------

#[test]
fn one_allow_opens_a_lease_for_a_burst_and_nothing_is_sent_before_it() {
    let mut r = rig("burst");
    let first = refused(r.call(&at(100.0, 300.0)));
    let Refusal::LeasePending { reference, .. } = &first else {
        panic!("{first:?}")
    };
    let ask = r.sent()[0].clone();
    assert_eq!(ask.style, Style::Lease);
    assert_eq!(ask.expires_ms - ask.created_ms, 10 * MIN, "the ask window");
    assert!(ask.reason.contains("30-minute lease"), "{}", ask.reason);
    // the agent sees a short reference, never the id it could answer with
    assert_eq!(reference.len(), 6);
    assert!(!first.to_string().contains(&ask.id));
    assert!(r.core.transport.sent_to_protected().is_empty());
    // retrying while the ask is open sends no second push
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    assert_eq!(r.sent().len(), 1);
    r.answer("allow", &ask.id, r.now());
    r.call(&at(100.0, 300.0)).unwrap();
    for _ in 0..5 {
        r.wall.advance(MIN);
        r.call(&at(100.0, 300.0)).unwrap();
        r.call(&ui_tree()).unwrap();
    }
    assert_eq!(r.sent().len(), 1, "one Allow covers the burst");
    assert_eq!(r.core.transport.sent_to_protected().len(), 6);
    // the adb guard lets through only the name core bound
    assert!(r.book.admits(PROT));
    assert!(!r.book.admits("192.0.2.61:5555"));
    // the lease ends on time, and the next call asks again
    r.wall.advance(30 * MIN);
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    assert_eq!(r.sent().len(), 2);
}

#[test]
fn the_protected_lease_also_satisfies_the_screen_lock_so_nothing_is_allowed_twice() {
    use agentroid_core::askscreen::Signals;
    use agentroid_core::screenlock::{Reading, ScreenLock};
    let mut r = rig("lock");
    // the phone is in use (focused app, keyboard up, touched a second ago): the screen lock on its
    // own refuses every acting call without an ask-screen lease
    let busy = Signals {
        wakefulness: Some("Awake".into()),
        idle_ms: Some(1_000),
        screen_kept_on: Some(false),
        focus: Some("com.example.notes".into()),
        ime_shown: Some(true),
        keyguard: Some(false),
        in_call: Some(false),
    };
    let lock_dir = r.dir.join("lock");
    let wall = r.wall.clone();
    r.core.set_screen_lock(ScreenLock::new(
        lock_dir,
        Box::new(move |_, mono| {
            let t = mono();
            Some(Reading {
                signals: busy.clone(),
                power_from: t,
                power_to: t,
            })
        }),
        Box::new(move || agentroid_adb_link::lease::WallClock::now_ms(&*wall).unwrap_or(0)),
        Box::new(|| 5_000_000),
    ));
    // the POCO (no protected lease) is held off by the lock
    r.core.set_allow(&[agentroid_core::transport::POCO_SERIAL]);
    assert!(matches!(
        r.core.call(Some(POCO), &at(10.0, 10.0)),
        Err(CoreError::Refused(Refusal::ScreenLock { .. }))
    ));
    // the protected phone: one Allow, and the lock does not ask again
    r.open_lease();
    r.call(&at(100.0, 300.0)).unwrap();
    assert_eq!(r.sent().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// blocker 1: notification previews, keyguard, shade, Recents
// ---------------------------------------------------------------------------------------------

#[test]
fn the_keyguard_the_shade_and_recents_refuse_reads_and_acts_under_a_lease() {
    let mut r = rig("previews");
    r.open_lease();
    let before = r.core.transport.sent_to_protected().len();
    for (name, xml) in [
        ("keyguard", keyguard()),
        ("shade", shade()),
        ("recents", recents()),
    ] {
        r.core.transport.show(xml);
        let shots = r.core.transport.shots();
        for c in [
            ui_tree(),
            Call::Screenshot { max_width: 720 },
            Call::Look { max_width: 720 },
            at(100.0, 300.0),
            Call::Key {
                name: "recents".into(),
            },
        ] {
            assert_eq!(protected_rule(r.call(&c)), "preview", "{name} {c:?}");
        }
        assert_eq!(r.core.transport.shots(), shots, "{name}: no shot was taken");
        // the way out still works
        r.call(&Call::Key {
            name: "back".into(),
        })
        .unwrap();
    }
    assert_eq!(
        r.core.transport.sent_to_protected().len(),
        before + 3,
        "only the three back keys"
    );
    // a status-bar icon naming a protected app is a preview too
    r.core.transport.show(page(&[n(
        "com.android.systemui",
        "com.android.systemui:id/icon",
        "",
        "WhatsApp notification: Dinner at 8?",
        "[0,0][40,40]",
    )]));
    assert_eq!(protected_rule(r.call(&ui_tree())), "preview");
}

#[test]
fn a_protected_app_on_screen_or_under_the_finger_is_never_touched() {
    let mut r = rig("apps");
    r.open_lease();
    // the launcher's WhatsApp icon
    assert_eq!(protected_rule(r.call(&at(100.0, 1100.0))), "app");
    // Gmail Go on screen: reads refused too
    r.core.transport.show(gmail_go());
    assert_eq!(protected_rule(r.call(&ui_tree())), "app");
    assert_eq!(protected_rule(r.call(&at(100.0, 250.0))), "app");
    // launches, refused before any ask
    r.core.transport.show(home());
    for c in [
        Call::OpenApp {
            package: "whatsapp".into(),
        },
        Call::OpenUrl {
            uri: "https://wa.me/15550000".into(),
            package: None,
        },
        Call::StartActivity {
            component: "com.google.android.gm.lite/.Inbox".into(),
            action: None,
            data: None,
        },
    ] {
        assert_eq!(protected_rule(r.call(&c)), "app", "{c:?}");
    }
    let mut fresh = rig("apps-nolease");
    assert_eq!(
        protected_rule(fresh.call(&Call::OpenApp {
            package: "com.whatsapp".into()
        })),
        "app"
    );
    assert!(
        fresh.sent().is_empty(),
        "no ask for a launch refused anyway"
    );
}

// ---------------------------------------------------------------------------------------------
// blocker 2: settle checks every poll
// ---------------------------------------------------------------------------------------------

#[test]
fn settle_checks_the_protected_screen_on_every_poll() {
    let mut r = rig("settle");
    r.open_lease();
    // the first poll is ordinary, the second shows the shade
    r.core.transport.show(home());
    r.core.transport.then(shade());
    let e = r
        .core
        .settle(Some(PROT), SettleOpts::default())
        .unwrap_err();
    assert!(
        matches!(
            e,
            CoreError::Refused(Refusal::Protected {
                rule: "preview",
                ..
            })
        ),
        "{e:?}"
    );
    // a stable, ordinary screen settles
    r.core.transport.show(home());
    assert!(r.core.settle(Some(PROT), SettleOpts::default()).is_ok());
    // the first poll already refused
    r.core.transport.show(keyguard());
    assert!(r.core.settle(Some(PROT), SettleOpts::default()).is_err());
}

// ---------------------------------------------------------------------------------------------
// blocker 3: file paths
// ---------------------------------------------------------------------------------------------

#[test]
fn pulls_and_pushes_naming_a_protected_app_are_refused_in_any_spelling() {
    let mut r = rig("paths");
    for p in [
        "/sdcard/WhatsApp/Media/a.jpg",
        "/SDCARD/Android/media/COM.WHATSAPP/a",
        "/sdcard//Android/data/com.google.android.gm/files/a",
        "/sdcard/./Download/../Gmail/a",
        "\\sdcard\\WhatsApp\\a",
    ] {
        for op in [FileOp::Pull, FileOp::Push] {
            let e = r.core.file_gate(Some(PROT), op, p).unwrap_err();
            assert!(
                matches!(
                    e,
                    CoreError::Refused(Refusal::Protected { rule: "path", .. })
                ),
                "{p}: {e:?}"
            );
        }
    }
    assert!(r.sent().is_empty(), "no ask for a refused path");
    // an ordinary path asks for the lease, then passes the file guard
    assert!(matches!(
        r.core
            .file_gate(Some(PROT), FileOp::Pull, "/sdcard/DCIM/Camera/a.jpg"),
        Err(CoreError::Refused(Refusal::LeasePending { .. }))
    ));
    let ask = r.sent()[0].clone();
    r.answer("allow", &ask.id, r.now());
    r.core
        .file_gate(Some(PROT), FileOp::Pull, "/sdcard/DCIM/Camera/a.jpg")
        .unwrap();
    // raw shell lines never run on it, lease or not
    assert!(matches!(
        r.core.shell_gate(Some(PROT), "dumpsys notification"),
        Err(CoreError::Refused(Refusal::Protected { rule: "shell", .. }))
    ));
    // nor do link segments
    assert!(matches!(
        r.core.lock_admit(Some(PROT), "link.segment"),
        Err(CoreError::Refused(Refusal::Protected { rule: "link", .. }))
    ));
}

// ---------------------------------------------------------------------------------------------
// blocker 4: holder and job
// ---------------------------------------------------------------------------------------------

#[test]
fn the_lease_and_the_pending_ask_belong_to_one_holder_and_job() {
    let mut r = rig("holder");
    r.core
        .protected_mut()
        .unwrap()
        .set_job(1, Some("run-7".into()));
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    let ask_a = r.sent()[0].clone();
    assert!(
        ask_a.reason.contains("lane-a, job run-7"),
        "{}",
        ask_a.reason
    );
    // another holder takes the phone before the owner answers: A's ask is not B's
    *r.holder.lock().unwrap() = Some(Holder {
        name: "lane-b".into(),
        epoch: 2,
    });
    r.answer("allow", &ask_a.id, r.now());
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    assert_eq!(r.sent().len(), 2, "a fresh ask for lane-b");
    assert!(
        r.book.active().is_none(),
        "lane-a's Allow opened nothing for lane-b"
    );
    let ask_b = r.sent()[1].clone();
    r.answer("allow", &ask_b.id, r.now());
    r.call(&at(100.0, 300.0)).unwrap();
    let lease = r.book.active().unwrap();
    assert_eq!(lease.holder.epoch, 2);
    // the job running under lane-b's lease (same epoch) is admitted and named
    r.core
        .protected_mut()
        .unwrap()
        .set_job(2, Some("run-9".into()));
    r.call(&at(100.0, 300.0)).unwrap();
    assert_eq!(r.book.active().unwrap().job.as_deref(), Some("run-9"));
    // lane-a, back again with a new grant, never inherits it
    *r.holder.lock().unwrap() = Some(Holder {
        name: "lane-a".into(),
        epoch: 3,
    });
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    assert!(r.book.active().is_none());
    // the holder's connection goes: the daemon closes its lease
    *r.holder.lock().unwrap() = Some(Holder {
        name: "lane-a".into(),
        epoch: 3,
    });
    let ask = r.sent().last().cloned().unwrap();
    r.answer("allow", &ask.id, r.now());
    r.call(&at(100.0, 300.0)).unwrap();
    assert!(r.core.protected_mut().unwrap().close_epoch(2).is_none());
    assert!(r.core.protected_mut().unwrap().close_epoch(3).is_some());
    assert!(!r.book.admits(PROT));
    // no daemon lease at all: nothing is asked
    *r.holder.lock().unwrap() = None;
    let n = r.sent().len();
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeaseRefused { .. }
    ));
    assert_eq!(r.sent().len(), n);
}

// ---------------------------------------------------------------------------------------------
// blocker 5: a Deny holds
// ---------------------------------------------------------------------------------------------

#[test]
fn an_overnight_deny_holds_past_ten_minutes_until_the_owner_is_awake() {
    let mut r = rig("deny-night");
    r.presence("asleep", r.now());
    r.call(&at(100.0, 300.0))
        .expect("the overnight rule opens a lease");
    assert_eq!(r.book.active().unwrap().granted, Granted::Overnight);
    let notice = r.sent()[0].clone();
    assert_eq!(notice.style, Style::Notice);
    // the owner taps Deny on the notice; the next poll (10 s on) closes the lease
    r.answer("deny", &notice.id, r.now() + 1_000);
    r.wall.advance(10_000);
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeaseRefused { .. }
    ));
    assert!(r.book.active().is_none());
    // ten minutes on, still asleep: held, and no push goes out
    for step in [10 * MIN, 30 * MIN, 120 * MIN] {
        r.wall.set(Some(T0 + step));
        r.presence("asleep", r.now());
        let e = refused(r.call(&at(100.0, 300.0)));
        assert!(e.to_string().contains("held"), "{step}: {e}");
        assert_eq!(r.sent().len(), 1, "{step}: no new push");
    }
    // a stale or forged presence file does not end the hold either
    r.wall.advance(60 * MIN);
    assert!(refused(r.call(&at(100.0, 300.0)))
        .to_string()
        .contains("held"));
    // the owner is up: the hold ends and the next call asks
    r.presence("present", r.now());
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    assert_eq!(r.sent().len(), 2);
    assert_eq!(r.sent()[1].style, Style::Lease);
}

#[test]
fn an_explicit_re_allow_lifts_a_hold() {
    let mut r = rig("reallow");
    r.presence("asleep", r.now());
    r.call(&at(100.0, 300.0)).unwrap();
    let notice = r.sent()[0].clone();
    r.answer("deny", &notice.id, r.now() + 1_000);
    r.wall.advance(10_000);
    assert!(r.call(&at(100.0, 300.0)).is_err());
    // an allow dated before the Deny is not a re-allow
    r.answer("allow", &notice.id, T0 + 500);
    r.wall.advance(MIN);
    r.presence("asleep", r.now());
    assert!(refused(r.call(&at(100.0, 300.0)))
        .to_string()
        .contains("held"));
    r.answer("allow", &notice.id, r.now());
    r.call(&at(100.0, 300.0)).expect("re-allowed");
    assert!(matches!(
        r.book.active().unwrap().granted,
        Granted::Approved { .. }
    ));
}

#[test]
fn a_daytime_deny_holds_for_the_ask_window_and_then_asks_again() {
    let mut r = rig("deny-day");
    r.call(&at(100.0, 300.0)).unwrap_err();
    let ask = r.sent()[0].clone();
    r.answer("deny", &ask.id, r.now());
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeaseRefused { .. }
    ));
    r.wall.advance(9 * MIN);
    assert!(refused(r.call(&at(100.0, 300.0)))
        .to_string()
        .contains("held"));
    assert_eq!(r.sent().len(), 1);
    r.wall.advance(MIN);
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    assert_eq!(r.sent().len(), 2);
}

#[test]
fn a_deny_channel_that_cannot_be_read_is_not_no_deny() {
    let mut r = rig("poll-fail");
    r.open_lease();
    r.inbox.lock().unwrap().poll_fails = true;
    r.wall.advance(10_000);
    r.call(&at(100.0, 300.0))
        .expect("one failed poll is not yet a close");
    r.wall.advance(60_000);
    r.call(&at(100.0, 300.0)).unwrap();
    r.wall.advance(60_000);
    let e = refused(r.call(&at(100.0, 300.0)));
    assert!(e.to_string().contains("could not be checked"), "{e}");
    assert!(r.book.active().is_none());
}

// ---------------------------------------------------------------------------------------------
// blocker 6: the presence file
// ---------------------------------------------------------------------------------------------

#[test]
fn only_a_signed_fresh_presence_file_opens_an_overnight_lease() {
    type Write = Box<dyn Fn(&Rig)>;
    let cases: [(&str, Write); 5] = [
        ("none", Box::new(|_| {})),
        (
            "stale",
            Box::new(|r: &Rig| r.presence("asleep", r.now() - 16 * MIN)),
        ),
        (
            "ahead",
            Box::new(|r: &Rig| r.presence("asleep", r.now() + 3 * MIN)),
        ),
        (
            "forged",
            Box::new(|r: &Rig| {
                std::fs::write(
                    &r.presence_file,
                    sign_presence(b"some-other-key-0123456789", "asleep", r.now()),
                )
                .unwrap()
            }),
        ),
        (
            "unsigned",
            Box::new(|r: &Rig| {
                std::fs::write(
                    &r.presence_file,
                    format!("{{\"state\":\"asleep\",\"written_at\":{}}}", r.now()),
                )
                .unwrap()
            }),
        ),
    ];
    for (name, write) in cases {
        let mut r = rig(&format!("presence-{name}"));
        write(&r);
        assert!(
            matches!(
                refused(r.call(&at(100.0, 300.0))),
                Refusal::LeasePending { .. }
            ),
            "{name}: asks instead"
        );
        assert_eq!(r.sent()[0].style, Style::Lease, "{name}");
        assert!(r.book.active().is_none(), "{name}");
    }
    // the key file missing: off too
    let mut r = rig("presence-nokey");
    r.presence("asleep", r.now());
    std::fs::remove_file(r.dir.join("presence.key")).unwrap();
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
}

#[test]
fn an_overnight_lease_never_asks_and_ends_when_the_owner_wakes() {
    let mut r = rig("overnight");
    r.presence("asleep", r.now());
    r.call(&at(100.0, 300.0)).unwrap();
    // a destructive tap would need a yes: overnight nothing is asked, so it is refused
    r.core.transport.show(settings());
    let e = refused(r.call(&at(500.0, 550.0)));
    assert!(e.to_string().contains("overnight"), "{e}");
    assert_eq!(r.sent().len(), 1, "only the notice");
    // the presence file now says present: the overnight lease ends, and the call asks
    r.core.transport.show(home());
    r.presence("present", r.now());
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    // no notice could be sent: no overnight lease
    let mut q = rig("overnight-nosend");
    q.presence("asleep", q.now());
    q.inbox.lock().unwrap().send_fails = true;
    assert!(matches!(
        refused(q.call(&at(100.0, 300.0))),
        Refusal::LeaseRefused { .. }
    ));
    assert!(q.book.active().is_none());
}

// ---------------------------------------------------------------------------------------------
// should-fix items
// ---------------------------------------------------------------------------------------------

#[test]
fn read_only_and_helper_win_over_any_lease_and_ask_nothing() {
    let mut r = rig("order");
    r.core.set_mode(S24_SERIAL, DeviceMode::ReadOnly);
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::ReadOnly { .. }
    ));
    r.core.set_mode(S24_SERIAL, DeviceMode::Helper);
    assert!(matches!(
        refused(r.call(&Call::OpenApp {
            package: "com.android.settings".into()
        })),
        Refusal::NeedsAdbShell { .. }
    ));
    assert!(r.sent().is_empty());
    // unlock is never sent, lease or not
    r.core.set_mode(S24_SERIAL, DeviceMode::Full);
    assert_eq!(protected_rule(r.call(&Call::Unlock)), "unlock");
    // a ruled read needs no lease
    r.core
        .call(
            Some(PROT),
            &Call::Getprop {
                key: "ro.serialno".into(),
            },
        )
        .unwrap();
    assert!(r.sent().is_empty());
}

#[test]
fn digits_on_a_pin_prompt_are_refused_one_at_a_time() {
    let mut r = rig("pin");
    r.open_lease();
    r.core.transport.show(pin_prompt());
    let before = r.core.transport.sent_to_protected().len();
    for c in [
        Call::TypeText {
            text: "1".into(),
            index: None,
            submit: false,
        },
        Call::TypeText {
            text: "2".into(),
            index: None,
            submit: false,
        },
        Call::Key {
            name: "KEYCODE_3".into(),
        },
        at(100.0, 1100.0),
    ] {
        assert_eq!(protected_rule(r.call(&c)), "pin", "{c:?}");
    }
    assert_eq!(r.core.transport.sent_to_protected().len(), before);
}

#[test]
fn a_screenshot_is_checked_again_after_it_is_taken() {
    let mut r = rig("race");
    r.open_lease();
    // ordinary before the shot, Gmail Go after it
    r.core.transport.show(home());
    r.core.transport.then(gmail_go());
    let shots = r.core.transport.shots();
    let e = r
        .core
        .call(Some(PROT), &Call::Screenshot { max_width: 720 });
    assert!(
        matches!(
            e,
            Err(CoreError::Refused(Refusal::Protected { rule: "app", .. }))
        ),
        "{e:?}"
    );
    assert_eq!(r.core.transport.shots(), shots + 1, "taken, then dropped");
    // look: the tree read after the shot decides
    r.core.transport.show(home());
    r.core.transport.then(shade());
    let e = r.core.call(Some(PROT), &Call::Look { max_width: 720 });
    assert!(
        matches!(e, Err(CoreError::Refused(Refusal::Protected { .. }))),
        "{e:?}"
    );
    // the screen cannot be read again: dropped too
    r.core.transport.show(home());
    let ok = r
        .core
        .call(Some(PROT), &Call::Screenshot { max_width: 720 });
    assert!(ok.unwrap().image.is_some());
    r.core.transport.tree_fails = true;
    assert!(r
        .core
        .call(Some(PROT), &Call::Screenshot { max_width: 720 })
        .is_err());
}

#[test]
fn an_unreadable_clock_or_no_channel_opens_nothing() {
    let mut r = rig("clock");
    r.wall.set(None);
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeaseRefused { .. }
    ));
    assert!(r.sent().is_empty());
    let mut q = rig("nochannel");
    q.inbox.lock().unwrap().send_fails = true;
    assert!(matches!(
        refused(q.call(&at(100.0, 300.0))),
        Refusal::LeaseRefused { .. }
    ));
}

#[test]
fn an_answer_to_another_request_opens_nothing() {
    let mut r = rig("forgery");
    r.call(&at(100.0, 300.0)).unwrap_err();
    let ask = r.sent()[0].clone();
    // the agent knows only the 6-character reference; an allow naming anything else is no answer
    let forged = format!("{}{}", &ask.id[..6], "a".repeat(14));
    r.answer("allow", &forged, r.now());
    r.answer("allow ", &ask.id, r.now());
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    assert!(r.book.active().is_none());
}
