//! The library crates running inside the daemon, end to end. Fakes only:
//! core's fake device for the engine, care's fake phone (behind adb-link's real `GuardedRunner`)
//! for care's shell reads and writes, and link-hub's fake phone over a real WebSocket on
//! 127.0.0.1 for the link. No adb, no phone, no window.
//!
//! * the link hub in bridged: one epoch source, segments signed and sent, R2 refused unsent,
//!   `link.health` / `link.pair_status`, never `0.0.0.0` or the LAN;
//! * care during jobs: the heat pause at 42 °C and the resume strictly below 37 °C (37.0 rests,
//!   36.9 resumes), job-scoped leases in `leases.json` shown as `changes_in_force` and restored at
//!   the end, read_only refused with nothing sent;
//! * the take-over: getevent Detector 1 finds a person's touch mid-step, the step is journalled
//!   `interrupted` (by a human) and re-verified on resume, never re-sent; the hub's own injection
//!   is filtered;
//! * the job view's HTTP side (WIRING.md): binds, peers, Host, token, Origin, headers, NDJSON, SSE.

#[path = "../../link-hub/tests/common/mod.rs"]
mod phone;

use agentroid_adb_link::guard::GuardedRunner;
use agentroid_core::fake::{FakeClock as CoreClock, FakeDevice, FAKE_POCO_ADB};
use agentroid_core::gate::DeviceMode;
use agentroid_core::transport::{
    Backend, Core, DeviceInfo, Input, Screenshot, Transport, TransportError, POCO_SERIAL,
};
use bridged::jobcare::CareConfig;
use bridged::jobs::{DynRunner, JobsConfig};
use bridged::jobview::{JobView, JobViewConfig, ViewCtx};
use bridged::link::LinkConfig;
use bridged::listener::{duplex_listener, DuplexConnector};
use bridged::proto::*;
use bridged::registry::Registry;
use bridged::server::{self, CoreBackend, Daemon};
use care::clock::{CareClock, FakeClock, TEST_EPOCH_MS};
use care::fake::FakePhone as CarePhone;
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{
    AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf,
};
use tokio::sync::watch;

const APP: &str = "com.android.settings";

// ---------------------------------------------------------------- harness

/// core's fake device, with a gate that holds `input` (a tap in flight) until the test opens it.
#[derive(Clone)]
struct Shared {
    dev: Arc<Mutex<FakeDevice>>,
    hold_input: Arc<AtomicBool>,
    in_input: Arc<AtomicBool>,
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
        self.dev.lock().unwrap().tree_xml(s, b)
    }
    fn input(&mut self, s: &str, i: &Input) -> Result<(), TransportError> {
        let t = Instant::now();
        while self.hold_input.load(Ordering::SeqCst) && t.elapsed() < Duration::from_secs(20) {
            self.in_input.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(2));
        }
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

fn flow(steps: Value) -> Value {
    json!({ "flow": "wiring-test", "version": 1, "app": { "package": APP, "version_range": "*" }, "steps": steps })
}

fn two_taps() -> Value {
    flow(
        json!([{ "id": "bt", "kind": "tap", "select": { "text": "Bluetooth" } }, { "id": "disp", "kind": "tap", "select": { "text": "Display" } }]),
    )
}

/// A tap followed by a verify step: a cut-off tap is re-verified by the verify step's selectors.
fn tap_then_verify() -> Value {
    flow(
        json!([{ "id": "bt", "kind": "tap", "select": { "text": "Bluetooth" } }, { "id": "check", "kind": "verify", "selectors": [{ "text": "Display" }] }]),
    )
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("wiring")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn scaffold() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// care's side of the test phone: its fake shell, its clock, and how much the job has slept.
struct CareRig {
    phone: CarePhone,
    clock: Arc<FakeClock>,
    slept: Arc<AtomicU64>,
    leases: PathBuf,
}

/// A care config over care's fake phone (reached at the POCO's adb address), with a sleep that
/// advances the fake clock and lets `on_sleep(total_slept)` change the phone (a cooling battery).
fn care_rig(
    dir: &Path,
    on_sleep: impl Fn(&CarePhone, u64) + Send + Sync + 'static,
) -> (CareRig, CareConfig) {
    let phone = CarePhone::new(POCO_SERIAL);
    phone.state().tokens.push(FAKE_POCO_ADB.to_string());
    let clock = Arc::new(FakeClock::new(TEST_EPOCH_MS));
    let slept = Arc::new(AtomicU64::new(0));
    let runner = Arc::new(GuardedRunner::new(DynRunner(Arc::new(phone.clone()))));
    let leases = dir.join("leases.json");
    let notifier = Arc::new(bridged::jobcare::LogNotifier(dir.join("push.log")));
    let mut cfg = CareConfig::new(runner, &leases, clock.clone(), notifier);
    let (c, s, p) = (clock.clone(), slept.clone(), phone.clone());
    cfg.sleep = Arc::new(move |ms| {
        c.advance(ms);
        let total = s.fetch_add(ms, Ordering::SeqCst) + ms;
        on_sleep(&p, total);
    });
    (
        CareRig {
            phone,
            clock,
            slept,
            leases,
        },
        cfg,
    )
}

struct Harness {
    connector: DuplexConnector,
    _shutdown: watch::Sender<bool>,
    daemon: Arc<Daemon>,
    dev: Arc<Mutex<FakeDevice>>,
    hold_input: Arc<AtomicBool>,
    in_input: Arc<AtomicBool>,
    dir: PathBuf,
}

fn start(dir: &Path, mode: DeviceMode, care: Option<CareConfig>) -> Harness {
    let dev = Arc::new(Mutex::new(FakeDevice::new()));
    dev.lock().unwrap().xml_override = Some(tree(&[]));
    let (hold_input, in_input) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let mut backend = CoreBackend::new(Core::new(
        Shared {
            dev: dev.clone(),
            hold_input: hold_input.clone(),
            in_input: in_input.clone(),
        },
        CoreClock::new(),
    ));
    backend.set_mode(POCO_SERIAL, mode);
    let mut cfg = JobsConfig::new(dir.join("runs"));
    cfg.care = care;
    let daemon = Daemon::with_jobs(Registry::in_memory(), Box::new(backend), Some(cfg));
    let (listener, connector) = duplex_listener();
    let (shutdown, rx) = watch::channel(false);
    tokio::spawn(server::serve(daemon.clone(), listener, rx));
    Harness {
        connector,
        _shutdown: shutdown,
        daemon,
        dev,
        hold_input,
        in_input,
        dir: dir.to_path_buf(),
    }
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
        c.ok("hello", json!({ "session": session })).await;
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
        serde_json::from_str(&line).unwrap()
    }
    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.call(method, params).await;
        assert!(r.get("result").is_some(), "{method} failed: {r}");
        r["result"].clone()
    }
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

fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn run_dir(h: &Harness, run_id: &str) -> PathBuf {
    h.dir.join("runs").join(run_id)
}

