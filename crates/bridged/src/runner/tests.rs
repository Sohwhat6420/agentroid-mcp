//! Tests for slice a. Every test runs offline on core's `FakeDevice` (wrapped by [`Phone`] so a
//! screen can change when something is tapped or launched) and a fake clock. No adb, no device.

use super::dryrun::{dry_run, SavedTree, Verdict2};
use super::engine::{
    run, CycleDetector, Hooks, Refused, Resume, ResumeMode, RunOpts, RunReport, Stop,
    CYCLE_THRESHOLD, RELAUNCH_CAP, WALL_CAP_MS,
};
use super::flow::{
    is_rfc3339ish, load_value, load_yaml, parse_yaml_value, validate_schema, Flow, FlowError, Kind,
};
use super::popups::{
    classify, validate_rule, Context, Crop, PauseReason, Rulebook, Screen, Verdict,
};
use super::sink::{
    rfc3339_utc, EventKind, JournalStatus, LeaseHandle, MemoryCheckpoints, MemorySink,
    RegistryLease,
};
use crate::registry::Registry;
use agentroid_adb_link::runner::Clock;
use agentroid_core::fake::{FakeClock, FakeDevice};
use agentroid_core::gate::{DeviceMode, Refusal};
use agentroid_core::transport::{
    Backend, Core, DeviceInfo, Input, Screenshot, Transport, TransportError, POCO_SERIAL,
};
use serde_json::{json, Value};
use std::cell::Cell;
use std::path::PathBuf;

pub(super) const APP: &str = "com.android.settings";
pub(super) const POWER: &str = "com.miui.powerkeeper";

pub(super) fn scaffold() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

// ---------------------------------------------------------------- the scripted fake phone

/// Core's `FakeDevice`, plus screen changes: a tap inside a rectangle shows a new screen, a launch
/// (`monkey`) shows a new screen, or the screen changes by itself after N tree reads.
pub(super) struct Phone {
    pub(super) dev: FakeDevice,
    pub(super) on_tap: Vec<([i32; 4], String)>,
    pub(super) on_launch: Option<String>,
    pub(super) later: Option<(usize, String)>,
    pub(super) reads: usize,
}

impl Phone {
    pub(super) fn new(xml: &str) -> Phone {
        let mut dev = FakeDevice::new();
        dev.xml_override = Some(xml.to_string());
        Phone {
            dev,
            on_tap: vec![],
            on_launch: None,
            later: None,
            reads: 0,
        }
    }
    pub(super) fn inputs(&self) -> usize {
        self.dev.calls.iter().filter(|c| **c == "input").count()
    }
}

impl Transport for Phone {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        self.dev.devices()
    }
    fn screen_size(&mut self, s: &str) -> Result<(i32, i32), TransportError> {
        self.dev.screen_size(s)
    }
    fn screen_state(&mut self, s: &str) -> Result<String, TransportError> {
        self.dev.screen_state(s)
    }
    fn screenshot(&mut self, s: &str, w: i32) -> Result<Screenshot, TransportError> {
        self.dev.screenshot(s, w)
    }
    fn tree_xml(&mut self, s: &str, b: Backend) -> Result<String, TransportError> {
        self.reads += 1;
        if let Some((n, x)) = &self.later {
            if self.reads > *n {
                self.dev.xml_override = Some(x.clone());
            }
        }
        self.dev.tree_xml(s, b)
    }
    fn input(&mut self, s: &str, input: &Input) -> Result<(), TransportError> {
        let r = self.dev.input(s, input);
        if let Input::Tap { x, y, .. } = input {
            if let Some((_, next)) = self
                .on_tap
                .iter()
                .find(|(b, _)| (b[0]..b[2]).contains(x) && (b[1]..b[3]).contains(y))
            {
                self.dev.xml_override = Some(next.clone());
            }
        }
        r
    }
    fn packages(&mut self, s: &str) -> Result<Vec<String>, TransportError> {
        self.dev.packages(s)
    }
    fn shell(&mut self, s: &str, line: &str) -> Result<String, TransportError> {
        let r = self.dev.shell(s, line);
        if line.starts_with("monkey ") {
            if let Some(x) = &self.on_launch {
                self.dev.xml_override = Some(x.clone());
            }
        }
        r
    }
    fn helper_status(&mut self, s: &str) -> Result<(), String> {
        self.dev.helper_status(s)
    }
}

/// A clock that jumps an hour on every sleep (for the 4 h wall cap).
#[derive(Default)]
struct JumpClock {
    now: Cell<u64>,
}

impl Clock for JumpClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
    fn sleep_ms(&self, _ms: u64) {
        self.now.set(self.now.get() + 3_600_000);
    }
}

/// `(package, text, desc, clickable, [l, t, r, b])` nodes under a package-less root.
pub(super) type N<'a> = (&'a str, &'a str, &'a str, bool, [i32; 4]);

pub(super) fn screen(nodes: &[N<'_>]) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('\'', "&apos;")
    };
    let mut s = String::from("<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\"><node class=\"android.widget.FrameLayout\" package=\"\" text=\"\" content-desc=\"\" resource-id=\"\" clickable=\"false\" bounds=\"[0,0][1220,2712]\">");
    for (pkg, text, desc, click, b) in nodes {
        let class = if *click {
            "android.widget.Button"
        } else {
            "android.widget.TextView"
        };
        s += &format!(
            "<node class=\"{class}\" package=\"{pkg}\" text=\"{}\" content-desc=\"{}\" resource-id=\"\" clickable=\"{click}\" bounds=\"[{},{}][{},{}]\"/>",
            esc(text),
            esc(desc),
            b[0],
            b[1],
            b[2],
            b[3]
        );
    }
    s + "</node></hierarchy>"
}

pub(super) fn app_nodes() -> Vec<N<'static>> {
    vec![
        (APP, "Settings", "", false, [48, 200, 400, 280]),
        (APP, "", "Search", true, [48, 600, 1172, 700]),
        (APP, "Bluetooth", "", true, [0, 720, 1220, 880]),
        (APP, "Display", "", true, [0, 900, 1220, 1060]),
    ]
}

pub(super) fn app_screen() -> String {
    screen(&app_nodes())
}

