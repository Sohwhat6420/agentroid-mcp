//! End-to-end tests: the real child binary, a real Job Object, a fake bridge. No adb, no device.
#![cfg(windows)]

use agentroid_core::gate::{Call, DeviceMode, ACTING_TOOLS};
use sandbox::calls::sample_args;
use sandbox::shell::RiskClass;
use sandbox::{Bridge, Device, Limits, Outcome, RunResult, Sandbox};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const MB: usize = 1024 * 1024;
const TWO_S: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Fake {
    calls: Vec<String>,
    shells: Vec<Vec<String>>,
}

impl Bridge for Fake {
    fn call(&mut self, call: &Call, _raw: &Value) -> Result<Value, String> {
        self.calls.push(call.name().to_string());
        Ok(json!({"done": call.name()}))
    }
    fn shell(&mut self, argv: &[String]) -> Result<String, String> {
        self.shells.push(argv.to_vec());
        Ok(format!("fake:{}", argv.join(" ")))
    }
}

fn sandbox() -> Sandbox {
    Sandbox::new(env!("CARGO_BIN_EXE_agentroid-sandbox"))
}

fn device(mode: DeviceMode) -> Device {
    Device {
        serial: "192.0.2.50:37123".into(),
        hw_serial: "DEVICE0000EXAMPLE".into(),
        allowed: true,
        mode,
    }
}

fn full() -> Device {
    device(DeviceMode::Full)
}

fn run_with(sb: &Sandbox, script: &str, dev: &Device) -> (RunResult, Fake, Duration) {
    let mut fake = Fake::default();
    let t = Instant::now();
    let r = sb.run(script, dev, &mut fake);
    (r, fake, t.elapsed())
}

fn run(script: &str, dev: &Device) -> (RunResult, Fake, Duration) {
    run_with(&sandbox(), script, dev)
}

/// The parent is still able to run a normal script afterwards.
fn assert_parent_healthy() {
    let (r, _, _) = run("1 + 1", &full());
    assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
    assert_eq!(r.value, json!(2));
}

const BOMB: &str = "const a = []; while (true) a.push(new Array(100000).fill(1.5));";

#[test]
fn a_normal_script_returns_json_stdout_and_usage() {
    let (r, fake, _) = run("console.log('hi', {a: 1}); agentroid.log('two'); const t = agentroid.call('tap', {x: 1, y: 2}); ({t, n: [1, 'x', null]})", &full());
    assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
    assert_eq!(r.value, json!({"t": {"done": "tap"}, "n": [1, "x", null]}));
    assert_eq!(
        r.stdout,
        vec!["hi {\"a\":1}".to_string(), "two".to_string()]
    );
    assert_eq!(fake.calls, vec!["tap"]);
    assert_eq!(r.calls, 1);
    assert!(r.refusals.is_empty());
    assert!(r.wall_ms > 0 && r.wall_ms < 5_000, "{}", r.wall_ms);
    assert!(r.cpu_ms < 5_000, "{}", r.cpu_ms);
}

#[test]
fn async_scripts_and_undefined_results() {
    let (r, _, _) = run(
        "(async () => { await null; return agentroid.call('wait', {seconds: 0}); })()",
        &full(),
    );
    assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
    assert_eq!(r.value, json!({"done": "wait"}));
    let (r, _, _) = run("undefined", &full());
    assert_eq!((r.outcome, r.value), (Outcome::Ok, Value::Null));
}

#[test]
fn script_errors_come_back_as_errors() {
    let (r, _, _) = run("throw new RangeError('boom')", &full());
    assert_eq!(r.outcome, Outcome::ScriptError);
    assert_eq!(r.error.as_deref(), Some("RangeError: boom"));
    let (r, _, _) = run("this is not javascript", &full());
    assert_eq!(r.outcome, Outcome::ScriptError);
    assert!(r.error.unwrap().contains("SyntaxError"));
}

