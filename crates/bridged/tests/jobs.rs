//! Task 2.10 slice b: the `job.*` JSON-RPC methods and `job.run_script`, end to end over the
//! in-memory duplex listener. Fake device only: no adb, no phone, no real pipe.

use agentroid_core::fake::{FakeClock, FakeDevice};
use agentroid_core::gate::DeviceMode;
use agentroid_core::transport::{
    Backend, Core, DeviceInfo, Input, Screenshot, Transport, TransportError,
};
use bridged::jobs::JobsConfig;
use bridged::listener::{duplex_listener, DuplexConnector};
use bridged::proto::*;
use bridged::registry::Registry;
use bridged::server::{self, CoreBackend, Daemon};
use serde_json::{json, Value};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::watch;

const POCO: &str = "DEVICE0000EXAMPLE";
const APP: &str = "com.android.settings";

fn run<F: Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(60), f)
            .await
            .expect("test timed out")
    })
}

/// The fake device, plus a gate the test can close to make `tree_xml` block (a slow phone).
#[derive(Clone)]
struct Shared {
    dev: Arc<Mutex<FakeDevice>>,
    hold: Arc<AtomicBool>,
    script: Arc<Script>,
}

/// Screen-read scripting for the 2.11a tests.
#[derive(Default)]
struct Script {
    reads: AtomicUsize,
    /// With `hold` set: 0 blocks every read (the old behaviour); n blocks only reads after the n-th.
    block_after: AtomicUsize,
    /// When the first blocked read began.
    blocked_at: Mutex<Option<Instant>>,
    /// After this many reads the screen becomes this XML.
    later: Mutex<Option<(usize, String)>>,
}

impl Transport for Shared {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        self.dev.lock().unwrap().devices()
    }
    fn screen_size(&mut self, s: &str) -> Result<(i32, i32), TransportError> {
        self.dev.lock().unwrap().screen_size(s)
    }
    fn screen_state(&mut self, s: &str) -> Result<String, TransportError> {
        self.dev.lock().unwrap().screen_state(s)
    }
    fn screenshot(&mut self, s: &str, w: i32) -> Result<Screenshot, TransportError> {
        self.dev.lock().unwrap().screenshot(s, w)
    }
    fn tree_xml(&mut self, s: &str, b: Backend) -> Result<String, TransportError> {
        let n = self.script.reads.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some((after, xml)) = &*self.script.later.lock().unwrap() {
            if n > *after {
                self.dev.lock().unwrap().xml_override = Some(xml.clone());
            }
        }
        let after = self.script.block_after.load(Ordering::SeqCst);
        let t = Instant::now();
        while self.hold.load(Ordering::SeqCst)
            && (after == 0 || n > after)
            && t.elapsed() < Duration::from_secs(20)
        {
            self.script.blocked_at.lock().unwrap().get_or_insert(t);
            std::thread::sleep(Duration::from_millis(5));
        }
        self.dev.lock().unwrap().tree_xml(s, b)
    }
    fn input(&mut self, s: &str, i: &Input) -> Result<(), TransportError> {
        self.dev.lock().unwrap().input(s, i)
    }
    fn packages(&mut self, s: &str) -> Result<Vec<String>, TransportError> {
        self.dev.lock().unwrap().packages(s)
    }
    fn shell(&mut self, s: &str, l: &str) -> Result<String, TransportError> {
        self.dev.lock().unwrap().shell(s, l)
    }
    fn helper_status(&mut self, s: &str) -> Result<(), String> {
        self.dev.lock().unwrap().helper_status(s)
    }
}

/// A Settings screen whose nodes carry the app's package (the engine's foreground check).
fn tree(extra: &[(&str, &str, &str)]) -> String {
    let mut nodes = vec![
        (APP, "Settings", "[48,200][400,280]"),
        (APP, "Bluetooth", "[0,720][1220,880]"),
        (APP, "Display", "[0,900][1220,1060]"),
    ];
    nodes.extend_from_slice(extra);
    let mut s = String::from("<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\"><node class=\"android.widget.FrameLayout\" package=\"\" text=\"\" content-desc=\"\" resource-id=\"\" clickable=\"false\" bounds=\"[0,0][1220,2712]\">");
    for (pkg, text, b) in nodes {
        s += &format!("<node class=\"android.widget.Button\" package=\"{pkg}\" text=\"{text}\" content-desc=\"\" resource-id=\"\" clickable=\"true\" bounds=\"{b}\"/>");
    }
    s + "</node></hierarchy>"
}

/// An overlay no popup rule knows: the engine pauses with needs_attention.
fn unknown_overlay() -> String {
    tree(&[("com.example.nag", "Rate us!", "[100,1200][1120,1500]")])
}