async fn wait_for(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
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

// ---------------------------------------------------------------- care: heat, floor, leases

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hot_phone_pauses_the_job_at_42_and_resumes_only_strictly_below_37() {
    let dir = tmp_dir("heat");
    // the battery is 42.5 °C; after one minute of rest it is 37.0 (still resting), after two 36.9
    let (rig, care) = care_rig(&dir, |p, total| {
        let t = if total >= 120_000 {
            369
        } else if total >= 60_000 {
            370
        } else {
            425
        };
        p.state().battery_dc = t;
    });
    rig.phone.state().battery_dc = 425;
    let h = start(&dir, DeviceMode::Full, Some(care));
    let mut a = Client::named(&h, "claude-a").await;
    a.ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await;
    let run_id = a
        .ok(
            "job.start",
            json!({ "device": POCO_SERIAL, "flow": two_taps() }),
        )
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let s = a.settled(&run_id).await;
    assert_eq!(s["status"], "done", "{s}");
    assert_eq!(taps(&h), 2);
    let ev = lines(&run_dir(&h, &run_id).join("progress.ndjson"));
    let heat: Vec<&Value> = ev
        .iter()
        .filter(|e| e["msg"].as_str().is_some_and(|m| m.starts_with("heat:")))
        .collect();
    let pause = heat
        .iter()
        .find(|e| e["kind"] == "pause")
        .expect("a heat pause event");
    assert_eq!(pause["temp_c"], 42.5, "{pause}");
    assert!(
        pause["msg"].as_str().unwrap().contains("42.0 °C"),
        "{pause}"
    );
    let resting = heat
        .iter()
        .find(|e| e["kind"] == "progress")
        .expect("a resting event at 37.0");
    assert_eq!(resting["temp_c"], 37.0, "37.0 °C rests: {resting}");
    let resume = heat
        .iter()
        .find(|e| e["kind"] == "resume")
        .expect("a heat resume event");
    assert_eq!(
        resume["temp_c"], 36.9,
        "the job resumed strictly below 37.0 °C: {resume}"
    );
    // no step ran while resting: the first tap's events come after the resume
    let resume_seq = resume["seq"].as_u64().unwrap();
    assert_eq!(pause["step_id"], "bt", "the rest is before the first step");
    let first_step = ev
        .iter()
        .find(|e| e["step_id"] == "bt" && e["kind"] == "step")
        .unwrap();
    assert!(
        first_step["seq"].as_u64().unwrap() > resume_seq,
        "nothing ran while the phone rested"
    );
    // the screen slept for the rest; the rest counts as cooling time in the receipt
    assert!(
        rig.phone
            .commands()
            .iter()
            .any(|c| c == "input keyevent KEYCODE_SLEEP"),
        "{:?}",
        rig.phone.commands()
    );
    let rc = a.ok("job.receipt", json!({ "run_id": run_id })).await;
    assert!(
        rc["detail"]["time_s"]["cooling"].as_f64().unwrap() >= 120.0,
        "{rc}"
    );
    assert!(rig.slept.load(Ordering::SeqCst) >= 120_000);
    // a job that ends done gives its settings back too
    assert!(h.daemon.changes_in_force(POCO_SERIAL).is_empty());
    let journal = lines(&run_dir(&h, &run_id).join("journal.ndjson"));
    assert!(
        journal
            .iter()
            .any(|r| r["step_id"] == "care-restore-system-screen_off_timeout"),
        "{journal:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_scoped_leases_show_as_changes_in_force_and_are_restored_at_the_end() {
    let dir = tmp_dir("leases");
    let (rig, care) = care_rig(&dir, |_, _| {});
    rig.phone.set("system", "screen_off_timeout", "30000");
    rig.phone.set("system", "screen_brightness", "128");
    let h = start(&dir, DeviceMode::Full, Some(care));
    let mut a = Client::named(&h, "claude-a").await;
    a.ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await;
    // an unknown overlay: the job pauses (needs_attention), keeping its lease and its settings
    h.dev.lock().unwrap().xml_override = Some(tree(&[(
        "com.example.nag",
        "Rate us!",
        "[100,1200][1120,1500]",
    )]));
    let run_id = a
        .ok(
            "job.start",
            json!({ "device": POCO_SERIAL, "flow": two_taps() }),
        )
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(a.settled(&run_id).await["status"], "paused");
    // while the job holds the phone: the leases are in leases.json and in devices.list
    let list = a.ok("devices.list", json!({})).await;
    let row = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == POCO_SERIAL)
        .unwrap()
        .clone();
    let changes = row["changes_in_force"].as_array().unwrap();
    let timeout = changes
        .iter()
        .find(|c| c["key"] == "screen_off_timeout")
        .unwrap_or_else(|| panic!("{row}"));
    assert_eq!(timeout["before"], "30000");
    assert_eq!(timeout["owner"]["job_id"], run_id.as_str());
    assert_eq!(timeout["scope"], "job");
    assert_eq!(
        rig.phone.get("system", "screen_off_timeout").as_deref(),
        timeout["applied"].as_str()
    );
    assert!(rig.leases.is_file());
    // the job view's object carries the same list
    let view = h.daemon.view_job(&run_id).unwrap();
    assert_eq!(
        view["changes_in_force"].as_array().unwrap().len(),
        changes.len()
    );
    // stopped while paused: restored by compare-and-swap; nothing left in force
    a.ok("job.stop", json!({ "run_id": run_id })).await;
    assert_eq!(
        a.ok("job.status", json!({ "run_id": run_id })).await["status"],
        "stopped"
    );
    assert_eq!(
        rig.phone.get("system", "screen_off_timeout").as_deref(),
        Some("30000")
    );
    assert_eq!(
        rig.phone.get("system", "screen_brightness").as_deref(),
        Some("128")
    );
    let list = a.ok("devices.list", json!({})).await;
    let row = list["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device"] == POCO_SERIAL)
        .unwrap()
        .clone();
    assert_eq!(row["changes_in_force"], json!([]), "{row}");
    // every care event is in the job's journal; the receipt lists the settings changed and how they ended
    let journal = lines(&run_dir(&h, &run_id).join("journal.ndjson"));
    assert!(
        journal
            .iter()
            .any(|r| r["step_id"] == "care-change-system-screen_off_timeout"),
        "{journal:?}"
    );
    assert!(journal
        .iter()
        .any(|r| r["step_id"] == "care-restore-system-screen_off_timeout"));
    let rc = a.ok("job.receipt", json!({ "run_id": run_id })).await;
    let changed = rc["detail"]["settings_changed"].as_array().unwrap();
    let t = changed
        .iter()
        .find(|c| c["key"] == "screen_off_timeout")
        .unwrap_or_else(|| panic!("{rc}"));
    assert_eq!(
        (t["before"].as_str(), t["ended"].as_str()),
        (Some("30000"), Some("restored")),
        "{t}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_gets_no_lease_and_no_care_command_at_all() {
    let dir = tmp_dir("readonly");
    let (rig, care) = care_rig(&dir, |_, _| {});
    rig.phone.state().battery_dc = 450; // hot: would pause a full device
    let h = start(&dir, DeviceMode::ReadOnly, Some(care));
    let mut a = Client::named(&h, "claude-a").await;
    a.ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await;
    let f =
        flow(json!([{ "id": "check", "kind": "verify", "selectors": [{ "text": "Bluetooth" }] }]));
    let run_id = a
        .ok("job.start", json!({ "device": POCO_SERIAL, "flow": f }))
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    a.settled(&run_id).await;
    assert!(
        rig.phone.commands().is_empty(),
        "care sent nothing to a read_only phone: {:?}",
        rig.phone.commands()
    );
    let journal = lines(&run_dir(&h, &run_id).join("journal.ndjson"));
    let refused: Vec<&Value> = journal
        .iter()
        .filter(|r| {
            r["step_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("care-refused-"))
        })
        .collect();
    assert!(
        !refused.is_empty(),
        "the refusal is journalled: {journal:?}"
    );
    assert!(
        std::fs::read_to_string(&rig.leases).map_or(true, |t| !t.contains("screen_off_timeout")),
        "no lease was written"
    );
    let ev = lines(&run_dir(&h, &run_id).join("progress.ndjson"));
    assert!(
        !ev.iter()
            .any(|e| e["msg"].as_str().is_some_and(|m| m.starts_with("heat:"))),
        "no heat pause on a read_only phone"
    );
}

// ---------------------------------------------------------------- the take-over (2.15)

fn getevent(name: &str) -> String {
    std::fs::read_to_string(
        scaffold()
            .join("crates")
            .join("handover")
            .join("fixtures")
            .join("getevent")
            .join(name),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_human_touch_mid_step_interrupts_it_and_resume_re_verifies_without_re_sending() {
    let dir = tmp_dir("takeover");
    let (rig, mut care) = care_rig(&dir, |_, _| {});
    care.profiles = Some(scaffold().join("profiles"));
    let clock = rig.clock.clone();
    let h = start(&dir, DeviceMode::Full, Some(care));
    let mut a = Client::named(&h, "claude-a").await;
    a.ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await;
    h.hold_input.store(true, Ordering::SeqCst);
    let run_id = a
        .ok(
            "job.start",
            json!({ "device": POCO_SERIAL, "flow": tap_then_verify() }),
        )
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let flag = h.in_input.clone();
    wait_for("the tap in flight", move || flag.load(Ordering::SeqCst)).await;
    // the agent profile is leased through care
    assert_eq!(
        rig.phone.get("global", "window_animation_scale").as_deref(),
        Some("0")
    );
    // an overlay report while our own tap is on its way: the hub's own injection, dropped
    let own = h
        .daemon
        .human_touch(
            POCO_SERIAL,
            clock.mono_ms(),
            handover::mode::Source::Overlay,
        )
        .unwrap();
    assert!(own.own_injection && !own.take_over, "{own:?}");
    // the hub's injected events never reach Detector 1 (another input node)
    let map = getevent("synthetic-map.txt");
    assert!(h
        .daemon
        .feed_getevent(POCO_SERIAL, &map, &getevent("synthetic-injected-500.txt"))
        .is_empty());
    // a person's touch: take_over, with the in-flight step
    clock.advance(1_000);
    let outs = h
        .daemon
        .feed_getevent(POCO_SERIAL, &map, &getevent("synthetic-human-20.txt"));
    let first = outs.first().expect("Detector 1 found the touches");
    assert!(first.take_over, "{first:?}");
    assert_eq!(first.interrupted.as_deref(), Some("bt"));
    assert_eq!(
        outs.iter().filter(|o| o.take_over).count(),
        1,
        "one take-over, however many touches"
    );
    // the human profile is back at once
    assert_eq!(
        rig.phone.get("global", "window_animation_scale").as_deref(),
        Some("1.0")
    );
    h.hold_input.store(false, Ordering::SeqCst);
    let s = a.settled(&run_id).await;
    assert_eq!(s["status"], "taken_over", "{s}");
    let journal = lines(&run_dir(&h, &run_id).join("journal.ndjson"));
    let cut = journal
        .iter()
        .find(|r| r["step_id"] == "bt" && r["status"] == "interrupted")
        .unwrap_or_else(|| panic!("an interrupted record for bt: {journal:?}"));
    assert_eq!(cut["post"]["interrupted_by"], "human", "{cut}");
    let ev = lines(&run_dir(&h, &run_id).join("progress.ndjson"));
    assert!(ev.iter().any(|e| e["kind"] == "taken_over"), "{ev:?}");
    assert_eq!(taps(&h), 1);
    // hand back: resume re-verifies the cut-off tap from the verify step, never taps again
    a.ok("job.resume", json!({ "run_id": run_id })).await;
    let s = a.settled(&run_id).await;
    assert_eq!(s["status"], "done", "{s}");
    assert_eq!(taps(&h), 1, "the interrupted tap was not sent again");
    let journal = lines(&run_dir(&h, &run_id).join("journal.ndjson"));
    assert!(
        journal.iter().any(|r| r["step_id"] == "bt"
            && r["status"] == "done"
            && r["intent"]
                .as_str()
                .is_some_and(|i| i.contains("re-verified"))),
        "{journal:?}"
    );
}

// ---------------------------------------------------------------- the link hub (2.12) and segments (2.14)

fn link_cfg(dir: &Path, bind: &str) -> LinkConfig {
    LinkConfig {
        state_dir: dir.to_path_buf(),
        bind: bind.into(),
        port: 0,
        key: phone::hub_key(),
        clock: None,
        tls: None,
        lan: false,
    }
}

fn r1_steps() -> Value {
    let p = scaffold()
        .join("schemas")
        .join("fixtures")
        .join("segment")
        .join("valid-r1-tap.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    v["segment"]["steps"].clone()
}

fn r2_steps() -> Value {
    let p = scaffold()
        .join("schemas")
        .join("fixtures")
        .join("segment")
        .join("valid-r2-step-for-refusal-test.json");
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    v["segment"]["steps"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_owns_the_hub_on_127_0_0_1_and_never_binds_every_interface_or_the_lan() {
    let dir = tmp_dir("hub-bind");
    let h = start(&dir, DeviceMode::Full, None);
    for bad in ["0.0.0.0", "192.168.1.20", "10.0.0.7", "::"] {
        let d = dir.join(format!("bad-{}", bad.replace([':', '.'], "-")));
        assert!(
            h.daemon.attach_link(link_cfg(&d, bad)).is_err(),
            "{bad} refused"
        );
        assert!(h.daemon.link().is_none(), "a refused hub attaches nothing");
    }
    // --link-lan without a certificate: refused before anything binds (home Wi-Fi needs TLS)
    let mut plain_lan = link_cfg(&dir.join("bad-lan-plain"), "192.168.1.20");
    plain_lan.lan = true;
    let err = h
        .daemon
        .attach_link(plain_lan)
        .err()
        .expect("--link-lan without TLS refused");
    assert!(err.contains("without TLS"), "{err}");
    assert!(h.daemon.link().is_none(), "a refused hub attaches nothing");
    let link = h
        .daemon
        .attach_link(link_cfg(&dir.join("hub"), "127.0.0.1"))
        .expect("the hub starts on 127.0.0.1");
    let addr = link.local_addr();
    assert!(addr.ip().is_loopback(), "bound {addr}");
    assert!(
        h.daemon
            .attach_link(link_cfg(&dir.join("hub2"), "127.0.0.1"))
            .is_err(),
        "one hub per daemon"
    );
    let mut a = Client::named(&h, "claude-a").await;
    let hello = a.ok("hello", json!({})).await;
    assert!(hello["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "link:hub,segments"));
    link.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segments_are_signed_fenced_with_the_lease_epoch_and_r2_is_refused_unsent() {
    let dir = tmp_dir("hub-seg");
    let h = start(&dir, DeviceMode::Full, None);
    let mut a = Client::named(&h, "claude-a").await;
    // no hub yet: the link methods say so
    assert_eq!(
        code(
            &a.call("link.health", json!({ "device": POCO_SERIAL }))
                .await
        ),
        STATE_ERROR
    );
    let hub_dir = dir.join("hub");
    let link = h
        .daemon
        .attach_link(link_cfg(&hub_dir, "127.0.0.1"))
        .unwrap();
    let p = phone::FakePhone::new(phone::PhoneCfg {
        serial: POCO_SERIAL.into(),
        key: phone::phone_key(41),
        hub_spki: link.hub().hub_key().spki().to_vec(),
        sign_with: None,
        lie_epoch_seen: None,
        record: true,
        tls_fp: None,
    });
    phone::pair_direct(&hub_dir, POCO_SERIAL, &p.cfg.key);
    // the phone takes the segment (an answer with `accepted: true`)
    p.st().canned.insert(
        "seg-a".into(),
        json!({"accepted": true, "outcome": "started", "segment_id": "seg-a"}),
    );
    // not leased: adb is the transport until the link is up
    let before = a.ok("link.health", json!({ "device": POCO_SERIAL })).await;
    assert_eq!(before["transport"], "adb", "{before}");
    let runner = phone::PhoneRunner::start(p.clone(), link.hub().url());
    let hub = link.hub();
    phone::wait_until("the phone is connected", 20, || {
        hub.status(POCO_SERIAL).is_some_and(|s| s.connected)
    })
    .await;
    // one epoch source: the lease's epoch is the link's
    let lease = a
        .ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await;
    let epoch = lease["epoch"].as_u64().unwrap();
    assert_eq!(
        hub.epoch(POCO_SERIAL).unwrap(),
        epoch,
        "a new lease moves the link to its epoch"
    );
    // the phone re-handshakes at the new epoch
    phone::wait_until("the phone is back at the lease epoch", 20, || {
        hub.status(POCO_SERIAL)
            .is_some_and(|s| s.connected && s.epoch == Some(epoch))
            && p.st().epoch == epoch
    })
    .await;
    let health = a.ok("link.health", json!({ "device": POCO_SERIAL })).await;
    assert_eq!(
        (
            health["transport"].as_str(),
            health["link_epoch"].as_u64(),
            health["lease_epoch"].as_u64()
        ),
        (Some("link"), Some(epoch), Some(epoch)),
        "{health}"
    );
    let pairs = a.ok("link.pair_status", json!({})).await;
    assert_eq!(pairs["devices"][0]["device"], POCO_SERIAL);
    assert_eq!(pairs["devices"][0]["revoked"], false);
    // an R0/R1 segment: classified, signed with the hub key, sent once in a segment frame
    let r = a.ok("link.segment", json!({ "device": POCO_SERIAL, "segment_id": "seg-a", "run_id": "run-a", "steps": r1_steps() })).await;
    assert_eq!(
        (r["classification"].as_str(), r["epoch"].as_u64()),
        (Some("R1"), Some(epoch)),
        "{r}"
    );
    assert_eq!(r["response"]["result"]["accepted"], true, "{r}");
    {
        let st = p.st();
        assert_eq!(st.segments_seen.len(), 1);
        let seg = &st.segments_seen[0];
        assert_eq!(
            (
                seg["segment"]["device"].as_str(),
                seg["segment"]["epoch"].as_u64()
            ),
            (Some(POCO_SERIAL), Some(epoch))
        );
        let frame = st.hub_frames.iter().find(|f| f["t"] == "segment").unwrap();
        assert_eq!(link_hub::proto::validate(frame), Ok(()));
        // signed with the hub's own link key: the one key the phone pinned at pairing
        assert_eq!(
            seg["signature"]["key_id"].as_str(),
            Some(hub.hub_key().key_id())
        );
        let signed = agentroid_segments::SignedSegment::from_json(&seg.to_string()).unwrap();
        let bytes = agentroid_segments::segment::signed_bytes(&signed.segment).unwrap();
        let sig = link_hub::crypto::unb64(seg["signature"]["value"].as_str().unwrap()).unwrap();
        assert!(
            link_hub::crypto::verify(hub.hub_key().spki(), &bytes, &sig),
            "the signature verifies with the hub key"
        );
    }
    // R2: refused by the hub before anything is sent (Phase 2 has no approval channel)
    let r = a.call("link.segment", json!({ "device": POCO_SERIAL, "segment_id": "seg-b", "run_id": "run-a", "steps": r2_steps() })).await;
    assert_eq!(code(&r), REFUSED, "{r}");
    assert!(
        r["error"]["data"]["decisions"]
            .as_array()
            .is_some_and(|d| !d.is_empty()),
        "{r}"
    );
    assert_eq!(
        p.st().segments_seen.len(),
        1,
        "nothing was sent for the R2 segment"
    );
    // only the holder sends segments
    let mut b = Client::named(&h, "claude-b").await;
    assert_eq!(code(&b.call("link.segment", json!({ "device": POCO_SERIAL, "segment_id": "seg-c", "run_id": "run-b", "steps": r1_steps() })).await), NOT_HOLDER);
    // a newer lease: the old epoch is fenced (the next segment goes out at the new one)
    a.ok("lease.release", json!({ "device": POCO_SERIAL }))
        .await;
    let epoch2 = a
        .ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await["epoch"]
        .as_u64()
        .unwrap();
    assert!(epoch2 > epoch);
    assert_eq!(hub.epoch(POCO_SERIAL).unwrap(), epoch2);
    assert!(matches!(
        hub.send_request_fenced(POCO_SERIAL, epoch, json!({"method": "x"}))
            .await,
        Err(link_hub::hub::HubError::StaleEpoch { .. })
    ));
    // the report call reaches the phone fenced; the fake phone echoes, which is not a report
    let rep = a
        .call(
            "link.segment_report",
            json!({ "device": POCO_SERIAL, "segment_id": "seg-a" }),
        )
        .await;
    assert_eq!(code(&rep), DEVICE_ERROR, "{rep}");
    runner.stop().await;
    link.shutdown();
}

/// Wiring 2, item 1: what the phone's runner.segment handler answers, as the daemon reads it. The
/// fake phone is told what to answer (the Java side of this is `RunnerMethodsTest`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_refusal_is_a_refusal_and_an_accepted_segments_pulled_report_is_checked() {
    let dir = tmp_dir("hub-seg-phone");
    let h = start(&dir, DeviceMode::Full, None);
    let mut a = Client::named(&h, "claude-a").await;
    let hub_dir = dir.join("hub");
    let link = h
        .daemon
        .attach_link(link_cfg(&hub_dir, "127.0.0.1"))
        .unwrap();
    let p = phone::FakePhone::new(phone::PhoneCfg {
        serial: POCO_SERIAL.into(),
        key: phone::phone_key(43),
        hub_spki: link.hub().hub_key().spki().to_vec(),
        sign_with: None,
        lie_epoch_seen: None,
        record: true,
        tls_fp: None,
    });
    phone::pair_direct(&hub_dir, POCO_SERIAL, &p.cfg.key);
    let runner = phone::PhoneRunner::start(p.clone(), link.hub().url());
    let hub = link.hub();
    phone::wait_until("the phone is connected", 20, || {
        hub.status(POCO_SERIAL).is_some_and(|s| s.connected)
    })
    .await;
    let epoch = a
        .ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await["epoch"]
        .as_u64()
        .unwrap();
    phone::wait_until("the phone is back at the lease epoch", 20, || {
        hub.status(POCO_SERIAL)
            .is_some_and(|s| s.connected && s.epoch == Some(epoch))
            && p.st().epoch == epoch
    })
    .await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let step = |id: &str, ms: u64| json!({"id": id, "status": "done", "post_verified": true, "decision": "allowed", "ts_ms": now_ms + ms, "ms": 5});
    {
        let mut st = p.st();
        st.canned.insert(
            "seg-ok".into(),
            json!({"accepted": true, "outcome": "started", "segment_id": "seg-ok"}),
        );
        st.canned.insert("seg-no".into(), json!({"accepted": false, "outcome": "refused", "code": "bad_signature", "reason": "signature does not verify", "segment_id": "seg-no"}));
        let report = json!({"v": 1, "t": "segment_report", "segment_id": "seg-ok", "run_id": "run-a", "device": POCO_SERIAL, "epoch": epoch, "outcome": "completed", "steps": [step("s1", 1), step("s2", 20), step("s3", 40)]});
        st.canned.insert("report-seg-ok".into(), report);
    }
    // the phone took it: the answer passes through for the caller to read
    let ok = a.ok("link.segment", json!({ "device": POCO_SERIAL, "segment_id": "seg-ok", "run_id": "run-a", "steps": r1_steps() })).await;
    assert_eq!(ok["response"]["result"]["accepted"], true, "{ok}");
    // the phone refused it: that is a refusal with the phone's code, not a success
    let no = a.call("link.segment", json!({ "device": POCO_SERIAL, "segment_id": "seg-no", "run_id": "run-a", "steps": r1_steps() })).await;
    assert_eq!(code(&no), REFUSED, "{no}");
    assert_eq!(
        no["error"]["data"]["phone"]["code"], "bad_signature",
        "{no}"
    );
    assert!(
        no["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bad_signature"),
        "{no}"
    );
    // the report is pulled by id and checked against what the hub sent
    let rep = a
        .ok(
            "link.segment_report",
            json!({ "device": POCO_SERIAL, "segment_id": "seg-ok" }),
        )
        .await;
    assert_eq!(
        (rep["ok"].as_bool(), rep["outcome"].as_str()),
        (Some(true), Some("completed")),
        "{rep}"
    );
    runner.stop().await;
    link.shutdown();
}

/// Wiring 2, item 3, through the daemon: the hub prunes its answers by age while it runs, and the
/// daemon tells it which answers are a running segment's, which are never pruned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_keeps_a_running_segments_answer_when_the_hub_prunes_old_ones() {
    let dir = tmp_dir("hub-prune");
    let h = start(&dir, DeviceMode::Full, None);
    let mut a = Client::named(&h, "claude-a").await;
    let hub_dir = dir.join("hub");
    let fake = Arc::new(link_hub::clock::FakeClock::new(1_000));
    let mut cfg = link_cfg(&hub_dir, "127.0.0.1");
    cfg.clock = Some(fake.clone());
    let link = h.daemon.attach_link(cfg).unwrap();
    let p = phone::FakePhone::new(phone::PhoneCfg {
        serial: POCO_SERIAL.into(),
        key: phone::phone_key(44),
        hub_spki: link.hub().hub_key().spki().to_vec(),
        sign_with: None,
        lie_epoch_seen: None,
        record: true,
        tls_fp: None,
    });
    phone::pair_direct(&hub_dir, POCO_SERIAL, &p.cfg.key);
    let runner = phone::PhoneRunner::start(p.clone(), link.hub().url());
    let hub = link.hub();
    phone::wait_until("the phone is connected", 20, || {
        hub.status(POCO_SERIAL).is_some_and(|s| s.connected)
    })
    .await;
    let epoch = a
        .ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await["epoch"]
        .as_u64()
        .unwrap();
    phone::wait_until("the phone is back at the lease epoch", 20, || {
        hub.status(POCO_SERIAL)
            .is_some_and(|s| s.connected && s.epoch == Some(epoch))
            && p.st().epoch == epoch
    })
    .await;

    // a segment (running: it expires in minutes of real time), a segment the phone refuses (a cached answer that is
    // NOT a running segment's: review defect 5), and an ordinary request, all answered
    p.st().accept_segments = true;
    p.st().canned.insert("seg-no".into(), json!({"accepted": false, "outcome": "refused", "code": "bad_signature", "reason": "signature does not verify", "segment_id": "seg-no"}));
    a.ok("link.segment", json!({ "device": POCO_SERIAL, "segment_id": "seg-run", "run_id": "run-a", "steps": r1_steps() })).await;
    let no = a.call("link.segment", json!({ "device": POCO_SERIAL, "segment_id": "seg-no", "run_id": "run-a", "steps": r1_steps() })).await;
    assert_eq!(code(&no), REFUSED, "{no}");
    assert!(
        link.segment_is_running(POCO_SERIAL, "seg-run")
            && !link.segment_is_running(POCO_SERIAL, "seg-no"),
        "the refused segment is not running"
    );
    hub.send_request(POCO_SERIAL, json!({"method": "ui.tap", "id": "plain-1"}))
        .await
        .unwrap();
    let file = hub_dir
        .join("hub-state")
        .join("responses")
        .join(format!("{}.jsonl", link_hub::store::file_stem(POCO_SERIAL)));
    let ids = || -> Vec<String> {
        std::fs::read_to_string(&file)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect()
    };
    phone::wait_until("all three answers are cached", 20, || {
        ids() == ["seg-run", "seg-no", "plain-1"]
    })
    .await;
    // eight days on the hub's clock, one more answer: the write prunes
    fake.advance(8 * 24 * 3600 * 1000);
    hub.send_request(POCO_SERIAL, json!({"method": "ui.tap", "id": "plain-2"}))
        .await
        .unwrap();
    phone::wait_until(
        "the old ordinary answer and the refused segment's are pruned, the running segment's kept",
        20,
        || ids() == ["seg-run", "plain-2"],
    )
    .await;
    runner.stop().await;
    link.shutdown();
}

/// The error object a phone answers with (the Java side of these is `RunnerMethodsTest` and `DispatcherTest`).
fn phone_err(code: i64, message: &str, reason: &str) -> Value {
    json!({"error": {"code": code, "message": message, "data": {"reason": reason}}})
}

/// Review defects 4 and 5, through the daemon: what each kind of phone answer to `runner.segment` means. Only
/// `accepted: true` is Ok; a definite refusal is REFUSED and the segment stops counting as running; an answer that
/// does not say whether the segment started is an error that is NOT a refusal and leaves it running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_accepted_true_is_ok_a_refusal_frees_the_segment_and_an_unknown_outcome_does_not() {
    let dir = tmp_dir("hub-seg-answers");
    let h = start(&dir, DeviceMode::Full, None);
    let mut a = Client::named(&h, "claude-a").await;
    let hub_dir = dir.join("hub");
    let link = h
        .daemon
        .attach_link(link_cfg(&hub_dir, "127.0.0.1"))
        .unwrap();
    let p = phone::FakePhone::new(phone::PhoneCfg {
        serial: POCO_SERIAL.into(),
        key: phone::phone_key(45),
        hub_spki: link.hub().hub_key().spki().to_vec(),
        sign_with: None,
        lie_epoch_seen: None,
        record: true,
        tls_fp: None,
    });
    phone::pair_direct(&hub_dir, POCO_SERIAL, &p.cfg.key);
    let runner = phone::PhoneRunner::start(p.clone(), link.hub().url());
    let hub = link.hub();
    phone::wait_until("the phone is connected", 20, || {
        hub.status(POCO_SERIAL).is_some_and(|s| s.connected)
    })
    .await;
    let epoch = a
        .ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await["epoch"]
        .as_u64()
        .unwrap();
    phone::wait_until("the phone is back at the lease epoch", 20, || {
        hub.status(POCO_SERIAL)
            .is_some_and(|s| s.connected && s.epoch == Some(epoch))
            && p.st().epoch == epoch
    })
    .await;
    let started =
        |id: &str| json!({"result": {"accepted": true, "outcome": "started", "segment_id": id}});
    {
        let mut st = p.st();
        // busy: transient (not remembered by anything), then taken when the same id is sent again
        st.scripted.insert(
            "seg-busy".into(),
            [
                phone_err(-32011, "busy: a segment is already running", "busy"),
                started("seg-busy"),
            ]
            .into(),
        );
        // a JSON-RPC error that says nothing ran: the gate, bad params
        st.scripted.insert(
            "seg-gate".into(),
            [phone_err(-32002, "not authenticated", "auth_required")].into(),
        );
        st.scripted.insert(
            "seg-params".into(),
            [phone_err(
                -32602,
                "runner.segment needs the signed segment as params",
                "",
            )]
            .into(),
        );
        // an answer that does not say whether it started: the runner timed out, an internal error, no `accepted`
        st.scripted.insert(
            "seg-slow".into(),
            [phone_err(
                -32011,
                "the runner did not answer in time",
                "runner_timeout",
            )]
            .into(),
        );
        st.scripted.insert(
            "seg-boom".into(),
            [phone_err(
                -32603,
                "internal error: IllegalStateException",
                "",
            )]
            .into(),
        );
        st.scripted.insert(
            "seg-echo".into(),
            [json!({"result": {"method": "runner.segment", "run": 1}})].into(),
        );
        st.scripted
            .insert("seg-ok".into(), [started("seg-ok")].into());
        // the phone says it already started this id (a hub that lost its supervisor sent it again)
        st.scripted.insert("seg-dup".into(), [json!({"result": {"accepted": false, "outcome": "refused", "code": "duplicate_segment", "reason": "already started", "segment_id": "seg-dup"}})].into());
    }
    let seg = |id: &str| json!({ "device": POCO_SERIAL, "segment_id": id, "run_id": "run-a", "steps": r1_steps() });

    // only accepted:true is Ok, and the segment is running
    let ok = a.ok("link.segment", seg("seg-ok")).await;
    assert_eq!(ok["response"]["result"]["accepted"], true, "{ok}");
    assert!(link.segment_is_running(POCO_SERIAL, "seg-ok"));

    // busy: a REFUSED that says it may be retried; nothing ran, so it is not running; the SAME id then goes through
    let busy = a.call("link.segment", seg("seg-busy")).await;
    assert_eq!(code(&busy), REFUSED, "{busy}");
    assert_eq!(
        (
            busy["error"]["data"]["phone"]["code"].as_str(),
            busy["error"]["data"]["retry"].as_bool()
        ),
        (Some("busy"), Some(true)),
        "{busy}"
    );
    assert!(
        !link.segment_is_running(POCO_SERIAL, "seg-busy"),
        "a refused segment is not running"
    );
    let again = a.ok("link.segment", seg("seg-busy")).await;
    assert_eq!(again["response"]["result"]["accepted"], true, "{again}");
    assert_eq!(
        p.st().executed["seg-busy"],
        2,
        "the hub did not cache the busy answer: the phone was asked twice"
    );
    assert_eq!(again["from_hub_cache"], false);
    assert!(
        link.segment_is_running(POCO_SERIAL, "seg-busy"),
        "sent again and taken: running again"
    );

    // JSON-RPC errors that say nothing ran are refusals, not Ok
    for id in ["seg-gate", "seg-params"] {
        let r = a.call("link.segment", seg(id)).await;
        assert_eq!(code(&r), REFUSED, "{id}: {r}");
        assert!(
            r["error"]["data"]["phone"]["code"].is_i64(),
            "{id}: the phone's error rides in data.phone: {r}"
        );
        assert!(
            !link.segment_is_running(POCO_SERIAL, id),
            "{id}: nothing ran"
        );
    }

    // outcomes that are not known: an error, but NOT a refusal, and the segment stays running (its report decides)
    for id in ["seg-slow", "seg-boom", "seg-echo"] {
        let r = a.call("link.segment", seg(id)).await;
        assert_eq!(code(&r), DEVICE_ERROR, "{id}: {r}");
        assert_eq!(r["error"]["data"]["outcome_unknown"], true, "{id}: {r}");
        assert_eq!(r["error"]["data"]["segment_id"], id, "{r}");
        assert!(
            link.segment_is_running(POCO_SERIAL, id),
            "{id}: it may be running"
        );
    }

    // a phone-side duplicate refusal is a refusal, but it must not free the segment: the first run is still its own
    let dup = a.call("link.segment", seg("seg-dup")).await;
    assert_eq!(code(&dup), REFUSED, "{dup}");
    assert_eq!(
        dup["error"]["data"]["phone"]["code"], "duplicate_segment",
        "{dup}"
    );
    assert!(
        link.segment_is_running(POCO_SERIAL, "seg-dup"),
        "a duplicate refusal leaves the earlier run's state alone"
    );

    // Review round 2, defect B: after an unknown outcome (the runner timed out and the segment may have started) or a
    // taken segment, the same segment id is not sent again: the hub refuses it before the phone hears of it, so it
    // cannot run twice. (A refused id, like seg-busy above, was sent again and taken.)
    for id in ["seg-slow", "seg-ok", "seg-busy"] {
        let before = p.st().executed.get(id).copied().unwrap_or(0);
        let r = a.call("link.segment", seg(id)).await;
        assert_eq!(code(&r), REFUSED, "{id}: {r}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("duplicate_segment"),
            "{id}: {r}"
        );
        assert_eq!(
            p.st().executed.get(id).copied().unwrap_or(0),
            before,
            "{id}: nothing was sent"
        );
        assert!(
            link.segment_is_running(POCO_SERIAL, id),
            "{id}: the first one is untouched"
        );
    }
    runner.stop().await;
    link.shutdown();
}

/// Review defect 3: a report pulled before the segment has finished is "not ready", and asking again under the same
/// request id (`report-<segment id>`) after it has finished gets the report: the early answer was not remembered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_report_pulled_before_the_segment_finished_does_not_hide_the_report_that_follows() {
    let dir = tmp_dir("hub-seg-report-early");
    let h = start(&dir, DeviceMode::Full, None);
    let mut a = Client::named(&h, "claude-a").await;
    let hub_dir = dir.join("hub");
    let link = h
        .daemon
        .attach_link(link_cfg(&hub_dir, "127.0.0.1"))
        .unwrap();
    let p = phone::FakePhone::new(phone::PhoneCfg {
        serial: POCO_SERIAL.into(),
        key: phone::phone_key(46),
        hub_spki: link.hub().hub_key().spki().to_vec(),
        sign_with: None,
        lie_epoch_seen: None,
        record: true,
        tls_fp: None,
    });
    phone::pair_direct(&hub_dir, POCO_SERIAL, &p.cfg.key);
    let runner = phone::PhoneRunner::start(p.clone(), link.hub().url());
    let hub = link.hub();
    phone::wait_until("the phone is connected", 20, || {
        hub.status(POCO_SERIAL).is_some_and(|s| s.connected)
    })
    .await;
    let epoch = a
        .ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await["epoch"]
        .as_u64()
        .unwrap();
    phone::wait_until("the phone is back at the lease epoch", 20, || {
        hub.status(POCO_SERIAL)
            .is_some_and(|s| s.connected && s.epoch == Some(epoch))
            && p.st().epoch == epoch
    })
    .await;

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let step = |id: &str, ms: u64| json!({"id": id, "status": "done", "post_verified": true, "decision": "allowed", "ts_ms": now_ms + ms, "ms": 5});
    let report = json!({"v": 1, "t": "segment_report", "segment_id": "seg-late", "run_id": "run-a", "device": POCO_SERIAL, "epoch": epoch, "outcome": "completed", "steps": [step("s1", 1), step("s2", 20), step("s3", 40)]});
    {
        let mut st = p.st();
        st.accept_segments = true;
        // not ready twice (the phone's transient "no report yet"), then the report
        st.scripted.insert(
            "report-seg-late".into(),
            [
                phone_err(-32011, "no report for segment seg-late (yet)", "no_report"),
                phone_err(-32011, "no report for segment seg-late (yet)", "no_report"),
                json!({"result": report}),
            ]
            .into(),
        );
    }
    a.ok("link.segment", json!({ "device": POCO_SERIAL, "segment_id": "seg-late", "run_id": "run-a", "steps": r1_steps() })).await;
    for _ in 0..2 {
        let early = a
            .call(
                "link.segment_report",
                json!({ "device": POCO_SERIAL, "segment_id": "seg-late" }),
            )
            .await;
        assert_eq!(code(&early), DEVICE_ERROR, "{early}");
        assert_eq!(
            (
                early["error"]["data"]["ready"].as_bool(),
                early["error"]["data"]["segment_id"].as_str()
            ),
            (Some(false), Some("seg-late")),
            "{early}"
        );
        assert!(
            early["error"]["message"].as_str().unwrap().contains("yet"),
            "{early}"
        );
    }
    assert!(
        link.segment_is_running(POCO_SERIAL, "seg-late"),
        "no report yet: still running"
    );
    // the segment has finished and filed its report: the SAME request id now gets it
    let rep = a
        .ok(
            "link.segment_report",
            json!({ "device": POCO_SERIAL, "segment_id": "seg-late" }),
        )
        .await;
    assert_eq!(
        (rep["ok"].as_bool(), rep["outcome"].as_str()),
        (Some(true), Some("completed")),
        "{rep}"
    );
    assert_eq!(
        p.st().executed["report-seg-late"],
        3,
        "the phone was asked three times under one id: nothing cached the early answers"
    );
    assert!(
        !link.segment_is_running(POCO_SERIAL, "seg-late"),
        "reported: no longer running"
    );
    runner.stop().await;
    link.shutdown();
}

// ---------------------------------------------------------------- the job view (2.11b)

fn dist(dir: &Path) -> PathBuf {
    let d = dir.join("dist");
    std::fs::create_dir_all(d.join("assets")).unwrap();
    std::fs::write(
        d.join("index.html"),
        "<!doctype html><html><head><title>t</title></head><body></body></html>",
    )
    .unwrap();
    std::fs::write(d.join("theme-boot.js"), "void 0;\n").unwrap();
    std::fs::write(d.join("assets").join("index-abc.js"), "console.log(1);\n").unwrap();
    std::fs::write(dir.join("secret.txt"), "outside dist\n").unwrap();
    d
}

struct Http {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Http {
    fn header(&self, n: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.as_str())
    }
}

fn parse_http(raw: &str) -> Http {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let mut it = head.split("\r\n");
    let status = it
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = it
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Http {
        status,
        headers,
        body: body.to_string(),
    }
}

async fn http(addr: SocketAddr, req: &str) -> Http {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_string(&mut out))
        .await
        .expect("the view answered")
        .unwrap();
    parse_http(&out)
}

fn get(addr: SocketAddr, path: &str, extra: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{extra}\r\n")
}

fn post(addr: SocketAddr, path: &str, extra: &str, body: &str) -> String {
    format!("POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}", body.len())
}

#[test]
fn the_view_refuses_every_interface_the_lan_and_the_tailnet_outside_hub_mode() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tmp_dir("view-bind");
        let d = dist(&dir);
        let h = start(&dir, DeviceMode::Full, None);
        for (ip, hub_mode) in [
            ("0.0.0.0", false),
            ("0.0.0.0", true),
            ("::", true),
            ("192.168.1.20", false),
            ("192.168.1.20", true),
            ("10.1.2.3", true),
            ("100.100.1.1", false),
        ] {
            let mut cfg = JobViewConfig::localhost(&d);
            cfg.bind.push(ip.parse().unwrap());
            cfg.hub_mode = hub_mode;
            assert!(
                JobView::start(h.daemon.clone(), cfg).is_err(),
                "{ip} (hub mode {hub_mode}) refused before binding"
            );
        }
        let mut cfg = JobViewConfig::localhost(&d);
        cfg.state_dir = Some(dir.clone());
        let v = JobView::start(h.daemon.clone(), cfg).unwrap();
        assert!(!v.local_addrs().is_empty());
        for a in v.local_addrs() {
            assert!(
                a.ip().is_loopback(),
                "the view is bound on {a}, not loopback"
            );
        }
        let f: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("jobview.json")).unwrap())
                .unwrap();
        assert_eq!(
            f["port"].as_u64(),
            Some(u64::from(v.local_addrs()[0].port())),
            "{f}"
        );
        // a client from the LAN (or anywhere but this laptop) gets 403, whatever it asks
        for (peer, hub_mode, want) in [
            ("192.168.1.5", false, 403),
            ("192.168.1.5", true, 403),
            ("100.64.1.2", false, 403),
            ("100.64.1.2", true, 200),
            ("127.0.0.1", false, 200),
        ] {
            let ctx = ViewCtx {
                daemon: h.daemon.clone(),
                dist: d.clone(),
                token: "t".into(),
                hub_mode,
            };
            let (client, server) = tokio::io::duplex(1 << 16);
            let peer: IpAddr = peer.parse().unwrap();
            let task =
                tokio::spawn(
                    async move { bridged::jobview::serve_stream(&ctx, server, peer).await },
                );
            let (mut r, mut w) = tokio::io::split(client);
            w.write_all(b"GET /jobs HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n")
                .await
                .unwrap();
            let mut out = String::new();
            r.read_to_string(&mut out).await.unwrap();
            task.await.unwrap();
            assert_eq!(
                parse_http(&out).status,
                want,
                "peer {peer} hub mode {hub_mode}"
            );
        }
        v.shutdown();
        assert!(
            !dir.join("jobview.json").exists(),
            "the port file goes with the view"
        );
    });
}

