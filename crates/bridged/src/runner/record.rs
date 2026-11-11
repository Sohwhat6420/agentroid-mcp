//! Record and replay (Phase 6, task 6.1, step 8 of the 25 Oct sprint).
//!
//! A job started with `record: true` that ends `done` leaves two files beside its run directory,
//! where the 7-day pruning (directories only) never touches them:
//!
//! * `<run id>.flow.yaml`: the flow as it ran, in the one flow format (`schemas/flow.json`), so
//!   `job.start {flow: "<run id>.flow.yaml"}` replays it with no model in the loop;
//! * `<run id>.maestro.yaml`: the same steps as Maestro commands (an export, never read back).
//!
//! The recorded steps are the ones the run really completed: a top-level step with a `Step` event in
//! `progress.ndjson`, in flow order. The values of the flow's secret variables (`vars.secret` names)
//! never reach either file: the names stay (a replay must be given the values again), the values are
//! removed from `vars`, and a recording in which a secret's value still appears anywhere (typed
//! literally into a step, held by another variable) is refused, never rewritten.

use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// What `record` produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub flow_yaml: String,
    pub maestro_yaml: String,
    /// Top-level steps in the recording.
    pub steps: usize,
    /// Steps the Maestro export had to leave as comments (no Maestro form), `<step id>: why`.
    pub maestro_skipped: Vec<String>,
}