#[test]
fn the_child_runs_inside_the_job_object_with_the_limits() {
    let (r, _, _) = run("1", &full());
    assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
    assert!(r.job.child_in_job, "IsProcessInJob(child, job) was false");
    assert!(
        r.job.child_reported_in_job,
        "the child's own IsProcessInJob was false"
    );
    assert_eq!(r.job.memory_limit, 64 * MB);
    assert_eq!(r.job.active_process_limit, 1);
    assert!(r.job.kill_on_close);
}

#[test]
fn memory_bomb_is_refused_by_the_quickjs_limit_within_2s() {
    // Job limit raised far out of the way: only QuickJS's own limit can stop this bomb.
    let sb = sandbox().with_limits(Limits {
        job_mem_bytes: 1024 * MB,
        ..Limits::default()
    });
    let (r, _, took) = run_with(&sb, BOMB, &full());
    assert!(took < TWO_S, "{took:?}");
    assert_eq!(r.outcome, Outcome::MemoryLimit, "{r:?}");
    assert!(
        r.error.as_deref().unwrap_or("").contains("out of memory"),
        "{r:?}"
    );
    assert!(
        r.job.peak_process_memory < 96 * MB,
        "peak {} MB: the QuickJS limit did not hold",
        r.job.peak_process_memory / MB
    );
    assert_parent_healthy();
}

#[test]
fn memory_bomb_is_stopped_by_the_job_limit_within_2s() {
    // QuickJS limit raised far out of the way: only the Job Object can stop this bomb.
    let sb = sandbox().with_limits(Limits {
        js_mem_bytes: 2048 * MB,
        ..Limits::default()
    });
    let (r, _, took) = run_with(&sb, BOMB, &full());
    assert!(took < TWO_S, "{took:?}");
    assert_eq!(r.outcome, Outcome::MemoryLimit, "{r:?}");
    assert!(
        r.job.peak_process_memory <= 64 * MB,
        "peak {} MB: the job memory limit did not hold",
        r.job.peak_process_memory / MB
    );
    assert_parent_healthy();
}

#[test]
fn memory_bomb_with_default_limits_is_refused_within_2s() {
    for bomb in [
        BOMB,
        "const a = []; for (;;) a.push('x'.repeat(1 << 20) + a.length);",
        "const m = new Map(); for (let i = 0;; i++) m.set(i, {i});",
    ] {
        let (r, _, took) = run(bomb, &full());
        assert!(took < TWO_S, "{bomb}: {took:?}");
        // killed or refused: a slow bomb may meet the CPU deadline before the memory limit
        assert!(
            matches!(r.outcome, Outcome::MemoryLimit | Outcome::CpuDeadline),
            "{bomb}: {r:?}"
        );
        assert!(r.job.peak_process_memory <= 64 * MB);
    }
    assert_parent_healthy();
}

#[test]
fn infinite_loop_is_killed_within_2s() {
    for script in [
        "while (1) {}",
        "for (;;) { try { while (true) {} } catch (e) {} }",
        "function f() { for (;;) {} } try { f() } finally { for (;;) {} }",
    ] {
        let (r, _, took) = run(script, &full());
        assert!(took < TWO_S, "{script}: {took:?}");
        assert_eq!(r.outcome, Outcome::CpuDeadline, "{script}: {r:?}");
    }
    assert_parent_healthy();
}

#[test]
fn the_parent_kills_on_the_wall_deadline() {
    // sleeping is not CPU, so the child's interrupt never fires; the parent must kill the job
    let sb = sandbox().with_limits(Limits {
        wall_ms: 800,
        ..Limits::default()
    });
    let (r, _, took) = run_with(&sb, "while (true) agentroid.sleep(5000);", &full());
    assert!(took < TWO_S, "{took:?}");
    assert_eq!(r.outcome, Outcome::WallDeadline, "{r:?}");
    assert_parent_healthy();
}