#[test]
fn the_view_serves_the_page_the_jobs_and_their_events_with_the_contract_headers() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tmp_dir("view-serve");
        let d = dist(&dir);
        let h = start(&dir, DeviceMode::Full, None);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO_SERIAL }))
            .await;
        let run_id = a
            .ok(
                "job.start",
                json!({ "device": POCO_SERIAL, "flow": two_taps() }),
            )
            .await["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(a.settled(&run_id).await["status"], "done");
        let v = JobView::start(h.daemon.clone(), JobViewConfig::localhost(&d)).unwrap();
        let addr = v.local_addrs()[0];
        // the page, with this start's token straight after <head>
        let page = http(addr, &get(addr, "/", "")).await;
        assert_eq!(page.status, 200);
        assert!(
            page.body.contains(&format!(
                "<head>\n<meta name=\"agentroid-token\" content=\"{}\">",
                v.token()
            )),
            "{}",
            page.body
        );
        assert_eq!(v.token().len(), 64);
        assert_eq!(
            page.header("content-type"),
            Some("text/html; charset=utf-8")
        );
        for r in [
            &page,
            &http(addr, &get(addr, "/jobs", "")).await,
            &http(addr, &get(addr, "/nope.js", "")).await,
        ] {
            assert_eq!(
                r.header("content-security-policy"),
                Some(bridged::jobview::CSP)
            );
            assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
            assert!(
                r.header("date").is_some_and(|d| d.ends_with(" GMT")),
                "a Date header"
            );
            assert_eq!(r.header("cache-control"), Some("no-store"));
        }
        let asset = http(addr, &get(addr, "/assets/index-abc.js", "")).await;
        assert_eq!(
            (asset.status, asset.header("content-type")),
            (200, Some("text/javascript; charset=utf-8"))
        );
        assert!(asset
            .header("cache-control")
            .is_some_and(|c| c.contains("max-age=31536000")));
        for bad in [
            "/../secret.txt",
            "/assets/../../secret.txt",
            "/%2e%2e/secret.txt",
            "/..%5csecret.txt",
        ] {
            assert_eq!(http(addr, &get(addr, bad, "")).await.status, 404, "{bad}");
        }
        // a page served under another name (DNS rebinding) cannot read the jobs
        let evil = http(addr, "GET /jobs HTTP/1.1\r\nHost: evil.example:80\r\n\r\n").await;
        assert_eq!(evil.status, 403);
        // the jobs
        let jobs: Value =
            serde_json::from_str(&http(addr, &get(addr, "/jobs", "")).await.body).unwrap();
        assert_eq!(jobs[0]["job_id"], run_id.as_str());
        let job = http(addr, &get(addr, &format!("/jobs/{run_id}"), "")).await;
        let jv: Value = serde_json::from_str(&job.body).unwrap();
        assert_eq!(
            (
                jv["status"].as_str(),
                jv["changes_in_force"].as_array().map(Vec::len)
            ),
            (Some("done"), Some(0)),
            "{jv}"
        );
        assert_eq!(
            http(addr, &get(addr, "/jobs/job-0-none", "")).await.status,
            404
        );
        // the NDJSON backlog, all of it and after a seq
        let all = http(
            addr,
            &get(
                addr,
                &format!("/jobs/{run_id}/events"),
                "Accept: application/x-ndjson\r\n",
            ),
        )
        .await;
        assert_eq!(
            all.header("content-type"),
            Some("application/x-ndjson; charset=utf-8")
        );
        let seqs: Vec<u64> = all
            .body
            .lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["seq"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert!(seqs.len() >= 3, "{seqs:?}");
        let later = http(
            addr,
            &get(
                addr,
                &format!("/jobs/{run_id}/events?after={}", seqs[1]),
                "Accept: application/x-ndjson\r\n",
            ),
        )
        .await;
        assert_eq!(later.body.lines().count(), seqs.len() - 2);
        // SSE for an ended job: the backlog as id/data frames, then the stream ends
        let sse = http(
            addr,
            &get(
                addr,
                &format!("/jobs/{run_id}/events"),
                "Accept: text/event-stream\r\n",
            ),
        )
        .await;
        assert_eq!(
            (sse.status, sse.header("content-type")),
            (200, Some("text/event-stream; charset=utf-8"))
        );
        let ids: Vec<u64> = sse
            .body
            .lines()
            .filter_map(|l| l.strip_prefix("id: "))
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(ids, seqs);
        for l in sse.body.lines().filter_map(|l| l.strip_prefix("data: ")) {
            assert!(
                serde_json::from_str::<Value>(l).is_ok(),
                "each data: is one JSON line"
            );
        }
        assert!(sse.body.contains("event: job\n"));
        // reconnecting with Last-Event-ID at the end: 204, so EventSource stops
        let last = *seqs.last().unwrap();
        let again = http(
            addr,
            &get(
                addr,
                &format!("/jobs/{run_id}/events"),
                &format!("Accept: text/event-stream\r\nLast-Event-ID: {last}\r\n"),
            ),
        )
        .await;
        assert_eq!(again.status, 204);
        // frame metadata and the recorder are not recorded yet: 404 (the page says so)
        for p in [
            "frame.json",
            "frame.jpg",
            "recorder.json",
            "recorder/crop.jpg",
        ] {
            assert_eq!(
                http(addr, &get(addr, &format!("/jobs/{run_id}/{p}"), ""))
                    .await
                    .status,
                404,
                "{p}"
            );
        }
        v.shutdown();
    });
}