/// Secret variable names and the text of their values (a string, or a number's digits), from a flow
/// object's `vars`. A secret with no value (not passed) has nothing to leak.
fn secrets_of(flow: &Value) -> Vec<(String, Option<String>)> {
    let vars = flow.get("vars").and_then(Value::as_object);
    let names: Vec<String> = vars
        .and_then(|v| v.get("secret"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    names
        .into_iter()
        .map(|n| {
            let v = match vars.and_then(|m| m.get(&n)) {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Number(x)) => Some(x.to_string()),
                _ => None,
            };
            (n, v)
        })
        .collect()
}

/// `flow` with the values of its secret variables removed (their names stay in `vars.secret`): what
/// a run directory keeps at rest, so a pruned-late or copied run never holds a password.
pub fn without_secret_values(flow: &Value) -> Value {
    let names: Vec<String> = secrets_of(flow).into_iter().map(|(n, _)| n).collect();
    let mut out = flow.clone();
    if let Some(vars) = out.get_mut("vars").and_then(Value::as_object_mut) {
        for n in &names {
            vars.remove(n);
        }
    }
    out
}

/// The inverse for one running job: `stored` (a flow without its secret values) with each secret
/// variable's value put back from `live`, the vars the job holds in memory. A secret the job does
/// not hold (the daemon restarted since) stays absent, so the recording's leak check has nothing to
/// find and the step that needs it fails with "unknown variable" on resume.
pub fn with_secret_values(stored: &Value, live: &Map<String, Value>) -> Value {
    let names: Vec<String> = secrets_of(stored).into_iter().map(|(n, _)| n).collect();
    let mut out = stored.clone();
    if let Some(vars) = out.get_mut("vars").and_then(Value::as_object_mut) {
        for n in names {
            if let Some(v) = live.get(&n) {
                vars.insert(n, v.clone());
            }
        }
    }
    out
}

/// Does `s` hold a secret's value? A value of four characters or more counts wherever it occurs; a
/// shorter one only when it is the whole string (a one-letter secret is in every other string).
fn holds(s: &str, val: &str) -> bool {
    !val.is_empty()
        && if val.chars().count() >= 4 {
            s.contains(val)
        } else {
            s == val
        }
}

/// The first place in `v` (a string or an object key) that holds a secret's value, as `<path>: <name>`.
/// The value itself is never put in the answer.
fn find_leak(v: &Value, at: &str, secrets: &[(String, Option<String>)]) -> Option<String> {
    let hit = |s: &str| {
        secrets
            .iter()
            .find(|(_, val)| val.as_deref().is_some_and(|x| holds(s, x)))
            .map(|(n, _)| n.clone())
    };
    match v {
        Value::String(s) => hit(s).map(|n| format!("{at} holds the value of secret variable {n}")),
        Value::Array(a) => a
            .iter()
            .enumerate()
            .find_map(|(i, x)| find_leak(x, &format!("{at}/{i}"), secrets)),
        Value::Object(o) => o.iter().find_map(|(k, x)| match hit(k) {
            // the key is the leak: say where it is, never what it is
            Some(n) => Some(format!(
                "{at}/<a key> holds the value of secret variable {n}"
            )),
            None => find_leak(x, &format!("{at}/{}", sanitize(k)), secrets),
        }),
        _ => None,
    }
}

/// Text for a comment or a message: control characters (a newline would end the comment) become `?`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The flow object that was run, cut down to the steps in `done_ids`, with the values of its secret
/// variables removed. `Ok(None)` when no step of the flow was completed. `Err` when a secret's value
/// still appears anywhere in what would be written (typed literally into a step, or held by another
/// variable): such a recording is refused, never rewritten, because the engine substitutes `${name}`
/// only in some fields and in one pass, so a rewritten recording could replay as something else.
pub fn recorded_flow(flow: &Value, done_ids: &HashSet<String>) -> Result<Option<Value>, String> {
    let secrets = secrets_of(flow);
    let Some(all) = flow.get("steps").and_then(Value::as_array) else {
        return Ok(None);
    };
    let steps: Vec<Value> = all
        .iter()
        .filter(|s| {
            s.get("id")
                .and_then(Value::as_str)
                .is_some_and(|i| done_ids.contains(i))
        })
        .cloned()
        .collect();
    if steps.is_empty() {
        return Ok(None);
    }
    let mut out = flow.clone();
    let Some(obj) = out.as_object_mut() else {
        return Ok(None);
    };
    obj.insert("steps".into(), Value::Array(steps));
    if let Some(vars) = obj.get_mut("vars").and_then(Value::as_object_mut) {
        for (name, _) in &secrets {
            vars.remove(name);
        }
    }
    if let Some(why) = find_leak(&out, "", &secrets) {
        return Err(format!("not recorded: {why}. Use ${{name}} in the step instead of the value, and pass the value in vars"));
    }
    Ok(Some(out))
}

/// The recording of a finished run: the flow as YAML (with a header comment) and its Maestro export.
/// `Ok(None)` when nothing was completed; `Err` is why a recording was refused.
pub fn record(
    flow: &Value,
    done_ids: &HashSet<String>,
    run_id: &str,
) -> Result<Option<Recorded>, String> {
    let Some(f) = recorded_flow(flow, done_ids)? else {
        return Ok(None);
    };
    let steps = f.get("steps").and_then(Value::as_array).map_or(0, Vec::len);
    let body = serde_yaml::to_string(&f).map_err(|e| format!("not recorded: {e}"))?;
    let secret_names: Vec<String> = secrets_of(&f)
        .into_iter()
        .map(|(n, _)| sanitize(&n))
        .collect();
    let mut flow_yaml = format!(
        "# Recorded from job {}: {steps} step(s). Replay: job.start {{flow: \"{}.flow.yaml\"}}.\n",
        sanitize(run_id),
        sanitize(run_id)
    );
    if !secret_names.is_empty() {
        flow_yaml.push_str(&format!(
            "# Secret variables ({}) are not saved: pass their values in vars when you replay.\n",
            secret_names.join(", ")
        ));
    }
    flow_yaml.push_str(&body);
    let (maestro_yaml, maestro_skipped) = maestro(&f, run_id);
    Ok(Some(Recorded {
        flow_yaml,
        maestro_yaml,
        steps,
        maestro_skipped,
    }))
}

// ---------------------------------------------------------------- Maestro export

/// A double-quoted YAML scalar (JSON strings are valid YAML).
fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// Maestro matches `text` and `id` as regular expressions: the literal parts of `s` are escaped, a
/// `${var}` reference is left for Maestro to fill from its environment.
fn regex_text(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        let (lit, tail) = rest.split_at(i);
        out.push_str(&escape(lit));
        match tail.find('}') {
            Some(j) => {
                out.push_str(&tail[..=j]);
                rest = &tail[j + 1..];
            }
            None => {
                out.push_str(&escape(tail));
                rest = "";
            }
        }
    }
    out.push_str(&escape(rest));
    out
}

fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        if ".*+?^$()[]{}|\\".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// How a flow selector is written as a Maestro argument: the lines after `- tapOn:` (or `None`).
fn maestro_selector(sel: &Value) -> Result<Vec<String>, String> {
    let o = sel
        .as_object()
        .filter(|o| o.len() == 1)
        .ok_or("the selector is not a single key")?;
    let (k, v) = o.iter().next().ok_or("empty selector")?;
    match (k.as_str(), v) {
        ("text" | "desc", Value::String(s)) => Ok(vec![format!("text: {}", q(&regex_text(s)))]),
        ("id", Value::String(s)) => Ok(vec![format!("id: {}", q(&regex_text(s)))]),
        ("rel", Value::Object(r)) => {
            let pct = |n: &str| {
                r.get(n)
                    .and_then(Value::as_f64)
                    .map(|f| (f * 100.0).round() as i64)
            };
            match (pct("x"), pct("y")) {
                (Some(x), Some(y)) => Ok(vec![format!("point: \"{x}%,{y}%\"")]),
                _ => Err("rel needs x and y".into()),
            }
        }
        ("near_text", _) => Err("a select by neighbouring text has no Maestro form".into()),
        ("ref", _) => Err("a stable ref is Agentroid's own and has no Maestro form".into()),
        (other, _) => Err(format!("selector {other} has no Maestro form")),
    }
}

fn push_cmd(out: &mut Vec<String>, cmd: &str, args: &[String]) {
    out.push(format!("- {cmd}:"));
    for a in args {
        out.push(format!("    {a}"));
    }
}

/// The steps as a Maestro flow. Steps with no Maestro form are kept as comments so the file still
/// says what the recording did; each is also returned in the second value.
pub fn maestro(flow: &Value, run_id: &str) -> (String, Vec<String>) {
    let pkg = flow
        .pointer("/app/package")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut skipped = Vec::new();
    let mut lines = vec![format!("# Exported from Agentroid job {run_id}. An export only: Agentroid replays the .flow.yaml."), format!("appId: {}", q(pkg))];
    // plain variables become Maestro env values; a secret's value was never saved (pass it with maestro test -e NAME=...)
    let vars: Vec<(&String, &Value)> = flow
        .get("vars")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(k, v)| k.as_str() != "secret" && v.is_string())
                .collect()
        })
        .unwrap_or_default();
    if !vars.is_empty() {
        lines.push("env:".into());
        lines.extend(
            vars.iter()
                .map(|(k, v)| format!("    {}: {}", q(k), q(v.as_str().unwrap_or("")))),
        );
    }
    lines.push("---".into());
    lines.push("- launchApp".into());
    let steps = flow
        .get("steps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for s in &steps {
        // ids and kinds go into comments: a newline there would start a Maestro command of its own
        let id = &sanitize(s.get("id").and_then(Value::as_str).unwrap_or("?"));
        let kind = &sanitize(s.get("kind").and_then(Value::as_str).unwrap_or("?"));
        let sel = |key: &str| s.get(key).map(maestro_selector);
        let mut unsupported = |why: String, lines: &mut Vec<String>| {
            lines.push(format!("# step {id} ({kind}): {why}"));
            skipped.push(format!("{id}: {why}"));
        };
        match kind.as_str() {
            "tap" => match sel("select") {
                Some(Ok(a)) => push_cmd(&mut lines, "tapOn", &a),
                Some(Err(e)) => unsupported(e, &mut lines),
                None => unsupported("a tap with no selector".into(), &mut lines),
            },
            "type" => {
                let text = s.get("value").and_then(Value::as_str).unwrap_or("");
                match sel("select") {
                    Some(Ok(a)) => {
                        push_cmd(&mut lines, "tapOn", &a);
                        lines.push(format!("- inputText: {}", q(text)));
                    }
                    Some(Err(e)) => unsupported(e, &mut lines),
                    None => lines.push(format!("- inputText: {}", q(text))),
                }
            }
            "assert" => match sel("select") {
                Some(Ok(a)) => push_cmd(&mut lines, "assertVisible", &a),
                Some(Err(e)) => unsupported(e, &mut lines),
                None => unsupported("an assert with no selector".into(), &mut lines),
            },
            "wait" => {
                if let Some(u) = s.get("until") {
                    match maestro_selector(u) {
                        Ok(a) => {
                            let mut args = vec!["visible:".to_string()];
                            args.extend(a.iter().map(|l| format!("  {l}")));
                            args.push(format!(
                                "timeout: {}",
                                s.get("timeout_ms").and_then(Value::as_u64).unwrap_or(5000)
                            ));
                            push_cmd(&mut lines, "extendedWaitUntil", &args);
                        }
                        Err(e) => unsupported(e, &mut lines),
                    }
                } else {
                    unsupported("a fixed wait is not exported (Maestro waits for the screen to settle by itself)".into(), &mut lines);
                }
            }
            "back" => lines.push("- back".into()),
            "scroll" => match s.get("direction").and_then(Value::as_str).unwrap_or("down") {
                "down" => lines.push("- scroll".into()),
                other => unsupported(
                    format!("a scroll {other} has no plain Maestro command"),
                    &mut lines,
                ),
            },
            other => unsupported(format!("{other} steps have no Maestro form"), &mut lines),
        }
    }
    lines.push(String::new());
    (lines.join("\n"), skipped)
}

