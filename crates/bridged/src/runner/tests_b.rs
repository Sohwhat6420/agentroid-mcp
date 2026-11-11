//! Tests for slice b: the journal on disk, crash resume, the receipt, the unreadable-screen fix and
//! the pause/stop hook. Offline: core's fake device (slice a's scripted `Phone`) and a fake clock.
//! Files go to a fresh directory under the OS temp dir per test.

use super::engine::{run, Hooks, Resume, ResumeMode, RunReport, Stop};
use super::flow::{is_rfc3339ish, Flow};
use super::journal::{
    read_checkpoint, read_progress, FileCheckpoints, GroupCommit, JournalWriter, JOURNAL_FILE,
    MAX_RECORD_BYTES,
};
use super::popups::{classify, Context, PauseReason, Rulebook, Screen, Verdict};
use super::receipt::{build, ReceiptInput};
use super::resume::{load_journal, plan, LoadError, Start};
use super::sink::{
    EventKind, Interrupt, JobControl, JournalRecord, JournalStatus, MemoryCheckpoints,
    ProgressEvent, RunSink,
};
use super::tests::{
    app_nodes, app_screen, battery_sheet, flow_of, opts, scaffold, screen, Phone, APP, GOT_IT,
    POWER,
};
use agentroid_core::fake::FakeClock;
use agentroid_core::transport::{
    Backend, Core, DeviceInfo, Input, Screenshot, Transport, TransportError,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const RUN: &str = "run-1";
const BT: [i32; 4] = [0, 720, 1220, 880];
const DISPLAY_TAP: &str = "Tap { x: 610, y: 980, display: 0 }";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("agentroid-2.10b-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The screen after Bluetooth was tapped: the app plus "Connected devices".
fn bt_open() -> String {
    let mut n = app_nodes();
    n.push((APP, "Connected devices", "", false, [0, 1100, 1220, 1200]));
    screen(&n)
}

fn phone(xml: &str) -> Phone {
    let mut p = Phone::new(xml);
    p.on_tap.push((BT, bt_open()));
    p
}

/// Slice a's phone plus a crash recorder: at the `at_input`-th input it keeps the part of the
/// journal file that is known to be fsynced (what a power cut would leave on disk).
struct CrashAt {
    phone: Phone,
    journal: PathBuf,
    durable: Option<Arc<AtomicU64>>,
    at_input: usize,
    inputs: usize,
    snap: Option<Vec<u8>>,
}

impl CrashAt {
    fn new(phone: Phone, at_input: usize) -> CrashAt {
        CrashAt {
            phone,
            journal: PathBuf::new(),
            durable: None,
            at_input,
            inputs: 0,
            snap: None,
        }
    }
}

impl Transport for CrashAt {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        self.phone.devices()
    }
    fn screen_size(&mut self, s: &str) -> Result<(i32, i32), TransportError> {
        self.phone.screen_size(s)
    }
    fn screen_state(&mut self, s: &str) -> Result<String, TransportError> {
        self.phone.screen_state(s)
    }
    fn screenshot(&mut self, s: &str, w: i32) -> Result<Screenshot, TransportError> {
        self.phone.screenshot(s, w)
    }
    fn tree_xml(&mut self, s: &str, b: Backend) -> Result<String, TransportError> {
        self.phone.tree_xml(s, b)
    }
    fn input(&mut self, s: &str, input: &Input) -> Result<(), TransportError> {
        self.inputs += 1;
        if self.inputs == self.at_input {
            let bytes = std::fs::read(&self.journal).unwrap_or_default();
            let n = self
                .durable
                .as_ref()
                .map_or(0, |d| d.load(Ordering::SeqCst) as usize)
                .min(bytes.len());
            self.snap = Some(bytes[..n].to_vec());
        }
        self.phone.input(s, input)
    }
    fn packages(&mut self, s: &str) -> Result<Vec<String>, TransportError> {
        self.phone.packages(s)
    }
    fn shell(&mut self, s: &str, line: &str) -> Result<String, TransportError> {
        self.phone.shell(s, line)
    }
    fn helper_status(&mut self, s: &str) -> Result<(), String> {
        self.phone.helper_status(s)
    }
}

fn writer(dir: &Path) -> JournalWriter {
    JournalWriter::open(dir, RUN, GroupCommit::default()).unwrap()
}

fn go_disk<T: Transport>(
    core: &mut Core<T, FakeClock>,
    flow: &Flow,
    w: &mut JournalWriter,
    resume: Option<Resume>,
    control: Option<&dyn JobControl>,
) -> RunReport {
    let mut cps = FileCheckpoints::new(w.dir());
    run(
        core,
        flow,
        &Rulebook::seeded(),
        opts(),
        Hooks {
            sink: w,
            checkpoints: &mut cps,
            lease: None,
            control,
        },
        resume,
    )
}

fn records_of(bytes: &[u8]) -> Vec<JournalRecord> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect()
}