#[test]
fn view_controls_need_the_token_and_the_origin_and_answer_by_state() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = tmp_dir("view-control");
        let d = dist(&dir);
        let h = start(&dir, DeviceMode::Full, None);
        let mut a = Client::named(&h, "claude-a").await;
        a.ok("lease.acquire", json!({ "device": POCO_SERIAL }))
            .await;
        // an unknown overlay: the job pauses (needs_attention) and keeps its lease
        h.dev.lock().unwrap().xml_override = Some(tree(&[(
            "com.example.nag",
            "Rate us!",
            "[100,1200][1120,1500]",
        )]));
        let run_id = a
            .ok(
                "job.start",
                json!({ "device": POCO_SERIAL, "flow": two_taps() }),
            )
            .await["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(a.settled(&run_id).await["status"], "paused");
        let v = JobView::start(h.daemon.clone(), JobViewConfig::localhost(&d)).unwrap();
        let addr = v.local_addrs()[0];
        let path = format!("/jobs/{run_id}/control");
        let tok = format!("X-Agentroid-Token: {}\r\n", v.token());
        let origin_ok = format!("Origin: http://{addr}\r\n");
        let body = |a: &str| format!("{{\"action\":\"{a}\"}}");
        assert_eq!(
            http(addr, &post(addr, &path, "", &body("stop")))
                .await
                .status,
            403,
            "no token"
        );
        assert_eq!(
            http(
                addr,
                &post(addr, &path, "X-Agentroid-Token: 00\r\n", &body("stop"))
            )
            .await
            .status,
            403,
            "wrong token"
        );
        assert_eq!(
            http(
                addr,
                &post(
                    addr,
                    &path,
                    &format!("{tok}Origin: http://evil.example\r\n"),
                    &body("stop")
                )
            )
            .await
            .status,
            403,
            "another origin"
        );
        assert_eq!(
            http(addr, &get(addr, &path, &tok)).await.status,
            405,
            "GET on control"
        );
        assert_eq!(
            http(
                addr,
                &post(addr, &path, &format!("{tok}{origin_ok}"), &body("pause"))
            )
            .await
            .status,
            409,
            "a paused job cannot pause"
        );
        assert_eq!(
            http(
                addr,
                &post(addr, &path, &format!("{tok}{origin_ok}"), &body("retry"))
            )
            .await
            .status,
            501
        );
        assert_eq!(
            http(
                addr,
                &post(addr, &path, &format!("{tok}{origin_ok}"), &body("explode"))
            )
            .await
            .status,
            400
        );
        assert_eq!(
            http(
                addr,
                &post(addr, &path, &format!("{tok}{origin_ok}"), "not json")
            )
            .await
            .status,
            400
        );
        // a paused job's SSE stays open: the backlog arrives, then the test hangs up
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            get(
                addr,
                &format!("/jobs/{run_id}/events"),
                "Accept: text/event-stream\r\n",
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut buf = vec![0u8; 1 << 16];
        let mut got = String::new();
        let t = Instant::now();
        while !got.contains("event: job") && t.elapsed() < Duration::from_secs(10) {
            let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf))
                .await
                .unwrap()
                .unwrap();
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(
            got.contains("\r\n\r\nretry: 2000")
                && got.contains("id: ")
                && got.contains("event: job"),
            "{got}"
        );
        drop(s);
        // stop, accepted (no Origin header is fine: a same-origin fetch may omit it)
        let r = http(addr, &post(addr, &path, &tok, &body("stop"))).await;
        assert_eq!((r.status, r.body.trim()), (202, "{\"accepted\":true}"));
        assert_eq!(
            a.ok("job.status", json!({ "run_id": run_id })).await["status"],
            "stopped"
        );
        v.shutdown();
    });
}

