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
    /// Heads-up answers served in turn; then `hun`.
    hun_seq: VecDeque<bool>,
    hun: bool,
    hun_fails: bool,
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
    /// Screen dumps of the protected phone so far.
    fn dumps(&self) -> usize {
        self.calls
            .iter()
            .filter(|c| *c == &format!("tree {PROT}"))
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
    fn heads_up(&mut self, serial: &str) -> Result<bool, TransportError> {
        self.calls.push(format!("heads_up {serial}"));
        if self.hun_fails {
            return Err(TransportError("no answer".into()));
        }
        Ok(self.hun_seq.pop_front().unwrap_or(self.hun))
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
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    rig_in(dir)
}

/// A rig over `dir` as it is (a restart keeps its holds file and presence key).
fn rig_in(dir: PathBuf) -> Rig {
    let key_file = dir.join("presence.key");
    std::fs::write(&key_file, KEY).unwrap();
    let presence_file = dir.join("presence.json");
    let wall = Arc::new(ManualClock::new(T0));
    let book = Arc::new(LeaseBook::new(wall.clone()));
    let cfg = ProtectedConfig::new(
        30,
        10,
        Some(PresenceConfig::new(&presence_file, &key_file, 15)),
    )
    .with_holds_file(dir.join("protected-holds.json"));
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
    /// The asks sent (lease asks), without the notices.
    fn asks(&self) -> Vec<Request> {
        self.sent()
            .into_iter()
            .filter(|r| r.style == Style::Lease)
            .collect()
    }
    /// Log every decision to an audit log in the rig's folder; returns its path.
    fn audit(&mut self) -> PathBuf {
        let path = self.dir.join("audit.log");
        self.core
            .set_audit(agentroid_core::audit::AuditLog::open(&path).unwrap());
        path
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
    // round 2, item 2: one quiet notice, same request id, with until when and a Deny
    let notice = r.sent()[1].clone();
    assert_eq!(notice.style, Style::Notice);
    assert_eq!(notice.id, ask.id, "its Deny ends this very lease");
    assert!(notice.reason.contains("open until"), "{}", notice.reason);
    for _ in 0..5 {
        r.wall.advance(MIN);
        r.call(&at(100.0, 300.0)).unwrap();
        r.call(&ui_tree()).unwrap();
    }
    assert_eq!(
        r.sent().len(),
        2,
        "one Allow (and its notice) covers the burst"
    );
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
    assert_eq!(r.asks().len(), 2);
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
    assert_eq!(r.asks().len(), 1);
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
        Refusal::LeaseRefused { .. }
    ));
    assert!(
        r.book.active().is_none(),
        "lane-a's Allow opened nothing for lane-b"
    );
    // round 2, item 5: no fresh push until the ask window since the last one has passed
    assert_eq!(r.asks().len(), 1, "one ask per phone");
    r.wall.advance(10 * MIN);
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    let ask_b = r.asks()[1].clone();
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
    assert!(r.call(&at(100.0, 300.0)).is_err());
    assert!(r.book.active().is_none());
    r.wall.advance(10 * MIN);
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    // the holder's connection goes: the daemon closes its lease
    *r.holder.lock().unwrap() = Some(Holder {
        name: "lane-a".into(),
        epoch: 3,
    });
    let ask = r.asks().last().cloned().unwrap();
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
    // the Deny already applied counts once: the Deny polls of the re-allowed lease do not close it
    for _ in 0..3 {
        r.wall.advance(11_000);
        r.call(&at(100.0, 300.0))
            .expect("the old Deny does not close the re-allowed lease");
    }
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

// ---------------------------------------------------------------------------------------------
// round 2 of the review
// ---------------------------------------------------------------------------------------------

fn type_text(text: &str) -> Call {
    Call::TypeText {
        text: text.into(),
        index: None,
        submit: true,
    }
}

/// The rules of the audit log at `path`, in order.
fn audit_rules(path: &std::path::Path) -> Vec<(String, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let r = agentroid_core::audit::Record::from_json(l).unwrap();
            (r.entry.rule, r.entry.reason)
        })
        .collect()
}