/// The names of the recorded files, for a run id.
pub fn file_names(run_id: &str) -> (String, String) {
    (
        format!("{run_id}.flow.yaml"),
        format!("{run_id}.maestro.yaml"),
    )
}

/// The `recording` object a receipt carries.
pub fn recording_value(
    flow_path: &str,
    maestro_path: &str,
    steps: usize,
    skipped: &[String],
) -> Value {
    json!({"flow": flow_path, "maestro": maestro_path, "steps": steps, "maestro_skipped": skipped})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::flow::{load_value, parse_yaml_value};

    fn ids(v: &[&str]) -> HashSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn sample() -> Value {
        json!({
            "flow": "t", "version": 1, "app": {"package": "com.android.settings", "version_range": "*"},
            "vars": {"secret": ["pw"], "pw": "hunter22", "who": "Ann"},
            "steps": [
                {"id": "a", "kind": "tap", "select": {"text": "Wi-Fi (2.4)"}},
                {"id": "b", "kind": "type", "select": {"id": "com.x:id/pass"}, "value": "${pw}"},
                {"id": "c", "kind": "type", "value": "${who}"},
                {"id": "d", "kind": "wait", "until": {"text": "Done"}, "timeout_ms": 3000},
                {"id": "e", "kind": "tap", "select": {"rel": {"x": 0.5, "y": 0.25}}},
                {"id": "f", "kind": "tap", "select": {"ref": "r3fa91c"}},
                {"id": "g", "kind": "tap", "select": {"near_text": "Row"}},
                {"id": "h", "kind": "back"},
                {"id": "i", "kind": "scroll", "direction": "up"},
                {"id": "j", "kind": "settings_set", "namespace": "system", "key": "k", "value": "1", "scope": "job"},
                {"id": "k", "kind": "assert", "select": {"desc": "Back"}}
            ]
        })
    }

    #[test]
    fn only_completed_steps_are_kept_and_no_secret_value_survives() {
        let f = recorded_flow(&sample(), &ids(&["a", "b", "k"]))
            .unwrap()
            .unwrap();
        let steps = f["steps"].as_array().unwrap();
        assert_eq!(
            steps
                .iter()
                .map(|s| s["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", "k"]
        );
        assert_eq!(
            steps[1]["value"], "${pw}",
            "the step keeps its variable reference"
        );
        assert_eq!(f["vars"]["secret"], json!(["pw"]), "the name stays");
        assert!(f["vars"].get("pw").is_none(), "the value goes");
        assert_eq!(f["vars"]["who"], "Ann");
        assert!(!f.to_string().contains("hunter22"));
        assert_eq!(
            recorded_flow(&sample(), &ids(&[])),
            Ok(None),
            "nothing completed, nothing recorded"
        );
    }

    #[test]
    fn secret_values_leave_the_stored_flow_and_come_back_only_from_the_live_vars() {
        let stored = without_secret_values(&sample());
        assert!(!stored.to_string().contains("hunter22"), "{stored}");
        assert_eq!(stored["vars"]["secret"], json!(["pw"]), "the name stays");
        assert_eq!(stored["vars"]["who"], "Ann", "a plain variable stays");
        assert_eq!(stored["steps"], sample()["steps"], "steps are untouched");
        // the running job still holds the value: put back
        let live = sample()["vars"].as_object().cloned().unwrap();
        assert_eq!(with_secret_values(&stored, &live), sample());
        // a job loaded after a restart holds none: nothing comes back
        let empty = Map::new();
        assert_eq!(with_secret_values(&stored, &empty), stored);
        // a flow with no secrets is returned as it was
        let plain = json!({"flow": "t", "vars": {"who": "Ann"}, "steps": []});
        assert_eq!(without_secret_values(&plain), plain);
        // a value that is only in live vars under a non-secret name is not copied in
        let mut live2 = Map::new();
        live2.insert("who".into(), json!("Zed"));
        assert_eq!(with_secret_values(&stored, &live2)["vars"]["who"], "Ann");
    }

    /// A flow with one secret and a step list, for the refusal tests.
    fn with_secret(val: Value, vars_extra: Value, steps: Value) -> Value {
        let mut vars = json!({"secret": ["pw"], "pw": val});
        for (k, v) in vars_extra.as_object().cloned().unwrap_or_default() {
            vars[k] = v;
        }
        json!({"flow": "t", "version": 1, "app": {"package": "a.b", "version_range": "*"}, "vars": vars, "steps": steps})
    }

    #[test]
    fn a_secret_value_anywhere_in_what_would_be_written_refuses_the_recording_and_is_never_echoed()
    {
        let a_step = |v: Value| json!([{"id": "a", "kind": "type", "value": v}]);
        for (what, flow) in [
            (
                "typed literally into a step",
                with_secret(json!("hunter22"), json!({}), a_step(json!("hunter22"))),
            ),
            (
                "inside a longer text",
                with_secret(json!("hunter22"), json!({}), a_step(json!("ann:hunter22"))),
            ),
            (
                "held by another variable",
                with_secret(
                    json!("hunter22"),
                    json!({"login": "ann:hunter22"}),
                    a_step(json!("x")),
                ),
            ),
            (
                "as another variable's whole value",
                with_secret(
                    json!("hunter22"),
                    json!({"pw2": "hunter22"}),
                    a_step(json!("x")),
                ),
            ),
            (
                "a number used as text",
                with_secret(json!(4821), json!({}), a_step(json!("PIN 4821"))),
            ),
            (
                "as an object key",
                with_secret(
                    json!("hunter22"),
                    json!({}),
                    json!([{"id": "a", "kind": "for_each", "table": [{"hunter22": "x"}]}]),
                ),
            ),
            (
                "a short secret that is the whole string",
                with_secret(json!("1"), json!({}), a_step(json!("1"))),
            ),
        ] {
            let e = record(&flow, &ids(&["a"]), "job-1-1").expect_err(what);
            assert!(e.contains("secret variable pw"), "{what}: {e}");
            assert!(
                !e.contains("hunter22") && !e.contains("4821"),
                "{what}: the refusal repeats the value: {e}"
            );
        }
        // a short secret inside another string is not a leak (it would be in every string), and a reference is fine
        let ok = with_secret(
            json!("1"),
            json!({}),
            json!([{"id": "a", "kind": "type", "value": "room 101"}, {"id": "b", "kind": "type", "value": "${pw}"}]),
        );
        assert!(record(&ok, &ids(&["a", "b"]), "job-1-1").unwrap().is_some());
        // a secret that was never given a value has nothing to leak
        let none = json!({"flow": "t", "version": 1, "app": {"package": "a.b", "version_range": "*"}, "vars": {"secret": ["pw"]}, "steps": [{"id": "a", "kind": "type", "value": "${pw}"}]});
        assert!(record(&none, &ids(&["a"]), "job-1-1").unwrap().is_some());
    }

    #[test]
    fn nothing_a_flow_names_can_start_a_line_of_its_own_in_a_comment_or_the_env_block() {
        let flow = json!({"flow": "t", "version": 1, "app": {"package": "a.b", "version_range": "*"},
            "vars": {"secret": ["pw\nsteps: []"], "bad\nkey": "v"},
            "steps": [{"id": "x\n- runScript: evil.js", "kind": "settings_set", "namespace": "system", "key": "k", "value": "1", "scope": "job"}]});
        let r = record(&flow, &ids(&["x\n- runScript: evil.js"]), "job-1-1")
            .unwrap()
            .unwrap();
        for l in r.maestro_yaml.lines().chain(r.flow_yaml.lines()) {
            assert!(
                !l.starts_with("- runScript")
                    && !l.starts_with("steps: []")
                    && !l.starts_with("key\""),
                "a line was injected: {l:?}"
            );
        }
        assert!(
            r.maestro_yaml
                .contains("# step x?- runScript: evil.js (settings_set)"),
            "{}",
            r.maestro_yaml
        );
        assert!(
            r.maestro_yaml.contains(r#"    "bad\nkey": "v""#),
            "env keys are quoted:\n{}",
            r.maestro_yaml
        );
    }

    #[test]
    fn the_recorded_yaml_loads_again_as_the_same_flow() {
        let all: Vec<String> = sample()["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect();
        let done: HashSet<String> = all
            .iter()
            .filter(|i| ["a", "b", "c", "d", "e", "f", "h", "i", "k"].contains(&i.as_str()))
            .cloned()
            .collect();
        let r = record(&sample(), &done, "job-1-1").unwrap().unwrap();
        assert!(
            r.flow_yaml
                .starts_with("# Recorded from job job-1-1: 9 step(s)."),
            "{}",
            r.flow_yaml
        );
        assert!(r.flow_yaml.contains("pw") && !r.flow_yaml.contains("hunter22"));
        let back = parse_yaml_value(&r.flow_yaml).unwrap();
        assert_eq!(Some(back.clone()), recorded_flow(&sample(), &done).unwrap());
        load_value(back).expect("the recording is a valid flow");
    }

    #[test]
    fn maestro_export_maps_what_has_a_form_and_comments_the_rest() {
        let all = ids(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"]);
        let r = record(&sample(), &all, "job-1-1").unwrap().unwrap();
        let m = &r.maestro_yaml;
        assert!(
            m.starts_with("# Exported from Agentroid job job-1-1"),
            "{m}"
        );
        assert!(
            m.contains(
                "appId: \"com.android.settings\"\nenv:\n    \"who\": \"Ann\"\n---\n- launchApp\n"
            ),
            "{m}"
        );
        assert!(
            m.contains(
                r#"- tapOn:
    text: "Wi-Fi \\(2\\.4\\)""#
            ),
            "regex characters in text are escaped:\n{m}"
        );
        assert!(
            m.contains(
                r#"- tapOn:
    id: "com\\.x:id/pass"
- inputText: "${pw}""#
            ),
            "{m}"
        );
        assert!(m.contains("- inputText: \"${who}\""), "{m}");
        assert!(
            m.contains(
                "- extendedWaitUntil:\n    visible:\n      text: \"Done\"\n    timeout: 3000"
            ),
            "{m}"
        );
        assert!(m.contains("- tapOn:\n    point: \"50%,25%\""), "{m}");
        assert!(m.contains("- back\n"), "{m}");
        assert!(m.contains("- assertVisible:\n    text: \"Back\""), "{m}");
        let sk: Vec<&str> = r
            .maestro_skipped
            .iter()
            .map(|s| s.split(':').next().unwrap())
            .collect();
        assert_eq!(sk, ["f", "g", "i", "j"], "{:?}", r.maestro_skipped);
        for id in ["f", "g", "i", "j"] {
            assert!(
                m.contains(&format!("# step {id} (")),
                "step {id} is kept as a comment:\n{m}"
            );
        }
    }

    #[test]
    fn a_var_reference_inside_a_selector_is_left_for_maestro_to_fill() {
        assert_eq!(regex_text("a.b ${x} (c)"), r"a\.b ${x} \(c\)");
        assert_eq!(regex_text("no close ${x"), r"no close \$\{x");
    }
}