// ---------------------------------------------------------------- wiring 2 (25 Oct): the phone's clock

/// The phone's clock reading (microseconds) at hub time `hub_ms`, for a phone whose clock runs
/// `skew_ms` ahead of the hub's (negative: behind).
fn phone_us(hub_ms: u64, skew_ms: i64) -> u64 {
    u64::try_from((i128::from(hub_ms) + i128::from(skew_ms)) * 1000)
        .expect("a phone clock cannot be negative")
}

fn stamp(us: u64) -> String {
    format!("[{:>5}.{:06}]", us / 1_000_000, us % 1_000_000)
}

/// `getevent -lt` lines for a finger tapping the touchscreen (event5) at phone time `us`.
fn tap_lines(us: u64) -> String {
    let (d, u) = (stamp(us), stamp(us + 70_000));
    format!(
        "{d} /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   00000041\n{d} /dev/input/event5: EV_KEY       BTN_TOUCH            DOWN\n{d} /dev/input/event5: EV_SYN       SYN_REPORT           00000000\n\
         {u} /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   ffffffff\n{u} /dev/input/event5: EV_KEY       BTN_TOUCH            UP\n{u} /dev/input/event5: EV_SYN       SYN_REPORT           00000000\n"
    )
}

/// A volume-key event: not a touch, but it carries the phone's clock.
fn key_lines(us: u64) -> String {
    let s = stamp(us);
    format!("{s} /dev/input/event1: EV_KEY       KEY_VOLUMEUP         DOWN\n{s} /dev/input/event1: EV_SYN       SYN_REPORT           00000000\n")
}