fn journal_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join(RUN).join(JOURNAL_FILE)).unwrap()
}

/// Run `flow` on `xml` and return the fsynced journal prefix at the `at_input`-th tap.
fn crash_journal(flow: &Flow, xml: &str, at_input: usize) -> Vec<u8> {
    let dir = tmp(&format!("crashsrc-{}-{at_input}", flow.steps[0].id));
    let mut core = Core::new(CrashAt::new(phone(xml), at_input), FakeClock::new());
    let mut w = writer(&dir);
    core.transport.durable = Some(w.durable_handle());
    core.transport.journal = w.journal_path();
    let _ = go_disk(&mut core, flow, &mut w, None, None);
    core.transport
        .snap
        .clone()
        .expect("the crash point was reached")
}

/// Put `bytes` in a fresh run directory as the journal a crashed daemon left behind.
fn crashed_dir(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = tmp(name);
    std::fs::create_dir_all(dir.join(RUN)).unwrap();
    std::fs::write(dir.join(RUN).join(JOURNAL_FILE), bytes).unwrap();
    dir
}

fn resume_from(dir: &Path) -> Resume {
    let loaded = load_journal(&dir.join(RUN).join(JOURNAL_FILE)).unwrap();
    let cp = read_checkpoint(&dir.join(RUN)).unwrap();
    match plan(RUN, &loaded.records, cp.as_ref(), false) {
        Start::From(r) => r,
        Start::Fresh => panic!("nothing durable to resume from"),
    }
}

fn tap_flow() -> Flow {
    flow_of(json!([
        {"id": "bt", "kind": "tap", "select": {"text": "Bluetooth"}, "post": {"kind": "present", "select": {"text": "Connected devices"}}},
        {"id": "disp", "kind": "tap", "select": {"text": "Display"}}
    ]))
}

// ---------------------------------------------------------------- the intent is durable before the send; resume re-verifies