pub(super) fn with_overlay(extra: &[N<'_>]) -> String {
    let mut n = app_nodes();
    n.extend_from_slice(extra);
    screen(&n)
}

pub(super) const GOT_IT: [i32; 4] = [100, 2400, 1120, 2550];

pub(super) fn battery_sheet(pkg: &str) -> String {
    with_overlay(&[
        (pkg, "Battery saver", "", false, [0, 1800, 1220, 1900]),
        (
            pkg,
            "Battery saver is on. Some apps may be limited.",
            "",
            false,
            [0, 1920, 1220, 2300],
        ),
        (pkg, "Got it", "", true, GOT_IT),
    ])
}

pub(super) fn core_on(xml: &str) -> Core<Phone, FakeClock> {
    Core::new(Phone::new(xml), FakeClock::new())
}

pub(super) fn flow_of(steps: Value) -> Flow {
    load_value(json!({"flow": "t", "version": 1, "app": {"package": APP, "version_range": "*"}, "steps": steps})).unwrap()
}

pub(super) fn opts() -> RunOpts {
    let mut o = RunOpts::new("run-1");
    o.start_unix_ms = 1_792_454_400_000;
    o.oem = Some("xiaomi".into());
    o.sdk = Some(34);
    o
}

fn go<C: Clock>(
    core: &mut Core<Phone, C>,
    flow: &Flow,
    rules: &Rulebook,
    opts: RunOpts,
    lease: Option<&mut dyn LeaseHandle>,
    resume: Option<Resume>,
) -> (RunReport, MemorySink, MemoryCheckpoints) {
    let mut sink = MemorySink::default();
    let mut cps = MemoryCheckpoints::default();
    let report = match lease {
        // reborrow so the trait object's lifetime can shrink to the hooks' lifetime
        Some(l) => run(
            core,
            flow,
            rules,
            opts,
            Hooks {
                sink: &mut sink,
                checkpoints: &mut cps,
                lease: Some(&mut *l),
                control: None,
            },
            resume,
        ),
        None => run(
            core,
            flow,
            rules,
            opts,
            Hooks {
                sink: &mut sink,
                checkpoints: &mut cps,
                lease: None,
                control: None,
            },
            resume,
        ),
    };
    (report, sink, cps)
}

fn go_simple(
    core: &mut Core<Phone, FakeClock>,
    flow: &Flow,
) -> (RunReport, MemorySink, MemoryCheckpoints) {
    go(core, flow, &Rulebook::seeded(), opts(), None, None)
}

// ---------------------------------------------------------------- flow files and the schema

#[test]
fn flow_fixtures_agree_with_the_schema() {
    // the rule each invalid fixture breaks, as our validator words it
    let named = [
        (
            "invalid-expires-without-until.json",
            "expires is only allowed with scope until",
        ),
        (
            "invalid-for-each-without-table.json",
            "for_each requires table",
        ),
        (
            "invalid-permanent-below-r2.json",
            "scope permanent needs risk R2",
        ),
        (
            "invalid-permanent-r1-boundary.json",
            "scope permanent needs risk R2",
        ),
        (
            "invalid-push-missing-phone-path.json",
            "push requires phone_path",
        ),
        ("invalid-rel-raw-pixels.json", "is not a fraction 0-1"),
        ("invalid-retry-above-3.json", "/retry: invalid value 4"),
        (
            "invalid-retry-on-r2.json",
            "retry >= 1 is only for risk R0 or R1",
        ),
        ("invalid-risk-above-r5.json", "/risk: invalid value"),
        ("invalid-select-two-keys.json", "exactly one key"),
        (
            "invalid-settings-set-missing-scope.json",
            "settings_set requires scope",
        ),
        ("invalid-unknown-step-kind.json", "/kind: invalid value"),
    ];
    let dir = scaffold().join("schemas").join("fixtures").join("flow");
    let mut seen = 0;
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in &names {
        let text = std::fs::read_to_string(dir.join(name)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        let want_valid = name.starts_with("valid-");
        assert!(want_valid || name.starts_with("invalid-"), "{name}");
        let errs = validate_schema(&v);
        assert_eq!(errs.is_empty(), want_valid, "{name}: {errs:?}");
        // the same object written as YAML and loaded back gives the same value and the same verdict
        let yaml = serde_yaml::to_string(&v).unwrap();
        let back = parse_yaml_value(&yaml).unwrap();
        assert_eq!(back, v, "{name} changed through YAML:\n{yaml}");
        assert_eq!(
            validate_schema(&back).is_empty(),
            want_valid,
            "{name} via YAML"
        );
        if !want_valid {
            let (_, rule) = named
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("no named rule for {name}"));
            assert!(
                errs.iter().any(|e| e.contains(rule)),
                "{name} failed, but not for {rule:?}: {errs:?}"
            );
        }
        seen += 1;
    }
    assert_eq!(seen, 17, "{names:?}");
}

#[test]
fn schema_rules_beyond_the_fixtures() {
    let base = |step: Value| json!({"flow": "f", "version": 1, "app": {"package": "p", "version_range": "*"}, "steps": [step]});
    let ok = |v: &Value| validate_schema(v).is_empty();
    // allOf branches no fixture reaches
    assert!(!ok(&base(json!({"id": "a", "kind": "verify"}))));
    assert!(!ok(&base(json!({"id": "a", "kind": "require"}))));
    assert!(ok(&base(
        json!({"id": "a", "kind": "verify", "selectors": [{"text": "x"}]})
    )));
    assert!(!ok(&base(
        json!({"id": "a", "kind": "settings_set", "namespace": "system", "key": "k", "value": 1, "scope": "job"})
    )));
    assert!(!ok(&base(
        json!({"id": "a", "kind": "settings_set", "namespace": "system", "key": "k", "value": "1", "scope": "until"})
    )));
    assert!(
        ok(&base(
            json!({"id": "a", "kind": "tap", "retry": 0, "risk": "R4"})
        )),
        "retry 0 does not trigger the R0/R1 rule"
    );
    assert!(
        !ok(&base(json!({"id": "a", "kind": "tap", "retry": 1}))),
        "retry needs a risk"
    );
    assert!(
        !ok(&base(
            json!({"id": "a", "kind": "tap", "retry": "1", "risk": "R2"})
        )),
        "a non-number retry fails its type"
    );
    assert!(
        ok(&base(
            json!({"id": "a", "kind": "tap", "retry": 2.0, "risk": "R1"})
        )),
        "2.0 is an integer in 2020-12"
    );
    assert!(!ok(&base(
        json!({"id": "a", "kind": "tap", "retry": 1.5, "risk": "R1"})
    )));
    assert!(!ok(&base(
        json!({"id": "a", "kind": "tap", "expires": "2026-10-21T02:00:00Z"})
    )));
    assert!(!ok(&base(
        json!({"id": "a", "kind": "settings_set", "namespace": "system", "key": "k", "value": "1", "scope": "until", "expires": "2026-10-21 02:00"})
    )));
    assert!(!ok(&base(
        json!({"id": "a", "kind": "tap", "select": {"rel": {"x": 0.5, "y": 0.5, "device": {"w": 0, "h": 10}}}})
    )));
    // `ref` is a select key like the others: one key only, and a string
    assert!(ok(&base(
        json!({"id": "a", "kind": "tap", "select": {"ref": "r3fa91c"}})
    )));
    assert!(!ok(&base(
        json!({"id": "a", "kind": "tap", "select": {"ref": "r3fa91c", "text": "x"}})
    )));
    assert!(!ok(&base(
        json!({"id": "a", "kind": "tap", "select": {"ref": 7}})
    )));
    assert!(!ok(&base(json!({"id": "", "kind": "tap"}))));
    assert!(!ok(&base(json!({"id": "a", "kind": "tap", "extra": 1}))));
    assert!(!ok(
        &json!({"flow": "f", "version": 1.5, "app": {"package": "p", "version_range": "*"}, "steps": [{"id": "a", "kind": "back"}]})
    ));
    assert!(!ok(
        &json!({"flow": "f", "version": 1, "app": {"package": "p", "version_range": "*"}, "steps": []})
    ));
    assert!(!ok(
        &json!({"flow": "f", "version": 1, "app": {"package": "p", "version_range": "*", "x": 1}, "steps": [{"id": "a", "kind": "back"}]})
    ));
    assert!(!ok(
        &json!({"flow": "f", "version": 1, "app": {"package": "p", "version_range": "*"}, "vars": {"secret": ["a", "a"]}, "steps": [{"id": "a", "kind": "back"}]})
    ));
    assert!(!ok(
        &json!({"flow": "f", "version": 1, "app": {"package": "p", "version_range": "*"}, "migratable": "no", "steps": [{"id": "a", "kind": "back"}]})
    ));
    // the timestamp pattern
    for good in [
        "2026-10-21T02:00:00Z",
        "2026-10-21T02:00:00.5+08:00",
        "2026-10-21T02:00:00-05:30",
    ] {
        assert!(is_rfc3339ish(good), "{good}");
    }
    for bad in [
        "2026-10-21T02:00:00",
        "2026-10-21T02:00:00.Z",
        "2026-10-21T02:00:00Z ",
        "2026-10-21 02:00:00Z",
        "2026-10-21T02:00:00+0800",
    ] {
        assert!(!is_rfc3339ish(bad), "{bad}");
    }
}

#[test]
fn yaml_flows_load_and_refuse_what_json_cannot_say() {
    let text = "
# a flow in the 2.10 format
flow: wifi-check
version: 1
app: {package: com.android.settings, version_range: '>=14'}
vars: {who: alex, pw: x, secret: [pw]}
steps:
  - {id: open, kind: tap, select: {text: Bluetooth}, risk: R1}
  - id: loop
    kind: for_each
    table: [{name: Display}, {name: Bluetooth}]
    value:
      - {id: hit, kind: tap, select: {text: '${name}'}, retry: 1, risk: R0}
  - {id: done, kind: assert, select: {text: Settings}}
";
    let f = load_yaml(text).unwrap();
    assert_eq!(f.flow, "wifi-check");
    assert_eq!(f.steps[1].body.len(), 1);
    assert_eq!(f.planned(), 1 + (1 + 2) + 1);
    assert_eq!(f.secrets(), vec!["pw".to_string()]);
    // duplicate keys, tags and non-string keys have no JSON equivalent
    assert!(matches!(
        load_yaml("flow: a\nflow: b\n"),
        Err(FlowError::Yaml(_))
    ));
    assert!(matches!(
        load_yaml("flow: !custom a\n"),
        Err(FlowError::Yaml(_))
    ));
    assert!(matches!(load_yaml("1: a\n"), Err(FlowError::Yaml(_))));
    // schema errors and semantic errors are told apart
    assert!(matches!(load_yaml("flow: a\nversion: 1\napp: {package: p, version_range: '*'}\nsteps: [{id: a, kind: swipe}]\n"), Err(FlowError::Schema(_))));
    let sem = load_yaml("flow: a\nversion: 1\napp: {package: p, version_range: '*'}\nsteps: [{id: a, kind: tap}, {id: a, kind: back}]\n");
    match sem {
        Err(FlowError::Semantic(e)) => assert!(
            e.iter().any(|x| x.contains("tap needs select"))
                && e.iter().any(|x| x.contains("duplicate step id")),
            "{e:?}"
        ),
        other => panic!("{other:?}"),
    }
    // a for_each body is checked against the step schema too (retry on R2 inside a body)
    let body = load_yaml("flow: a\nversion: 1\napp: {package: p, version_range: '*'}\nsteps: [{id: l, kind: for_each, table: [{}], value: [{id: x, kind: tap, select: {text: a}, retry: 2, risk: R2}]}]\n");
    assert!(
        matches!(body, Err(FlowError::Semantic(ref e)) if e.iter().any(|x| x.contains("retry >= 1"))),
        "{body:?}"
    );
}

#[test]
fn popup_fixtures_agree_with_the_schema_and_the_seeded_rulebook_is_valid() {
    let dir = scaffold().join("schemas").join("fixtures").join("popup");
    let mut seen = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(&name)).unwrap()).unwrap();
        assert_eq!(
            validate_rule(&v).is_empty(),
            name.starts_with("valid-"),
            "{name}: {:?}",
            validate_rule(&v)
        );
        seen += 1;
    }
    assert_eq!(seen, 8);
    let book = Rulebook::seeded();
    let has = |pkg: &str, button: &str| {
        book.rules
            .iter()
            .any(|r| r.package == pkg && r.button == button)
    };
    assert!(has(POWER, "Got it"), "HyperOS battery-saver sheet");
    assert!(
        has("com.google.android.permissioncontroller", "Allow"),
        "permission dialogs"
    );
    assert!(has("android", "Wait"), "app isn't responding");
    assert!(
        has("com.android.vending", "Not now") && has("com.android.updater", "Later"),
        "update prompts"
    );
    assert!(
        book.rules
            .iter()
            .any(|r| r.note.as_deref().is_some_and(|n| n.contains("Low storage"))),
        "low storage"
    );
    assert!(
        book.rules.iter().any(|r| r
            .note
            .as_deref()
            .is_some_and(|n| n.contains("security scan"))),
        "post-install security scan"
    );
    // every Allow in the seeded book is a pause rule
    assert!(book
        .rules
        .iter()
        .filter(|r| r.button == "Allow")
        .all(|r| r.action == super::popups::Action::Pause));
    assert!(
        Rulebook::from_yaml("- {package: a, window: {title: t}, button: OK, action: swipe}\n")
            .is_err()
    );
}

