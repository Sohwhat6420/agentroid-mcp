//! D3 red-team suite (10 Nov 2026; D3b added 13 hostile and 4 control cases in Thai, Hindi, Tamil and
//! Arabic): the 44 hostile cases, the controls, the freshness checks and the
//! known limits in `tests/redteam/cases.json`, run through the real gate (`Core::call`: S24 rule,
//! allowlist, device mode, then the screen guard) on core's fake phone. No device, adb, emulator or
//! model. Every case runs under three profiles: the POCO in `full` mode, the POCO in `read_only`,
//! and the protected phone. Every decision goes to one hash-chained audit log, which is then verified, tampered
//! with and verified again, in-process and through the `agentroid audit verify` binary.
//!
//! Outputs (regenerated on every run): `generated/redteam/results.json`, `results.md`,
//! `audit.jsonl` (+ `.head`), `audit-tampered.jsonl`, `verify.txt`.

use agentroid_core::audit::{self, AuditLog};
use agentroid_core::fake::{FakeClock, FakeDevice, FAKE_S24_ADB};
use agentroid_core::gate::{Call, DeviceMode, Refusal, Target};
use agentroid_core::selector::Selector;
use agentroid_core::transport::{Core, CoreError, POCO_SERIAL};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn here() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn out_dir() -> PathBuf {
    let d = here().join("../../generated/redteam");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// One fixture node as uiautomator XML.
fn node_xml(n: &Value, pkg: &str, out: &mut String) {
    let s = |k: &str| n.get(k).and_then(Value::as_str).unwrap_or("");
    let b = |k: &str| n.get(k).and_then(Value::as_bool).unwrap_or(false);
    let p = if s("p").is_empty() { pkg } else { s("p") };
    let bb: Vec<i64> = n["b"]
        .as_array()
        .map(|a| a.iter().map(|x| x.as_i64().unwrap()).collect())
        .unwrap_or_else(|| vec![0, 0, 1220, 2712]);
    write!(
        out,
        "<node index=\"0\" text=\"{}\" resource-id=\"{}\" class=\"{}\" package=\"{}\" content-desc=\"{}\" checkable=\"{}\" checked=\"false\" clickable=\"{}\" enabled=\"true\" focusable=\"{}\" focused=\"{}\" scrollable=\"false\" long-clickable=\"false\" password=\"{}\" selected=\"false\" bounds=\"[{},{}][{},{}]\"",
        esc(s("t")), esc(s("id")), esc(s("c")), esc(p), esc(s("d")),
        b("ck"), b("k"), b("k"), b("f"), b("pw"),
        bb[0], bb[1], bb[2], bb[3]
    )
    .unwrap();
    match n.get("n").and_then(Value::as_array) {
        Some(kids) if !kids.is_empty() => {
            out.push('>');
            for k in kids {
                node_xml(k, p, out);
            }
            out.push_str("</node>");
        }
        _ => out.push_str(" />"),
    }
}

fn screen_xml(case: &Value, key: &str) -> Option<String> {
    if key == "screen" {
        if let Some(r) = case.get("recorded").and_then(Value::as_str) {
            return Some(
                std::fs::read_to_string(here().join("tests/redteam/recorded").join(r)).unwrap(),
            );
        }
    }
    let sc = case.get(key)?;
    let pkg = sc["p"].as_str().unwrap_or("");
    let mut x = String::from(
        "<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\">",
    );
    let root = json!({ "c": "android.widget.FrameLayout", "p": pkg, "b": [0, 0, 1220, 2712], "n": sc["n"] });
    node_xml(&root, pkg, &mut x);
    x.push_str("</hierarchy>");
    Some(x)
}

fn pt(v: &Value) -> (f64, f64) {
    (v[0].as_f64().unwrap(), v[1].as_f64().unwrap())
}

fn to_call(c: &Value) -> Call {
    let s = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_string);
    let target = || {
        if let Some(t) = s("text") {
            Target::Select(Selector::text(&t))
        } else if let Some(d) = s("desc") {
            Target::Select(Selector::desc(&d))
        } else if let Some(i) = s("id") {
            Target::Select(Selector::id(&i))
        } else {
            let (x, y) = pt(&c["point"]);
            Target::Point {
                x,
                y,
                space: Some("device".into()),
                image_width: None,
            }
        }
    };
    match c["tool"].as_str().unwrap() {
        "tap" => Call::Tap { target: target() },
        "long_press" => Call::LongPress {
            target: target(),
            ms: 800,
        },
        "swipe" => {
            let ((x1, y1), (x2, y2)) = (pt(&c["from"]), pt(&c["to"]));
            Call::Swipe {
                x1,
                y1,
                x2,
                y2,
                ms: 400,
                space: Some("device".into()),
                image_width: None,
            }
        }
        "type_text" => Call::TypeText {
            text: s("text").unwrap(),
            index: None,
            submit: c.get("submit").and_then(Value::as_bool).unwrap_or(false),
        },
        "key" => Call::Key {
            name: s("name").unwrap(),
        },
        "open_app" => Call::OpenApp {
            package: s("package").unwrap(),
        },
        "open_url" => Call::OpenUrl {
            uri: s("uri").unwrap(),
            package: s("package"),
        },
        "start_activity" => Call::StartActivity {
            component: s("component").unwrap(),
            action: s("action"),
            data: s("data"),
        },
        other => panic!("unknown tool {other}"),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Profile {
    PocoFull,
    PocoReadOnly,
    S24,
}

impl Profile {
    fn name(self) -> &'static str {
        match self {
            Profile::PocoFull => "poco_full",
            Profile::PocoReadOnly => "poco_read_only",
            Profile::S24 => "s24",
        }
    }
}

struct Run {
    actual: String,
    rule: String,
    message: String,
    /// Input events or shell lines that reached the fake phone during the case's call.
    reached: usize,
}

fn run_case(case: &Value, profile: Profile, log: &Path) -> Run {
    let mut fake = FakeDevice::new();
    fake.xml_override = screen_xml(case, "screen");
    let mut core = Core::new(fake, FakeClock::new());
    core.set_audit(AuditLog::open(log).expect("audit log opens and verifies"));
    let serial = match profile {
        Profile::PocoFull => None,
        Profile::PocoReadOnly => {
            core.set_mode(POCO_SERIAL, DeviceMode::ReadOnly);
            None
        }
        Profile::S24 => Some(FAKE_S24_ADB),
    };
    // the agent looks first, as a model would (refused on the protected phone, which is fine)
    let _ = core.call(
        serial,
        &Call::UiTree {
            interactive_only: false,
            require: vec![],
            timeout_ms: None,
        },
    );
    if let Some(pre) = case.get("pre") {
        let _ = core.call(serial, &to_call(pre));
    }
    if let Some(x) = screen_xml(case, "then_screen") {
        core.transport.xml_override = Some(x);
    }
    core.clock()
        .advance(case.get("advance_ms").and_then(Value::as_u64).unwrap_or(0));
    let before = core.transport.acted.len();
    let r = core.call(serial, &to_call(&case["call"]));
    let reached = core.transport.acted.len() - before;
    let (actual, rule, message) = match r {
        Ok(o) => ("allowed".to_string(), "-".to_string(), o.text),
        Err(CoreError::Refused(rf)) => (rf.verdict().to_string(), rf.rule(), rf.to_string()),
        Err(e) => ("error".to_string(), "-".to_string(), e.to_string()),
    };
    Run {
        actual,
        rule,
        message,
        reached,
    }
}

fn no_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

fn run_verify_binary(log: &Path) -> (i32, String) {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_agentroid"));
    cmd.args(["audit", "verify"]).arg(log);
    no_window(&mut cmd);
    let out = cmd.output().expect("agentroid runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text.trim().to_string())
}

#[test]
fn redteam_suite() {
    let cases: Value = serde_json::from_str(
        &std::fs::read_to_string(here().join("tests/redteam/cases.json")).unwrap(),
    )
    .unwrap();
    let cases = cases["cases"].as_array().unwrap();
    assert_eq!(
        cases.iter().filter(|c| c["set"] == "hostile").count(),
        44,
        "the suite is 30 hostile cases from D3, 13 from D3b (Thai, Hindi, Tamil, Arabic) and RT44 (GrabPay sheet, 11 Nov)"
    );
    let dir = out_dir();
    let log = dir.join("audit.jsonl");
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_file(audit::head_path(&log));

    let mut rows: Vec<Value> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut unsafe_actions = 0usize;
    for c in cases {
        let id = c["id"].as_str().unwrap();
        let set = c["set"].as_str().unwrap();
        let expected = c["expected"].as_str().unwrap();
        let mut per: serde_json::Map<String, Value> = serde_json::Map::new();
        for p in [Profile::PocoFull, Profile::PocoReadOnly, Profile::S24] {
            let r = run_case(c, p, &log);
            // an unsafe action: the safe outcome was a refusal, yet something reached the phone
            let unsafe_here = expected != "allowed" && r.reached > 0;
            if unsafe_here && set != "limit" {
                unsafe_actions += 1;
            }
            // a refused call must never reach the phone, in any profile or set
            if r.actual != "allowed" && r.reached > 0 {
                failures.push(format!(
                    "{id} {}: refused but {} action(s) reached the phone",
                    p.name(),
                    r.reached
                ));
            }
            if p == Profile::PocoFull && set != "limit" && r.actual != expected {
                failures.push(format!(
                    "{id}: expected {expected}, got {} ({})",
                    r.actual, r.message
                ));
            }
            if p != Profile::PocoFull && set == "hostile" && r.actual == "allowed" {
                failures.push(format!("{id} {}: allowed", p.name()));
            }
            per.insert(
                p.name().to_string(),
                json!({ "actual": r.actual, "rule": r.rule, "message": r.message, "reached_phone": r.reached, "unsafe": unsafe_here }),
            );
        }
        let full = &per["poco_full"];
        let pass = full["actual"] == expected;
        rows.push(json!({
            "id": id, "set": set, "owasp": c["owasp"], "category": c["category"], "title": c["title"],
            "call": c["call"], "expected": expected, "pass": pass, "profiles": per,
        }));
    }

    // the audit log: verify, then tamper and verify again (in-process and through the binary)
    let verified = audit::verify_file(&log).expect("the run's audit log verifies");
    let (ok_code, ok_text) = run_verify_binary(&log);
    let text = std::fs::read_to_string(&log).unwrap();
    // a refusal in the middle of the log, turned into an "allowed"
    let first_blocked = text
        .lines()
        .enumerate()
        .find(|(i, l)| *i >= 40 && l.contains("\"decision\":\"blocked\""))
        .map(|(i, _)| i)
        .expect("a blocked record past line 40");
    let tampered: String = text
        .lines()
        .enumerate()
        .map(|(i, l)| {
            if i == first_blocked {
                l.replacen("\"decision\":\"blocked\"", "\"decision\":\"allowed\"", 1)
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let tlog = dir.join("audit-tampered.jsonl");
    std::fs::write(&tlog, &tampered).unwrap();
    let _ = std::fs::remove_file(audit::head_path(&tlog));
    let (bad_code, bad_text) = run_verify_binary(&tlog);
    let tamper_err = audit::verify_str(&tampered, None).unwrap_err();
    assert_eq!(ok_code, 0, "{ok_text}");
    assert_eq!(bad_code, 1, "{bad_text}");
    assert_eq!(tamper_err.line, first_blocked + 1);
    // the attacker also recomputes the edited line's hash: the NEXT line's link breaks
    let mut lines: Vec<String> = tampered.lines().map(str::to_string).collect();
    let mut rec = audit::Record::from_json(&lines[first_blocked]).unwrap();
    rec.hash = audit::chain_hash(&rec.prev, rec.seq, rec.ts_ms, &rec.entry);
    lines[first_blocked] = rec.to_json();
    let rehashed = lines.join("\n") + "\n";
    let rlog = dir.join("audit-rehashed.jsonl");
    std::fs::write(&rlog, &rehashed).unwrap();
    let _ = std::fs::remove_file(audit::head_path(&rlog));
    let (re_code, re_text) = run_verify_binary(&rlog);
    assert_eq!(re_code, 1, "{re_text}");
    assert_eq!(
        audit::verify_str(&rehashed, None).unwrap_err().line,
        first_blocked + 2
    );
    // the last 5 records cut off: caught against the head file
    let all: Vec<&str> = text.lines().collect();
    let cut = all[..all.len() - 5].join("\n") + "\n";
    let clog = dir.join("audit-cut.jsonl");
    std::fs::write(&clog, &cut).unwrap();
    std::fs::copy(audit::head_path(&log), audit::head_path(&clog)).unwrap();
    let (cut_code, cut_text) = run_verify_binary(&clog);
    assert_eq!(cut_code, 1, "{cut_text}");
    let rel = |t: &str| t.replace(&dir.display().to_string(), "generated/redteam");
    std::fs::write(
        dir.join("verify.txt"),
        format!(
            "$ agentroid audit verify generated/redteam/audit.jsonl\n{}\n(exit {ok_code})\n\n\
             # audit-tampered.jsonl: line {} edited, \"decision\":\"blocked\" -> \"allowed\"\n\
             $ agentroid audit verify generated/redteam/audit-tampered.jsonl\n{}\n(exit {bad_code})\n\n\
             # audit-rehashed.jsonl: the same edit, and the attacker recomputes that line's hash\n\
             $ agentroid audit verify generated/redteam/audit-rehashed.jsonl\n{}\n(exit {re_code})\n\n\
             # audit-cut.jsonl: the last 5 records removed (the head file kept)\n\
             $ agentroid audit verify generated/redteam/audit-cut.jsonl\n{}\n(exit {cut_code})\n",
            rel(&ok_text),
            first_blocked + 1,
            rel(&bad_text),
            rel(&re_text),
            rel(&cut_text)
        ),
    )
    .unwrap();

    // the tables
    let count = |set: &str, pass: bool| {
        rows.iter()
            .filter(|r| r["set"] == set && r["pass"] == pass)
            .count()
    };
    let summary = json!({
        "hostile": { "pass": count("hostile", true), "fail": count("hostile", false) },
        "control": { "pass": count("control", true), "fail": count("control", false) },
        "freshness": { "pass": count("freshness", true), "fail": count("freshness", false) },
        "limit": { "caught": count("limit", true), "missed": count("limit", false) },
        "unsafe_actions_outside_limits": unsafe_actions,
        "audit": { "records": verified.records, "allowed": verified.allowed, "blocked": verified.blocked, "asked": verified.asked, "head": verified.head_hash },
        "tamper": { "edited_line": first_blocked + 1, "verify_exit": bad_code, "verify_says": rel(&bad_text) },
    });
    std::fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(&json!({ "summary": summary, "cases": rows })).unwrap() + "\n",
    )
    .unwrap();
    let mut md = String::from("| Case | Set | OWASP | What | Call | Expected | Actual (POCO full) | Rule | Pass | read_only | S24 |\n|---|---|---|---|---|---|---|---|---|---|---|\n");
    for r in &rows {
        let call = r["call"].to_string().replace('|', "\\|");
        let p = &r["profiles"];
        writeln!(
            md,
            "| {} | {} | {} | {} | `{}` | {} | {} | {} | {} | {} | {} |",
            r["id"].as_str().unwrap(),
            r["set"].as_str().unwrap(),
            r["owasp"].as_str().unwrap(),
            r["title"].as_str().unwrap().replace('|', "\\|"),
            call,
            r["expected"].as_str().unwrap(),
            p["poco_full"]["actual"].as_str().unwrap(),
            p["poco_full"]["rule"].as_str().unwrap(),
            if r["pass"].as_bool().unwrap() {
                "yes"
            } else {
                "NO"
            },
            p["poco_read_only"]["actual"].as_str().unwrap(),
            p["s24"]["actual"].as_str().unwrap(),
        )
        .unwrap();
    }
    std::fs::write(dir.join("results.md"), md).unwrap();
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    assert!(
        failures.is_empty(),
        "red-team failures:\n{}",
        failures.join("\n")
    );
    assert_eq!(unsafe_actions, 0);
}

/// What the guard costs per acting call when it reuses the stored tree: parse + classify + decide on
/// the two recorded POCO dumps (30 KB each). Printed, not asserted (it depends on the build profile).
#[test]
fn guard_cost_on_recorded_dumps() {
    use agentroid_core::screenguard::{decide, Screen, Touch};
    use agentroid_core::tree::parse;
    let tap = Call::Tap {
        target: Target::Index(0),
    };
    for f in [
        "poco-launcher-2026-10-20.xml",
        "poco-apps-list-2026-10-20.xml",
    ] {
        let xml = std::fs::read_to_string(here().join("tests/redteam/recorded").join(f)).unwrap();
        let n = 200u32;
        let t0 = std::time::Instant::now();
        let mut allowed = 0;
        for _ in 0..n {
            let tree = parse(&xml, false).unwrap();
            let sc = Screen::new(&tree);
            if decide(Some(&sc), &tap, Touch::At(467, 1763)).verdict
                == agentroid_core::screenguard::Verdict::Allowed
            {
                allowed += 1;
            }
        }
        let per = t0.elapsed().as_secs_f64() * 1000.0 / f64::from(n);
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        println!(
            "guard cost {f}: {per:.3} ms per call ({profile} build, {n} runs, {allowed} allowed)"
        );
        let _ = std::fs::write(
            out_dir().join(format!("cost-{profile}-{f}.txt")),
            format!("{per:.3} ms per parse+classify+decide ({profile} build, {n} runs)\n"),
        );
    }
}

/// A refusal's verdict and rule are what the log and the table show.
#[test]
fn refusal_words() {
    let r = Refusal::Screen {
        tool: "tap",
        verdict: "asked",
        kind: "send",
        reason: "x".into(),
    };
    assert_eq!((r.verdict(), r.rule().as_str()), ("asked", "screen:send"));
    assert_eq!(Refusal::ReadOnly { tool: "tap" }.verdict(), "blocked");
}

/// Mark the first editable field focused when a fixture marks none (blind cases say "types into the
/// focused or first editable field"; the fake phone needs an explicit focus).
fn focus_first_edit(n: &mut Value) -> bool {
    if n["c"].as_str().is_some_and(|c| c.contains("EditText")) {
        n["f"] = Value::Bool(true);
        return true;
    }
    if let Some(kids) = n.get_mut("n").and_then(Value::as_array_mut) {
        for k in kids {
            if focus_first_edit(k) {
                return true;
            }
        }
    }
    false
}

fn any_focused(n: &Value) -> bool {
    n["f"].as_bool() == Some(true)
        || n["n"]
            .as_array()
            .is_some_and(|kids| kids.iter().any(any_focused))
}

fn has_attr(n: &Value, k: &str, v: &str) -> bool {
    n[k].as_str() == Some(v)
        || n["n"]
            .as_array()
            .is_some_and(|kids| kids.iter().any(|x| has_attr(x, k, v)))
}

/// The blind cases' call shapes, mapped to this suite's (`type`, `x`/`y`, `key`, `url`).
fn normalise_blind(case: &Value) -> Value {
    let mut c = case.clone();
    let call = c["call"].as_object_mut().unwrap();
    if call.get("tool").and_then(Value::as_str) == Some("type") {
        call.insert("tool".into(), json!("type_text"));
    }
    if let (Some(x), Some(y)) = (call.get("x").cloned(), call.get("y").cloned()) {
        call.insert("point".into(), json!([x, y]));
    }
    if let Some(k) = call.get("key").cloned() {
        call.insert("name".into(), k);
    }
    if let Some(u) = call.get("url").cloned() {
        call.insert("uri".into(), u);
    }
    // A blind case may name by `text` a label its fixture only carries as a content description. A
    // real agent reads the tree first and would select by description, so do the same (marked in the
    // results as `selector_fixed`, never hidden).
    if let Some(t) = c["call"]["text"].as_str().map(str::to_string) {
        let root = json!({ "n": c["screen"]["n"].clone() });
        if !has_attr(&root, "t", &t) && has_attr(&root, "d", &t) {
            let call = c["call"].as_object_mut().unwrap();
            call.remove("text");
            call.insert("desc".into(), json!(t));
            c["selector_fixed"] = json!(true);
        }
    }
    if c["call"]["tool"] == "type_text" {
        let mut root = json!({ "n": c["screen"]["n"].clone() });
        if !any_focused(&root) {
            focus_first_edit(&mut root);
            c["screen"]["n"] = root["n"].take();
        }
    }
    c
}

/// Blind cases (S2.5): written by a fresh agent from the OWASP list and the public promise only, never
/// from the guard's rules. Their pass rate is PUBLISHED, whatever it is, so a miss does not fail this
/// test. What does fail it: a refused call that still reached the phone.
/// One file per round: round 1 `blind-cases.json` -> `generated/redteam/results-blind.{json,md}`
/// (no longer blind: its misses were fixed; the before-fix table is `results-blind-round1-before-fixes.md`),
/// round 2 `blind-cases-r2.json` -> `results-blind-r2.{json,md}`, round 3 `blind-cases-r3.json` ->
/// `results-blind-r3.{json,md}`, round 4 `blind-cases-r4.json` -> `results-blind-r4.{json,md}` (each
/// skipped until its file exists).
#[test]
fn blind_cases() {
    for (file, stem) in [
        ("blind-cases.json", "results-blind"),
        ("blind-cases-r2.json", "results-blind-r2"),
        ("blind-cases-r3.json", "results-blind-r3"),
        ("blind-cases-r4.json", "results-blind-r4"),
    ] {
        run_blind_set(file, stem);
    }
}

fn run_blind_set(file: &str, stem: &str) {
    let f = here().join("tests/redteam").join(file);
    let Ok(text) = std::fs::read_to_string(&f) else {
        return; // this round is not written yet
    };
    let all: Value = serde_json::from_str(&text).unwrap();
    let dir = out_dir();
    let log = std::env::temp_dir().join(format!("agentroid-{stem}-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_file(audit::head_path(&log));
    let mut rows = Vec::new();
    let mut md = String::from(
        "| id | OWASP | category | expected | got | pass | title |\n|---|---|---|---|---|---|---|\n",
    );
    let (mut pass_n, mut total) = (0usize, 0usize);
    for raw in all["cases"].as_array().unwrap() {
        let c = normalise_blind(raw);
        let r = run_case(&c, Profile::PocoFull, &log);
        assert!(
            r.actual == "allowed" || r.reached == 0,
            "{}: refused but {} action(s) reached the phone",
            c["id"],
            r.reached
        );
        let expected = c["expected"].as_str().unwrap();
        let pass = r.actual == expected;
        total += 1;
        pass_n += usize::from(pass);
        writeln!(
            md,
            "| {} | {} | {} | {} | {} | {} | {} |",
            c["id"].as_str().unwrap_or(""),
            c["owasp"].as_str().unwrap_or(""),
            c["category"].as_str().unwrap_or(""),
            expected,
            r.actual,
            if pass { "yes" } else { "**no**" },
            c["title"].as_str().unwrap_or("").replace('|', "/")
        )
        .unwrap();
        rows.push(json!({ "id": c["id"], "owasp": c["owasp"], "category": c["category"], "title": c["title"],
            "call": raw["call"], "selector_fixed": c["selector_fixed"].as_bool().unwrap_or(false), "expected": expected, "actual": r.actual, "rule": r.rule, "message": r.message, "pass": pass }));
    }
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_file(audit::head_path(&log));
    let summary = json!({ "pass": pass_n, "total": total, "about": all["about"] });
    std::fs::write(
        dir.join(format!("{stem}.json")),
        serde_json::to_string_pretty(&json!({ "summary": summary, "cases": rows })).unwrap() + "\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(format!("{stem}.md")),
        format!("Blind cases ({file}): {pass_n} of {total} as expected (POCO, full mode).\n\n{md}"),
    )
    .unwrap();
    println!("blind cases {file}: {pass_n} of {total} as expected");
}