#[test]
fn intent_is_fsynced_before_the_tap_and_a_crash_there_is_reverified_not_resent() {
    let f = tap_flow();
    // the crash point: the moment the first tap reaches the phone
    let snap = crash_journal(&f, &app_screen(), 1);
    let recs = records_of(&snap);
    let bt: Vec<JournalStatus> = recs
        .iter()
        .filter(|r| r.step_id == "bt")
        .map(|r| r.status)
        .collect();
    assert_eq!(
        bt,
        vec![JournalStatus::Planned, JournalStatus::Sent],
        "planned + sent must be on disk (fsynced) before the tap is sent: {recs:?}"
    );
    assert!(
        recs.iter().all(|r| r.status != JournalStatus::Done),
        "nothing after the send is durable yet"
    );

    // (a) the crash came before the tap reached the phone: the post-condition does not hold
    let dir = crashed_dir("crash-a", &snap);
    let res = resume_from(&dir);
    assert_eq!(
        (res.mode, res.from.path.clone(), res.from.step_id.as_str()),
        (ResumeMode::Reverify, vec![0], "bt")
    );
    let mut core = Core::new(phone(&app_screen()), FakeClock::new());
    let mut w = writer(&dir);
    assert_eq!(w.next_seq(), 2, "sequence numbers continue after the crash");
    let rep = go_disk(&mut core, &f, &mut w, Some(res), None);
    drop(w);
    let Stop::Paused(p) = &rep.stop else {
        panic!("{}", rep.stop.describe())
    };
    assert!(
        matches!(&p.reason, PauseReason::UnverifiedIntent { step, .. } if step == "bt"),
        "{:?}",
        p.reason
    );
    assert_eq!(p.reason.state(), "needs_attention");
    assert_eq!(
        core.transport.inputs(),
        0,
        "an intent-only step is never re-sent blindly"
    );
    let after = records_of(&journal_bytes(&dir));
    assert_eq!(
        after.last().map(|r| (r.step_id.as_str(), r.status)),
        Some(("bt", JournalStatus::Interrupted))
    );
    assert!(
        after.iter().enumerate().all(|(i, r)| r.seq == i as u64),
        "one file, seq 0, 1, 2, ..."
    );
    assert_eq!(
        read_checkpoint(&dir.join(RUN)).unwrap().map(|c| c.reason),
        Some("unverified_intent".into())
    );

    // (b) the tap landed before the crash: the post-condition holds, so bt counts as done and is not re-sent
    let dir = crashed_dir("crash-b", &snap);
    let res = resume_from(&dir);
    let mut core = Core::new(phone(&bt_open()), FakeClock::new());
    let mut w = writer(&dir);
    let rep = go_disk(&mut core, &f, &mut w, Some(res), None);
    drop(w);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert_eq!(
        core.transport.dev.acted,
        vec![DISPLAY_TAP.to_string()],
        "only the next step was sent"
    );
    let after = records_of(&journal_bytes(&dir));
    assert!(
        after.iter().any(|r| r.step_id == "bt"
            && r.status == JournalStatus::Done
            && r.intent.contains("re-verified")),
        "{after:?}"
    );
    assert_eq!(rep.done, 2);
}

#[test]
fn reverify_uses_the_next_verify_step_and_pauses_when_there_is_nothing_to_check() {
    // no post-condition, but a verify step right after: its selectors decide
    let f = flow_of(json!([
        {"id": "x", "kind": "tap", "select": {"text": "Bluetooth"}},
        {"id": "chk", "kind": "verify", "selectors": [{"text": "Connected devices"}]}
    ]));
    let snap = crash_journal(&f, &app_screen(), 1);
    let dir = crashed_dir("verify-next", &snap);
    let mut core = Core::new(phone(&bt_open()), FakeClock::new());
    let mut w = writer(&dir);
    let rep = go_disk(&mut core, &f, &mut w, Some(resume_from(&dir)), None);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert_eq!(core.transport.inputs(), 0);

    // neither a post-condition nor a verify/require step: pause with needs_attention, send nothing
    let f = flow_of(
        json!([{"id": "y", "kind": "tap", "select": {"text": "Bluetooth"}}, {"id": "disp", "kind": "tap", "select": {"text": "Display"}}]),
    );
    let snap = crash_journal(&f, &app_screen(), 1);
    let dir = crashed_dir("verify-none", &snap);
    let mut core = Core::new(phone(&bt_open()), FakeClock::new());
    let mut w = writer(&dir);
    let rep = go_disk(&mut core, &f, &mut w, Some(resume_from(&dir)), None);
    let Stop::Paused(p) = &rep.stop else {
        panic!("{}", rep.stop.describe())
    };
    assert!(
        matches!(&p.reason, PauseReason::UnverifiedIntent { why, .. } if why.contains("no post-condition")),
        "{:?}",
        p.reason
    );
    assert_eq!(p.reason.state(), "needs_attention");
    assert_eq!(core.transport.inputs(), 0);
}

// ---------------------------------------------------------------- torn tail, corrupt middle