// ---------------------------------------------------------------- step kinds

#[test]
fn every_step_kind_runs_on_the_fake() {
    let table: Vec<(Kind, Value, &str)> = vec![
        (
            Kind::Tap,
            json!({"id": "s", "kind": "tap", "select": {"text": "Bluetooth"}}),
            "done",
        ),
        (
            Kind::Type,
            json!({"id": "s", "kind": "type", "select": {"desc": "Search"}, "value": "hi"}),
            "done",
        ),
        (
            Kind::SetValue,
            json!({"id": "s", "kind": "set_value", "select": {"text": "Display"}, "value": 5}),
            "failed",
        ),
        (
            Kind::Wait,
            json!({"id": "s", "kind": "wait", "value": 500}),
            "done",
        ),
        (
            Kind::Assert,
            json!({"id": "s", "kind": "assert", "select": {"text": "Settings"}, "value": "Settings"}),
            "done",
        ),
        (
            Kind::Read,
            json!({"id": "s", "kind": "read", "select": {"text": "Bluetooth"}}),
            "done",
        ),
        (
            Kind::ForEach,
            json!({"id": "s", "kind": "for_each", "table": [{"t": "Bluetooth"}], "value": [{"id": "b", "kind": "tap", "select": {"text": "${t}"}}]}),
            "done",
        ),
        (
            Kind::Checkpoint,
            json!({"id": "s", "kind": "checkpoint", "auto_success": {"text": "Settings"}}),
            "done",
        ),
        (
            Kind::Route,
            json!({"id": "s", "kind": "route", "tier": "intent"}),
            "failed",
        ),
        (
            Kind::Push,
            json!({"id": "s", "kind": "push", "local": "a.jpg", "phone_path": "/sdcard/a.jpg"}),
            "failed",
        ),
        (
            Kind::SettingsSet,
            json!({"id": "s", "kind": "settings_set", "namespace": "system", "key": "k", "value": "1", "scope": "job"}),
            "failed",
        ),
        (
            Kind::Verify,
            json!({"id": "s", "kind": "verify", "selectors": [{"text": "Settings"}, {"text": "Nope"}]}),
            "done",
        ),
        (
            Kind::Require,
            json!({"id": "s", "kind": "require", "selectors": [{"text": "Settings"}], "timeout_ms": 1000}),
            "done",
        ),
        (
            Kind::Scroll,
            json!({"id": "s", "kind": "scroll", "direction": "down"}),
            "done",
        ),
        (Kind::Back, json!({"id": "s", "kind": "back"}), "done"),
    ];
    let kinds: std::collections::HashSet<Kind> = table.iter().map(|t| t.0).collect();
    assert_eq!(
        kinds.len(),
        Kind::ALL.len(),
        "every kind is in the table once"
    );
    for (kind, step, want) in table {
        let mut core = core_on(&app_screen());
        let f = flow_of(json!([step]));
        let (rep, sink, _) = go_simple(&mut core, &f);
        assert_eq!(
            rep.stop.code(),
            want,
            "{}: {}",
            kind.as_str(),
            rep.stop.describe()
        );
        if want == "failed" {
            assert!(
                rep.stop
                    .describe()
                    .contains("not supported by the v0 engine"),
                "{}",
                rep.stop.describe()
            );
            assert_eq!(
                core.transport.inputs(),
                0,
                "{}: an unsupported kind sent input",
                kind.as_str()
            );
        }
        let acted = core.transport.dev.acted.len();
        assert_eq!(
            acted > 0,
            matches!(
                kind,
                Kind::Tap | Kind::Type | Kind::ForEach | Kind::Scroll | Kind::Back
            ),
            "{}: acted {:?}",
            kind.as_str(),
            core.transport.dev.acted
        );
        assert_eq!(
            sink.events.last().map(|e| e.kind),
            Some(if want == "done" {
                EventKind::Done
            } else {
                EventKind::Error
            })
        );
    }
}