/// A screen lock on the rig whose phone reads in use while `busy` is set.
fn with_lock(r: &mut Rig, busy: Arc<Mutex<bool>>) -> PathBuf {
    use agentroid_core::askscreen::Signals;
    use agentroid_core::screenlock::{Reading, ScreenLock};
    let in_use = Signals {
        wakefulness: Some("Awake".into()),
        idle_ms: Some(1_000),
        screen_kept_on: Some(false),
        focus: Some("com.example.notes".into()),
        ime_shown: Some(true),
        keyguard: Some(false),
        in_call: Some(false),
    };
    let idle = Signals {
        wakefulness: Some("Asleep".into()),
        idle_ms: Some(3_600_000),
        screen_kept_on: Some(false),
        focus: Some(String::new()),
        ime_shown: Some(false),
        keyguard: Some(true),
        in_call: Some(false),
    };
    let lock_dir = r.dir.join("lock");
    let wall = r.wall.clone();
    let mono = r.wall.clone();
    r.core.set_screen_lock(ScreenLock::new(
        lock_dir.clone(),
        Box::new(move |_, mono| {
            let t = mono();
            let s = if *busy.lock().unwrap() {
                in_use.clone()
            } else {
                idle.clone()
            };
            Some(Reading {
                signals: s,
                power_from: t,
                power_to: t,
            })
        }),
        Box::new(move || agentroid_adb_link::lease::WallClock::now_ms(&*wall).unwrap_or(0)),
        // the lock's own clock moves with the rig's, so its kept verdicts age
        Box::new(move || agentroid_adb_link::lease::WallClock::now_ms(&*mono).unwrap_or(0)),
    ));
    lock_dir
}

// item 1: the protected apps' web hosts, past notifications, and mentions outside the launcher

#[test]
fn the_protected_apps_web_hosts_are_refused_in_open_url_and_type_text() {
    let mut r = rig("hosts");
    for uri in [
        "https://mail.google.com/mail/u/0/",
        "https://INBOX.google.com",
        "https://www.gmail.com/",
        "https://web.whatsapp.com/",
        "https://wa.me/15550000",
        "https://www.whatsapp.com/download",
    ] {
        for package in [None, Some("com.android.chrome".to_string())] {
            let c = Call::OpenUrl {
                uri: uri.into(),
                package,
            };
            assert_eq!(protected_rule(r.call(&c)), "app", "{c:?}");
        }
    }
    // typed into an address bar, the same hosts would open the same pages
    for text in [
        "mail.google.com",
        "go to WEB.WHATSAPP.COM",
        "wa.me/15550000",
        "https://inbox.google.com",
    ] {
        assert_eq!(protected_rule(r.call(&type_text(text))), "app", "{text}");
    }
    assert!(r.sent().is_empty(), "refused before any ask");
    // ordinary pages and text still ask for (and then get) the lease
    r.open_lease();
    r.call(&Call::OpenUrl {
        uri: "https://example.com/".into(),
        package: Some("com.android.chrome".into()),
    })
    .unwrap();
    r.call(&type_text("weather tomorrow")).unwrap();
}

#[test]
fn notification_history_and_listener_screens_refuse_reads_and_acts() {
    let mut r = rig("notif-log");
    r.open_lease();
    for (name, xml) in [
        (
            "history list",
            page(&[n(
                "com.android.settings",
                "com.android.settings:id/notification_history_list",
                "",
                "",
                "[0,200][1080,2000]",
            )]),
        ),
        (
            "history title",
            page(&[n(
                "com.android.settings",
                "com.android.settings:id/collapsing_toolbar",
                "Notification history",
                "",
                "[0,100][1080,200]",
            )]),
        ),
        (
            "listener access",
            page(&[n(
                "com.android.settings",
                "com.android.settings:id/action_bar",
                "Device & app notifications",
                "",
                "[0,100][1080,200]",
            )]),
        ),
        (
            "notification log",
            page(&[n(
                "com.example.notilog",
                "com.example.notilog:id/notification_log_row",
                "Dinner at 8?",
                "",
                "[0,200][1080,300]",
            )]),
        ),
    ] {
        r.core.transport.show(xml);
        for c in [
            ui_tree(),
            Call::Screenshot { max_width: 720 },
            at(100.0, 250.0),
        ] {
            assert_eq!(protected_rule(r.call(&c)), "preview", "{name}: {c:?}");
        }
    }
    // a Settings row titled like it is no title bar: the notifications settings list stays readable
    r.core.transport.show(page(&[n(
        "com.android.settings",
        "android:id/title",
        "Notification history",
        "",
        "[0,200][1080,300]",
    )]));
    r.call(&ui_tree()).unwrap();
}