/// A report timestamp with nothing in it, from the touchscreen node: it carries the phone's clock
/// (a sample for the offset) and is not a touch.
fn idle_lines(us: u64) -> String {
    format!(
        "{} /dev/input/event5: EV_SYN       SYN_REPORT           00000000
",
        stamp(us)
    )
}

/// A job holding a tap in flight on a phone whose clock is `skew_ms` from the hub's; the hub has
/// been up `uptime_ms`. Returns once the tap is in flight, with the job's start time.
async fn skewed_job(name: &str, uptime_ms: u64) -> (Harness, CareRig, Client, String, u64) {
    let dir = tmp_dir(name);
    let (rig, mut care) = care_rig(&dir, |_, _| {});
    care.profiles = Some(scaffold().join("profiles"));
    rig.clock.advance(uptime_ms);
    let clock = rig.clock.clone();
    let h = start(&dir, DeviceMode::Full, Some(care));
    let mut a = Client::named(&h, "claude-a").await;
    a.ok("lease.acquire", json!({ "device": POCO_SERIAL }))
        .await;
    h.hold_input.store(true, Ordering::SeqCst);
    let run_id = a
        .ok(
            "job.start",
            json!({ "device": POCO_SERIAL, "flow": tap_then_verify() }),
        )
        .await["run_id"]
        .as_str()
        .unwrap()
        .to_string();
    let flag = h.in_input.clone();
    wait_for("the tap in flight", move || flag.load(Ordering::SeqCst)).await;
    let t0 = clock.mono_ms();
    (h, rig, a, run_id, t0)
}