struct Harness {
    connector: DuplexConnector,
    _shutdown: watch::Sender<bool>,
    daemon: Arc<Daemon>,
    dev: Arc<Mutex<FakeDevice>>,
    hold: Arc<AtomicBool>,
    script: Arc<Script>,
    dir: PathBuf,
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("jobs")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The slice-c child, built next to this test's exe by `cargo test --workspace`.
fn sandbox_exe() -> PathBuf {
    let me = std::env::current_exe().unwrap();
    let exe = me
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .join(if cfg!(windows) {
            "agentroid-sandbox.exe"
        } else {
            "agentroid-sandbox"
        });
    assert!(exe.exists(), "{} is missing: run `cargo test --offline --workspace` (or `cargo build --offline -p agentroid-sandbox`) first", exe.display());
    exe
}

fn start_with(name: &str, mode: DeviceMode, tweak: impl FnOnce(&mut JobsConfig)) -> Harness {
    start_full(name, mode, false, tweak)
}

/// `start_with`, and with `audit` core's audit log at `<dir>/audit.jsonl` (D3b).
fn start_full(
    name: &str,
    mode: DeviceMode,
    audit: bool,
    tweak: impl FnOnce(&mut JobsConfig),
) -> Harness {
    let dev = Arc::new(Mutex::new(FakeDevice::new()));
    dev.lock().unwrap().xml_override = Some(tree(&[]));
    let hold = Arc::new(AtomicBool::new(false));
    let script = Arc::new(Script::default());
    let dir = tmp_dir(name);
    let mut core = Core::new(
        Shared {
            dev: dev.clone(),
            hold: hold.clone(),
            script: script.clone(),
        },
        FakeClock::new(),
    );
    if audit {
        core.set_audit(agentroid_core::audit::AuditLog::open(&dir.join("audit.jsonl")).unwrap());
    }
    let mut backend = CoreBackend::new(core);
    backend.set_mode(POCO, mode);
    let mut cfg = JobsConfig::new(dir.join("runs"));
    tweak(&mut cfg);
    let daemon = Daemon::with_jobs(Registry::in_memory(), Box::new(backend), Some(cfg));
    let (listener, connector) = duplex_listener();
    let (shutdown, rx) = watch::channel(false);
    tokio::spawn(server::serve(daemon.clone(), listener, rx));
    Harness {
        connector,
        _shutdown: shutdown,
        daemon,
        dev,
        hold,
        script,
        dir,
    }
}

fn start(name: &str, mode: DeviceMode) -> Harness {
    start_with(name, mode, |_| {})
}

struct Client {
    r: BufReader<ReadHalf<DuplexStream>>,
    w: WriteHalf<DuplexStream>,
    next: u64,
}

impl Client {
    async fn named(h: &Harness, session: &str) -> Client {
        let (r, w) = tokio::io::split(h.connector.connect().unwrap());
        let mut c = Client {
            r: BufReader::new(r),
            w,
            next: 1,
        };
        let r = c.call("hello", json!({ "session": session })).await;
        assert_eq!(r["result"]["holder"], session);
        c
    }
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let text =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        self.w.write_all(text.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
        let mut line = String::new();
        assert!(
            self.r.read_line(&mut line).await.unwrap() > 0,
            "connection closed"
        );
        let r: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(r["id"], id);
        r
    }
    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.call(method, params).await;
        assert!(r.get("result").is_some(), "{method} failed: {r}");
        r["result"].clone()
    }
    /// Poll `job.status` until the job is no longer running.
    async fn settled(&mut self, run_id: &str) -> Value {
        let t = Instant::now();
        loop {
            let s = self.ok("job.status", json!({ "run_id": run_id })).await;
            if s["status"] != "running" {
                return s;
            }
            assert!(
                t.elapsed() < Duration::from_secs(30),
                "job never settled: {s}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn code(r: &Value) -> i64 {
    r["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected an error, got {r}"))
}

fn flow(steps: Value) -> Value {
    json!({ "flow": "jobs-test", "version": 1, "app": { "package": APP, "version_range": "*" }, "steps": steps })
}

fn two_taps() -> Value {
    flow(
        json!([{ "id": "bt", "kind": "tap", "select": { "text": "Bluetooth" } }, { "id": "disp", "kind": "tap", "select": { "text": "Display" } }]),
    )
}

fn journal_records(path: &str) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// `true` when `v` has exactly the keys of `schema` (all required ones, nothing undeclared).
fn keys_match(v: &Value, schema: &Value) {
    let props: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
    for k in v.as_object().unwrap().keys() {
        assert!(props.contains(&k), "{k} is not in the schema");
    }
    for r in schema["required"].as_array().unwrap() {
        assert!(v.get(r.as_str().unwrap()).is_some(), "required {r} missing");
    }
}

// ---------------------------------------------------------------- the lease

#[test]
fn every_job_tool_needs_the_lease() {
    run(async {
        let h = start("lease", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        let mut b = Client::named(&h, "claude-b").await;
        // no lease at all: the device-level tools refuse before doing anything
        for (m, p) in [
            ("job.start", json!({ "device": POCO, "flow": two_taps() })),
            (
                "job.dry_run",
                json!({ "device": POCO, "flow": two_taps(), "trees": [{ "name": "t", "xml": tree(&[]) }] }),
            ),
            (
                "job.run_script",
                json!({ "device": POCO, "script": "1 + 1" }),
            ),
        ] {
            let r = a.call(m, p).await;
            assert_eq!(code(&r), NOT_HOLDER, "{m}: {r}");
            assert!(
                r["error"]["message"].as_str().unwrap().contains("lease"),
                "{m}: {r}"
            );
        }
        assert!(
            std::fs::read_dir(h.dir.join("runs")).map_or(true, |mut d| d.next().is_none()),
            "a refused start creates nothing"
        );
        // a holds the lease and starts a job; b (no lease) may not see or steer it
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        h.dev.lock().unwrap().xml_override = Some(unknown_overlay());
        let started = a
            .ok("job.start", json!({ "device": POCO, "flow": two_taps() }))
            .await;
        let run_id = started["run_id"].as_str().unwrap().to_string();
        assert_eq!(a.settled(&run_id).await["status"], "paused");
        for (m, p) in [
            ("job.status", json!({ "run_id": run_id })),
            ("job.pause", json!({ "run_id": run_id })),
            ("job.resume", json!({ "run_id": run_id })),
            ("job.stop", json!({ "run_id": run_id })),
            ("job.receipt", json!({ "run_id": run_id })),
            ("job.start", json!({ "device": POCO, "flow": two_taps() })),
            (
                "job.dry_run",
                json!({ "device": POCO, "flow": two_taps(), "trees": [] }),
            ),
            (
                "job.run_script",
                json!({ "device": POCO, "script": "1 + 1" }),
            ),
        ] {
            let r = b.call(m, p).await;
            assert_eq!(code(&r), NOT_HOLDER, "{m}: {r}");
            assert!(
                r["error"]["message"].as_str().unwrap().contains("claude-a"),
                "the refusal names the holder: {r}"
            );
        }
        // b's attempts changed nothing
        let s = a.ok("job.status", json!({ "run_id": run_id })).await;
        assert_eq!(s["status"], "paused", "{s}");
        assert!(h.dev.lock().unwrap().acted.is_empty());
        assert_eq!(
            code(
                &a.call("job.status", json!({ "run_id": "job-0-nope" }))
                    .await
            ),
            UNKNOWN_JOB
        );
        assert_eq!(
            code(&a.call("job.status", json!({ "run_id": "..\\..\\x" })).await),
            UNKNOWN_JOB
        );
    });
}

#[test]
fn a_job_without_a_jobs_dir_is_a_state_error_not_a_crash() {
    run(async {
        let dev = Arc::new(Mutex::new(FakeDevice::new()));
        let backend = CoreBackend::new(Core::new(
            Shared {
                dev,
                hold: Arc::new(AtomicBool::new(false)),
                script: Arc::default(),
            },
            FakeClock::new(),
        ));
        let daemon = Daemon::new(Registry::in_memory(), Box::new(backend));
        let (listener, connector) = duplex_listener();
        let (_tx, rx) = watch::channel(false);
        tokio::spawn(server::serve(daemon, listener, rx));
        let (r, w) = tokio::io::split(connector.connect().unwrap());
        let mut c = Client {
            r: BufReader::new(r),
            w,
            next: 1,
        };
        assert_eq!(
            code(&c.call("job.status", json!({ "run_id": "x" })).await),
            STATE_ERROR
        );
    });
}

// ---------------------------------------------------------------- job.start

#[test]
fn job_start_refuses_an_r2_flow_before_creating_anything() {
    run(async {
        let h = start("r2", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let f = flow(
            json!([{ "id": "bt", "kind": "tap", "select": { "text": "Bluetooth" } }, { "id": "wipe", "kind": "tap", "select": { "text": "Display" }, "risk": "R2" }]),
        );
        let r = a
            .call("job.start", json!({ "device": POCO, "flow": f }))
            .await;
        assert_eq!(code(&r), REFUSED, "{r}");
        assert!(
            r["error"]["message"].as_str().unwrap().contains("R2"),
            "{r}"
        );
        assert_eq!(r["error"]["data"]["step_id"], "wipe");
        assert!(
            std::fs::read_dir(h.dir.join("runs")).map_or(true, |mut d| d.next().is_none()),
            "nothing was created"
        );
        assert!(
            h.dev.lock().unwrap().acted.is_empty(),
            "nothing reached the phone"
        );
        // an invalid flow is INVALID_PARAMS, from the loader
        let r = a
            .call(
                "job.start",
                json!({ "device": POCO, "flow": { "flow": "x" } }),
            )
            .await;
        assert_eq!(code(&r), INVALID_PARAMS, "{r}");
        // YAML text works as well as an object
        let yaml = format!("flow: y\nversion: 1\napp: {{package: {APP}, version_range: '*'}}\nsteps: [{{id: a, kind: tap, select: {{text: Bluetooth}}, risk: R3}}]\n");
        assert_eq!(
            code(
                &a.call("job.start", json!({ "device": POCO, "flow": yaml }))
                    .await
            ),
            REFUSED
        );
    });
}

#[test]
fn a_job_runs_to_done_and_its_receipt_matches_receipt_json() {
    run(async {
        let h = start("done", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let started = a
            .ok("job.start", json!({ "device": POCO, "flow": two_taps() }))
            .await;
        assert_eq!(started["status"], "running");
        let run_id = started["run_id"].as_str().unwrap().to_string();
        let s = a.settled(&run_id).await;
        assert_eq!(s["status"], "done", "{s}");
        assert_eq!(
            (s["steps_planned"].as_u64(), s["steps_done"].as_u64()),
            (Some(2), Some(2))
        );
        assert_eq!(
            h.dev
                .lock()
                .unwrap()
                .acted
                .iter()
                .filter(|x| x.starts_with("Tap"))
                .count(),
            2,
            "{:?}",
            h.dev.lock().unwrap().acted
        );
        // one job per device
        let again = a
            .ok("job.start", json!({ "device": POCO, "flow": two_taps() }))
            .await;
        a.settled(again["run_id"].as_str().unwrap()).await;
        // the journal: intent (planned, sent) before every tap, all records shaped by journal.json
        let recs = journal_records(started["journal"].as_str().unwrap());
        let statuses: Vec<(&str, &str)> = recs
            .iter()
            .map(|r| {
                (
                    r["step_id"].as_str().unwrap(),
                    r["status"].as_str().unwrap(),
                )
            })
            .collect();
        for step in ["bt", "disp"] {
            let pos = |st: &str| {
                statuses
                    .iter()
                    .position(|x| *x == (step, st))
                    .unwrap_or_else(|| panic!("{step} {st} missing: {statuses:?}"))
            };
            assert!(
                pos("planned") < pos("sent") && pos("sent") < pos("done"),
                "{statuses:?}"
            );
        }
        let jschema: Value = serde_json::from_str(
            &std::fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/journal.json"),
            )
            .unwrap(),
        )
        .unwrap();
        for (i, r) in recs.iter().enumerate() {
            keys_match(r, &jschema);
            assert_eq!(r["seq"].as_u64(), Some(i as u64));
        }
        // the receipt: exactly receipt.json's card, plus the panel detail
        let r = a.ok("job.receipt", json!({ "run_id": run_id })).await;
        let schema: Value = serde_json::from_str(
            &std::fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/receipt.json"),
            )
            .unwrap(),
        )
        .unwrap();
        keys_match(&r["receipt"], &schema);
        assert_eq!(r["receipt"]["steps_done"], 2);
        assert_eq!(r["detail"]["outcome"], "done");
        assert_eq!(r["detail"]["steps"]["not_run"], 0);
        for k in ["wall", "work", "cooling", "waiting"] {
            assert!(r["detail"]["time_s"][k].is_number(), "time_s.{k}: {r}");
        }
        // the stored receipt is the same one, and the Python check validates it with jsonschema
        let stored: Value = serde_json::from_str(
            &std::fs::read_to_string(h.dir.join("runs").join(&run_id).join("receipt.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stored["receipt"], r["receipt"]);
        let out = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../generated/receipt-daemon-sample.json");
        std::fs::write(
            out,
            serde_json::to_string_pretty(&r["receipt"]).unwrap() + "\n",
        )
        .unwrap();
        // done is terminal
        assert_eq!(
            code(&a.call("job.stop", json!({ "run_id": run_id })).await),
            JOB_ERROR
        );
        assert_eq!(
            code(&a.call("job.resume", json!({ "run_id": run_id })).await),
            JOB_ERROR
        );
    });
}

// ---------------------------------------------------------------- pause, resume, stop

/// Close the gate so the job blocks inside its first screen read; open it after `ms`.
fn slow_phone(h: &Harness, ms: u64) {
    h.hold.store(true, Ordering::SeqCst);
    let hold = h.hold.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(ms));
        hold.store(false, Ordering::SeqCst);
    });
}

#[test]
fn pause_resume_and_stop_steer_a_running_job_at_step_boundaries() {
    run(async {
        let h = start("steer", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        slow_phone(&h, 400);
        let run_id = a
            .ok("job.start", json!({ "device": POCO, "flow": two_taps() }))
            .await["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let s = a.ok("job.pause", json!({ "run_id": run_id })).await;
        assert_eq!(s["status"], "paused", "{s}");
        assert_eq!(s["pause_reason"], "paused_by_user");
        assert_eq!(
            s["steps_done"], 0,
            "paused before the first step started: {s}"
        );
        assert!(h.dev.lock().unwrap().acted.is_empty());
        // pausing a paused job is fine; resume carries on to done, each tap sent once
        a.ok("job.pause", json!({ "run_id": run_id })).await;
        a.ok("job.resume", json!({ "run_id": run_id })).await;
        let s = a.settled(&run_id).await;
        assert_eq!(s["status"], "done", "{s}");
        assert_eq!(
            h.dev.lock().unwrap().acted.len(),
            2,
            "{:?}",
            h.dev.lock().unwrap().acted
        );
        // stop a running job
        slow_phone(&h, 400);
        let run2 = a
            .ok("job.start", json!({ "device": POCO, "flow": two_taps() }))
            .await["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let s = a.ok("job.stop", json!({ "run_id": run2 })).await;
        assert_eq!(s["status"], "stopped", "{s}");
        let r = a.ok("job.receipt", json!({ "run_id": run2 })).await;
        assert_eq!(r["detail"]["outcome"], "stopped");
        assert_eq!(r["detail"]["steps"]["not_run"], 2, "{r}");
    });
}

#[test]
fn a_job_paused_by_a_popup_resumes_after_the_screen_is_fixed_even_after_a_restart() {
    run(async {
        let h = start("popup", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        h.dev.lock().unwrap().xml_override = Some(unknown_overlay());
        let run_id = a
            .ok("job.start", json!({ "device": POCO, "flow": two_taps() }))
            .await["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let s = a.settled(&run_id).await;
        assert_eq!(s["status"], "paused", "{s}");
        assert!(s["pause_reason"].as_str().is_some(), "{s}");
        assert!(h.dev.lock().unwrap().acted.is_empty());
        // a fresh daemon on the same jobs dir (the restart), same phone, screen fixed
        h.dev.lock().unwrap().xml_override = Some(tree(&[]));
        let dir = h.dir.join("runs");
        let backend = CoreBackend::new(Core::new(
            Shared {
                dev: h.dev.clone(),
                hold: h.hold.clone(),
                script: Arc::default(),
            },
            FakeClock::new(),
        ));
        let d2 = Daemon::with_jobs(
            Registry::in_memory(),
            Box::new(backend),
            Some(JobsConfig::new(dir)),
        );
        let (listener, connector) = duplex_listener();
        let (_tx, rx) = watch::channel(false);
        tokio::spawn(server::serve(d2, listener, rx));
        let (r, w) = tokio::io::split(connector.connect().unwrap());
        let mut c = Client {
            r: BufReader::new(r),
            w,
            next: 1,
        };
        c.ok("hello", json!({ "session": "claude-a" })).await;
        c.ok("lease.acquire", json!({ "device": POCO })).await;
        let s = c.ok("job.status", json!({ "run_id": run_id })).await;
        assert_eq!(s["status"], "paused", "loaded from disk: {s}");
        c.ok(
            "job.resume",
            json!({ "run_id": run_id, "human_confirmed": true }),
        )
        .await;
        let s = c.settled(&run_id).await;
        assert_eq!(s["status"], "done", "{s}");
        let recs = journal_records(
            h.dir
                .join("runs")
                .join(&run_id)
                .join("journal.ndjson")
                .to_str()
                .unwrap(),
        );
        for (i, r) in recs.iter().enumerate() {
            assert_eq!(
                r["seq"].as_u64(),
                Some(i as u64),
                "seq continues across the restart"
            );
        }
        let _ = &h.daemon;
    });
}

// ---------------------------------------------------------------- dry run

#[test]
fn dry_run_matches_saved_trees_without_touching_the_phone() {
    run(async {
        let h = start("dry", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let r = a.ok("job.dry_run", json!({ "device": POCO, "flow": two_taps(), "trees": [{ "name": "settings", "xml": tree(&[]) }] })).await;
        assert_eq!(r["matched"], 2, "{r}");
        assert_eq!(r["rows"].as_array().unwrap().len(), 2);
        assert!(r["text"].as_str().unwrap().contains("bt"));
        let d = h.dev.lock().unwrap();
        assert!(
            d.acted.is_empty() && !d.calls.contains(&"tree_xml"),
            "{:?}",
            d.calls
        );
    });
}

// ---------------------------------------------------------------- run_script

#[test]
#[cfg_attr(
    not(windows),
    ignore = "run_script needs a Windows Job Object; on other systems the sandbox refuses by design"
)]
fn run_script_on_read_only_journals_every_refusal_and_sends_nothing() {
    run(async {
        let h = start_with("script-ro", DeviceMode::ReadOnly, |c| {
            c.sandbox_exe = Some(sandbox_exe())
        });
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let script = "let n = 0;\n\
            for (const [t, args] of [['tap', {x: 1, y: 2}], ['type', {text: 'hi'}], ['swipe', {x1: 1, y1: 1, x2: 5, y2: 5}], ['key', {key: 'back'}]]) {\n\
              try { agentroid.call(t, args); } catch (e) { n++; }\n\
            }\n\
            try { agentroid.shell('getprop ro.product.model'); } catch (e) { n++; }\n\
            try { agentroid.call('ui_tree', {}); } catch (e) { n++; }\n\
            ({refused: n})";
        let r = a
            .ok(
                "job.run_script",
                json!({ "device": POCO, "script": script }),
            )
            .await;
        assert_eq!(r["outcome"], "ok", "{r}");
        assert_eq!(r["value"]["refused"], 6, "{r}");
        let refusals = r["refusals"].as_array().unwrap();
        assert_eq!(refusals.len(), 6, "{r}");
        assert_eq!(r["journalled_refusals"], 6);
        let recs = journal_records(r["journal"].as_str().unwrap());
        let journalled: Vec<&Value> = recs
            .iter()
            .filter(|x| {
                x["step_id"]
                    .as_str()
                    .unwrap()
                    .starts_with("script-refusal-")
            })
            .collect();
        assert_eq!(
            journalled.len(),
            refusals.len(),
            "every refusal is in the journal: {recs:?}"
        );
        for (rec, refusal) in journalled.iter().zip(refusals) {
            assert_eq!(rec["status"], "failed");
            assert_eq!(rec["post"]["refusal"]["seq"], refusal["seq"]);
            assert!(
                rec["intent"]
                    .as_str()
                    .unwrap()
                    .contains(refusal["reason"].as_str().unwrap()),
                "{rec}"
            );
        }
        let jschema: Value = serde_json::from_str(
            &std::fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/journal.json"),
            )
            .unwrap(),
        )
        .unwrap();
        for (i, rec) in recs.iter().enumerate() {
            keys_match(rec, &jschema);
            assert_eq!(rec["seq"].as_u64(), Some(i as u64));
        }
        let d = h.dev.lock().unwrap();
        assert!(d.acted.is_empty(), "nothing acted: {:?}", d.acted);
        assert!(
            !d.touched(),
            "read_only: not even a read reached the phone: {:?}",
            d.calls
        );
    });
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "run_script needs a Windows Job Object; on other systems the sandbox refuses by design"
)]
fn run_script_on_a_full_device_acts_through_core_and_shell_needs_a_runner() {
    run(async {
        let h = start_with("script-full", DeviceMode::Full, |c| {
            c.sandbox_exe = Some(sandbox_exe())
        });
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let script = "agentroid.call('tap', {x: 100, y: 650});\n\
            let shell = 'none'; try { agentroid.shell('getprop ro.product.model'); } catch (e) { shell = String(e.message); }\n\
            let bad = 'none'; try { agentroid.shell('rm -rf /sdcard'); } catch (e) { bad = String(e.message); }\n\
            ({shell, bad})";
        let r = a
            .ok(
                "job.run_script",
                json!({ "device": POCO, "script": script }),
            )
            .await;
        assert_eq!(r["outcome"], "ok", "{r}");
        assert!(
            r["value"]["shell"]
                .as_str()
                .unwrap()
                .contains("no adb runner"),
            "{r}"
        );
        assert!(
            r["value"]["bad"].as_str().unwrap().starts_with("refused"),
            "{r}"
        );
        assert_eq!(
            r["journalled_refusals"],
            r["refusals"].as_array().unwrap().len()
        );
        assert_eq!(
            h.dev.lock().unwrap().acted.len(),
            1,
            "only the tap reached the phone"
        );
        // the tap's intent was journalled before it was sent
        let recs = journal_records(r["journal"].as_str().unwrap());
        let st: Vec<&str> = recs
            .iter()
            .filter(|x| x["step_id"] == "script-call-1")
            .map(|x| x["status"].as_str().unwrap())
            .collect();
        assert_eq!(st, ["planned", "sent", "done"], "{recs:?}");
    });
}

/// D3b: every `agentroid.shell` line ends in core's audit log: the ones the sandbox refuses
/// (through `Bridge::shell_refused`), and the ones that reach the daemon's shell guard. No runner is
/// configured here, so an allowed line is logged as allowed and then answers "no adb runner".
#[test]
#[cfg_attr(
    not(windows),
    ignore = "run_script needs a Windows Job Object; on other systems the sandbox refuses by design"
)]
fn run_script_shell_lines_are_in_the_audit_log() {
    run(async {
        let h = start_full("script-shell-audit", DeviceMode::Full, true, |c| {
            c.sandbox_exe = Some(sandbox_exe())
        });
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let script = "let out = [];\n\
            for (const cmd of ['getprop ro.product.model', 'rm -rf /sdcard', 'pm uninstall com.whatsapp', 'wipe data', 'dumpsys battery; wipe data']) {\n\
              try { agentroid.shell(cmd); out.push('ok'); } catch (e) { out.push(String(e.message)); }\n\
            }\n\
            out";
        let r = a
            .ok(
                "job.run_script",
                json!({ "device": POCO, "script": script }),
            )
            .await;
        assert_eq!(r["outcome"], "ok", "{r}");
        let out = r["value"].as_array().unwrap();
        assert!(out[0].as_str().unwrap().contains("no adb runner"), "{r}");
        for o in &out[1..] {
            assert!(o.as_str().unwrap().starts_with("refused"), "{r}");
        }
        let log = h.dir.join("audit.jsonl");
        let v = agentroid_core::audit::verify_file(&log).unwrap();
        // 5 lines: 1 allowed (getprop, logged before the runner is looked for), 4 blocked by the
        // sandbox and recorded through shell_refused
        assert_eq!((v.records, v.allowed, v.blocked), (5, 1, 4), "{v:?}");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("\"rule\":\"shell_read_only\""), "{text}");
        assert_eq!(
            text.matches("\"rule\":\"shell:sandbox\"").count(),
            4,
            "{text}"
        );
        assert!(text.contains("pm uninstall com.whatsapp"), "{text}");
        assert_eq!(
            h.dev.lock().unwrap().acted.len(),
            0,
            "nothing reached the phone"
        );
    });
}

#[test]
#[cfg_attr(
    not(windows),
    ignore = "run_script needs a Windows Job Object; on other systems the sandbox refuses by design"
)]
fn a_bridge_call_that_hangs_times_out() {
    run(async {
        let h = start_with("script-timeout", DeviceMode::Full, |c| {
            c.sandbox_exe = Some(sandbox_exe());
            c.call_timeout_ms = 300;
        });
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        slow_phone(&h, 3_000);
        let t = Instant::now();
        let r = a.ok("job.run_script", json!({ "device": POCO, "script": "let m = 'none'; try { agentroid.call('ui_tree', {}); } catch (e) { m = String(e.message); } m" })).await;
        assert!(
            r["value"]
                .as_str()
                .unwrap()
                .contains("timed out after 300 ms"),
            "{r}"
        );
        assert!(
            t.elapsed() < Duration::from_millis(2_500),
            "the script did not wait for the phone: {:?}",
            t.elapsed()
        );
    });
}

