//! Selftest scaffold (task 2.2, item 6): the 31 checks of phone-hands' `selftest.py`, in the same
//! order, driven through [`Core`] and a [`Transport`]. Against [`crate::fake::FakeDevice`] they run
//! in tests; the real "31 of 31 on the POCO, exit code 0" is the same [`run`] with the live
//! transport swapped in.
//!
//! Numbering (the Python file's checks, in order):
//! 1-2 device list; 3-6 S24 refused for screenshot, unlock, open_url, start_activity; 7-9 parser and
//! tap-by-index fixture; 10-13 borrowed labels; 14-18 coordinate frames and clamping; 19-20 quoting
//! and the URI scheme; 21-24 live reads; 25 unlock; 26-28 the helper backend; 29 tap; 30
//! start_activity; 31 Unicode typing. Checks that need the phone's input (25, 29-31) are skipped with
//! `no_input`; the helper checks (26-28) are skipped when the helper is not usable. Skips are not counted.

use crate::frame::{Frame, Shot};
use crate::gate::{Call, Target};
use crate::shell;
use crate::transport::{Backend, Core, CoreError, Transport, POCO_SERIAL};
use crate::tree::parse;
use agentroid_adb_link::guard::S24_SERIAL;
use agentroid_adb_link::runner::Clock;
use std::collections::HashSet;

const FIXTURE: &str = "UI hierchary dumped to: /dev/tty<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\">
<node class=\"android.widget.FrameLayout\" text=\"\" content-desc=\"\" resource-id=\"\" clickable=\"false\" bounds=\"[0,0][1220,2712]\">
 <node class=\"android.widget.TextView\" text=\"Settings\" content-desc=\"\" resource-id=\"android:id/title\" clickable=\"false\" bounds=\"[48,200][400,280]\"/>
 <node class=\"android.widget.LinearLayout\" text=\"\" content-desc=\"Wi-Fi\" resource-id=\"com.android.settings:id/wifi\" clickable=\"true\" bounds=\"[0,400][1220,560]\"/>
 <node class=\"android.widget.Switch\" text=\"\" content-desc=\"\" resource-id=\"switch_widget\" checkable=\"true\" checked=\"true\" clickable=\"true\" bounds=\"[1000,440][1180,520]\"/>
 <node class=\"android.widget.EditText\" text=\"\" content-desc=\"Search\" resource-id=\"search\" clickable=\"true\" focused=\"true\" password=\"false\" bounds=\"[48,600][1172,700]\"/>
 <node class=\"android.view.View\" text=\"\" content-desc=\"\" resource-id=\"\" clickable=\"false\" bounds=\"[0,800][1220,900]\"/>
</node></hierarchy>";
const COMPOSE: &str = include_str!("../fixtures/compose_dump.xml");
const IDONLY: &str = include_str!("../fixtures/idonly_dump.xml");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// 1-based position in phone-hands' selftest (0 for extra checks).
    pub n: usize,
    pub name: String,
    pub verdict: Verdict,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    fn add(&mut self, n: usize, name: &str, ok: bool, detail: impl Into<String>) {
        self.checks.push(Check {
            n,
            name: name.into(),
            verdict: if ok { Verdict::Pass } else { Verdict::Fail },
            detail: detail.into(),
        });
    }
    fn skip(&mut self, n: usize, name: &str, why: &str) {
        self.checks.push(Check {
            n,
            name: name.into(),
            verdict: Verdict::Skip,
            detail: why.into(),
        });
    }
    pub fn passed(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.verdict == Verdict::Pass)
            .count()
    }
    /// Checks that ran (passed or failed); skips are not counted.
    pub fn total(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.verdict != Verdict::Skip)
            .count()
    }
    pub fn skipped(&self) -> usize {
        self.checks.len() - self.total()
    }
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.verdict != Verdict::Fail)
    }
    /// 0 when nothing failed, as `selftest.py` exits.
    pub fn exit_code(&self) -> i32 {
        i32::from(!self.ok())
    }
    pub fn render(&self) -> String {
        let mut s = String::new();
        for c in &self.checks {
            let tag = match c.verdict {
                Verdict::Pass => "PASS",
                Verdict::Fail => "FAIL",
                Verdict::Skip => "SKIP",
            };
            s += &format!("{tag}  [{}] {}  {}\n", c.n, c.name, c.detail);
        }
        s += &format!(
            "{}/{} passed, {} skipped\n",
            self.passed(),
            self.total(),
            self.skipped()
        );
        s
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Skip everything that injects input (`--no-input`).
    pub no_input: bool,
}

fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn refused<T: Transport, C: Clock>(
    r: &mut Report,
    n: usize,
    name: &str,
    core: &mut Core<T, C>,
    call: Call,
) {
    match core.call(Some(S24_SERIAL), &call) {
        Err(CoreError::Refused(why)) => r.add(n, name, true, cut(&why.to_string(), 60)),
        other => r.add(
            n,
            name,
            false,
            format!("the gate let it through: {other:?}"),
        ),
    }
}

/// Run the 31 checks.
pub fn run<T: Transport, C: Clock>(core: &mut Core<T, C>, opts: Options) -> Report {
    let mut r = Report::default();

    // 1-2: the list
    let listing = core
        .call(None, &Call::ListDevices)
        .map(|o| o.text)
        .unwrap_or_else(|e| e.to_string());
    r.add(
        1,
        "POCO allowed",
        listing
            .lines()
            .any(|l| l.contains("DEVICE00") && l.contains("ALLOWED")),
        listing.replace('\n', " | "),
    );
    r.add(
        2,
        "S24 refused",
        listing
            .lines()
            .filter(|l| l.contains("PROTECT"))
            .all(|l| l.contains("refused")),
        "",
    );

    // 3-6: S24 refused before the phone is asked for anything
    refused(
        &mut r,
        3,
        "S24 screenshot refused",
        core,
        Call::Screenshot { max_width: 720 },
    );
    refused(&mut r, 4, "S24 unlock refused", core, Call::Unlock);
    refused(
        &mut r,
        5,
        "S24 open_url refused",
        core,
        Call::OpenUrl {
            uri: "https://example.com".into(),
            package: None,
        },
    );
    refused(
        &mut r,
        6,
        "S24 start_activity refused",
        core,
        Call::StartActivity {
            component: "com.android.settings/.Settings".into(),
            action: None,
            data: None,
        },
    );

    // 7-9: fixtures, no phone needed
    match parse(FIXTURE, false) {
        Ok(t) => {
            let lines: Vec<String> = t.elements.iter().map(|e| e.line()).collect();
            r.add(
                7,
                "parser keeps labelled + interactive, drops empty",
                t.elements.len() == 4,
                lines.join(" | "),
            );
            let sw = t.elements.get(2);
            r.add(
                8,
                "parser centre and flags",
                sw.is_some_and(|e| e.centre() == (1090, 480) && e.flags.checked),
                lines.get(2).cloned().unwrap_or_default(),
            );
            r.add(
                9,
                "tap target by index",
                t.elements.get(1).is_some_and(|e| e.centre() == (610, 480)),
                "",
            );
        }
        Err(e) => {
            for (n, name) in [
                (7, "parser keeps labelled + interactive, drops empty"),
                (8, "parser centre and flags"),
                (9, "tap target by index"),
            ] {
                r.add(n, name, false, e.to_string());
            }
        }
    }

    // 10-13: borrowed labels (Compose and Xiaomi shapes)
    match (parse(COMPOSE, false), parse(IDONLY, false)) {
        (Ok(c), Ok(i)) => {
            let card = c
                .elements
                .iter()
                .find(|e| (e.bounds.l, e.bounds.t, e.bounds.r, e.bounds.b) == (0, 300, 1220, 600));
            let tab = c.elements.iter().find(|e| {
                (e.bounds.l, e.bounds.t, e.bounds.r, e.bounds.b) == (600, 2500, 900, 2712)
            });
            let install = c.elements.iter().find(|e| e.text == "Install");
            let hint = |e: Option<&crate::tree::Element<'_>>| {
                e.and_then(|e| e.hint.as_ref())
                    .map(|h| (h.text.to_string(), h.src.as_str()))
            };
            r.add(
                10,
                "unlabelled card borrows its sibling labels",
                hint(card) == Some(("Shizuku / Xingchen & Rikka".into(), "sibling")),
                card.map(|e| e.line()).unwrap_or_default(),
            );
            r.add(
                11,
                "unlabelled tab borrows its child label",
                hint(tab) == Some(("Search".into(), "child")),
                tab.map(|e| e.line()).unwrap_or_default(),
            );
            r.add(
                12,
                "labelled button keeps its own label",
                install.is_some_and(|e| e.hint.is_none() && e.line().contains("\"Install\"")),
                install.map(|e| e.line()).unwrap_or_default(),
            );
            let me = i.elements.first();
            r.add(
                13,
                "id-only clickable borrows its child's text and keeps its id",
                me.is_some_and(|e| {
                    e.hint.as_ref().is_some_and(|h| h.text == "ME")
                        && e.line().contains("id=navBarMe")
                }),
                me.map(|e| e.line()).unwrap_or_default(),
            );
        }
        _ => {
            for n in 10..=13 {
                r.add(n, "borrowed-label fixture", false, "fixture did not parse");
            }
        }
    }

    // 14-18: coordinate spaces and clamping (a 480 px wide screenshot of a 1220x2712 screen)
    let shot = Shot {
        dev_w: 1220,
        dev_h: 2712,
        img_w: 480,
        img_h: 1067,
    };
    let at = |f: Result<Frame, _>, x: f64, y: f64| {
        f.map_err(|e: crate::frame::FrameError| e.to_string())
            .and_then(|f| {
                f.map(x, y)
                    .map(|m| (m, f.describe()))
                    .map_err(|e| e.to_string())
            })
    };
    let img = at(
        Frame::resolve(None, 1220, 2712, Some(shot), None),
        100.0,
        200.0,
    );
    r.add(
        14,
        "image px -> device px (latest screenshot)",
        img.as_ref()
            .is_ok_and(|(m, d)| (m.x, m.y) == (254, 508) && d.contains("image px")),
        format!("{img:?}"),
    );
    let w720 = at(
        Frame::resolve(None, 1220, 2712, Some(shot), Some(720)),
        100.0,
        200.0,
    );
    r.add(
        15,
        "image_width overrides the latest screenshot's scale",
        w720.as_ref().is_ok_and(|(m, _)| (m.x, m.y) == (169, 339)),
        format!("{w720:?}"),
    );
    let dev = at(
        Frame::resolve(Some("device"), 1220, 2712, Some(shot), None),
        610.0,
        1400.0,
    );
    r.add(
        16,
        "space='device' passes device px through",
        dev.as_ref()
            .is_ok_and(|(m, d)| (m.x, m.y) == (610, 1400) && d.contains("device px")),
        format!("{dev:?}"),
    );
    let off = at(
        Frame::resolve(None, 1220, 2712, Some(shot), None),
        360.0,
        1400.0,
    ); // the 17 Oct bug
    r.add(
        17,
        "off-screen point clamped and reported",
        off.as_ref().is_ok_and(|(m, _)| {
            (m.x, m.y) == (915, 2711) && m.notes.iter().any(|n| n.contains("CLAMPED"))
        }),
        format!("{off:?}"),
    );
    let none = Frame::resolve(Some("image"), 1220, 2712, None, None);
    r.add(
        18,
        "space='image' without a screenshot is refused",
        none.is_err(),
        cut(&format!("{none:?}"), 60),
    );

    // 19-20
    r.add(
        19,
        "shell quoting keeps & and ' in links",
        shell::sh_quote("market://search?q=it's&c=apps") == "'market://search?q=it'\\''s&c=apps'",
        "",
    );
    let bad = core.call(
        None,
        &Call::OpenUrl {
            uri: "no-scheme-here".into(),
            package: None,
        },
    );
    r.add(
        20,
        "open_url needs a scheme",
        matches!(bad, Err(CoreError::Invalid(_))),
        "",
    );

    // 21-24: live, read-only
    let serial = poco_serial(core);
    match core.call(None, &Call::Screenshot { max_width: 720 }) {
        Ok(o) => {
            r.add(
                21,
                "POCO screenshot",
                o.image.as_ref().is_some_and(|i| i.len() > 10_000),
                format!(
                    "{} bytes; {}",
                    o.image.as_ref().map_or(0, Vec::len),
                    cut(&o.text, 80)
                ),
            );
            let latest = o.shot.or(Some(shot));
            let f = latest.map(|s| Frame::resolve(None, s.dev_w, s.dev_h, Some(s), None));
            r.add(
                22,
                "default space is 'image' once a screenshot exists",
                f.as_ref()
                    .is_some_and(|f| f.as_ref().is_ok_and(|f| f.describe().contains("image px"))),
                "",
            );
        }
        Err(e) => {
            r.add(21, "POCO screenshot", false, e.to_string());
            r.add(
                22,
                "default space is 'image' once a screenshot exists",
                false,
                "no screenshot",
            );
        }
    }
    let tree = core.call(
        None,
        &Call::UiTree {
            interactive_only: false,
            require: vec![],
            timeout_ms: None,
        },
    );
    match &tree {
        Ok(o) => {
            let asleep = o.text.starts_with("SCREEN IS");
            r.add(
                23,
                "POCO ui_tree (uiautomator)",
                o.text.matches('\n').count() >= 3 || asleep,
                cut(o.text.lines().next().unwrap_or(""), 90),
            );
        }
        Err(e) => r.add(23, "POCO ui_tree (uiautomator)", false, e.to_string()),
    }
    let apps = core.call(
        None,
        &Call::ListApps {
            filter: "settings".into(),
        },
    );
    r.add(
        24,
        "list_apps",
        apps.as_ref()
            .is_ok_and(|o| o.text.contains("com.android.settings")),
        "",
    );

    if opts.no_input {
        r.skip(25, "unlock wakes and clears the lock screen", "--no-input");
    } else {
        match core.call(None, &Call::Unlock) {
            Ok(o) => r.add(
                25,
                "unlock wakes and clears the lock screen",
                o.text.contains("now Awake") && !o.text.contains("STILL LOCKED"),
                o.text,
            ),
            Err(e) => r.add(
                25,
                "unlock wakes and clears the lock screen",
                false,
                e.to_string(),
            ),
        }
        core.clock().sleep_ms(1500); // let the screen settle so both tree reads see the same thing
    }

    // 26-28: the helper backend
    let helper = serial
        .as_ref()
        .map(|s| core.transport.helper_status(s))
        .unwrap_or(Err("no POCO connected".into()));
    match (&helper, &serial) {
        (Ok(()), Some(s)) => {
            let hel = core.transport.tree_xml(s, Backend::Helper).map_err(|e| e.0);
            let uia = core
                .transport
                .tree_xml(s, Backend::Uiautomator)
                .map_err(|e| e.0);
            match (&hel, &uia) {
                (Ok(h), Ok(u)) => match (parse(h, false), parse(u, false)) {
                    (Ok(h), Ok(u)) => {
                        let labels = |t: &crate::tree::Tree<'_>| {
                            t.elements
                                .iter()
                                .map(|e| e.label().to_string())
                                .filter(|l| !l.is_empty())
                                .collect::<HashSet<_>>()
                        };
                        let (hl, ul) = (labels(&h), labels(&u));
                        let common = hl.intersection(&ul).count();
                        // The helper adds the status bar and skips "not important" views: compare loosely.
                        r.add(
                            26,
                            "helper tree parses and agrees with uiautomator",
                            h.elements.len() > 3 && common * 2 >= ul.len(),
                            format!(
                                "helper {} els, uiautomator {}, {common} texts in common",
                                h.elements.len(),
                                u.elements.len()
                            ),
                        );
                    }
                    _ => r.add(
                        26,
                        "helper tree parses and agrees with uiautomator",
                        false,
                        "a tree did not parse",
                    ),
                },
                _ => r.add(
                    26,
                    "helper tree parses and agrees with uiautomator",
                    false,
                    format!("{:?} / {:?}", hel.err(), uia.err()),
                ),
            }
            let lost = core
                .transport
                .helper_forget_token(s)
                .and_then(|()| core.transport.tree_xml(s, Backend::Helper));
            r.add(
                27,
                "helper re-pushes a lost token on 401",
                lost.as_ref().is_ok_and(|x| x.contains("<hierarchy")),
                "",
            );
            core.backend = Backend::Helper;
            let out = core.call(
                None,
                &Call::UiTree {
                    interactive_only: false,
                    require: vec![],
                    timeout_ms: None,
                },
            );
            core.backend = Backend::Uiautomator;
            r.add(
                28,
                "PHONE_HANDS_TREE=helper is used by ui_tree",
                out.as_ref()
                    .is_ok_and(|o| o.text.contains("tree from helper")),
                out.map(|o| cut(o.text.lines().next().unwrap_or(""), 90))
                    .unwrap_or_default(),
            );
        }
        _ => {
            let why = helper.err().unwrap_or_default();
            for (n, name) in [
                (26, "helper backend"),
                (27, "helper token re-push"),
                (28, "helper used by ui_tree"),
            ] {
                r.skip(n, name, &why);
            }
        }
    }

    if opts.no_input {
        for (n, name) in [
            (29, "tap"),
            (30, "start_activity opens Settings"),
            (31, "Unicode typing reaches the field"),
        ] {
            r.skip(n, name, "--no-input");
        }
        return r;
    }

    // 29: tap (a phone that refuses input must say how to fix it)
    match core.call(
        None,
        &Call::Tap {
            target: Target::Point {
                x: 10.0,
                y: 10.0,
                space: Some("device".into()),
                image_width: None,
            },
        },
    ) {
        Ok(_) => r.add(29, "tap", true, "input allowed"),
        Err(CoreError::InputRefused(h)) => r.add(
            29,
            "tap refused with the Xiaomi hint",
            h.contains("Security settings"),
            cut(&h, 70),
        ),
        Err(e) => r.add(29, "tap", false, e.to_string()),
    }
    // 30
    let act = core.call(
        None,
        &Call::StartActivity {
            component: "com.android.settings/.Settings".into(),
            action: None,
            data: None,
        },
    );
    r.add(
        30,
        "start_activity opens Settings",
        act.as_ref().is_ok_and(|o| o.text.contains("status=ok")),
        act.map(|o| o.text).unwrap_or_else(|e| e.to_string()),
    );
    core.clock().sleep_ms(1500);
    // 31: Unicode typing; the search box is found in the listing and the query read back from the tree
    let listing = core
        .call(
            None,
            &Call::UiTree {
                interactive_only: false,
                require: vec![],
                timeout_ms: None,
            },
        )
        .map(|o| o.text)
        .unwrap_or_default();
    let box_index = listing
        .lines()
        .skip(1)
        .find(|l| l.to_lowercase().contains("search"))
        .and_then(|l| l.strip_prefix('['))
        .and_then(|l| l.split(']').next())
        .and_then(|n| n.parse::<usize>().ok());
    match box_index {
        None => r.add(
            31,
            "Unicode typing",
            false,
            "no search box found in Settings",
        ),
        Some(i) => {
            let _ = core.call(
                None,
                &Call::Tap {
                    target: Target::Index(i),
                },
            );
            core.clock().sleep_ms(1200);
            let typed = core.call(
                None,
                &Call::TypeText {
                    text: "café 東京".into(),
                    index: None,
                    submit: false,
                },
            );
            core.clock().sleep_ms(800);
            let after = core
                .call(
                    None,
                    &Call::UiTree {
                        interactive_only: false,
                        require: vec![],
                        timeout_ms: None,
                    },
                )
                .map(|o| o.text)
                .unwrap_or_default();
            r.add(
                31,
                "Unicode typing reaches the field",
                after.contains("café 東京"),
                typed.map(|o| o.text).unwrap_or_else(|e| e.to_string()),
            );
            let _ = core.call(
                None,
                &Call::Key {
                    name: "back".into(),
                },
            );
            let _ = core.call(
                None,
                &Call::Key {
                    name: "back".into(),
                },
            );
        }
    }
    let _ = core.call(
        None,
        &Call::Key {
            name: "home".into(),
        },
    );
    r
}