/// A touch that happened before the job started, delivered late, is not a take-over; a person's
/// touch during the job is, whatever the phone's clock reads.
async fn skewed_touch_case(name: &str, skew_ms: i64, uptime_ms: u64) {
    let (h, rig, mut a, run_id, t0) = skewed_job(name, uptime_ms).await;
    let clock = rig.clock.clone();
    let map = getevent("synthetic-map.txt");
    let dev = POCO_SERIAL;
    // 1. the stream is flowing: a touchscreen report calibrates the kernel-clock offset (30 ms transport
    // delay). A volume key on another node is not a touchscreen sample and calibrates nothing.
    clock.advance(30);
    assert!(
        h.daemon
            .feed_getevent_live(dev, &map, &key_lines(phone_us(t0, skew_ms)))
            .is_empty(),
        "a key press is not a touch"
    );
    assert_eq!(
        h.daemon.phone_to_hub_ms(
            dev,
            handover::touch::PhoneClock::Kernel,
            phone_us(t0, skew_ms)
        ),
        None,
        "only the touchscreen's lines are samples"
    );
    assert!(
        h.daemon
            .feed_getevent_live(dev, &map, &idle_lines(phone_us(t0, skew_ms)))
            .is_empty(),
        "an idle report is not a touch"
    );
    let off = h
        .daemon
        .phone_to_hub_ms(
            dev,
            handover::touch::PhoneClock::Kernel,
            phone_us(t0, skew_ms),
        )
        .expect("calibrated");
    assert!(
        (t0..=t0 + 30).contains(&off),
        "the report maps to hub time {off}, it happened at {t0}"
    );
    // 2. a touch from 1 s BEFORE the job began reaches the hub 1.5 s late: older than the agent's hold. (A chunk more
    // than the 2 s break limit late cannot be told from a phone that slept for that long, and is dated from its
    // arrival instead, so that a person is never ignored: see `a_chunk_that_disagrees_with_the_stored_offset...`.)
    clock.advance(500);
    let stale = h
        .daemon
        .feed_getevent_live(dev, &map, &tap_lines(phone_us(t0 - 1_000, skew_ms)));
    assert_eq!(stale.len(), 1, "Detector 1 saw the touch");
    assert!(
        !stale[0].take_over,
        "a touch from before the job is not a take-over: {:?}",
        stale[0]
    );
    assert_eq!(
        a.ok("job.status", json!({ "run_id": run_id })).await["status"],
        "running",
        "the job carries on"
    );
    // 3. a person taps now (30 ms before the lines arrive)
    clock.advance(1_000);
    let now = clock.mono_ms();
    let outs = h
        .daemon
        .feed_getevent_live(dev, &map, &tap_lines(phone_us(now - 30, skew_ms)));
    let first = outs.first().expect("the touch reached the machine");
    assert!(first.take_over, "{first:?}");
    assert_eq!(
        first.interrupted.as_deref(),
        Some("bt"),
        "the step in flight"
    );
    // the segment/job is blocked: nothing runs after the cut, however long we wait
    h.hold_input.store(false, Ordering::SeqCst);
    let s = a.settled(&run_id).await;
    assert_eq!(s["status"], "taken_over", "{s}");
    assert_eq!(taps(&h), 1, "only the tap that was already in flight");
    let journal = lines(&run_dir(&h, &run_id).join("journal.ndjson"));
    assert!(
        !journal
            .iter()
            .any(|r| r["step_id"] == "check" && r["status"] == "done"),
        "the verify step never ran: {journal:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_clock_hours_ahead_is_mapped_onto_the_hub_clock_before_a_touch_is_judged() {
    skewed_touch_case("skew-ahead", 3 * 3_600_000, 600_000).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_clock_behind_the_hub_still_stops_the_job_on_a_touch() {
    skewed_touch_case("skew-behind", -1_800_000, 3_600_000).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_overlay_report_on_the_phones_elapsed_clock_is_checked_against_the_hubs_injection_windows(
) {
    // the phone's elapsed clock is 30 minutes behind the hub's mono clock (it rebooted later)
    let skew = -1_800_000i64;
    let (h, rig, mut a, run_id, t0) = skewed_job("skew-overlay", 3_600_000).await;
    let clock = rig.clock.clone();
    let dev = POCO_SERIAL;
    clock.advance(500);
    let now = clock.mono_ms();
    assert!(now > t0);
    // the overlay saw our own tap's pointer 20 ms ago and reports it with its own elapsedRealtime
    let out = h
        .daemon
        .phone_touch(
            dev,
            handover::touch::PhoneClock::Elapsed,
            phone_us(now - 20, skew),
            phone_us(now, skew),
            handover::mode::Source::Overlay,
        )
        .expect("a job with take-over runs here");
    assert!(
        out.own_injection && !out.take_over,
        "the hub's own injection, dropped: {out:?}"
    );
    // the two phone clocks never mix: nothing was learned about the kernel clock from the overlay
    assert_eq!(
        h.daemon.phone_to_hub_ms(
            dev,
            handover::touch::PhoneClock::Kernel,
            phone_us(now, skew)
        ),
        None
    );
    // the job was not disturbed
    h.hold_input.store(false, Ordering::SeqCst);
    let s = a.settled(&run_id).await;
    assert_eq!(s["status"], "done", "{s}");
    assert_eq!(taps(&h), 1);
}

// ---------------------------------------------------------------- review fixes: clock breaks, split frames

/// Calibrated, then the phone reboots: its kernel clock restarts near zero, hours behind what the
/// stored offset assumed. `touch_first`: the person's touch is the very first chunk after the reboot.
/// Otherwise two idle reports re-anchor the map first. Either way the job must be taken over.
async fn rebooted_phone_case(name: &str, touch_first: bool) {
    let skew = 3 * 3_600_000i64;
    let (h, rig, mut a, run_id, t0) = skewed_job(name, 600_000).await;
    let clock = rig.clock.clone();
    let map = getevent("synthetic-map.txt");
    let dev = POCO_SERIAL;
    let kernel = handover::touch::PhoneClock::Kernel;
    clock.advance(30);
    h.daemon
        .feed_getevent_live(dev, &map, &idle_lines(phone_us(t0, skew)));
    // 20 s on, the phone has rebooted: its clock reads 5 s at hub time t0 + 20 s
    clock.advance(20_000);
    let reboot_at = clock.mono_ms();
    let after = |hub: u64| (hub - reboot_at) * 1000 + 5_000_000; // phone microseconds
    if !touch_first {
        clock.advance(500);
        h.daemon
            .feed_getevent_live(dev, &map, &idle_lines(after(clock.mono_ms())));
        clock.advance(500);
        h.daemon
            .feed_getevent_live(dev, &map, &idle_lines(after(clock.mono_ms())));
        let m = h
            .daemon
            .phone_to_hub_ms(dev, kernel, after(clock.mono_ms()))
            .expect("calibrated");
        assert!(
            m.abs_diff(clock.mono_ms()) <= 100,
            "two agreeing reports re-anchored the map: {m} vs {}",
            clock.mono_ms()
        );
    }
    clock.advance(1_000);
    let now = clock.mono_ms();
    let outs = h
        .daemon
        .feed_getevent_live(dev, &map, &tap_lines(after(now - 30)));
    let first = outs.first().expect("the touch reached the machine");
    assert!(
        first.take_over,
        "a touch after a phone reboot is still a take-over: {first:?}"
    );
    h.hold_input.store(false, Ordering::SeqCst);
    assert_eq!(a.settled(&run_id).await["status"], "taken_over");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_touch_right_after_the_phone_rebooted_is_still_a_take_over() {
    rebooted_phone_case("reboot-first", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_reports_after_a_reboot_re_anchor_the_clock_and_the_next_touch_is_a_take_over() {
    rebooted_phone_case("reboot-then", false).await;
}

/// Review round 2, defect 1: a SHORT suspend (2 to 15 s, a screen-off phone sleeping in bursts) keeps the stale offset
/// inside the plausibility window, and the chunk's own sample is only a candidate. The touch must not be dated from
/// the stale offset (5 s early, before the agent's hold, so the person would be ignored): the chunk disagrees with
/// the stored offset by more than the clock-break limit, so it is dated from its arrival.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_touch_right_after_a_five_second_suspend_is_still_a_take_over() {
    let skew = 3 * 3_600_000i64;
    let (h, rig, mut a, run_id, t0) = skewed_job("suspend-5s", 600_000).await;
    let clock = rig.clock.clone();
    let map = getevent("synthetic-map.txt");
    let dev = POCO_SERIAL;
    clock.advance(30);
    h.daemon
        .feed_getevent_live(dev, &map, &idle_lines(phone_us(t0, skew)));
    // the phone sleeps for 5 s right after (its kernel clock stops), then a person taps: the chunk reads 5 s behind
    clock.advance(5_200);
    let now = clock.mono_ms();
    let suspended = 5_000i64;
    let outs =
        h.daemon
            .feed_getevent_live(dev, &map, &tap_lines(phone_us(now - 30, skew - suspended)));
    let first = outs.first().expect("the touch reached the machine");
    assert!(
        first.take_over,
        "a touch right after a short suspend is a take-over, not ignored as old: {first:?}"
    );
    let _ = &mut a;
    h.hold_input.store(false, Ordering::SeqCst);
    assert_eq!(a.settled(&run_id).await["status"], "taken_over");
}

/// The same case at the dating itself, where the early date is visible: the touch happened 30 ms before the chunk
/// arrived. After a 5 s suspend the stored offset would put it 5 s early (inside the 15 s plausibility window); the
/// chunk disagrees with the offset by 5 s, so it is dated from its arrival. A chunk that is merely 1 s late agrees and
/// still maps through the stored offset to when the touch really happened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chunk_that_disagrees_with_the_stored_offset_dates_its_touch_from_its_arrival() {
    let skew = 3 * 3_600_000i64;
    let (h, rig, _a, _run_id, t0) = skewed_job("suspend-dating", 600_000).await;
    let clock = rig.clock.clone();
    let map = getevent("synthetic-map.txt");
    let dev = POCO_SERIAL;
    let kernel = handover::touch::PhoneClock::Kernel;
    clock.advance(30);
    h.daemon
        .feed_getevent_live(dev, &map, &idle_lines(phone_us(t0, skew)));
    // a 5 s suspend, then a touch 30 ms before its chunk arrives (the chunk's last line is 70 ms after the touch)
    clock.advance(5_200);
    let now = clock.mono_ms();
    let (touch, sent) = (
        phone_us(now - 100, skew - 5_000),
        phone_us(now - 30, skew - 5_000),
    );
    let stale = h
        .daemon
        .phone_to_hub_ms(dev, kernel, touch)
        .expect("calibrated");
    assert!(
        stale.abs_diff(now - 5_100) <= 100,
        "the stored offset alone would date it 5 s early: {stale} vs {now}"
    );
    let at = h.daemon.touch_time_on_hub(dev, kernel, touch, sent, now);
    assert!(
        at.abs_diff(now - 30) <= 100,
        "dated from its arrival, not from the stale offset: {at} vs {now}"
    );
    // the map re-anchors on two agreeing samples after the suspend; then a chunk that is 1 s late (inside the break
    // limit) still maps through the stored offset to when the touch really happened
    clock.advance(1_000);
    h.daemon.feed_getevent_live(
        dev,
        &map,
        &idle_lines(phone_us(clock.mono_ms(), skew - 5_000)),
    );
    clock.advance(10);
    h.daemon.feed_getevent_live(
        dev,
        &map,
        &idle_lines(phone_us(clock.mono_ms(), skew - 5_000)),
    );
    clock.advance(1_000);
    let now = clock.mono_ms();
    let (touch, sent) = (
        phone_us(now - 1_100, skew - 5_000),
        phone_us(now - 1_030, skew - 5_000),
    );
    let at = h.daemon.touch_time_on_hub(dev, kernel, touch, sent, now);
    assert!(
        at.abs_diff(now - 1_100) <= 100,
        "a 1 s late chunk still maps through the stored offset: {at} vs {}",
        now - 1_100
    );
    h.hold_input.store(false, Ordering::SeqCst);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_garbled_timestamp_does_not_move_the_clock_and_a_touch_in_the_same_chunk_still_counts()
{
    let skew = 0i64;
    let (h, rig, mut a, run_id, t0) = skewed_job("skew-garbage", 600_000).await;
    let clock = rig.clock.clone();
    let map = getevent("synthetic-map.txt");
    let dev = POCO_SERIAL;
    let kernel = handover::touch::PhoneClock::Kernel;
    clock.advance(30);
    h.daemon
        .feed_getevent_live(dev, &map, &idle_lines(phone_us(t0, skew)));
    // a touchscreen line whose timestamp reads five hours ahead: not a sample
    clock.advance(2_000);
    h.daemon.feed_getevent_live(
        dev,
        &map,
        &idle_lines(phone_us(clock.mono_ms(), skew) + 5 * 3_600_000_000),
    );
    let m = h
        .daemon
        .phone_to_hub_ms(dev, kernel, phone_us(clock.mono_ms(), skew))
        .expect("calibrated");
    assert!(
        m.abs_diff(clock.mono_ms()) <= 100,
        "the map was not moved by one wild line: {m} vs {}",
        clock.mono_ms()
    );
    // a real touch arrives in the same chunk as another wild line
    clock.advance(1_000);
    let now = clock.mono_ms();
    let chunk = format!(
        "{}{}",
        tap_lines(phone_us(now - 30, skew)),
        idle_lines(phone_us(now, skew) + 9 * 3_600_000_000)
    );
    let outs = h.daemon.feed_getevent_live(dev, &map, &chunk);
    let first = outs.first().expect("the touch reached the machine");
    assert!(first.take_over, "{first:?}");
    h.hold_input.store(false, Ordering::SeqCst);
    assert_eq!(a.settled(&run_id).await["status"], "taken_over");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_touch_frame_split_across_two_reads_or_fed_a_line_at_a_time_is_one_take_over() {
    // split in two: the DOWN is in the first read, the SYN_REPORT that closes the frame in the second
    let (h, rig, mut a, run_id, t0) = skewed_job("split-two", 600_000).await;
    let clock = rig.clock.clone();
    let map = getevent("synthetic-map.txt");
    clock.advance(500);
    let now = clock.mono_ms();
    let frame = tap_lines(phone_us(now, 0));
    let down: Vec<&str> = frame.lines().take(3).collect();
    let (first_half, rest) = (down[..2].join("\n") + "\n", down[2..].join("\n") + "\n");
    assert!(
        h.daemon
            .feed_getevent_live(POCO_SERIAL, &map, &first_half)
            .is_empty(),
        "the frame is not closed yet"
    );
    clock.advance(5);
    let outs = h.daemon.feed_getevent_live(POCO_SERIAL, &map, &rest);
    assert_eq!(
        outs.len(),
        1,
        "the frame closed in the second read: {outs:?}"
    );
    assert!(outs[0].take_over, "{:?}", outs[0]);
    h.hold_input.store(false, Ordering::SeqCst);
    assert_eq!(a.settled(&run_id).await["status"], "taken_over");
    let _ = t0;

    // a line at a time
    let (h, rig, mut a, run_id, _t0) = skewed_job("split-lines", 600_000).await;
    let clock = rig.clock.clone();
    clock.advance(500);
    let now = clock.mono_ms();
    let mut outs = vec![];
    for l in tap_lines(phone_us(now, 0)).lines() {
        outs.extend(
            h.daemon
                .feed_getevent_live(POCO_SERIAL, &map, &format!("{l}\n")),
        );
    }
    assert_eq!(outs.len(), 1, "one touch, however it is fed: {outs:?}");
    assert!(outs[0].take_over);
    h.hold_input.store(false, Ordering::SeqCst);
    assert_eq!(a.settled(&run_id).await["status"], "taken_over");
}