fn full_journal(name: &str) -> Vec<u8> {
    let f = flow_of(
        json!([{"id": "bt", "kind": "tap", "select": {"text": "Bluetooth"}}, {"id": "disp", "kind": "tap", "select": {"text": "Display"}}]),
    );
    let dir = tmp(name);
    let mut core = Core::new(phone(&app_screen()), FakeClock::new());
    let mut w = writer(&dir);
    assert_eq!(go_disk(&mut core, &f, &mut w, None, None).stop, Stop::Done);
    drop(w);
    journal_bytes(&dir)
}

fn first_lines(bytes: &[u8], n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for l in bytes.split_inclusive(|b| *b == b'\n').take(n) {
        out.extend_from_slice(l);
    }
    out
}

#[test]
fn a_corrupt_tail_is_truncated_to_the_last_good_record_and_resume_continues() {
    let full = full_journal("tail-src");
    assert_eq!(
        records_of(&full).len(),
        6,
        "planned/sent/done for each of 2 taps"
    );
    let good = first_lines(&full, 3); // bt planned, sent, done
    for (name, torn) in [
        (
            "torn",
            &b"{\"v\":1,\"run_id\":\"run-1\",\"seq\":3,\"step_id\":\"disp\",\"int"[..],
        ),
        ("garbage", &b"\x00\x00not json at all\n"[..]),
    ] {
        let mut bytes = good.clone();
        bytes.extend_from_slice(torn);
        let dir = crashed_dir(&format!("tail-{name}"), &bytes);
        let path = dir.join(RUN).join(JOURNAL_FILE);
        let loaded = load_journal(&path).unwrap();
        assert_eq!(loaded.records.len(), 3, "{name}");
        assert_eq!(loaded.truncated_bytes, torn.len() as u64, "{name}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            good,
            "{name}: truncated to the last good record"
        );
        let res = resume_from(&dir);
        assert_eq!(
            (res.mode, res.from.path.clone()),
            (ResumeMode::Skip, vec![0]),
            "{name}: bt is done, continue after it"
        );
        let f = flow_of(
            json!([{"id": "bt", "kind": "tap", "select": {"text": "Bluetooth"}}, {"id": "disp", "kind": "tap", "select": {"text": "Display"}}]),
        );
        let mut core = Core::new(phone(&app_screen()), FakeClock::new());
        let mut w = writer(&dir);
        let rep = go_disk(&mut core, &f, &mut w, Some(res), None);
        drop(w);
        assert_eq!(rep.stop, Stop::Done, "{name}: {}", rep.stop.describe());
        assert_eq!(
            core.transport.dev.acted,
            vec![DISPLAY_TAP.to_string()],
            "{name}: bt is not sent again"
        );
        let after = load_journal(&path).unwrap();
        assert_eq!(
            (after.records.len(), after.truncated_bytes),
            (6, 0),
            "{name}"
        );
    }
}

#[test]
fn a_corrupt_middle_refuses_the_resume_and_keeps_the_file() {
    let full = full_journal("middle-src");
    let lines: Vec<&[u8]> = full.split_inclusive(|b| *b == b'\n').collect();
    // a damaged record, and (separately) a record missing from the middle
    let mut damaged = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        damaged.extend_from_slice(if i == 1 {
            b"{\"v\":1,\"run_id\":\"run-1\",XXXX\n"
        } else {
            l
        });
    }
    let mut missing = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if i != 2 {
            missing.extend_from_slice(l);
        }
    }
    for (name, bytes, line) in [("damaged", damaged, 2), ("missing", missing, 3)] {
        let dir = crashed_dir(&format!("middle-{name}"), &bytes);
        let path = dir.join(RUN).join(JOURNAL_FILE);
        match load_journal(&path) {
            Err(LoadError::CorruptMiddle { line: l, .. }) => assert_eq!(l, line, "{name}"),
            other => panic!("{name}: {other:?}"),
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "{name}: the file is kept exactly as it was"
        );
        let e = JournalWriter::open(&dir, RUN, GroupCommit::default())
            .err()
            .expect("the writer refuses too");
        assert!(e.contains("corrupt"), "{e}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "{name}: still untouched"
        );
    }
}