#[test]
fn sleep_is_capped() {
    let sb = sandbox().with_limits(Limits {
        max_sleep_ms: 50,
        ..Limits::default()
    });
    let (r, _, took) = run_with(
        &sb,
        "agentroid.sleep(1e9); agentroid.sleep(NaN); agentroid.sleep(-5); 'ok'",
        &full(),
    );
    assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
    assert!(took < TWO_S, "{took:?}");
}

#[test]
fn a_million_taps_are_stopped_by_the_call_cap_within_2s() {
    let (r, fake, took) = run(
        "for (let i = 0; i < 1e6; i++) agentroid.call('tap', {x: 1, y: 1});",
        &full(),
    );
    assert!(took < TWO_S, "{took:?}");
    assert_eq!(r.outcome, Outcome::CallCap, "{r:?}");
    let cap = Limits::default().max_calls as usize;
    assert_eq!(fake.calls.len(), cap, "taps that reached the bridge");
    assert_eq!(r.calls as usize, cap + 1);
    // catching the errors does not help: the cap kills the run
    let (r, fake, took) = run("for (let i = 0; i < 1e6; i++) { try { agentroid.shell('getprop ro.serialno') } catch (e) {} }", &full());
    assert!(took < TWO_S, "{took:?}");
    assert_eq!(r.outcome, Outcome::CallCap, "{r:?}");
    assert_eq!(fake.shells.len(), cap);
    assert_parent_healthy();
}

#[test]
fn eval_and_function_are_refused() {
    let attempts = [
        "eval('1 + 1')",
        "globalThis.eval('1')",
        "(0, eval)('1')",
        "new Function('return 1')()",
        "Function('return 1')()",
        "globalThis.Function('return 1')()",
        "(function () {}).constructor('return 1')()",
        "(() => 0).constructor('return 1')()",
        "(async function () {}).constructor('return 1')",
        "(function* () {}).constructor('yield 1')",
        "(async function* () {}).constructor('')",
        "class A {}; A.constructor('return 1')()",
        "Object.getPrototypeOf(function () {}).constructor('return 1')()",
        "Reflect.construct(Object.getPrototypeOf(function () {}).constructor, ['return 1'])()",
        "agentroid.call.constructor('return 1')()",
        "const P = Object.getPrototypeOf(function () {}); delete P.constructor; P.constructor('return 1')()",
        "'use strict'; const P = Object.getPrototypeOf(function () {}); P.constructor = null; P.constructor('return 1')()",
        "Object.defineProperty(Object.getPrototypeOf(function () {}), 'constructor', {value: 1}); 1",
    ];
    for a in attempts {
        let (r, fake, took) = run(a, &full());
        assert_eq!(r.outcome, Outcome::ScriptError, "{a}: {r:?}");
        assert!(took < TWO_S, "{a}: {took:?}");
        assert!(fake.calls.is_empty() && fake.shells.is_empty());
    }
    let (r, _, _) = run("[typeof eval, typeof Function, typeof globalThis.eval, 'eval' in globalThis, 'Function' in globalThis]", &full());
    assert_eq!(
        r.value,
        json!(["undefined", "undefined", "undefined", false, false]),
        "{r:?}"
    );
    let (r, _, _) = run(
        "try { (function(){}).constructor('return 1') } catch (e) { e.message }",
        &full(),
    );
    assert_eq!(
        r.value,
        json!("eval and the Function constructor are disabled in run_script")
    );
}

#[test]
fn no_fs_net_or_timers() {
    let (r, _, _) = run(
        "['require','process','std','os','fetch','XMLHttpRequest','WebSocket','setTimeout','setInterval','setImmediate','queueMicrotask','importScripts','Deno','Bun'].filter(n => typeof globalThis[n] !== 'undefined')",
        &full(),
    );
    assert_eq!(r.value, json!([]), "{r:?}");
    let (r, _, _) = run("import('fs')", &full());
    assert_ne!(r.outcome, Outcome::Ok, "{r:?}");
}