#[test]
fn step_details_on_the_fake() {
    // tap lands on the element's centre; type sends the text after focusing; reads are recorded
    let mut core = core_on(&app_screen());
    let f = load_value(json!({"flow": "t", "version": 1, "app": {"package": APP, "version_range": "*"}, "vars": {"who": "alex"},
        "steps": [
            {"id": "tap", "kind": "tap", "select": {"text": "Bluetooth"}, "post": {"kind": "present", "select": {"text": "Bluetooth"}}},
            {"id": "type", "kind": "type", "select": {"desc": "Search"}, "value": "hi ${who}"},
            {"id": "read", "kind": "read", "select": {"text": "Display"}},
            {"id": "verify", "kind": "verify", "selectors": [{"text": "Settings"}, {"text": "Nope"}]},
            {"id": "wait", "kind": "wait", "value": 25000},
            {"id": "until", "kind": "wait", "until": {"text": "Display"}, "timeout_ms": 1000}
        ]}))
    .unwrap();
    let t0 = core.clock().now_ms();
    let (rep, _, _) = go_simple(&mut core, &f);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    let acted = &core.transport.dev.acted;
    assert_eq!(acted[0], "Tap { x: 610, y: 800, display: 0 }");
    assert_eq!(acted[1], "Tap { x: 610, y: 650, display: 0 }");
    assert!(
        acted[2].contains("Text(") && acted[2].contains("alex"),
        "{acted:?}"
    );
    assert_eq!(rep.reads["read"], "Display");
    assert!(
        rep.reads["verify"].contains("text=\"Settings\": yes")
            && rep.reads["verify"].contains("text=\"Nope\": no"),
        "{}",
        rep.reads["verify"]
    );
    assert!(
        core.clock().now_ms() - t0 >= 25_000,
        "the 25 s wait ran on the fake clock"
    );
    assert_eq!(rep.done, 6);
    // a post-condition that never holds fails the step
    let mut core = core_on(&app_screen());
    let f = flow_of(
        json!([{"id": "p", "kind": "tap", "select": {"text": "Bluetooth"}, "post": {"kind": "absent", "select": {"text": "Bluetooth"}}}]),
    );
    let (rep, _, _) = go_simple(&mut core, &f);
    assert!(
        matches!(&rep.stop, Stop::Failed { why, .. } if why.contains("post-condition absent")),
        "{}",
        rep.stop.describe()
    );
    // an assert that does not hold fails; a wrong label fails
    for step in [
        json!({"id": "a", "kind": "assert", "select": {"text": "Checkout"}}),
        json!({"id": "a", "kind": "assert", "select": {"text": "Settings"}, "value": "Home"}),
    ] {
        let mut core = core_on(&app_screen());
        let (rep, _, _) = go_simple(&mut core, &flow_of(json!([step])));
        assert_eq!(rep.stop.code(), "failed", "{}", rep.stop.describe());
    }
}

#[test]
fn retry_repeats_a_failed_step_and_succeeds_when_the_screen_catches_up() {
    let mut core = core_on(&app_screen());
    // each attempt settles with 2 reads; from the 5th read on the screen shows "Ready"
    let ready = with_overlay(&[(APP, "Ready", "", false, [0, 1200, 1220, 1300])]);
    core.transport.later = Some((4, ready));
    let f = flow_of(
        json!([{"id": "r", "kind": "assert", "select": {"text": "Ready"}, "retry": 3, "risk": "R0"}]),
    );
    let (rep, sink, _) = go_simple(&mut core, &f);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert_eq!((rep.executed, rep.retries), (3, 2));
    assert_eq!(
        sink.events
            .iter()
            .filter(|e| e.msg.as_deref().is_some_and(|m| m.contains("retrying")))
            .count(),
        2
    );
}

#[test]
fn for_each_runs_its_body_once_per_row_with_the_row_bound() {
    let mut core = core_on(&app_screen());
    let f = flow_of(
        json!([{"id": "fe", "kind": "for_each", "table": [{"t": "Bluetooth"}, {"t": "Display"}], "value": [{"id": "hit", "kind": "tap", "select": {"text": "${t}"}}]}]),
    );
    assert_eq!(f.planned(), 3);
    let (rep, _, _) = go_simple(&mut core, &f);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert_eq!(
        core.transport.dev.acted,
        vec![
            "Tap { x: 610, y: 800, display: 0 }".to_string(),
            "Tap { x: 610, y: 980, display: 0 }".to_string()
        ]
    );
    assert_eq!((rep.done, rep.executed), (3, 3));
}

#[test]
fn checkpoint_pauses_for_a_person_and_resumes_from_its_checkpoint() {
    let mut core = core_on(&app_screen());
    let f = flow_of(
        json!([{"id": "cp", "kind": "checkpoint", "awaiting_human": true}, {"id": "go", "kind": "tap", "select": {"text": "Bluetooth"}}]),
    );
    let reg = Registry::in_memory();
    reg.acquire(POCO_SERIAL, 3, "job-cp").unwrap();
    let mut lease = RegistryLease {
        registry: &reg,
        device: POCO_SERIAL.into(),
        conn: 3,
    };
    let (rep, sink, cps) = go(
        &mut core,
        &f,
        &Rulebook::seeded(),
        opts(),
        Some(&mut lease),
        None,
    );
    let Stop::Paused(p) = &rep.stop else {
        panic!("{}", rep.stop.describe())
    };
    assert_eq!(p.reason, PauseReason::AwaitingHuman { step: "cp".into() });
    assert_eq!(p.reason.state(), "awaiting_human");
    assert!(core.transport.dev.acted.is_empty());
    assert!(
        cps.saved.iter().any(|c| c.reason == "checkpoint")
            && cps.saved.iter().any(|c| c.reason == "awaiting_human")
    );
    assert_eq!(sink.events.last().unwrap().kind, EventKind::Pause);
    assert!(
        reg.lease_of(POCO_SERIAL).is_some() && !rep.lease_released,
        "a pause keeps the lease"
    );
    // slice b hands the checkpoint back; a person confirmed
    let from = p.checkpoint.clone();
    let mut lease = RegistryLease {
        registry: &reg,
        device: POCO_SERIAL.into(),
        conn: 3,
    };
    let (rep2, sink2, _) = go(
        &mut core,
        &f,
        &Rulebook::seeded(),
        opts(),
        Some(&mut lease),
        Some(Resume {
            from,
            human_confirmed: true,
            mode: ResumeMode::Rerun,
        }),
    );
    assert_eq!(rep2.stop, Stop::Done, "{}", rep2.stop.describe());
    assert_eq!(sink2.events[0].kind, EventKind::Resume);
    assert_eq!(core.transport.dev.acted.len(), 1);
    assert!(
        reg.lease_of(POCO_SERIAL).is_none() && rep2.lease_released,
        "a finished run releases the lease"
    );
    // without a confirmation the checkpoint pauses again
    let mut core = core_on(&app_screen());
    let (rep3, _, _) = go(
        &mut core,
        &f,
        &Rulebook::seeded(),
        opts(),
        None,
        Some(Resume {
            from: p.checkpoint.clone(),
            human_confirmed: false,
            mode: ResumeMode::Rerun,
        }),
    );
    assert_eq!(rep3.stop.code(), "paused");
}

// ---------------------------------------------------------------- read_only