// ---------------------------------------------------------------- on-disk shape, group commit, durability failure

/// A JSON Schema check for the keywords the workspace's schemas use. It reads the schema file
/// itself and refuses any keyword it does not implement, so it can never pass by ignoring a rule.
fn schema_errors(schema: &Value, root: &Value, v: &Value, at: &str, errs: &mut Vec<String>) {
    let Some(o) = schema.as_object() else {
        errs.push(format!("{at}: schema is not an object"));
        return;
    };
    for (k, rule) in o {
        match k.as_str() {
            "$schema" | "$id" | "title" | "description" | "$defs" | "properties" => {}
            "$ref" => {
                let name = rule
                    .as_str()
                    .and_then(|r| r.strip_prefix("#/$defs/"))
                    .unwrap_or("");
                match root.get("$defs").and_then(|d| d.get(name)) {
                    Some(sub) => schema_errors(sub, root, v, at, errs),
                    None => errs.push(format!("{at}: unresolved $ref {rule}")),
                }
            }
            "type" => {
                let types: Vec<&str> = match rule {
                    Value::String(s) => vec![s.as_str()],
                    Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                    _ => vec![],
                };
                let ok = types.iter().any(|t| match *t {
                    "object" => v.is_object(),
                    "array" => v.is_array(),
                    "string" => v.is_string(),
                    "boolean" => v.is_boolean(),
                    "number" => v.is_number(),
                    "integer" => {
                        v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0)
                    }
                    "null" => v.is_null(),
                    _ => false,
                });
                if !ok {
                    errs.push(format!("{at}: not of type {rule}"));
                }
            }
            "required" => {
                for r in rule
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if v.as_object().is_some_and(|m| !m.contains_key(r)) {
                        errs.push(format!("{at}: missing {r}"));
                    }
                }
            }
            "additionalProperties" => {
                if let (Some(m), Some(false)) = (v.as_object(), rule.as_bool()) {
                    let props = o.get("properties").and_then(Value::as_object);
                    for key in m.keys() {
                        if !props.is_some_and(|p| p.contains_key(key)) {
                            errs.push(format!("{at}: extra property {key}"));
                        }
                    }
                } else if rule.as_bool() != Some(false) {
                    errs.push(format!("{at}: unsupported additionalProperties {rule}"));
                }
            }
            "minimum" => {
                if v.as_f64().zip(rule.as_f64()).is_some_and(|(x, m)| x < m) {
                    errs.push(format!("{at}: below {rule}"));
                }
            }
            "maximum" => {
                if v.as_f64().zip(rule.as_f64()).is_some_and(|(x, m)| x > m) {
                    errs.push(format!("{at}: above {rule}"));
                }
            }
            "minLength" => {
                if v.as_str()
                    .zip(rule.as_u64())
                    .is_some_and(|(s, n)| (s.chars().count() as u64) < n)
                {
                    errs.push(format!("{at}: shorter than {rule}"));
                }
            }
            "const" => {
                if v != rule {
                    errs.push(format!("{at}: not {rule}"));
                }
            }
            "enum" => {
                if !rule.as_array().is_some_and(|a| a.contains(v)) {
                    errs.push(format!("{at}: {v} not in {rule}"));
                }
            }
            "items" => {
                for (i, item) in v.as_array().into_iter().flatten().enumerate() {
                    schema_errors(rule, root, item, &format!("{at}/{i}"), errs);
                }
            }
            "pattern" => {
                // the only pattern in these schemas is the RFC 3339 timestamp; slice a implements it
                let ts =
                    "^\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}(\\.\\d+)?(Z|[+-]\\d{2}:\\d{2})$";
                if rule.as_str() != Some(ts) {
                    errs.push(format!("{at}: unsupported pattern {rule}"));
                } else if v.as_str().is_some_and(|s| !is_rfc3339ish(s)) {
                    errs.push(format!("{at}: does not match the timestamp pattern"));
                }
            }
            other => errs.push(format!("{at}: unsupported keyword {other}")),
        }
    }
    if let (Some(props), Some(m)) = (
        o.get("properties").and_then(Value::as_object),
        v.as_object(),
    ) {
        for (k, sub) in props {
            if let Some(x) = m.get(k) {
                schema_errors(sub, root, x, &format!("{at}/{k}"), errs);
            }
        }
    }
}