#[test]
fn a_node_outside_the_launcher_naming_a_protected_app_refuses_reads() {
    let mut r = rig("mention");
    r.open_lease();
    // the launcher's icon is the one exception
    r.core.transport.show(home());
    r.call(&ui_tree()).unwrap();
    for (pkg, text, desc) in [
        ("com.android.settings", "WhatsApp", ""),
        ("com.android.chrome", "WhatsApp Web", ""),
        (
            "com.android.chrome",
            "",
            "Inbox (3) - someone@gmail.com - Gmail",
        ),
        ("com.example.share", "Share to WhatsApp", ""),
    ] {
        r.core
            .transport
            .show(page(&[n(pkg, "", text, desc, "[0,200][1080,300]")]));
        assert_eq!(
            protected_rule(r.call(&ui_tree())),
            "preview",
            "{pkg} {text} {desc}"
        );
    }
}

// item 2: a Deny of the open lease survives another ask, and the Allow is followed by a notice

#[test]
fn the_same_deny_closes_the_lease_with_or_without_a_concurrent_ask() {
    for concurrent in [false, true] {
        let mut r = rig(&format!("deny-concurrent-{concurrent}"));
        r.core.set_allow(&[agentroid_core::transport::POCO_SERIAL]);
        let ask = r.open_lease();
        r.answer("deny", &ask.id, r.now());
        if concurrent {
            // the POCO's own ask polls the same channel (and sees the Deny) before the lease does
            r.core.transport.show(settings());
            let e = r.core.call(Some(POCO), &at(500.0, 550.0));
            assert!(e.is_err(), "{e:?}");
            assert!(
                r.sent().iter().any(|q| q.style == Style::Action),
                "an ask for the POCO went out"
            );
            r.core.transport.show(home());
        }
        r.wall.advance(10_000);
        assert!(
            matches!(
                refused(r.call(&at(100.0, 300.0))),
                Refusal::LeaseRefused { .. }
            ),
            "concurrent: {concurrent}"
        );
        assert!(r.book.active().is_none(), "concurrent: {concurrent}");
    }
}

#[test]
fn an_allow_is_followed_by_one_quiet_notice_whose_deny_ends_the_lease() {
    let mut r = rig("notice");
    let ask = r.open_lease();
    let sent = r.sent();
    assert_eq!(sent.len(), 2);
    let notice = &sent[1];
    assert_eq!(notice.style, Style::Notice);
    assert_eq!(notice.id, ask.id);
    // T0 is 2027-01-15T08:00:00Z; the lease is 30 minutes
    assert!(
        notice.reason.contains("open until 08:30 UTC (30 min)"),
        "{}",
        notice.reason
    );
    // a Deny on it closes the lease
    r.answer("deny", &notice.id, r.now() + 1_000);
    r.wall.advance(10_000);
    assert!(r.call(&at(100.0, 300.0)).is_err());
    assert!(r.book.active().is_none());
    // a notice that cannot be sent leaves the lease open, and says so in the log
    let mut q = rig("notice-unsent");
    let log = q.audit();
    q.call(&at(100.0, 300.0)).unwrap_err();
    let ask = q.sent()[0].clone();
    q.inbox.lock().unwrap().send_fails = true;
    q.answer("allow", &ask.id, q.now());
    q.call(&at(100.0, 300.0)).unwrap();
    let rules: Vec<String> = audit_rules(&log).into_iter().map(|(r, _)| r).collect();
    assert!(
        rules.contains(&"lease:notice_unsent".to_string()),
        "{rules:?}"
    );
}

// item 3: the screen lock overnight, and an ask-screen No under an Allow