fn acting_step(kind: Kind) -> Value {
    match kind {
        Kind::Tap => json!({"id": "s", "kind": "tap", "select": {"text": "Bluetooth"}}),
        Kind::Type => {
            json!({"id": "s", "kind": "type", "select": {"desc": "Search"}, "value": "hi"})
        }
        Kind::SetValue => {
            json!({"id": "s", "kind": "set_value", "select": {"text": "Display"}, "value": 5})
        }
        Kind::Route => json!({"id": "s", "kind": "route", "tier": "intent"}),
        Kind::Push => {
            json!({"id": "s", "kind": "push", "local": "a.jpg", "phone_path": "/sdcard/a.jpg"})
        }
        Kind::SettingsSet => {
            json!({"id": "s", "kind": "settings_set", "namespace": "system", "key": "k", "value": "1", "scope": "job", "risk": "R1"})
        }
        Kind::Scroll => json!({"id": "s", "kind": "scroll"}),
        Kind::Back => json!({"id": "s", "kind": "back", "risk": "R1"}),
        other => panic!("{} is not acting", other.as_str()),
    }
}

#[test]
fn read_only_refuses_every_acting_step() {
    let mut refused = 0;
    for kind in Kind::ALL.into_iter().filter(|k| k.is_acting()) {
        let mut core = core_on(&app_screen());
        core.set_mode(POCO_SERIAL, DeviceMode::ReadOnly);
        // a read first, so the refusal is not just "nothing ran"
        let f = flow_of(
            json!([{"id": "look", "kind": "read", "select": {"text": "Settings"}}, acting_step(kind)]),
        );
        let (rep, sink, _) = go_simple(&mut core, &f);
        match &rep.stop {
            Stop::Refused {
                step_id,
                why: Refused::Gate(Refusal::ReadOnly { .. }),
            } => assert_eq!(step_id, "s"),
            other => panic!(
                "{} was not refused by read_only: {}",
                kind.as_str(),
                other.describe()
            ),
        }
        assert_eq!(
            rep.reads["look"], "Settings",
            "reads still run on read_only"
        );
        assert!(
            core.transport.dev.acted.is_empty(),
            "{}: {:?}",
            kind.as_str(),
            core.transport.dev.acted
        );
        assert_eq!(core.transport.inputs(), 0);
        assert!(sink.records.iter().any(|r| r.step_id == "s"
            && r.status == JournalStatus::Failed
            && r.intent.contains("read_only")));
        refused += 1;
    }
    assert_eq!(refused, 8);
    // every reading kind still runs on read_only
    let mut core = core_on(&app_screen());
    core.set_mode(POCO_SERIAL, DeviceMode::ReadOnly);
    let f = flow_of(json!([
        {"id": "a", "kind": "assert", "select": {"text": "Settings"}},
        {"id": "r", "kind": "read", "select": {"text": "Bluetooth"}},
        {"id": "v", "kind": "verify", "selectors": [{"text": "Display"}]},
        {"id": "q", "kind": "require", "selectors": [{"text": "Display"}]},
        {"id": "w", "kind": "wait", "until": {"text": "Display"}},
        {"id": "c", "kind": "checkpoint", "auto_success": {"text": "Settings"}}
    ]));
    let (rep, _, _) = go_simple(&mut core, &f);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert!(core.transport.dev.acted.is_empty());
    // and a pop-up cannot be dismissed on read_only either: the tap is refused, the job pauses
    let mut core = core_on(&battery_sheet(POWER));
    core.set_mode(POCO_SERIAL, DeviceMode::ReadOnly);
    let (rep, _, _) = go_simple(
        &mut core,
        &flow_of(json!([{"id": "r", "kind": "read", "select": {"text": "Settings"}}])),
    );
    match &rep.stop {
        Stop::Paused(p) => assert!(
            matches!(&p.reason, PauseReason::DismissFailed { why, .. } if why.contains("read_only")),
            "{:?}",
            p.reason
        ),
        other => panic!("{}", other.describe()),
    }
    assert!(core.transport.dev.acted.is_empty());
}

// ---------------------------------------------------------------- caps

#[test]
fn step_cap_trips_at_exactly_three_times_planned() {
    // planned 1, cap 3; the step would take 4 attempts (1 + retry 3), so the 4th is refused by the cap
    let mut core = core_on(&app_screen());
    let f = flow_of(
        json!([{"id": "never", "kind": "assert", "select": {"text": "Checkout"}, "retry": 3, "risk": "R0"}]),
    );
    let (rep, sink, _) = go_simple(&mut core, &f);
    assert_eq!(
        rep.stop,
        Stop::StepCap {
            executed: 3,
            cap: 3
        },
        "{}",
        rep.stop.describe()
    );
    assert_eq!(rep.executed, 3);
    assert!(sink
        .events
        .last()
        .unwrap()
        .msg
        .as_deref()
        .unwrap()
        .starts_with("step_cap"));
    // the boundary: success on the 3rd attempt (exactly the cap) is allowed
    let mut core = core_on(&app_screen());
    core.transport.later = Some((
        4,
        with_overlay(&[(APP, "Checkout", "", false, [0, 1200, 1220, 1300])]),
    ));
    let (rep, _, _) = go_simple(&mut core, &f);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert_eq!(rep.executed, 3);
}

#[test]
fn wall_cap_trips_at_four_hours_on_the_injected_clock() {
    let mut core = Core::new(Phone::new(&app_screen()), JumpClock::default());
    let f = flow_of(
        json!([{"id": "a", "kind": "back"}, {"id": "b", "kind": "back"}, {"id": "c", "kind": "back"}, {"id": "d", "kind": "back"}, {"id": "e", "kind": "back"}]),
    );
    let o = opts();
    assert_eq!(o.wall_cap_ms, WALL_CAP_MS);
    assert_eq!(WALL_CAP_MS, 4 * 60 * 60 * 1000);
    let (rep, _, _) = go(&mut core, &f, &Rulebook::seeded(), o, None, None);
    match rep.stop {
        Stop::WallCap { elapsed_ms, cap_ms } => {
            assert_eq!(cap_ms, WALL_CAP_MS);
            assert!(elapsed_ms >= WALL_CAP_MS);
        }
        other => panic!("{}", other.describe()),
    }
    assert!(rep.done < 5, "stopped before the end");
}

#[test]
fn app_relaunch_is_capped_at_three() {
    let launcher = screen(&[
        ("com.miui.home", "Settings", "", true, [0, 0, 200, 200]),
        ("com.miui.home", "Camera", "", true, [200, 0, 400, 200]),
    ]);
    // the app comes back on launch: one relaunch, then the flow finishes
    let mut core = core_on(&launcher);
    core.transport.on_launch = Some(app_screen());
    let (rep, sink, _) = go_simple(
        &mut core,
        &flow_of(json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}])),
    );
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert_eq!(rep.relaunches, 1);
    assert_eq!(
        sink.records
            .iter()
            .filter(|r| r.intent.starts_with("relaunch"))
            .count(),
        1
    );
    // it never comes back: 3 relaunches, then the cap
    let mut core = core_on(&launcher);
    let (rep, _, _) = go_simple(
        &mut core,
        &flow_of(json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}])),
    );
    assert_eq!(
        rep.stop,
        Stop::RelaunchCap {
            relaunches: RELAUNCH_CAP
        },
        "{}",
        rep.stop.describe()
    );
    assert_eq!(
        core.transport
            .dev
            .acted
            .iter()
            .filter(|a| a.starts_with("monkey "))
            .count(),
        3
    );
    assert_eq!(core.transport.inputs(), 0, "the step never ran");
}

// ---------------------------------------------------------------- cycle detector

#[test]
fn cycle_detector_trips_on_a_screen_that_keeps_coming_back() {
    // the battery-saver sheet comes back after every dismissal
    let mut core = core_on(&battery_sheet(POWER));
    let (rep, sink, _) = go_simple(
        &mut core,
        &flow_of(json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}])),
    );
    match &rep.stop {
        Stop::Stuck { step_id, seen, .. } => {
            assert_eq!(step_id, "t");
            assert_eq!(*seen, CYCLE_THRESHOLD);
        }
        other => panic!("{}", other.describe()),
    }
    assert_eq!(rep.dismissals.len() as u32, CYCLE_THRESHOLD - 1);
    assert_eq!(sink.events.last().unwrap().kind, EventKind::Stuck);
    // the unit: counts reset only when a new step completes
    let mut c = CycleDetector::new(3);
    assert_eq!((c.see(7), c.see(7)), (1, 2));
    c.progress();
    assert_eq!(c.see(7), 1);
    let n = c.see(7);
    assert!(!c.tripped(n));
    let n = c.see(7);
    assert!(c.tripped(n));
}