fn schema(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(scaffold().join("schemas").join(name)).unwrap())
        .unwrap()
}

fn validate(schema: &Value, v: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    schema_errors(schema, schema, v, "", &mut errs);
    errs
}

#[test]
fn the_schema_check_agrees_with_every_fixture() {
    for name in ["receipt", "journal", "progress"] {
        let s = schema(&format!("{name}.json"));
        let dir = scaffold().join("schemas").join("fixtures").join(name);
        let mut seen = 0;
        for e in std::fs::read_dir(&dir).unwrap() {
            let file = e.unwrap().file_name().to_string_lossy().into_owned();
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join(&file)).unwrap()).unwrap();
            let errs = validate(&s, &v);
            assert_eq!(
                errs.is_empty(),
                file.starts_with("valid-"),
                "{name}/{file}: {errs:?}"
            );
            seen += 1;
        }
        assert!(seen >= 4, "{name}: {seen} fixtures");
    }
}

#[test]
fn the_files_on_disk_match_their_schemas_and_commits_are_grouped() {
    let dir = tmp("ondisk");
    let mut core = Core::new(phone(&battery_sheet(POWER)), FakeClock::new());
    core.transport.on_tap.push((GOT_IT, app_screen()));
    let f = flow_of(json!([
        {"id": "a", "kind": "assert", "select": {"text": "Settings"}},
        {"id": "bt", "kind": "tap", "select": {"text": "Bluetooth"}, "post": {"kind": "present", "select": {"text": "Connected devices"}}},
        {"id": "r", "kind": "read", "select": {"text": "Connected devices"}},
        {"id": "disp", "kind": "tap", "select": {"text": "Display"}},
        {"id": "s", "kind": "scroll"}
    ]));
    let mut w = writer(&dir);
    let rep = go_disk(&mut core, &f, &mut w, None, None);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    let stats = w.stats.clone();
    drop(w);
    let (js, ps) = (schema("journal.json"), schema("progress.json"));
    let jtext = std::fs::read_to_string(dir.join(RUN).join(JOURNAL_FILE)).unwrap();
    let lines: Vec<&str> = jtext.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        assert!(
            l.len() <= MAX_RECORD_BYTES,
            "record {i} is {} bytes",
            l.len()
        );
        let v: Value = serde_json::from_str(l).unwrap();
        assert_eq!(validate(&js, &v), Vec::<String>::new(), "{l}");
        assert_eq!(v["seq"], i);
    }
    let events = read_progress(&dir.join(RUN));
    assert!(events.len() >= 6);
    for (i, e) in events.iter().enumerate() {
        let v = serde_json::to_value(e).unwrap();
        assert_eq!(validate(&ps, &v), Vec::<String>::new(), "{v}");
        assert_eq!(e.seq, i as u64);
    }
    // 1 dismissal + 3 acting steps x 3 records, in fewer commits than lines
    assert_eq!(lines.len(), 10);
    assert!(
        stats.commits < stats.lines,
        "group commit: {} commits for {} lines",
        stats.commits,
        stats.lines
    );
    eprintln!(
        "journal commit (write + fsync): p50 {} us, p99 {} us over {} commits",
        stats.quantile_us(0.5),
        stats.quantile_us(0.99),
        stats.commits
    );
}