#[test]
fn overnight_the_owner_picking_the_phone_up_stops_the_agent() {
    let mut r = rig("overnight-lock");
    let busy = Arc::new(Mutex::new(false));
    with_lock(&mut r, busy.clone());
    r.presence("asleep", r.now());
    r.call(&at(100.0, 300.0))
        .expect("the phone is idle: the overnight lease acts");
    assert_eq!(r.book.active().unwrap().granted, Granted::Overnight);
    *busy.lock().unwrap() = true;
    r.wall.advance(MIN);
    r.presence("asleep", r.now());
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::ScreenLock { .. }
    ));
    // an Allow (not the overnight rule) is the owner's own yes, and the lock does not ask again
    let mut q = rig("approved-lock");
    with_lock(&mut q, Arc::new(Mutex::new(true)));
    q.open_lease();
    q.call(&at(100.0, 300.0)).unwrap();
}

#[test]
fn an_ask_screen_no_still_refuses_under_an_allowed_lease() {
    use agentroid_core::askscreen::{save_memory, Memory};
    let mut r = rig("lock-no");
    let lock_dir = with_lock(&mut r, Arc::new(Mutex::new(false)));
    r.open_lease();
    r.call(&at(100.0, 300.0)).unwrap();
    save_memory(
        &lock_dir,
        S24_SERIAL,
        &Memory {
            denied_until_ms: r.now() + 5 * MIN,
            ..Memory::default()
        },
    )
    .unwrap();
    let e = refused(r.call(&at(100.0, 300.0)));
    assert!(matches!(e, Refusal::ScreenLock { .. }), "{e:?}");
    assert!(e.to_string().contains("said No"), "{e}");
    // reads are not screen-changing: they still go
    r.call(&ui_tree()).unwrap();
}

// item 4: the hold is kept on disk