#[test]
fn stdout_is_capped() {
    let sb = sandbox().with_limits(Limits {
        max_stdout_lines: 20,
        max_line_chars: 10,
        ..Limits::default()
    });
    let (r, _, _) = run_with(
        &sb,
        "for (let i = 0; i < 1000; i++) console.log('line ' + i + ' ' + 'x'.repeat(50)); 'ok'",
        &full(),
    );
    assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
    assert_eq!(r.stdout.len(), 20);
    assert!(r.stdout_truncated);
    assert!(
        r.stdout.iter().all(|l| l.chars().count() <= 11),
        "{:?}",
        r.stdout
    );
}

/// Calls every core tool once (errors caught) and returns how many were refused.
fn every_tool_script() -> String {
    let mut s = String::from("let refused = 0;\n");
    for c in Call::samples() {
        s.push_str(&format!("try {{ agentroid.call({:?}, {}) }} catch (e) {{ if (String(e.message).startsWith('refused')) refused++; }}\n", c.name(), sample_args(c.name())));
    }
    s.push_str("try { agentroid.shell('getprop ro.serialno') } catch (e) { refused++; }\nrefused");
    s
}

#[test]
fn read_only_refuses_every_call() {
    let (r, fake, _) = run(&every_tool_script(), &device(DeviceMode::ReadOnly));
    assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
    let n = Call::samples().len() + 1;
    assert_eq!(r.value, json!(n));
    assert!(
        fake.calls.is_empty() && fake.shells.is_empty(),
        "the bridge was reached: {:?} {:?}",
        fake.calls,
        fake.shells
    );
    assert_eq!(r.refusals.len(), n);
    for rec in &r.refusals {
        if ACTING_TOOLS.contains(&rec.request.as_str()) {
            assert_eq!(rec.kind, "gate", "{rec:?}");
            assert!(rec.reason.contains("read_only"), "{rec:?}");
        }
    }
    assert_eq!(
        r.refusals.iter().filter(|x| x.kind == "gate").count(),
        ACTING_TOOLS.len()
    );
}

#[test]
fn the_s24_is_refused_whatever_it_is_configured_as() {
    let s24 = Device {
        serial: "PROTECTED0000EXAMPLE".into(),
        hw_serial: "PROTECTED0000EXAMPLE".into(),
        allowed: true,
        mode: DeviceMode::Full,
    };
    let (r, fake, _) = run(&every_tool_script(), &s24);
    assert_eq!(r.value, json!(Call::samples().len() + 1), "{r:?}");
    assert!(fake.calls.is_empty() && fake.shells.is_empty());
}

#[test]
fn full_passes_every_tool_and_helper_follows_the_gate() {
    let (r, fake, _) = run(&every_tool_script(), &full());
    assert_eq!(r.value, json!(0), "{r:?}");
    assert_eq!(fake.calls.len(), Call::samples().len());
    let (r, fake, _) = run(&every_tool_script(), &device(DeviceMode::Helper));
    // helper: unlock, open_app, open_url, start_activity need adb shell; shell is full only
    assert_eq!(r.value, json!(5), "{r:?}");
    assert_eq!(fake.calls.len(), Call::samples().len() - 4);
    assert!(fake.shells.is_empty());
    let unlisted = Device {
        allowed: false,
        ..full()
    };
    let (r, fake, _) = run(&every_tool_script(), &unlisted);
    assert_eq!(r.value, json!(Call::samples().len() + 1), "{r:?}");
    assert!(fake.calls.is_empty());
}

#[test]
fn unknown_tools_are_refused_and_journalled() {
    let (r, fake, _) = run(
        "try { agentroid.call('run_script', {script: '1'}) } catch (e) { e.message }",
        &full(),
    );
    assert!(r.value.as_str().unwrap().starts_with("refused"), "{r:?}");
    assert!(fake.calls.is_empty());
    assert_eq!(r.refusals[0].kind, "invalid");
}