// ---------------------------------------------------------------- the pop-up rows

#[test]
fn battery_saver_got_it_is_dismissed_and_logged() {
    let mut core = core_on(&battery_sheet(POWER));
    core.transport.on_tap.push((GOT_IT, app_screen()));
    let (rep, sink, _) = go_simple(
        &mut core,
        &flow_of(json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}])),
    );
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert_eq!(
        core.transport.dev.acted,
        vec![
            "Tap { x: 610, y: 2475, display: 0 }".to_string(),
            "Tap { x: 610, y: 800, display: 0 }".to_string()
        ]
    );
    assert_eq!(rep.dismissals.len(), 1);
    assert_eq!(rep.dismissals[0].package, POWER);
    let entries: Vec<_> = sink
        .records
        .iter()
        .filter(|r| r.intent.starts_with("popup_dismiss"))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "exactly one journal entry per automatic dismissal"
    );
    assert_eq!(entries[0].status, JournalStatus::Done);
    assert!(sink.events.iter().any(|e| e
        .msg
        .as_deref()
        .is_some_and(|m| m.starts_with("pop-up dismissed"))));
}

#[test]
fn the_same_text_in_another_package_pauses() {
    for pkg in ["com.example.cleaner", APP] {
        let mut core = core_on(&battery_sheet(pkg));
        core.transport.on_tap.push((GOT_IT, app_screen()));
        let (rep, _, _) = go_simple(
            &mut core,
            &flow_of(json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}])),
        );
        let Stop::Paused(p) = &rep.stop else {
            panic!("{pkg}: {}", rep.stop.describe())
        };
        assert!(
            matches!(&p.reason, PauseReason::Lookalike { package, .. } if package == pkg),
            "{pkg}: {:?}",
            p.reason
        );
        assert_eq!(p.reason.state(), "needs_attention");
        assert!(
            core.transport.dev.acted.is_empty(),
            "{pkg}: {:?}",
            core.transport.dev.acted
        );
    }
}

#[test]
fn a_permission_allow_pauses() {
    let dialog = with_overlay(&[
        (
            "com.google.android.permissioncontroller",
            "Allow Settings to access this device's location?",
            "",
            false,
            [60, 1500, 1160, 1700],
        ),
        (
            "com.google.android.permissioncontroller",
            "Allow",
            "",
            true,
            [60, 1800, 1160, 1900],
        ),
        (
            "com.google.android.permissioncontroller",
            "Don't allow",
            "",
            true,
            [60, 1950, 1160, 2050],
        ),
    ]);
    let f = flow_of(json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}]));
    let mut core = core_on(&dialog);
    let (rep, _, _) = go_simple(&mut core, &f);
    let Stop::Paused(p) = &rep.stop else {
        panic!("{}", rep.stop.describe())
    };
    assert!(
        matches!(&p.reason, PauseReason::PermissionAllow { button, .. } if button == "Allow"),
        "{:?}",
        p.reason
    );
    assert!(core.transport.dev.acted.is_empty());
    // even a rule that says "tap Allow" (schema fixture valid-tap-by-title.json) pauses
    let fixture: Value = serde_json::from_str(
        &std::fs::read_to_string(scaffold().join("schemas/fixtures/popup/valid-tap-by-title.json"))
            .unwrap(),
    )
    .unwrap();
    let book = Rulebook {
        rules: vec![serde_json::from_value(fixture).unwrap()],
    };
    let sec = with_overlay(&[
        (
            "com.miui.securitycenter",
            "Permission request",
            "",
            false,
            [0, 1500, 1220, 1600],
        ),
        (
            "com.miui.securitycenter",
            "Allow",
            "",
            true,
            [0, 1800, 1220, 1900],
        ),
    ]);
    let mut core = core_on(&sec);
    let (rep, _, _) = go(&mut core, &f, &book, opts(), None, None);
    let Stop::Paused(p) = &rep.stop else {
        panic!("{}", rep.stop.describe())
    };
    assert!(
        matches!(p.reason, PauseReason::PermissionAllow { .. }),
        "{:?}",
        p.reason
    );
    assert!(core.transport.dev.acted.is_empty());
}

#[test]
fn an_unknown_overlay_pauses_keeps_the_lease_and_reports_only_a_crop() {
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
    let reg = Registry::in_memory();
    reg.acquire(POCO_SERIAL, 7, "job-1").unwrap();
    let mut lease = RegistryLease {
        registry: &reg,
        device: POCO_SERIAL.into(),
        conn: 7,
    };
    let mut core = core_on(&ad);
    let (rep, sink, cps) = go(
        &mut core,
        &flow_of(json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}])),
        &Rulebook::seeded(),
        opts(),
        Some(&mut lease),
        None,
    );
    let Stop::Paused(p) = &rep.stop else {
        panic!("{}", rep.stop.describe())
    };
    assert_eq!(
        p.reason,
        PauseReason::NeedsAttention {
            package: "com.unknown.ads".into()
        }
    );
    assert_eq!(p.reason.state(), "needs_attention");
    assert_eq!(
        p.crop,
        Some(Crop {
            l: 100,
            t: 1000,
            r: 1100,
            b: 1450
        }),
        "the overlay's rectangle, nothing else"
    );
    assert!(
        core.transport.dev.acted.is_empty(),
        "nothing tapped: {:?}",
        core.transport.dev.acted
    );
    assert!(
        !core.transport.dev.calls.contains(&"screenshot"),
        "no image is taken for the report"
    );
    assert_eq!(
        reg.lease_of(POCO_SERIAL).map(|l| l.holder),
        Some("job-1".to_string()),
        "the lease is kept"
    );
    assert!(!rep.lease_released);
    assert_eq!(
        cps.saved.last().map(|c| c.reason.as_str()),
        Some("unknown_overlay")
    );
    let last = sink.events.last().unwrap();
    assert_eq!(last.kind, EventKind::Pause);
    assert!(
        last.msg.as_deref().unwrap().contains("needs_attention")
            && last
                .msg
                .as_deref()
                .unwrap()
                .contains("[100,1000][1100,1450]")
    );
    // the classifier alone says the same
    let v = classify(
        &Rulebook::seeded(),
        &Screen {
            xml: &ad,
            activity: None,
        },
        &Context {
            app: APP,
            extra_packages: &[],
            oem: Some("xiaomi"),
            sdk: Some(34),
            auto_dismiss: true,
        },
    );
    assert!(
        matches!(
            v,
            Verdict::Pause {
                reason: PauseReason::NeedsAttention { .. },
                ..
            }
        ),
        "{v:?}"
    );
    // a package the job expects is not an overlay
    let extra = vec!["com.unknown.ads".to_string()];
    let v = classify(
        &Rulebook::seeded(),
        &Screen {
            xml: &ad,
            activity: None,
        },
        &Context {
            app: APP,
            extra_packages: &extra,
            oem: None,
            sdk: None,
            auto_dismiss: true,
        },
    );
    assert_eq!(v, Verdict::Clear);
}

#[test]
fn the_off_switch_makes_the_battery_saver_sheet_pause() {
    let mut core = core_on(&battery_sheet(POWER));
    core.transport.on_tap.push((GOT_IT, app_screen()));
    let mut o = opts();
    o.auto_dismiss = false;
    let (rep, _, _) = go(
        &mut core,
        &flow_of(json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}])),
        &Rulebook::seeded(),
        o,
        None,
        None,
    );
    let Stop::Paused(p) = &rep.stop else {
        panic!("{}", rep.stop.describe())
    };
    assert!(
        matches!(&p.reason, PauseReason::AutoDismissOff { package, .. } if package == POWER),
        "{:?}",
        p.reason
    );
    assert!(core.transport.dev.acted.is_empty());
    assert!(rep.dismissals.is_empty());
}