#[test]
fn a_sink_that_cannot_make_the_intent_durable_stops_the_step_unsent() {
    struct NoDisk;
    impl RunSink for NoDisk {
        fn progress(&mut self, _: &ProgressEvent) {}
        fn journal(&mut self, rec: &JournalRecord) -> Result<(), String> {
            if rec.status == JournalStatus::Sent {
                Err("disk full".into())
            } else {
                Ok(())
            }
        }
    }
    let mut core = Core::new(phone(&app_screen()), FakeClock::new());
    let (mut sink, mut cps) = (NoDisk, MemoryCheckpoints::default());
    let f = flow_of(json!([{"id": "bt", "kind": "tap", "select": {"text": "Bluetooth"}}]));
    let rep = run(
        &mut core,
        &f,
        &Rulebook::seeded(),
        opts(),
        Hooks {
            sink: &mut sink,
            checkpoints: &mut cps,
            lease: None,
            control: None,
        },
        None,
    );
    assert!(
        matches!(&rep.stop, Stop::Failed { why, .. } if why.contains("not durable") && why.contains("disk full")),
        "{}",
        rep.stop.describe()
    );
    assert_eq!(core.transport.inputs(), 0);
}

// ---------------------------------------------------------------- the receipt

#[test]
fn the_receipt_validates_against_receipt_json_and_carries_the_panel_fields() {
    let dir = tmp("receipt");
    let mut core = Core::new(phone(&battery_sheet(POWER)), FakeClock::new());
    core.transport.on_tap.push((GOT_IT, app_screen()));
    let f = flow_of(json!([
        {"id": "bt", "kind": "tap", "select": {"text": "Bluetooth"}},
        {"id": "late", "kind": "assert", "select": {"text": "Never there"}, "retry": 1, "risk": "R0"},
        {"id": "disp", "kind": "tap", "select": {"text": "Display"}}
    ]));
    let mut w = writer(&dir);
    let rep = go_disk(&mut core, &f, &mut w, None, None);
    drop(w);
    assert_eq!(rep.stop.code(), "failed");
    let records = load_journal(&dir.join(RUN).join(JOURNAL_FILE))
        .unwrap()
        .records;
    let events = read_progress(&dir.join(RUN));
    let inp = ReceiptInput {
        run_id: RUN,
        device: "DEVICE0000EXAMPLE",
        flow: &f.flow,
        outcome: "failed",
        planned: rep.planned,
        done: rep.done,
        retries: rep.retries,
        retried_steps: rep.retried_steps,
        wall_ms: 12_340,
        waiting_ms: 2_000,
        cooling_ms: 0,
        laptop_cpu_ms: Some(120),
        errors: vec![rep.stop.describe()],
        records: &records,
        events: &events,
    };
    let r = build(&inp);
    assert_eq!(
        validate(&schema("receipt.json"), &r.card),
        Vec::<String>::new(),
        "{}",
        r.card
    );
    assert_eq!(
        (
            r.card["steps_planned"].as_u64(),
            r.card["steps_done"].as_u64(),
            r.card["retries"].as_u64()
        ),
        (Some(3), Some(1), Some(1))
    );
    assert_eq!(r.card["model_calls"], 0);
    let d = &r.detail;
    assert_eq!(d["outcome"], "failed");
    assert_eq!(
        d["steps"],
        json!({"planned": 3, "done": 1, "retried": 1, "not_run": 2})
    );
    assert_eq!(
        d["time_s"],
        json!({"wall": 12.3, "work": 10.3, "cooling": 0.0, "waiting": 2.0})
    );
    let pops = d["popups_dismissed"].as_array().unwrap();
    assert_eq!(pops.len(), 1, "{d}");
    assert_eq!(pops[0]["package"], POWER);
    assert!(pops[0]["rule"].as_str().unwrap().contains("Got it"));
    assert_eq!(d["settings_changed"], json!([]));
    assert_eq!(
        d["measured"]["phone_peak_c"], false,
        "v0 reads no temperature, and says so"
    );
    assert!(
        r.card["phone_peak_c"].is_null() && r.card["battery_change"].is_null(),
        "unmeasured is null, never 0: {}",
        r.card
    );
    // the Python check (test_runner_receipt.py) validates the same card with the real jsonschema package
    let out = scaffold().join("generated").join("receipt-sample.json");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(
        &out,
        serde_json::to_string_pretty(&r.to_value()).unwrap() + "\n",
    )
    .unwrap();
}