#[test]
fn a_deny_is_kept_in_the_holds_file_and_holds_after_a_restart() {
    let r = {
        let mut r = rig("holds");
        r.call(&at(100.0, 300.0)).unwrap_err();
        let ask = r.sent()[0].clone();
        r.answer("deny", &ask.id, r.now());
        r.call(&at(100.0, 300.0)).unwrap_err();
        let path = r.dir.join("protected-holds.json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["hold"]["request_id"], ask.id.as_str());
        assert_eq!(v["hold"]["until"], "window");
        assert_eq!(v["hold"]["holder"]["epoch"], 1);
        assert!(
            !r.dir.join("protected-holds.json.tmp").exists(),
            "written by a rename, no temporary file left"
        );
        r
    };
    // a restart: a new core and lease state over the same state directory
    let mut again = rig_in(r.dir.clone());
    let e = refused(again.call(&at(100.0, 300.0)));
    assert!(e.to_string().contains("held"), "{e}");
    assert!(again.sent().is_empty(), "no push while held");
    // the window passes: the hold ends, the file is cleared, and the next call asks
    again.wall.advance(10 * MIN);
    assert!(matches!(
        refused(again.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    let v: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(again.dir.join("protected-holds.json")).unwrap(),
    )
    .unwrap();
    assert!(v["hold"].is_null());
    drop(again);
    drop(r);
}

#[test]
fn an_overnight_hold_is_kept_until_the_owner_is_awake_across_a_restart() {
    let mut r = rig("holds-night");
    r.presence("asleep", r.now());
    r.call(&at(100.0, 300.0)).unwrap();
    let notice = r.sent()[0].clone();
    r.answer("deny", &notice.id, r.now() + 1_000);
    r.wall.advance(10_000);
    r.call(&at(100.0, 300.0)).unwrap_err();
    let mut again = rig_in(r.dir.clone());
    again.wall.set(Some(T0 + 60 * MIN));
    again.presence("asleep", again.now());
    let e = refused(again.call(&at(100.0, 300.0)));
    assert!(e.to_string().contains("held"), "{e}");
    assert!(
        again.book.active().is_none(),
        "no overnight lease while held"
    );
    again.presence("present", again.now());
    assert!(matches!(
        refused(again.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
}

#[test]
fn a_corrupt_or_unreadable_holds_file_holds_until_a_trusted_presence_change() {
    for broken in ["junk", "other version", "directory"] {
        let dir = std::env::temp_dir().join(format!(
            "agentroid-plp-holds-{}-{}",
            broken.replace(' ', "-"),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("protected-holds.json");
        match broken {
            "junk" => std::fs::write(&path, "{\"version\":1,\"hold\":{\"request_id\"").unwrap(),
            "other version" => std::fs::write(&path, "{\"version\":2,\"hold\":null}").unwrap(),
            _ => std::fs::create_dir_all(&path).unwrap(),
        }
        let mut r = rig_in(dir);
        // no presence file: held, and nothing is asked
        let e = refused(r.call(&at(100.0, 300.0)));
        assert!(e.to_string().contains("could not be read"), "{broken}: {e}");
        // a trusted state, but no change yet: still held (no overnight lease either)
        r.presence("asleep", r.now());
        assert!(r.call(&at(100.0, 300.0)).is_err(), "{broken}");
        assert!(r.book.active().is_none(), "{broken}");
        r.wall.advance(30 * MIN);
        r.presence("asleep", r.now());
        assert!(r.call(&at(100.0, 300.0)).is_err(), "{broken}");
        assert!(r.sent().is_empty(), "{broken}: no push");
        // a forged change is no change
        std::fs::write(
            &r.presence_file,
            sign_presence(b"some-other-key-0123456789", "present", r.now()),
        )
        .unwrap();
        assert!(r.call(&at(100.0, 300.0)).is_err(), "{broken}");
        // the owner is up: the trusted state changed, and the next call asks
        r.presence("present", r.now());
        if broken == "directory" {
            // (the hold cannot be cleared on disk, but it ends in memory)
            let _ = std::fs::remove_dir_all(r.dir.join("protected-holds.json"));
        }
        assert!(
            matches!(
                refused(r.call(&at(100.0, 300.0))),
                Refusal::LeasePending { .. }
            ),
            "{broken}"
        );
    }
}

// item 5: one ask per phone

#[test]
fn one_ask_per_phone_and_a_new_holder_inherits_the_hold() {
    let mut r = rig("one-ask");
    r.call(&at(100.0, 300.0)).unwrap_err();
    assert_eq!(r.asks().len(), 1);
    // another holder while lane-a's ask is open: refused, no push
    *r.holder.lock().unwrap() = Some(Holder {
        name: "lane-b".into(),
        epoch: 2,
    });
    for _ in 0..3 {
        let e = refused(r.call(&at(100.0, 300.0)));
        assert!(matches!(e, Refusal::LeaseRefused { .. }), "{e:?}");
        assert!(e.to_string().contains("still open"), "{e}");
    }
    assert_eq!(r.asks().len(), 1, "no push for another epoch");
    // the owner denies lane-a's ask: lane-b is held too
    let ask = r.asks()[0].clone();
    r.answer("deny", &ask.id, r.now());
    r.call(&at(100.0, 300.0)).unwrap_err();
    r.wall.advance(5 * MIN);
    let e = refused(r.call(&at(100.0, 300.0)));
    assert!(e.to_string().contains("held"), "{e}");
    assert_eq!(r.asks().len(), 1);
}

#[test]
fn an_ask_that_ends_unanswered_cools_down_for_the_ask_window() {
    let mut r = rig("cooldown");
    r.call(&at(100.0, 300.0)).unwrap_err();
    // the ask expires: held for a window, then a single fresh ask
    r.wall.advance(10 * MIN);
    r.call(&at(100.0, 300.0)).unwrap_err();
    r.wall.advance(9 * MIN);
    r.call(&at(100.0, 300.0)).unwrap_err();
    assert_eq!(r.asks().len(), 1);
    r.wall.advance(MIN);
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::LeasePending { .. }
    ));
    assert_eq!(r.asks().len(), 2);
}

// item 6: the resolved package

#[test]
fn open_app_is_judged_on_the_package_it_resolves_to() {
    let mut r = rig("resolve");
    r.open_lease();
    let before = r.core.transport.sent_to_protected().len();
    for name in ["atsap", "WhatsApp", "gm.lite"] {
        let e = r.call(&Call::OpenApp {
            package: name.into(),
        });
        assert_eq!(protected_rule(e), "app", "{name}");
    }
    assert_eq!(r.core.transport.sent_to_protected().len(), before);
    r.call(&Call::OpenApp {
        package: "settings".into(),
    })
    .unwrap();
}

// item 7: one dump per tap and per ui_tree

#[test]
fn a_tap_and_a_ui_tree_dump_the_protected_screen_once() {
    let mut r = rig("dumps");
    r.open_lease();
    let d = r.core.transport.dumps();
    r.call(&at(100.0, 300.0)).unwrap();
    assert_eq!(r.core.transport.dumps() - d, 1, "tap");
    let d = r.core.transport.dumps();
    r.call(&ui_tree()).unwrap();
    assert_eq!(r.core.transport.dumps() - d, 1, "ui_tree");
    // a screenshot still reads once more after the shot
    let d = r.core.transport.dumps();
    r.call(&Call::Screenshot { max_width: 720 }).unwrap();
    assert_eq!(r.core.transport.dumps() - d, 2, "screenshot");
    let d = r.core.transport.dumps();
    r.call(&Call::Look { max_width: 720 }).unwrap();
    assert_eq!(r.core.transport.dumps() - d, 2, "look");
}

// item 9: the audit log, the re-allow, read_only

#[test]
fn every_lease_is_logged_before_it_opens_and_every_close_is_logged() {
    let mut r = rig("audit");
    let log = r.audit();
    r.open_lease();
    // released by its holder: logged by the daemon at once
    assert!(r.core.protected_mut().unwrap().close_epoch(1).is_some());
    r.core.protected_audit_closes("lease.release");
    assert!(
        audit_rules(&log)
            .iter()
            .any(|(rule, _)| rule == "lease:closed"),
        "logged at the release, not at the next call"
    );
    // a connection that went, and one that ran out
    r.wall.advance(10 * MIN + 1);
    r.open_lease();
    r.book.close_epoch(1, "connection closed");
    r.wall.advance(10 * MIN + 1);
    r.open_lease();
    r.wall.advance(30 * MIN);
    r.call(&at(100.0, 300.0)).unwrap_err();
    let rules = audit_rules(&log);
    let closes: Vec<&String> = rules
        .iter()
        .filter(|(rule, _)| rule == "lease:closed")
        .map(|(_, why)| why)
        .collect();
    assert_eq!(closes.len(), 3, "{rules:?}");
    assert!(closes[0].ends_with("closed: released"), "{}", closes[0]);
    assert!(
        closes[1].ends_with("closed: connection closed"),
        "{}",
        closes[1]
    );
    assert!(closes[2].ends_with("closed: expired"), "{}", closes[2]);
    assert_eq!(
        rules
            .iter()
            .filter(|(rule, _)| rule == "lease:opened")
            .count(),
        3
    );
}

#[test]
fn a_lease_whose_audit_write_fails_never_opens() {
    // asked and allowed, but the log cannot be written
    let mut r = rig("audit-fails");
    let log = r.audit();
    std::fs::create_dir_all(agentroid_core::audit::head_path(&log)).unwrap();
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::Audit { .. }
    ));
    let ask = r.sent()[0].clone();
    r.answer("allow", &ask.id, r.now());
    assert!(matches!(
        refused(r.call(&at(100.0, 300.0))),
        Refusal::Audit { .. }
    ));
    assert!(r.book.active().is_none());
    // overnight
    let mut q = rig("audit-fails-night");
    let log = q.audit();
    std::fs::create_dir_all(agentroid_core::audit::head_path(&log)).unwrap();
    q.presence("asleep", q.now());
    assert!(matches!(
        refused(q.call(&at(100.0, 300.0))),
        Refusal::Audit { .. }
    ));
    assert!(q.book.active().is_none());
    // a re-allow: the hold stands
    let mut h = rig("audit-fails-reallow");
    h.presence("asleep", h.now());
    h.call(&at(100.0, 300.0)).unwrap();
    let notice = h.sent()[0].clone();
    h.answer("deny", &notice.id, h.now() + 1_000);
    h.wall.advance(10_000);
    h.call(&at(100.0, 300.0)).unwrap_err();
    let log = h.audit();
    std::fs::create_dir_all(agentroid_core::audit::head_path(&log)).unwrap();
    h.wall.advance(MIN);
    h.presence("asleep", h.now());
    h.answer("allow", &notice.id, h.now());
    assert!(matches!(
        refused(h.call(&at(100.0, 300.0))),
        Refusal::Audit { .. }
    ));
    assert!(h.book.active().is_none());
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(h.dir.join("protected-holds.json")).unwrap())
            .unwrap();
    assert!(!v["hold"].is_null(), "the hold stands");
}

#[test]
fn a_re_allow_is_bound_to_the_denied_holder_and_the_ask_window() {
    let mut r = rig("reallow-bound");
    r.presence("asleep", r.now());
    r.call(&at(100.0, 300.0)).unwrap();
    let notice = r.sent()[0].clone();
    r.answer("deny", &notice.id, r.now() + 1_000);
    r.wall.advance(10_000);
    r.call(&at(100.0, 300.0)).unwrap_err();
    // another holder cannot use the owner's allow
    *r.holder.lock().unwrap() = Some(Holder {
        name: "lane-b".into(),
        epoch: 2,
    });
    r.wall.advance(MIN);
    r.presence("asleep", r.now());
    r.answer("allow", &notice.id, r.now());
    let e = refused(r.call(&at(100.0, 300.0)));
    assert!(e.to_string().contains("held"), "{e}");
    // the denied holder can
    *r.holder.lock().unwrap() = Some(Holder {
        name: "lane-a".into(),
        epoch: 1,
    });
    r.call(&at(100.0, 300.0)).expect("re-allowed for lane-a");
    // an allow after the ask window from the Deny is no re-allow
    let mut q = rig("reallow-late");
    q.presence("asleep", q.now());
    q.call(&at(100.0, 300.0)).unwrap();
    let notice = q.sent()[0].clone();
    q.answer("deny", &notice.id, q.now() + 1_000);
    q.wall.advance(10_000);
    q.call(&at(100.0, 300.0)).unwrap_err();
    q.wall.advance(11 * MIN);
    q.presence("asleep", q.now());
    q.answer("allow", &notice.id, q.now());
    let e = refused(q.call(&at(100.0, 300.0)));
    assert!(e.to_string().contains("held"), "{e}");
}

#[test]
fn read_only_reads_on_the_protected_phone_ask_nothing() {
    let mut r = rig("read-only-reads");
    r.core.set_mode(S24_SERIAL, DeviceMode::ReadOnly);
    for c in [
        ui_tree(),
        Call::Screenshot { max_width: 720 },
        Call::Look { max_width: 720 },
    ] {
        assert!(matches!(refused(r.call(&c)), Refusal::S24 { .. }), "{c:?}");
    }
    assert!(r.sent().is_empty());
    assert_eq!(r.core.transport.dumps(), 0);
}

// item 10: heads-up notifications

#[test]
fn no_image_is_taken_or_kept_while_a_heads_up_is_showing() {
    let mut r = rig("heads-up");
    r.open_lease();
    for c in [
        Call::Screenshot { max_width: 720 },
        Call::Look { max_width: 720 },
    ] {
        // showing throughout: waited for twice, then refused, and no shot taken
        r.core.transport.hun = true;
        let shots = r.core.transport.shots();
        let asked = |p: &Phone| p.calls.iter().filter(|x| x.starts_with("heads_up")).count();
        let before = asked(&r.core.transport);
        assert_eq!(protected_rule(r.call(&c)), "preview", "{c:?}");
        assert_eq!(r.core.transport.shots(), shots, "{c:?}");
        assert_eq!(asked(&r.core.transport) - before, 3, "{c:?}: 1 + 2 retries");
        // gone after one wait: the shot is taken
        r.core.transport.hun = false;
        r.core.transport.hun_seq = VecDeque::from([true]);
        r.call(&c).expect("the heads-up went");
        assert_eq!(r.core.transport.shots(), shots + 1, "{c:?}");
        // one that comes up during the shot: dropped
        r.core.transport.hun_seq = VecDeque::from([false, true]);
        assert_eq!(protected_rule(r.call(&c)), "preview", "{c:?}");
        assert_eq!(r.core.transport.shots(), shots + 2, "{c:?}");
        // a check that cannot be made refuses
        r.core.transport.hun_fails = true;
        assert_eq!(protected_rule(r.call(&c)), "screen", "{c:?}");
        r.core.transport.hun_fails = false;
    }
    // the POCO is not asked
    r.core.set_allow(&[agentroid_core::transport::POCO_SERIAL]);
    r.core.transport.hun = true;
    r.core
        .call(Some(POCO), &Call::Screenshot { max_width: 720 })
        .unwrap();
    assert!(!r.core.transport.calls.contains(&format!("heads_up {POCO}")));
}