#[test]
fn a_scoped_rule_needs_a_known_oem_and_sdk() {
    let sheet = battery_sheet(POWER);
    let ctx = |oem, sdk| Context {
        app: APP,
        extra_packages: &[],
        oem,
        sdk,
        auto_dismiss: true,
    };
    let book = Rulebook::seeded();
    assert!(matches!(
        classify(
            &book,
            &Screen {
                xml: &sheet,
                activity: None
            },
            &ctx(Some("Xiaomi"), Some(35))
        ),
        Verdict::Dismiss(_)
    ));
    assert!(matches!(
        classify(
            &book,
            &Screen {
                xml: &sheet,
                activity: None
            },
            &ctx(None, None)
        ),
        Verdict::Pause {
            reason: PauseReason::NeedsAttention { .. },
            ..
        }
    ));
    assert!(matches!(
        classify(
            &book,
            &Screen {
                xml: &sheet,
                activity: None
            },
            &ctx(Some("samsung"), Some(34))
        ),
        Verdict::Pause { .. }
    ));
    // a tap rule whose button is not state-preserving pauses
    let book = Rulebook::from_yaml("- {package: com.miui.powerkeeper, window: {title: Battery saver}, button: Got it, action: tap}\n- {package: x.y, window: {title: Update}, button: Update now, action: tap}\n").unwrap();
    let upd = with_overlay(&[
        ("x.y", "Update", "", false, [0, 1500, 1220, 1600]),
        ("x.y", "Update now", "", true, [0, 1800, 1220, 1900]),
    ]);
    assert!(matches!(
        classify(
            &book,
            &Screen {
                xml: &upd,
                activity: None
            },
            &ctx(None, None)
        ),
        Verdict::Pause {
            reason: PauseReason::NotStatePreserving { .. },
            ..
        }
    ));
    // nothing foreign on screen: clear
    assert_eq!(
        classify(
            &book,
            &Screen {
                xml: &app_screen(),
                activity: None
            },
            &ctx(None, None)
        ),
        Verdict::Clear
    );
}

// ---------------------------------------------------------------- R2+

#[test]
fn r2_and_above_are_refused_before_the_phone_is_touched() {
    for risk in ["R2", "R3", "R4", "R5"] {
        let mut core = core_on(&app_screen());
        let f = flow_of(
            json!([{"id": "ok", "kind": "back", "risk": "R0"}, {"id": "bad", "kind": "tap", "select": {"text": "Bluetooth"}, "risk": risk}]),
        );
        let reg = Registry::in_memory();
        reg.acquire(POCO_SERIAL, 1, "job").unwrap();
        let mut lease = RegistryLease {
            registry: &reg,
            device: POCO_SERIAL.into(),
            conn: 1,
        };
        let (rep, sink, _) = go(
            &mut core,
            &f,
            &Rulebook::seeded(),
            opts(),
            Some(&mut lease),
            None,
        );
        match &rep.stop {
            Stop::Refused {
                step_id,
                why: Refused::Phase2Risk { risk: r },
            } => {
                assert_eq!(step_id, "bad");
                assert_eq!(r.as_str(), risk);
            }
            other => panic!("{risk}: {}", other.describe()),
        }
        assert!(
            core.transport.dev.calls.is_empty(),
            "{risk}: the phone was touched: {:?}",
            core.transport.dev.calls
        );
        assert_eq!(
            sink.records
                .iter()
                .filter(|r| r.status == JournalStatus::Failed && r.step_id == "bad")
                .count(),
            1,
            "one journal entry per refusal"
        );
        assert!(rep.lease_released);
    }
    // inside a for_each body too, and on a read_only device too
    let mut core = core_on(&app_screen());
    core.set_mode(POCO_SERIAL, DeviceMode::ReadOnly);
    let f = flow_of(
        json!([{"id": "fe", "kind": "for_each", "table": [{}], "value": [{"id": "deep", "kind": "back", "risk": "R2"}]}]),
    );
    let (rep, _, _) = go_simple(&mut core, &f);
    assert!(
        matches!(&rep.stop, Stop::Refused { step_id, why: Refused::Phase2Risk { .. } } if step_id == "deep"),
        "{}",
        rep.stop.describe()
    );
    assert!(core.transport.dev.calls.is_empty());
    // the schema fixture with a permanent R2 settings change
    let fixture: Value = serde_json::from_str(
        &std::fs::read_to_string(scaffold().join("schemas/fixtures/flow/valid-permanent-r2.json"))
            .unwrap(),
    )
    .unwrap();
    let f = load_value(fixture).unwrap();
    let mut core = core_on(&app_screen());
    let (rep, _, _) = go_simple(&mut core, &f);
    assert!(
        matches!(&rep.stop, Stop::Refused { step_id, .. } if step_id == "s1"),
        "{}",
        rep.stop.describe()
    );
    assert!(core.transport.dev.calls.is_empty());
}

// ---------------------------------------------------------------- lease, secrets, event shapes

struct LostLease;
impl LeaseHandle for LostLease {
    fn held(&self) -> bool {
        false
    }
    fn release(&mut self) {}
}

#[test]
fn a_lost_lease_stops_before_anything_is_sent() {
    let mut core = core_on(&app_screen());
    let mut lease = LostLease;
    let (rep, _, _) = go(
        &mut core,
        &flow_of(json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}])),
        &Rulebook::seeded(),
        opts(),
        Some(&mut lease),
        None,
    );
    assert_eq!(
        rep.stop,
        Stop::LeaseLost {
            step_id: "t".into()
        }
    );
    assert!(core.transport.dev.acted.is_empty());
}

#[test]
fn typed_text_never_reaches_the_events_or_the_journal() {
    let mut core = core_on(&app_screen());
    let f = load_value(json!({"flow": "t", "version": 1, "app": {"package": APP, "version_range": "*"}, "vars": {"pw": "hunter2", "secret": ["pw"]},
        "steps": [{"id": "pw", "kind": "type", "select": {"desc": "Search"}, "value": "${pw}"}]}))
    .unwrap();
    let (rep, sink, _) = go_simple(&mut core, &f);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert!(
        core.transport
            .dev
            .acted
            .iter()
            .any(|a| a.contains("hunter2")),
        "it was typed"
    );
    let all = format!("{:?}{:?}", sink.events, sink.records);
    assert!(!all.contains("hunter2"), "{all}");
}