// ---------------------------------------------------------------- the unreadable-screen fix (slice-a review follow-up 1)

#[test]
fn an_unparseable_screen_pauses_with_unreadable_screen() {
    let ctx = Context {
        app: APP,
        extra_packages: &[],
        oem: Some("xiaomi"),
        sdk: Some(34),
        auto_dismiss: true,
    };
    for xml in [
        "",
        "not xml",
        "<hierarchy><node package=\"com.evil\"",
        "<?xml version='1.0'?><hierarchy rotation=\"0\"><node bounds=\"[0,0][1,1]\"",
    ] {
        match classify(
            &Rulebook::seeded(),
            &Screen {
                xml,
                activity: None,
            },
            &ctx,
        ) {
            Verdict::Pause {
                reason: PauseReason::UnreadableScreen { .. },
                crop: None,
            } => {}
            other => {
                panic!("{xml:?} gave {other:?}; an unreadable tree must pause, never be clear")
            }
        }
    }
    assert_eq!(
        PauseReason::UnreadableScreen { why: String::new() }.detail(),
        "unreadable_screen"
    );
    // and in the engine: nothing is tapped on a screen that cannot be read
    let mut core = Core::new(
        phone("<hierarchy><node text=\"Bluetooth\""),
        FakeClock::new(),
    );
    let f = flow_of(json!([{"id": "bt", "kind": "tap", "select": {"rel": {"x": 0.5, "y": 0.3}}}]));
    let dir = tmp("unreadable");
    let mut w = writer(&dir);
    let rep = go_disk(&mut core, &f, &mut w, None, None);
    drop(w);
    let Stop::Paused(p) = &rep.stop else {
        panic!("{}", rep.stop.describe())
    };
    assert!(
        matches!(p.reason, PauseReason::UnreadableScreen { .. }),
        "{:?}",
        p.reason
    );
    assert_eq!(core.transport.inputs(), 0);
    assert!(read_progress(&dir.join(RUN))
        .iter()
        .any(|e| e.kind == EventKind::Pause
            && e.msg
                .as_deref()
                .is_some_and(|m| m.contains("unreadable_screen"))));
}

// ---------------------------------------------------------------- job.pause / job.stop through the hook

#[test]
fn the_control_hook_pauses_or_stops_before_the_next_step() {
    struct Ask(Interrupt);
    impl JobControl for Ask {
        fn interrupt(&self) -> Option<Interrupt> {
            Some(self.0)
        }
    }
    let f = tap_flow();
    for (ask, code) in [
        (Interrupt::Pause, "paused"),
        (Interrupt::Stop, "stopped"),
        (Interrupt::TakeOver, "paused"),
    ] {
        let dir = tmp(&format!("control-{ask:?}"));
        let mut core = Core::new(phone(&app_screen()), FakeClock::new());
        let mut w = writer(&dir);
        let rep = go_disk(&mut core, &f, &mut w, None, Some(&Ask(ask)));
        drop(w);
        assert_eq!(rep.stop.code(), code);
        assert_eq!(core.transport.inputs(), 0);
        let events = read_progress(&dir.join(RUN));
        let last = events.last().unwrap();
        match ask {
            Interrupt::Pause => {
                assert_eq!(last.kind, EventKind::Pause);
                assert_eq!(
                    read_checkpoint(&dir.join(RUN)).unwrap().map(|c| c.reason),
                    Some("paused_by_user".into())
                );
            }
            Interrupt::Stop => assert!(
                last.kind == EventKind::Done
                    && last
                        .msg
                        .as_deref()
                        .is_some_and(|m| m.starts_with("stopped_by_user")),
                "{last:?}"
            ),
            // a take-over before a step sends nothing and pauses `taken_over` (2.15)
            Interrupt::TakeOver => {
                assert_eq!(last.kind, EventKind::TakenOver);
                assert_eq!(
                    read_checkpoint(&dir.join(RUN)).unwrap().map(|c| c.reason),
                    Some("taken_over".into())
                );
            }
        }
    }
}