const FIXTURES: [&str; 7] = [
    "pm clear com.x",
    "dumpsys battery set level 5",
    "dumpsys battery reset",
    "dumpsys battery unplug",
    "dumpsys deviceidle force-idle",
    "dumpsys deviceidle unforce",
    "dumpsys deviceidle step",
];

fn ten_times(cmd: &str) -> String {
    format!("let refused = 0; for (let i = 0; i < 10; i++) {{ try {{ agentroid.shell({cmd:?}) }} catch (e) {{ if (String(e.message).startsWith('refused')) refused++; }} }} refused")
}

#[test]
fn refusal_fixtures_are_refused_10_of_10_on_full_devices() {
    for cmd in FIXTURES {
        let (r, fake, _) = run(&ten_times(cmd), &full());
        assert_eq!(r.outcome, Outcome::Ok, "{cmd}: {r:?}");
        assert_eq!(r.value, json!(10), "{cmd}: {r:?}");
        assert!(fake.shells.is_empty(), "{cmd} reached the bridge");
        assert_eq!(r.refusals.len(), 10, "{cmd}");
        for rec in &r.refusals {
            assert_eq!(
                (rec.binding, rec.kind, rec.class),
                ("shell", "shell_class", Some(RiskClass::R2)),
                "{rec:?}"
            );
            assert!(
                rec.reason.contains("no exception, no chat route"),
                "{rec:?}"
            );
            assert_eq!(rec.request, cmd);
        }
    }
}

#[test]
fn refusal_fixtures_are_refused_10_of_10_on_read_only_devices() {
    for cmd in FIXTURES {
        let (r, fake, _) = run(&ten_times(cmd), &device(DeviceMode::ReadOnly));
        assert_eq!(r.value, json!(10), "{cmd}: {r:?}");
        assert!(fake.shells.is_empty());
        assert_eq!(r.refusals.len(), 10);
    }
}

#[test]
fn an_allowlisted_read_passes_as_r0() {
    let (r, fake, _) = run("[agentroid.shell('getprop ro.serialno'), agentroid.shell('dumpsys battery'), agentroid.shell('pm list packages -3')]", &full());
    assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
    assert_eq!(
        r.value,
        json!([
            "fake:getprop ro.serialno",
            "fake:dumpsys battery",
            "fake:pm list packages -3"
        ])
    );
    assert_eq!(
        fake.shells[0],
        vec!["getprop".to_string(), "ro.serialno".to_string()]
    );
    assert!(r.refusals.is_empty());
}

#[test]
fn metacharacter_commands_are_refused_10_of_10() {
    for cmd in [
        "pm list packages; pm clear com.x",
        "getprop x && input tap 1 1",
        "getprop $(input tap 1 1)",
        "settings get system a|settings put system a 1",
        "getprop ro.serialno\ninput tap 1 1",
        "wm size 720x1280",
        "wm density 240",
    ] {
        let (r, fake, _) = run(&ten_times(cmd), &full());
        assert_eq!(r.value, json!(10), "{cmd:?}: {r:?}");
        assert!(fake.shells.is_empty(), "{cmd:?} reached the bridge");
        assert!(r.refusals.iter().all(|x| x.class == Some(RiskClass::R2)));
    }
}

#[test]
fn oversized_scripts_are_refused_before_spawning() {
    let big = "1;".repeat(200_000);
    let (r, _, _) = run(&big, &full());
    assert_eq!(r.outcome, Outcome::SandboxError);
    assert!(!r.job.child_in_job);
}

#[test]
fn a_missing_child_binary_is_a_sandbox_error() {
    let sb = Sandbox::new("Z:\\definitely\\not\\here\\agentroid-sandbox.exe");
    let (r, _, _) = run_with(&sb, "1", &full());
    assert_eq!(r.outcome, Outcome::SandboxError);
}