/// The shape rules of `schemas/progress.json` and `schemas/journal.json`, checked on real output.
#[test]
fn events_and_journal_records_match_their_schemas() {
    let mut core = core_on(&battery_sheet(POWER));
    core.transport.on_tap.push((GOT_IT, app_screen()));
    let f = flow_of(
        json!([{"id": "t", "kind": "tap", "select": {"text": "Bluetooth"}}, {"id": "r", "kind": "read", "select": {"text": "Display"}}, {"id": "w", "kind": "wait", "value": 300}]),
    );
    let (rep, sink, _) = go_simple(&mut core, &f);
    assert_eq!(rep.stop, Stop::Done);
    let kinds = [
        "step",
        "progress",
        "pause",
        "resume",
        "stuck",
        "taken_over",
        "done",
        "error",
    ];
    let allowed = [
        "v", "ts", "run_id", "seq", "kind", "step_id", "msg", "temp_c", "batt", "eta_s", "display",
    ];
    for (i, ev) in sink.events.iter().enumerate() {
        let v = serde_json::to_value(ev).unwrap();
        let o = v.as_object().unwrap();
        for k in o.keys() {
            assert!(allowed.contains(&k.as_str()), "extra key {k}");
        }
        for k in ["v", "ts", "run_id", "seq", "kind"] {
            assert!(o.contains_key(k), "missing {k}: {v}");
        }
        assert_eq!(o["v"], 1);
        assert!(is_rfc3339ish(o["ts"].as_str().unwrap()), "{v}");
        assert!(!o["run_id"].as_str().unwrap().is_empty());
        assert_eq!(o["seq"].as_u64(), Some(i as u64), "seq counts from 0");
        assert!(kinds.contains(&o["kind"].as_str().unwrap()));
        if let Some(e) = o.get("eta_s") {
            assert!(e.as_f64().unwrap() >= 0.0);
        }
        assert!(o.get("display").is_none_or(|d| d.as_u64().is_some()));
    }
    let statuses = ["planned", "sent", "done", "interrupted", "failed"];
    let required = [
        "v", "run_id", "seq", "step_id", "intent", "post", "status", "ts_phone", "ts_hub", "epoch",
        "display",
    ];
    assert!(!sink.records.is_empty());
    for (i, rec) in sink.records.iter().enumerate() {
        let v = serde_json::to_value(rec).unwrap();
        let o = v.as_object().unwrap();
        assert_eq!(o.len(), required.len(), "{v}");
        for k in required {
            assert!(o.contains_key(k), "missing {k}");
        }
        assert_eq!(o["v"], 1);
        assert_eq!(o["seq"].as_u64(), Some(i as u64));
        assert!(!o["step_id"].as_str().unwrap().is_empty());
        assert!(o["post"]["kind"].is_string());
        assert!(statuses.contains(&o["status"].as_str().unwrap()));
        assert!(
            is_rfc3339ish(o["ts_phone"].as_str().unwrap())
                && is_rfc3339ish(o["ts_hub"].as_str().unwrap())
        );
        assert!(o["epoch"].as_u64().is_some() && o["display"].as_u64().is_some());
        assert!(v.to_string().len() <= 1024, "at most 1 KB per record: {v}");
    }
    // an acting step is journalled planned, sent, done
    let tap: Vec<_> = sink
        .records
        .iter()
        .filter(|r| r.step_id == "t" && !r.intent.starts_with("popup"))
        .map(|r| r.status)
        .collect();
    assert_eq!(
        tap,
        vec![
            JournalStatus::Planned,
            JournalStatus::Sent,
            JournalStatus::Done
        ]
    );
    // timestamps
    assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(rfc3339_utc(1_000_000_000_000), "2001-09-09T01:46:40.000Z");
    assert_eq!(rfc3339_utc(951_782_400_123), "2000-02-29T00:00:00.123Z");
    assert_eq!(sink.events[0].ts.get(..10), Some("2026-10-20"));
}

// ---------------------------------------------------------------- dry run

#[test]
fn dry_run_on_saved_trees_reports_each_match_and_its_selector() {
    let fx = scaffold().join("crates").join("core").join("fixtures");
    let settings = std::fs::read_to_string(fx.join("settings_dump.xml")).unwrap();
    let compose = std::fs::read_to_string(fx.join("compose_dump.xml")).unwrap();
    let trees = [
        SavedTree {
            name: "settings_dump.xml",
            xml: &settings,
        },
        SavedTree {
            name: "compose_dump.xml",
            xml: &compose,
        },
    ];
    let f = flow_of(json!([
        {"id": "bt", "kind": "tap", "select": {"text": "Bluetooth"}},
        {"id": "wifi", "kind": "tap", "select": {"desc": "Wi-Fi"}},
        {"id": "search", "kind": "type", "select": {"id": "search"}, "value": "dark"},
        {"id": "install", "kind": "tap", "select": {"text": "Install"}},
        {"id": "nope", "kind": "assert", "select": {"text": "Checkout"}},
        {"id": "corner", "kind": "tap", "select": {"rel": {"x": 0.5, "y": 0.9}}},
        {"id": "both", "kind": "verify", "selectors": [{"text": "Settings"}, {"id": "wifi"}]},
        {"id": "fe", "kind": "for_each", "table": [{"t": "Bluetooth"}, {"t": "Shizuku"}], "value": [{"id": "row", "kind": "read", "select": {"text": "${t}"}}]},
        {"id": "danger", "kind": "tap", "select": {"text": "Bluetooth"}, "risk": "R3"},
        {"id": "sc", "kind": "scroll"}
    ]));
    let rep = dry_run(&f, &trees, DeviceMode::Full, &Rulebook::seeded());
    let row = |id: &str| {
        rep.rows
            .iter()
            .find(|r| r.step_id == id)
            .unwrap_or_else(|| panic!("no row {id}"))
    };
    match &row("bt").verdict {
        Verdict2::Match {
            tree,
            matched_by,
            status,
            label,
            ..
        } => {
            assert_eq!(
                (tree.as_str(), *matched_by, status.as_str(), label.as_str()),
                ("settings_dump.xml", "text", "unique", "Bluetooth")
            );
        }
        v => panic!("{v:?}"),
    }
    assert_eq!(row("bt").selector, "text=\"Bluetooth\"");
    assert!(
        matches!(
            &row("wifi").verdict,
            Verdict2::Match {
                matched_by: "desc",
                ..
            }
        ),
        "{:?}",
        row("wifi").verdict
    );
    assert!(
        matches!(
            &row("search").verdict,
            Verdict2::Match {
                matched_by: "id",
                ..
            }
        ),
        "{:?}",
        row("search").verdict
    );
    assert!(
        matches!(&row("install").verdict, Verdict2::Match { tree, .. } if tree == "compose_dump.xml"),
        "{:?}",
        row("install").verdict
    );
    assert!(
        matches!(&row("nope").verdict, Verdict2::NoMatch { tried } if tried.len() == 2),
        "{:?}",
        row("nope").verdict
    );
    assert!(
        matches!(&row("corner").verdict, Verdict2::Match { matched_by: "rel", status, .. } if status == "point"),
        "{:?}",
        row("corner").verdict
    );
    let both: Vec<_> = rep.rows.iter().filter(|r| r.step_id == "both").collect();
    assert_eq!(both.len(), 2);
    assert!(both
        .iter()
        .all(|r| matches!(r.verdict, Verdict2::Match { .. })));
    let rows: Vec<_> = rep.rows.iter().filter(|r| r.step_id == "row").collect();
    assert_eq!(rows.len(), 2);
    assert!(
        matches!(&rows[1].verdict, Verdict2::Match { tree, label, .. } if tree == "compose_dump.xml" && label == "Shizuku"),
        "{:?}",
        rows[1].verdict
    );
    assert_eq!(rows[1].row.as_deref(), Some("row 2 of fe"));
    assert!(matches!(&row("danger").verdict, Verdict2::Refused(w) if w.contains("R3")));
    assert_eq!(row("sc").verdict, Verdict2::NoSelector);
    let text = rep.render();
    assert!(text.contains("bt (tap) select=text=\"Bluetooth\": MATCH in settings_dump.xml: matched by text=\"Bluetooth\": unique"), "{text}");
    assert!(
        text.contains("pop-ups in settings_dump.xml: clear"),
        "{text}"
    );
    // on a read_only device every acting step is marked refused; reads still match
    let ro = dry_run(&f, &trees, DeviceMode::ReadOnly, &Rulebook::seeded());
    assert!(ro
        .rows
        .iter()
        .filter(
            |r| ["bt", "wifi", "search", "install", "corner", "sc"].contains(&r.step_id.as_str())
        )
        .all(|r| matches!(&r.verdict, Verdict2::Refused(w) if w.contains("read_only"))));
    assert!(matches!(
        ro.rows
            .iter()
            .find(|r| r.step_id == "nope")
            .unwrap()
            .verdict,
        Verdict2::NoMatch { .. }
    ));
    assert!(ro.matched() >= 4);
}