// ---------------------------------------------------------------- supervision (task 2.11a)

fn two_asserts() -> Value {
    flow(
        json!([{ "id": "bt", "kind": "assert", "select": { "text": "Bluetooth" } }, { "id": "disp", "kind": "assert", "select": { "text": "Display" } }]),
    )
}

fn progress_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .map(|l| serde_json::from_str(l).expect("never half a line"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_step_event_is_in_progress_ndjson_within_2_s_while_the_job_is_still_running() {
    run(async {
        let h = start("ndjson", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        // step bt reads the screen twice (settle); the phone then hangs inside step disp
        h.script.block_after.store(2, Ordering::SeqCst);
        h.hold.store(true, Ordering::SeqCst);
        let started = a
            .ok(
                "job.start",
                json!({ "device": POCO, "flow": two_asserts() }),
            )
            .await;
        let run_id = started["run_id"].as_str().unwrap().to_string();
        let progress = PathBuf::from(started["progress"].as_str().unwrap());
        assert_eq!(
            progress,
            h.dir.join("runs").join(&run_id).join("progress.ndjson")
        );
        // the moment the job got stuck inside the next step: bt's event was emitted before that
        let t = Instant::now();
        let blocked = loop {
            if let Some(b) = *h.script.blocked_at.lock().unwrap() {
                break b;
            }
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "the job never reached the second step"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        // a tail reading the file by path, while the job holds it open
        let found = loop {
            let lines = progress_lines(&progress);
            if let Some(ev) = lines
                .iter()
                .find(|e| e["kind"] == "step" && e["step_id"] == "bt")
            {
                break (Instant::now(), ev.clone());
            }
            assert!(
                blocked.elapsed() < Duration::from_secs(2),
                "bt's step event was not in progress.ndjson within 2 s: {lines:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert!(found.0.duration_since(blocked) < Duration::from_secs(2));
        let s = a.ok("job.status", json!({ "run_id": run_id })).await;
        assert_eq!(
            s["status"], "running",
            "the event was read while the job ran: {s}"
        );
        // the line is progress.json-shaped
        let schema: Value = serde_json::from_str(
            &std::fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/progress.json"),
            )
            .unwrap(),
        )
        .unwrap();
        keys_match(&found.1, &schema);
        assert_eq!(found.1["run_id"], run_id.as_str());
        h.hold.store(false, Ordering::SeqCst);
        assert_eq!(a.settled(&run_id).await["status"], "done");
        let lines = progress_lines(&progress);
        for (i, e) in lines.iter().enumerate() {
            keys_match(e, &schema);
            assert_eq!(e["seq"].as_u64(), Some(i as u64));
        }
        assert_eq!(lines.last().unwrap()["kind"], "done");
    });
}

#[test]
fn a_stuck_job_dumps_its_last_10_trees_and_3_frames_to_flight() {
    run(async {
        let h = start("flight", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        // after 8 steps the battery-saver sheet comes up and comes back after every Got it
        let sheet = tree(&[
            (
                "com.miui.powerkeeper",
                "Battery saver",
                "[0,1800][1220,1900]",
            ),
            (
                "com.miui.powerkeeper",
                "Battery saver is on. Some apps may be limited.",
                "[0,1920][1220,2300]",
            ),
            ("com.miui.powerkeeper", "Got it", "[100,2400][1120,2550]"),
        ]);
        *h.script.later.lock().unwrap() = Some((16, sheet));
        let steps: Vec<Value> = (0..12).map(|i| json!({ "id": format!("a{i}"), "kind": "assert", "select": { "text": "Bluetooth" } })).collect();
        let started = a.ok("job.start", json!({ "device": POCO, "flow": flow(Value::Array(steps)), "oem": "xiaomi", "sdk": 34 })).await;
        let run_id = started["run_id"].as_str().unwrap().to_string();
        let s = a.settled(&run_id).await;
        assert_eq!(s["status"], "stuck", "{s}");
        // the dump is on disk by the time the status says stuck
        let flight = h.dir.join("runs").join(&run_id).join("flight");
        let names: Vec<String> = std::fs::read_dir(&flight)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with("tree-") && n.ends_with(".xml"))
                .count(),
            10,
            "{names:?}"
        );
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with("frame-") && n.ends_with(".jpg"))
                .count(),
            3,
            "{names:?}"
        );
        let index: Value =
            serde_json::from_str(&std::fs::read_to_string(flight.join("index.json")).unwrap())
                .unwrap();
        assert_eq!(index["run_id"], run_id.as_str());
        assert_eq!(
            (
                index["trees"].as_array().unwrap().len(),
                index["frames"].as_array().unwrap().len()
            ),
            (10, 3)
        );
        assert!(std::fs::read_to_string(flight.join("tree-09.xml"))
            .unwrap()
            .contains("Battery saver"));
        assert_eq!(
            h.dev
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|c| **c == "screenshot")
                .count(),
            3,
            "3 frames, no more screenshots"
        );
        let lines = progress_lines(&PathBuf::from(started["progress"].as_str().unwrap()));
        assert_eq!(lines.last().unwrap()["kind"], "stuck");
    });
}

#[test]
fn job_start_prunes_an_8_day_old_run_and_keeps_a_6_day_old_one_by_the_injected_clock() {
    const DAY: u64 = 24 * 3600 * 1000;
    // a clock far from the real one: with the system clock both runs would look young
    const NOW: u64 = 1_900_000_000_000;
    run(async {
        let h = start_with("prune", DeviceMode::Full, |c| c.clock = Arc::new(|| NOW));
        let runs = h.dir.join("runs");
        let aged = |name: &str, age: u64| -> PathBuf {
            let d = runs.join(name);
            std::fs::create_dir_all(&d).unwrap();
            for f in ["job.json", "journal.ndjson", "progress.ndjson"] {
                std::fs::write(d.join(f), "{}\n").unwrap();
                let t = std::time::UNIX_EPOCH + Duration::from_millis(NOW - age);
                std::fs::File::options()
                    .write(true)
                    .open(d.join(f))
                    .unwrap()
                    .set_modified(t)
                    .unwrap();
            }
            d
        };
        let old = aged("job-1-old", 8 * DAY);
        let young = aged("job-2-young", 6 * DAY);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        assert!(
            old.exists() && young.exists(),
            "nothing is pruned before a job starts"
        );
        let started = a
            .ok(
                "job.start",
                json!({ "device": POCO, "flow": two_asserts() }),
            )
            .await;
        assert!(!old.exists(), "8 days old: pruned at job start");
        assert!(young.exists(), "6 days old: kept");
        let run_id = started["run_id"].as_str().unwrap().to_string();
        assert!(runs.join(&run_id).exists(), "the new run is there");
        assert_eq!(a.settled(&run_id).await["status"], "done");
    });
}

// ---------------------------------------------------------------- run_steps (job.start {steps, app}, 25 Oct)

fn ten_steps() -> Value {
    json!([
        { "kind": "assert", "select": { "text": "Settings" } },
        { "kind": "tap", "select": { "text": "Bluetooth" } },
        { "kind": "tap", "select": { "text": "Display" } },
        { "id": "named", "kind": "assert", "select": { "text": "Bluetooth" } },
        { "kind": "tap", "select": { "text": "Bluetooth" } },
        { "kind": "tap", "select": { "text": "Display" } },
        { "kind": "assert", "select": { "text": "Display" } },
        { "kind": "tap", "select": { "text": "Bluetooth" } },
        { "kind": "tap", "select": { "text": "Display" } },
        { "kind": "assert", "select": { "text": "Settings" } }
    ])
}

fn taps(h: &Harness) -> usize {
    h.dev
        .lock()
        .unwrap()
        .acted
        .iter()
        .filter(|x| x.starts_with("Tap"))
        .count()
}

/// What the MCP layer's sync mode does: start the steps, wait for the job to leave `running`, then
/// read its receipt (which carries one row per step). Returns the status with `receipt`, `detail`
/// and `steps` folded in.
async fn run_steps(a: &mut Client, steps: Value) -> Value {
    let started = a
        .ok(
            "job.start",
            json!({ "device": POCO, "steps": steps, "app": APP }),
        )
        .await;
    let run_id = started["run_id"].as_str().unwrap().to_string();
    let mut s = a.settled(&run_id).await;
    let rc = a.ok("job.receipt", json!({ "run_id": run_id })).await;
    for k in ["receipt", "detail", "steps"] {
        s[k] = rc[k].clone();
    }
    s
}

#[test]
fn run_steps_runs_ten_steps_in_order_and_answers_with_one_row_each() {
    run(async {
        let h = start("steps-ten", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let r = run_steps(&mut a, ten_steps()).await;
        assert_eq!(r["status"], "done", "{r}");
        assert_eq!(taps(&h), 6, "{:?}", h.dev.lock().unwrap().acted);
        let rows = r["steps"].as_array().unwrap();
        assert_eq!(rows.len(), 10, "{r}");
        let ids: Vec<&str> = rows
            .iter()
            .map(|x| x["step_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["s1", "s2", "s3", "named", "s5", "s6", "s7", "s8", "s9", "s10"],
            "unnamed steps are numbered by position"
        );
        assert!(rows.iter().all(|x| x["status"] == "done"), "{rows:?}");
        assert_eq!(r["receipt"]["steps_planned"], 10);
        assert_eq!(r["receipt"]["steps_done"], 10);
        assert_eq!(r["detail"]["outcome"], "done");
        assert!(r["line"].as_str().unwrap().starts_with("done"), "{r}");
        // the lease is still held, so the next run needs no new lease
        let again = run_steps(
            &mut a,
            json!([{ "kind": "assert", "select": { "text": "Settings" } }]),
        )
        .await;
        assert_eq!(again["status"], "done", "{again}");
    });
}

#[test]
fn run_steps_stops_at_the_first_failure_and_never_acts_after_it() {
    run(async {
        let h = start("steps-first-failure", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let mut steps = ten_steps();
        // step 4 asks for a row the phone never shows; every tap after it must stay unsent
        steps[3] = json!({ "id": "gone", "kind": "assert", "select": { "text": "No such row" }, "timeout_ms": 50 });
        let r = run_steps(&mut a, steps).await;
        assert_ne!(r["status"], "done", "{r}");
        assert_ne!(r["status"], "running", "{r}");
        assert_eq!(
            taps(&h),
            2,
            "only the two taps before the failing step ran: {:?}",
            h.dev.lock().unwrap().acted
        );
        let rows = r["steps"].as_array().unwrap();
        let st = |i: usize| rows[i]["status"].as_str().unwrap();
        assert_eq!((st(0), st(1), st(2)), ("done", "done", "done"), "{rows:?}");
        assert_eq!(
            st(3),
            "failed",
            "the failing step is the one reported failed: {rows:?}"
        );
        assert_eq!(rows[3]["step_id"], "gone");
        assert!(
            !rows[3]["note"].as_str().unwrap().is_empty(),
            "the note says why: {rows:?}"
        );
        for (i, row) in rows.iter().enumerate().skip(4) {
            assert_eq!(
                row["status"], "not_run",
                "step {i} after the failure: {rows:?}"
            );
        }
        assert_eq!(r["receipt"]["steps_done"], 3, "{r}");
        assert_eq!(r["detail"]["steps"]["not_run"], 7, "{r}");
    });
}

#[test]
fn run_steps_refuses_bad_input_before_creating_anything() {
    run(async {
        let h = start("steps-refused", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let one = json!([{ "kind": "assert", "select": { "text": "Settings" } }]);
        for (what, p, why) in [
            (
                "flow and steps together",
                json!({ "device": POCO, "flow": two_taps(), "steps": one, "app": APP }),
                "not both",
            ),
            (
                "neither flow nor steps",
                json!({ "device": POCO }),
                "needs a flow or a list of steps",
            ),
            (
                "steps without app",
                json!({ "device": POCO, "steps": one }),
                "need app",
            ),
            (
                "a blank app",
                json!({ "device": POCO, "steps": one, "app": "  " }),
                "need app",
            ),
            (
                "an app that is not a package",
                json!({ "device": POCO, "steps": one, "app": "settings" }),
                "need app",
            ),
            (
                "a package with a stray space inside",
                json!({ "device": POCO, "steps": one, "app": "com.android .settings" }),
                "need app",
            ),
            (
                "app with a flow",
                json!({ "device": POCO, "flow": two_taps(), "app": APP }),
                "app goes with steps",
            ),
            (
                "an empty list",
                json!({ "device": POCO, "steps": [], "app": APP }),
                "steps is empty",
            ),
            (
                "a step that is not an object",
                json!({ "device": POCO, "steps": ["tap"], "app": APP }),
                "must be an object",
            ),
            (
                "a step the schema refuses",
                json!({ "device": POCO, "steps": [{ "kind": "teleport" }], "app": APP }),
                "kind",
            ),
            (
                "a select with two keys",
                json!({ "device": POCO, "steps": [{ "kind": "tap", "select": { "text": "a", "id": "b" } }], "app": APP }),
                "exactly one key",
            ),
            (
                "an R2 step",
                json!({ "device": POCO, "steps": [{ "kind": "tap", "select": { "text": "Bluetooth" }, "risk": "R2" }], "app": APP }),
                "Phase 2 refuses R2",
            ),
        ] {
            let r = a.call("job.start", p).await;
            assert!(r.get("error").is_some(), "{what} was accepted: {r}");
            let msg = r["error"]["message"].as_str().unwrap_or("");
            assert!(
                msg.contains(why),
                "{what}: refused, but not for {why:?}: {msg}"
            );
        }
        assert!(
            std::fs::read_dir(h.dir.join("runs")).map_or(true, |mut d| d.next().is_none()),
            "a refused start creates nothing"
        );
        assert!(h.dev.lock().unwrap().acted.is_empty());
    });
}

#[test]
fn a_generated_step_id_never_takes_one_the_caller_wrote() {
    run(async {
        let h = start("steps-ids", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        // step 1 is named s2, so step 2 (which would be s2) is numbered past it
        let r = run_steps(&mut a, json!([{ "id": "s2", "kind": "assert", "select": { "text": "Settings" } }, { "kind": "assert", "select": { "text": "Display" } }])).await;
        assert_eq!(r["status"], "done", "{r}");
        let ids: Vec<&str> = r["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["step_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids[0], "s2");
        assert_ne!(ids[1], "s2", "{ids:?}");
        // an app id is trimmed
        let started = a.ok("job.start", json!({ "device": POCO, "steps": [{ "kind": "assert", "select": { "text": "Settings" } }], "app": format!("  {APP}  ") })).await;
        assert_eq!(
            a.settled(started["run_id"].as_str().unwrap()).await["status"],
            "done"
        );
    });
}

#[test]
fn run_steps_rows_for_a_paused_job_and_a_running_one() {
    run(async {
        // a popup no rule knows: the job pauses, step 1 is interrupted and its note says why
        let h = start("steps-paused", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        h.dev.lock().unwrap().xml_override = Some(unknown_overlay());
        let r = run_steps(&mut a, ten_steps()).await;
        assert_eq!(r["status"], "paused", "{r}");
        assert_eq!(r["detail"]["outcome"], "paused");
        let rows = r["steps"].as_array().unwrap();
        assert_eq!(
            (rows[0]["status"].as_str(), rows[0]["note"].as_str()),
            (Some("interrupted"), Some("unknown_overlay")),
            "{r}"
        );
        assert!(rows[1..].iter().all(|x| x["status"] == "not_run"), "{r}");
        assert!(h.dev.lock().unwrap().acted.is_empty());
        a.ok("job.stop", json!({ "run_id": r["run_id"] })).await;

        // a slow phone: the receipt of a job still running labels its current step `running`
        let h = start("steps-slow", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        slow_phone(&h, 400);
        let started = a
            .ok(
                "job.start",
                json!({ "device": POCO, "steps": ten_steps(), "app": APP }),
            )
            .await;
        let run_id = started["run_id"].as_str().unwrap().to_string();
        let rc = a.ok("job.receipt", json!({ "run_id": run_id })).await;
        assert_eq!(rc["detail"]["outcome"], "running", "{rc}");
        assert_eq!(rc["steps"][0]["status"], "running", "{rc}");
        assert!(
            rc["steps"].as_array().unwrap()[1..]
                .iter()
                .all(|x| x["status"] == "not_run"),
            "{rc}"
        );
        assert_eq!(a.settled(&run_id).await["status"], "done");
    });
}

#[test]
fn a_step_can_name_an_element_by_the_ref_the_tree_printed() {
    run(async {
        let h = start("steps-ref", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let t = a
            .ok(
                "device.call",
                json!({ "device": POCO, "call": { "tool": "ui_tree", "level": "full" } }),
            )
            .await;
        let text = t["text"].as_str().unwrap().to_string();
        let line = text
            .lines()
            .find(|l| l.contains("Display"))
            .unwrap_or_else(|| panic!("no Display line in {text}"));
        let rf = line
            .split_whitespace()
            .rev()
            .find(|w| {
                w.len() == 7 && w.starts_with('r') && w[1..].chars().all(|c| c.is_ascii_hexdigit())
            })
            .unwrap_or_else(|| panic!("no ref on {line}"))
            .to_string();
        let r = run_steps(&mut a, json!([{ "kind": "tap", "select": { "ref": rf } }])).await;
        assert_eq!(r["status"], "done", "{r}");
        assert_eq!(taps(&h), 1, "{:?}", h.dev.lock().unwrap().acted);
        // a ref that is not on the screen touches nothing and is not a success
        let r = run_steps(
            &mut a,
            json!([{ "kind": "tap", "select": { "ref": "rffffff" }, "timeout_ms": 50 }]),
        )
        .await;
        assert_ne!(r["status"], "done", "{r}");
        assert_eq!(taps(&h), 1);
    });
}

// ---------------------------------------------------------------- record / replay (step 8, 25 Oct)

fn five_steps() -> Value {
    json!([
        { "kind": "assert", "select": { "text": "Settings" } },
        { "kind": "tap", "select": { "text": "Bluetooth" } },
        { "kind": "tap", "select": { "text": "Display" } },
        { "kind": "assert", "select": { "text": "Display" } },
        { "kind": "tap", "select": { "text": "Bluetooth" } }
    ])
}

fn runs_dir(h: &Harness) -> PathBuf {
    h.dir.join("runs")
}

#[test]
fn a_recorded_run_replays_five_of_five_from_its_flow_file() {
    run(async {
        let h = start("record-replay", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let started = a
            .ok(
                "job.start",
                json!({ "device": POCO, "steps": five_steps(), "app": APP, "record": true }),
            )
            .await;
        let id = started["run_id"].as_str().unwrap().to_string();
        assert_eq!(a.settled(&id).await["status"], "done");
        let rc = a.ok("job.receipt", json!({ "run_id": id })).await;
        let (flow_file, maestro_file) = (
            runs_dir(&h).join(format!("{id}.flow.yaml")),
            runs_dir(&h).join(format!("{id}.maestro.yaml")),
        );
        assert!(flow_file.is_file() && maestro_file.is_file(), "{rc}");
        assert_eq!(rc["recording"]["steps"], 5, "{rc}");
        assert_eq!(
            rc["recording"]["flow"].as_str().map(PathBuf::from),
            Some(flow_file.clone()),
            "{rc}"
        );
        let yaml = std::fs::read_to_string(&flow_file).unwrap();
        assert!(yaml.starts_with("# Recorded from job"), "{yaml}");
        let maestro = std::fs::read_to_string(&maestro_file).unwrap();
        assert!(
            maestro.contains("appId: \"com.android.settings\"\n---\n- launchApp\n")
                || maestro.contains("appId: \"com.android.settings\"\r\n---"),
            "{maestro}"
        );
        assert!(
            maestro.contains("- tapOn:\n    text: \"Bluetooth\""),
            "{maestro}"
        );
        assert_eq!(taps(&h), 3);

        // replay by the file's name: the same five steps, the same three taps, five rows done
        let again = a
            .ok(
                "job.start",
                json!({ "device": POCO, "flow": format!("{id}.flow.yaml") }),
            )
            .await;
        let id2 = again["run_id"].as_str().unwrap().to_string();
        assert_eq!(a.settled(&id2).await["status"], "done");
        let rc2 = a.ok("job.receipt", json!({ "run_id": id2 })).await;
        let rows = rc2["steps"].as_array().unwrap();
        assert_eq!(rows.len(), 5, "{rc2}");
        assert!(rows.iter().all(|r| r["status"] == "done"), "{rc2}");
        assert_eq!(taps(&h), 6, "the replay sent the same three taps again");
        assert!(
            rc2.get("recording").is_none(),
            "a replay is not recorded unless it asks"
        );
        // a path is not a name, even one that points at the very same file
        let by_path = a
            .call(
                "job.start",
                json!({ "device": POCO, "flow": flow_file.display().to_string() }),
            )
            .await;
        assert!(
            by_path["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("bare file name"),
            "{by_path}"
        );
    });
}

#[test]
fn a_run_that_does_not_end_done_or_did_not_ask_leaves_no_recording() {
    run(async {
        let h = start("record-none", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        // not asked
        let s = a
            .ok(
                "job.start",
                json!({ "device": POCO, "steps": five_steps(), "app": APP }),
            )
            .await;
        assert_eq!(
            a.settled(s["run_id"].as_str().unwrap()).await["status"],
            "done"
        );
        // asked, but step 3 never finds its row
        let mut steps = five_steps();
        steps[2] =
            json!({ "kind": "assert", "select": { "text": "No such row" }, "timeout_ms": 50 });
        let s = a
            .ok(
                "job.start",
                json!({ "device": POCO, "steps": steps, "app": APP, "record": true }),
            )
            .await;
        assert_ne!(
            a.settled(s["run_id"].as_str().unwrap()).await["status"],
            "done"
        );
        let files: Vec<String> = std::fs::read_dir(runs_dir(&h))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".yaml"))
            .collect();
        assert!(files.is_empty(), "{files:?}");
    });
}

#[test]
fn a_secret_variables_value_is_never_written_to_a_recording() {
    run(async {
        let h = start("record-secret", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let fl = json!({
            "flow": "secret-row", "version": 1, "app": { "package": APP, "version_range": "*" },
            "vars": { "secret": ["row"], "row": "Display", "plain": "Bluetooth" },
            "steps": [
                { "id": "a", "kind": "tap", "select": { "text": "${row}" } },
                { "id": "b", "kind": "tap", "select": { "text": "${plain}" } },
                { "id": "c", "kind": "assert", "select": { "text": "${row}" } }
            ]
        });
        let s = a
            .ok(
                "job.start",
                json!({ "device": POCO, "flow": fl, "record": true }),
            )
            .await;
        let id = s["run_id"].as_str().unwrap().to_string();
        assert_eq!(a.settled(&id).await["status"], "done");
        // nor does the run directory's own copy hold it at rest (the run itself still had it: the taps landed)
        let stored = std::fs::read_to_string(runs_dir(&h).join(&id).join("flow.json")).unwrap();
        assert!(
            !stored.contains("Display"),
            "flow.json keeps the secret's value:\n{stored}"
        );
        assert!(
            stored.contains("Bluetooth") && stored.contains("\"row\""),
            "a plain variable and the secret's name stay:\n{stored}"
        );
        let yaml = std::fs::read_to_string(runs_dir(&h).join(format!("{id}.flow.yaml"))).unwrap();
        let maestro =
            std::fs::read_to_string(runs_dir(&h).join(format!("{id}.maestro.yaml"))).unwrap();
        for (what, text) in [("flow", &yaml), ("maestro", &maestro)] {
            assert!(
                !text.contains("Display"),
                "the secret's value is in the {what} file:\n{text}"
            );
            assert!(text.contains("${row}"), "{what}:\n{text}");
        }
        assert!(
            yaml.contains("plain"),
            "a plain variable keeps its value:\n{yaml}"
        );
        // the run directory's own copy is the run's, not the recording's
        let again = a
            .call(
                "job.start",
                json!({ "device": POCO, "flow": format!("{id}.flow.yaml") }),
            )
            .await;
        let id2 = again["result"]["run_id"].as_str().unwrap().to_string();
        let r = a.settled(&id2).await;
        assert_ne!(
            r["status"], "done",
            "a replay with no value for the secret must not run: {r}"
        );
        let before = taps(&h);
        let with = a.ok("job.start", json!({ "device": POCO, "flow": format!("{id}.flow.yaml"), "vars": { "row": "Display" } })).await;
        assert_eq!(
            a.settled(with["run_id"].as_str().unwrap()).await["status"],
            "done"
        );
        assert_eq!(taps(&h), before + 2);
    });
}

#[test]
fn replay_reads_only_files_inside_the_jobs_directory() {
    run(async {
        let h = start("record-refuse", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        let outside = h.dir.join("outside.yaml");
        std::fs::write(&outside, "flow: x\nversion: 1\napp: {package: com.android.settings, version_range: '*'}\nsteps: [{id: a, kind: back}]\n").unwrap();
        std::fs::create_dir_all(runs_dir(&h)).unwrap();
        for (what, name, why) in [
            (
                "a file that does not exist",
                "nothing.flow.yaml".to_string(),
                "no such file",
            ),
            (
                "a parent-directory climb",
                "../outside.yaml".to_string(),
                "bare file name",
            ),
            (
                "an absolute path outside",
                outside.display().to_string(),
                "bare file name",
            ),
            (
                "a UNC share (refused before any disk or network access)",
                r"\\attacker\share\x.yaml".to_string(),
                "bare file name",
            ),
            (
                "a path into a run directory",
                "job-1-1/flow.json".to_string(),
                "bare file name",
            ),
            (
                "a drive-relative name",
                "C:x.yaml".to_string(),
                "bare file name",
            ),
            (
                "a hidden name",
                ".hidden.yaml".to_string(),
                "bare file name",
            ),
        ] {
            let r = a
                .call("job.start", json!({ "device": POCO, "flow": name }))
                .await;
            let msg = r["error"]["message"]
                .as_str()
                .unwrap_or_else(|| panic!("{what} was accepted: {r}"));
            assert!(msg.contains(why), "{what}: {msg}");
        }
        assert!(h.dev.lock().unwrap().acted.is_empty());
        // YAML text is still YAML text
        let text = "flow: t\nversion: 1\napp: {package: com.android.settings, version_range: '*'}\nsteps: [{id: a, kind: assert, select: {text: Settings}}]\n";
        let s = a
            .ok("job.start", json!({ "device": POCO, "flow": text }))
            .await;
        assert_eq!(
            a.settled(s["run_id"].as_str().unwrap()).await["status"],
            "done"
        );
    });
}

#[test]
fn a_recording_that_would_hold_a_secret_is_refused_with_the_reason_and_the_job_still_ends_done() {
    run(async {
        let h = start("record-leak", DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO })).await;
        // the secret's value is typed into a step literally instead of by reference
        let fl = json!({
            "flow": "leaky", "version": 1, "app": { "package": APP, "version_range": "*" },
            "vars": { "secret": ["row"], "row": "Display" },
            "steps": [{ "id": "a", "kind": "tap", "select": { "text": "Display" } }]
        });
        let s = a
            .ok(
                "job.start",
                json!({ "device": POCO, "flow": fl, "record": true }),
            )
            .await;
        let id = s["run_id"].as_str().unwrap().to_string();
        assert_eq!(
            a.settled(&id).await["status"],
            "done",
            "the run is not failed by a recording problem"
        );
        let rc = a.ok("job.receipt", json!({ "run_id": id })).await;
        let why = rc["recording"]["error"]
            .as_str()
            .unwrap_or_else(|| panic!("the caller is not told why there is no recording: {rc}"));
        assert!(
            why.contains("secret variable row") && !why.contains("Display"),
            "{why}"
        );
        assert!(rc["recording"].get("flow").is_none());
        let yamls: Vec<String> = std::fs::read_dir(runs_dir(&h))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".yaml"))
            .collect();
        assert!(yamls.is_empty(), "{yamls:?}");
    });
}