fn poco_serial<T: Transport, C: Clock>(core: &mut Core<T, C>) -> Option<String> {
    core.transport
        .devices()
        .ok()?
        .into_iter()
        .find(|d| d.hw_serial == POCO_SERIAL)
        .map(|d| d.serial)
}

/// Extra, beyond the 31: the protected phone is refused for every tool that takes a target, whatever the mode.
pub fn s24_every_tool<T: Transport, C: Clock>(core: &mut Core<T, C>) -> Report {
    let mut r = Report::default();
    for call in Call::samples()
        .into_iter()
        .filter(|c| !matches!(c, Call::ListDevices | Call::Wait { .. }))
    {
        let name = format!("S24 {} refused", call.name());
        refused(&mut r, 0, &name, core, call);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeClock, FakeDevice};

    fn core(dev: FakeDevice) -> Core<FakeDevice, FakeClock> {
        Core::new(dev, FakeClock::new())
    }

    #[test]
    fn thirty_one_of_thirty_one_against_the_fake() {
        let mut c = core(FakeDevice::new());
        let rep = run(&mut c, Options::default());
        let text = rep.render();
        assert!(rep.ok(), "{text}");
        assert_eq!(
            (rep.passed(), rep.total(), rep.skipped()),
            (31, 31, 0),
            "{text}"
        );
        assert_eq!(rep.exit_code(), 0);
        let ns: Vec<usize> = rep.checks.iter().map(|c| c.n).collect();
        assert_eq!(ns, (1..=31).collect::<Vec<_>>());
        assert!(text.contains("31/31 passed"));
    }

    #[test]
    fn s24_checks_never_reach_the_transport() {
        let mut c = core(FakeDevice::new());
        let rep = s24_every_tool(&mut c);
        assert_eq!(rep.total(), 15);
        assert!(rep.ok(), "{}", rep.render());
        // only the device list was ever asked for
        assert!(!c.transport.touched(), "{:?}", c.transport.calls);
        assert!(c.transport.acted.is_empty());
    }

    #[test]
    fn no_input_touches_nothing_acting() {
        let mut c = core(FakeDevice::new());
        let rep = run(&mut c, Options { no_input: true });
        assert!(rep.ok(), "{}", rep.render());
        assert_eq!((rep.total(), rep.skipped()), (27, 4));
        assert!(c.transport.acted.is_empty(), "{:?}", c.transport.acted);
    }

    #[test]
    fn helper_absent_skips_only_its_checks() {
        let mut c = core(FakeDevice {
            helper_installed: false,
            ..FakeDevice::new()
        });
        let rep = run(&mut c, Options { no_input: true });
        assert!(rep.ok(), "{}", rep.render());
        assert_eq!((rep.total(), rep.skipped()), (24, 7));
    }

    #[test]
    fn a_failing_phone_fails_the_run_with_exit_code_1() {
        // an asleep screen cannot be read as a tree and has no search box: Unicode typing fails
        let mut c = core(FakeDevice {
            asleep: true,
            ..FakeDevice::new()
        });
        let rep = run(&mut c, Options::default());
        assert!(!rep.ok());
        assert_eq!(rep.exit_code(), 1);
        assert!(rep.render().contains("FAIL"));
    }

    #[test]
    fn a_phone_that_refuses_input_must_explain_itself() {
        let mut c = core(FakeDevice {
            refuse_input: true,
            ..FakeDevice::new()
        });
        let rep = run(&mut c, Options::default());
        let tap = rep.checks.iter().find(|c| c.n == 29).unwrap();
        assert_eq!(tap.verdict, Verdict::Pass);
        assert!(tap.name.contains("Xiaomi hint"));
    }
}
