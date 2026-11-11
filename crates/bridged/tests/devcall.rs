//! Task 2.4, daemon side: the 14 forwarded tools, `require` / `retry` / `verify`, and the `health`
//! member. Fakes only: no adb, no device, no pipe. Core's fake phone sits behind the same gate as the
//! real one; a wrapper backend adds failure injection and the phone-half hooks (pixel, crop/OCR).

use agentroid_core::fake::{FakeClock, FakeDevice};
use agentroid_core::gate::{Call, DeviceMode};
use agentroid_core::transport::{
    Backend, Core, CoreError, DeviceInfo, HealthHint, Input, Outcome, Screenshot, Transport,
    TransportError,
};
use agentroid_core::tree::Bounds;
use bridged::devcall::Waits;
use bridged::listener::{duplex_listener, DuplexConnector};
use bridged::proto::*;
use bridged::registry::Registry;
use bridged::server::{self, CoreBackend, CropOut, Daemon, DeviceBackend};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::watch;

const POCO: &str = "DEVICE0000EXAMPLE";
const S24: &str = "PROTECTED0000EXAMPLE";

fn run<F: Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), f)
            .await
            .expect("test timed out")
    })
}

#[derive(Clone)]
struct Shared(Arc<Mutex<FakeDevice>>);

impl Transport for Shared {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        self.0.lock().unwrap().devices()
    }
    fn screen_size(&mut self, s: &str) -> Result<(i32, i32), TransportError> {
        self.0.lock().unwrap().screen_size(s)
    }
    fn screen_state(&mut self, s: &str) -> Result<String, TransportError> {
        self.0.lock().unwrap().screen_state(s)
    }
    fn screenshot(&mut self, s: &str, w: i32) -> Result<Screenshot, TransportError> {
        self.0.lock().unwrap().screenshot(s, w)
    }
    fn tree_xml(&mut self, s: &str, b: Backend) -> Result<String, TransportError> {
        self.0.lock().unwrap().tree_xml(s, b)
    }
    fn input(&mut self, s: &str, i: &Input) -> Result<(), TransportError> {
        self.0.lock().unwrap().input(s, i)
    }
    fn packages(&mut self, s: &str) -> Result<Vec<String>, TransportError> {
        self.0.lock().unwrap().packages(s)
    }
    fn shell(&mut self, s: &str, l: &str) -> Result<String, TransportError> {
        self.0.lock().unwrap().shell(s, l)
    }
    fn helper_status(&mut self, s: &str) -> Result<(), String> {
        self.0.lock().unwrap().helper_status(s)
    }
    fn health_hint(&mut self, s: &str) -> HealthHint {
        self.0.lock().unwrap().health_hint(s)
    }
}

/// Core's backend plus: taps that fail on the transport a set number of times, and the phone-half hooks.
struct Wrapped {
    inner: CoreBackend<Shared, FakeClock>,
    /// Taps still to fail with a transport error.
    fail_taps: Arc<AtomicU32>,
    /// Taps that reached the backend (failed or not).
    tap_attempts: Arc<AtomicU32>,
    pixel_reads: Arc<AtomicU32>,
    /// open_app calls still to fail with a transport error (an idempotent acting call), and those that arrived.
    fail_opens: Arc<AtomicU32>,
    open_attempts: Arc<AtomicU32>,
}

impl DeviceBackend for Wrapped {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        self.inner.devices()
    }
    fn mode_of(&self, d: &DeviceInfo) -> DeviceMode {
        self.inner.mode_of(d)
    }
    fn call(&mut self, serial: &str, call: &Call) -> Result<Outcome, CoreError> {
        if matches!(call, Call::Tap { .. }) {
            self.tap_attempts.fetch_add(1, Ordering::SeqCst);
            if self.fail_taps.load(Ordering::SeqCst) > 0 {
                self.fail_taps.fetch_sub(1, Ordering::SeqCst);
                // the gate still runs first, as in core
                self.inner.admit(serial, call)?;
                return Err(CoreError::Transport(TransportError("link dropped".into())));
            }
        }
        if matches!(call, Call::OpenApp { .. }) {
            self.open_attempts.fetch_add(1, Ordering::SeqCst);
            if self.fail_opens.load(Ordering::SeqCst) > 0 {
                self.fail_opens.fetch_sub(1, Ordering::SeqCst);
                self.inner.admit(serial, call)?;
                return Err(CoreError::Transport(TransportError("link dropped".into())));
            }
        }
        self.inner.call(serial, call)
    }
    fn admit(&mut self, serial: &str, call: &Call) -> Result<(), CoreError> {
        self.inner.admit(serial, call)
    }
    fn allowed(&self, d: &DeviceInfo) -> bool {
        self.inner.allowed(d)
    }
    fn health(&mut self, d: &DeviceInfo) -> HealthHint {
        self.inner.health(d)
    }
    fn latest_tree(&mut self, d: &DeviceInfo) -> Option<(String, bool)> {
        self.inner.latest_tree(d)
    }
    fn snapshot_diff(&mut self, d: &DeviceInfo, since: &str) -> Result<String, CoreError> {
        self.inner.snapshot_diff(d, since)
    }
    fn pixel(
        &mut self,
        _d: &DeviceInfo,
        x: f64,
        y: f64,
        _space: Option<&str>,
    ) -> Result<[u8; 3], CoreError> {
        self.pixel_reads.fetch_add(1, Ordering::SeqCst);
        Ok([(x as u8), (y as u8), 30])
    }
    fn crop(&mut self, _d: &DeviceInfo, b: Bounds, ocr: bool) -> Result<CropOut, CoreError> {
        assert!(b.width() > 0 && b.height() > 0);
        Ok(if ocr {
            CropOut {
                text: Some("+2".into()),
                image_bytes: None,
            }
        } else {
            CropOut {
                text: None,
                image_bytes: Some(321),
            }
        })
    }
}

struct Harness {
    connector: DuplexConnector,
    shutdown: watch::Sender<bool>,
    daemon: Arc<Daemon>,
    device: Arc<Mutex<FakeDevice>>,
    fail_taps: Arc<AtomicU32>,
    tap_attempts: Arc<AtomicU32>,
    pixel_reads: Arc<AtomicU32>,
    fail_opens: Arc<AtomicU32>,
    open_attempts: Arc<AtomicU32>,
    /// The virtual clock `require` and `verify` wait by (ms): a wait of 5 s costs no real time.
    waited_ms: Arc<AtomicU64>,
    /// Sleeps that happened while the backend lock was free (the daemon waits without holding it).
    sleeps_unlocked: Arc<AtomicU32>,
    sleeps: Arc<AtomicU32>,
    /// When set, a keyboard that rewrites the focused field the moment the 150 ms re-read pause ends.
    rewrite_field: Arc<AtomicU32>,
}

fn start(mode: DeviceMode) -> Harness {
    let device = Arc::new(Mutex::new(FakeDevice::new()));
    let mut inner = CoreBackend::new(Core::new(Shared(device.clone()), FakeClock::new()));
    inner.set_mode(POCO, mode);
    let (fail_taps, tap_attempts, pixel_reads) = (
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
        Arc::new(AtomicU32::new(0)),
    );
    let (fail_opens, open_attempts) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
    let backend = Wrapped {
        inner,
        fail_taps: fail_taps.clone(),
        tap_attempts: tap_attempts.clone(),
        pixel_reads: pixel_reads.clone(),
        fail_opens: fail_opens.clone(),
        open_attempts: open_attempts.clone(),
    };
    let daemon = Daemon::new(Registry::in_memory(), Box::new(backend));
    let waited_ms = Arc::new(AtomicU64::new(0));
    let (sleeps_unlocked, sleeps) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
    let rewrite_field = Arc::new(AtomicU32::new(0));
    {
        let (now, nap) = (waited_ms.clone(), waited_ms.clone());
        let (free, all) = (sleeps_unlocked.clone(), sleeps.clone());
        let me = Arc::downgrade(&daemon);
        let (rewrite, dev) = (rewrite_field.clone(), device.clone());
        daemon.set_waits(Waits {
            now_ms: Box::new(move || now.load(Ordering::SeqCst)),
            sleep_ms: Box::new(move |ms| {
                if ms == 150 && rewrite.load(Ordering::SeqCst) > 0 {
                    dev.lock().unwrap().typed = "garbled".into();
                }
                all.fetch_add(1, Ordering::SeqCst);
                if me.upgrade().is_some_and(|d| d.backend_is_free()) {
                    free.fetch_add(1, Ordering::SeqCst);
                }
                nap.fetch_add(ms, Ordering::SeqCst);
            }),
        });
    }
    let (listener, connector) = duplex_listener();
    let (shutdown, rx) = watch::channel(false);
    tokio::spawn(server::serve(daemon.clone(), listener, rx));
    Harness {
        connector,
        shutdown,
        daemon,
        device,
        fail_taps,
        tap_attempts,
        pixel_reads,
        fail_opens,
        open_attempts,
        waited_ms,
        sleeps_unlocked,
        sleeps,
        rewrite_field,
    }
}

struct Client {
    r: BufReader<ReadHalf<DuplexStream>>,
    w: WriteHalf<DuplexStream>,
    next: u64,
}

impl Client {
    fn new(h: &Harness) -> Client {
        let (r, w) = tokio::io::split(h.connector.connect().unwrap());
        Client {
            r: BufReader::new(r),
            w,
            next: 1,
        }
    }
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let line =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
        let mut out = String::new();
        assert!(
            self.r.read_line(&mut out).await.unwrap() > 0,
            "connection closed"
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["id"], id);
        v
    }
    /// A client that already holds the lease on `device`.
    async fn leased(h: &Harness, device: &str) -> Client {
        let mut c = Client::new(h);
        let r = c.call("lease.acquire", json!({ "device": device })).await;
        assert!(r["result"]["epoch"].is_u64(), "{r}");
        c
    }
    async fn tool(&mut self, device: &str, call: Value) -> Value {
        self.call("device.call", json!({ "device": device, "call": call }))
            .await
    }
}

fn code(r: &Value) -> i64 {
    r["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected an error, got {r}"))
}

/// Every device.call answer past the lease check carries a health member with at least model and mode.
fn health_of(r: &Value) -> &Value {
    let h = if r.get("error").is_some() {
        &r["error"]["data"]["health"]
    } else {
        &r["result"]["health"]
    };
    assert!(h.is_object(), "no health member in {r}");
    assert!(
        h["model"].is_string() && h["mode"].is_string(),
        "health lacks identity: {r}"
    );
    h
}

const SEARCH: fn() -> Value = || json!({ "desc": "Search" });

#[test]
fn the_fourteen_tools_run_through_the_daemon_and_each_reply_has_health() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        c.tool(POCO, json!({ "tool": "ui_tree" })).await; // so tap-by-index has a tree
        let calls = [
            json!({ "tool": "screenshot", "max_width": 400 }),
            json!({ "tool": "long_press", "x": 100, "y": 650, "space": "device", "ms": 500 }),
            json!({ "tool": "long_press", "index": 1 }),
            json!({ "tool": "swipe", "x1": 100, "y1": 1000, "x2": 100, "y2": 400, "space": "device" }),
            json!({ "tool": "scroll", "direction": "down" }),
            json!({ "tool": "type_text", "text": "hi", "submit": true }),
            json!({ "tool": "key", "name": "back" }),
            json!({ "tool": "unlock" }),
            json!({ "tool": "open_app", "package": "com.android.settings" }),
            json!({ "tool": "open_url", "uri": "https://example.com" }),
            json!({ "tool": "start_activity", "component": "com.android.settings/.Settings" }),
            json!({ "tool": "list_apps", "filter": "maps" }),
            json!({ "tool": "wait", "seconds": 0.2 }),
            json!({ "tool": "getprop", "key": "ro.product.model" }),
            json!({ "tool": "set_value", "element": SEARCH(), "value": "hello" }),
            json!({ "tool": "pixel_probe", "x": 100, "y": 200, "space": "device" }),
            json!({ "tool": "look", "element": SEARCH() }),
            json!({ "tool": "look", "element": SEARCH(), "ocr": true }),
        ];
        for call in calls {
            let r = c.tool(POCO, call.clone()).await;
            assert!(r.get("error").is_none(), "{call} failed: {r}");
            assert_eq!(r["result"]["tool"], call["tool"]);
            assert_eq!(r["result"]["attempts"], 1);
            health_of(&r);
        }
        // specific answers
        let r = c
            .tool(POCO, json!({ "tool": "list_apps", "filter": "maps" }))
            .await;
        assert_eq!(r["result"]["text"], "com.google.android.apps.maps");
        let r = c
            .tool(POCO, json!({ "tool": "pixel_probe", "x": 100, "y": 200 }))
            .await;
        assert_eq!(r["result"]["rgb"], json!([100, 200, 30]));
        assert_eq!(
            h.pixel_reads.load(Ordering::SeqCst),
            2,
            "the phone-half hook ran for each probe"
        );
        // OCR: text only, no image, the path marked in `source`
        let r = c
            .tool(
                POCO,
                json!({ "tool": "look", "element": SEARCH(), "ocr": true }),
            )
            .await;
        assert_eq!(r["result"]["text"], "+2");
        assert_eq!(r["result"]["source"], "ocr");
        assert!(r["result"]["image_bytes"].is_null());
        let r = c
            .tool(POCO, json!({ "tool": "look", "element": SEARCH() }))
            .await;
        assert_eq!(r["result"]["image_bytes"], 321);
        // ocr without an element is invalid
        assert_eq!(
            code(&c.tool(POCO, json!({ "tool": "look", "ocr": true })).await),
            INVALID_PARAMS
        );
        let _ = &h.shutdown;
    });
}

#[test]
fn set_value_reads_back_and_a_canvas_control_is_left_to_the_phone() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        let r = c
            .tool(
                POCO,
                json!({ "tool": "set_value", "element": SEARCH(), "value": "hello" }),
            )
            .await;
        assert_eq!(r["result"]["value"], "hello", "{r}");
        assert!(r["result"]["text"]
            .as_str()
            .unwrap()
            .contains("read back \"hello\""));
        // the fake's DEL keys do nothing, so the second value is appended: the read-back differs and says so
        let r = c
            .tool(
                POCO,
                json!({ "tool": "set_value", "element": SEARCH(), "value": 7 }),
            )
            .await;
        assert_eq!(code(&r), DEVICE_ERROR);
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("read back"),
            "{r}"
        );
        assert_eq!(r["error"]["data"]["reason"], "read_back");
        // not a text field: the closed loop with calibration is the phone half
        let r = c
            .tool(
                POCO,
                json!({ "tool": "set_value", "element": { "desc": "Wi-Fi" }, "value": 3 }),
            )
            .await;
        assert_eq!(code(&r), DEVICE_ERROR);
        assert!(r["error"]["message"].as_str().unwrap().contains("CP-D"));
        // exactly one target
        assert_eq!(
            code(
                &c.tool(POCO, json!({ "tool": "set_value", "value": 1 }))
                    .await
            ),
            INVALID_PARAMS
        );
        assert_eq!(
            code(
                &c.tool(
                    POCO,
                    json!({ "tool": "set_value", "index": 0, "element": SEARCH(), "value": 1 })
                )
                .await
            ),
            INVALID_PARAMS
        );
        // an array is not a value
        assert_eq!(
            code(
                &c.tool(
                    POCO,
                    json!({ "tool": "set_value", "index": 0, "value": [1] })
                )
                .await
            ),
            INVALID_PARAMS
        );
    });
}

#[test]
fn read_only_refuses_every_acting_tool_and_reaches_nothing_but_reads_pass() {
    run(async {
        let h = start(DeviceMode::ReadOnly);
        let mut c = Client::leased(&h, POCO).await;
        h.device.lock().unwrap().calls.clear();
        let acting = [
            json!({ "tool": "tap", "x": 1, "y": 1, "space": "device" }),
            json!({ "tool": "long_press", "x": 1, "y": 1, "space": "device" }),
            json!({ "tool": "swipe", "x1": 1, "y1": 1, "x2": 2, "y2": 2, "space": "device" }),
            json!({ "tool": "scroll", "direction": "up" }),
            json!({ "tool": "type_text", "text": "x" }),
            json!({ "tool": "key", "name": "back" }),
            json!({ "tool": "unlock" }),
            json!({ "tool": "open_app", "package": "com.android.settings" }),
            json!({ "tool": "open_url", "uri": "https://example.com" }),
            json!({ "tool": "start_activity", "component": "com.android.settings/.Settings" }),
            json!({ "tool": "set_value", "element": SEARCH(), "value": "x" }),
            // the options do not get an acting call past the gate
            json!({ "tool": "tap", "x": 1, "y": 1, "space": "device", "require": [SEARCH()], "retry": 3, "verify": SEARCH() }),
            json!({ "tool": "set_value", "element": SEARCH(), "value": "x", "require": [SEARCH()], "retry": 3 }),
        ];
        for call in acting {
            let r = c.tool(POCO, call.clone()).await;
            assert_eq!(code(&r), REFUSED, "{call}: {r}");
            let msg = r["error"]["message"].as_str().unwrap();
            assert!(msg.contains("read_only"), "{msg}");
            assert!(
                msg.starts_with(call["tool"].as_str().unwrap()),
                "refusal names the tool the caller used: {msg}"
            );
            assert_eq!(health_of(&r)["mode"], "read_only");
            assert_eq!(
                r["error"]["data"]["attempts"], 1,
                "a refusal is never retried"
            );
        }
        {
            let d = h.device.lock().unwrap();
            assert!(d.acted.is_empty(), "nothing acted: {:?}", d.acted);
            assert!(
                d.calls.iter().all(|c| *c == "devices"),
                "a refusal reached the phone: {:?}",
                d.calls
            );
        }
        assert_eq!(h.tap_attempts.load(Ordering::SeqCst), 1, "the one plain tap reached core's gate and was refused there; the others were stopped before it");
        for call in [
            json!({ "tool": "screenshot" }),
            json!({ "tool": "list_apps" }),
            json!({ "tool": "pixel_probe", "x": 5, "y": 6 }),
            json!({ "tool": "wait", "seconds": 0 }),
            json!({ "tool": "look", "element": SEARCH(), "ocr": true }),
        ] {
            let r = c.tool(POCO, call.clone()).await;
            assert!(r.get("error").is_none(), "{call}: {r}");
            assert_eq!(health_of(&r)["mode"], "read_only");
        }
    });
}

#[test]
fn the_s24_is_refused_for_every_tool_and_no_health_probe_reaches_it() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, S24).await;
        h.device.lock().unwrap().hint = HealthHint {
            battery: Some(50),
            ..Default::default()
        };
        h.device.lock().unwrap().calls.clear();
        let tools = [
            json!({ "tool": "look" }),
            json!({ "tool": "screenshot" }),
            json!({ "tool": "ui_tree" }),
            json!({ "tool": "tap", "index": 0 }),
            json!({ "tool": "long_press", "index": 0 }),
            json!({ "tool": "swipe", "x1": 1, "y1": 1, "x2": 2, "y2": 2 }),
            json!({ "tool": "scroll", "direction": "up" }),
            json!({ "tool": "type_text", "text": "x" }),
            json!({ "tool": "key", "name": "home" }),
            json!({ "tool": "unlock" }),
            json!({ "tool": "open_app", "package": "x" }),
            json!({ "tool": "open_url", "uri": "https://x.y" }),
            json!({ "tool": "start_activity", "component": "a/.b" }),
            json!({ "tool": "list_apps" }),
            json!({ "tool": "getprop", "key": "ro.product.brand" }),
            json!({ "tool": "set_value", "index": 0, "value": 1 }),
            json!({ "tool": "pixel_probe", "x": 1, "y": 1 }),
            json!({ "tool": "look", "element": SEARCH(), "ocr": true }),
            json!({ "tool": "ui_tree", "require": [SEARCH()] }),
            json!({ "tool": "key", "name": "home", "require": [SEARCH()], "verify": SEARCH(), "retry": 3 }),
        ];
        for call in tools {
            let r = c.tool(S24, call.clone()).await;
            assert_eq!(code(&r), REFUSED, "{call}: {r}");
            assert!(
                {
                    let m = r["error"]["message"].as_str().unwrap();
                    m.contains("S24") || m.contains("protected phone")
                },
                "{r}"
            );
            let hl = health_of(&r);
            assert_eq!(hl["mode"], "read_only");
            assert!(
                hl.get("battery").is_none(),
                "the protected phone shows nothing the transport knew: {hl}"
            );
        }
        // `wait` touches nothing and is not a target call: still, no phone traffic and no probe
        c.tool(S24, json!({ "tool": "getprop", "key": "ro.serialno" }))
            .await;
        let d = h.device.lock().unwrap();
        assert!(
            d.acted.iter().all(|l| l.starts_with("getprop ro.serialno")),
            "only the approved read acted: {:?}",
            d.acted
        );
        assert!(
            d.calls.iter().all(|c| matches!(*c, "devices" | "shell")),
            "unexpected transport use on the protected phone: {:?}",
            d.calls
        );
        assert_eq!(
            d.hint_reads, 0,
            "the protected phone's health hint was never even read"
        );
    });
}

#[test]
fn doctor_push_and_pull_new_are_served_by_devtools_and_carry_the_health_member() {
    // After the A+B merge these three are no longer "planned" stubs: `devtools.rs` serves them (its own behaviour is
    // tested in tests/devtools.rs). What this file still pins is that their answers, ok or error, carry `health`.
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        for call in [
            json!({ "tool": "doctor", "diagnose": "dumpsys battery" }),
            json!({ "tool": "doctor" }),
            json!({ "tool": "push", "local": "a", "phone_path": "/sdcard/a" }),
            json!({ "tool": "pull_new", "dir": "/sdcard" }),
        ] {
            let r = c.tool(POCO, call.clone()).await;
            if r.get("error").is_some() {
                assert_ne!(code(&r), METHOD_NOT_FOUND, "{r}");
            }
            health_of(&r);
        }
    });
}

#[test]
fn require_gates_the_call_and_a_miss_does_nothing() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        // met: the tap runs
        let r = c.tool(POCO, json!({ "tool": "tap", "x": 100, "y": 650, "space": "device", "require": [{ "text": "Settings" }, SEARCH()] })).await;
        assert!(r.get("error").is_none(), "{r}");
        assert_eq!(h.device.lock().unwrap().acted.len(), 1);
        // missing: the error names it and nothing is tapped
        let r = c.tool(POCO, json!({ "tool": "tap", "x": 100, "y": 650, "space": "device", "require": [{ "text": "No such thing" }] })).await;
        assert_eq!(code(&r), DEVICE_ERROR);
        let m = r["error"]["message"].as_str().unwrap();
        assert!(
            m.contains("require failed") && m.contains("No such thing"),
            "{m}"
        );
        assert_eq!(r["error"]["data"]["reason"], "require");
        assert_eq!(
            h.device.lock().unwrap().acted.len(),
            1,
            "the tap did not run"
        );
        health_of(&r);
        // not retried: the screen is not a transport failure
        assert_eq!(r["error"]["data"]["attempts"], 1);
        let r = c.tool(POCO, json!({ "tool": "tap", "x": 1, "y": 1, "space": "device", "require": [{ "text": "No such thing" }], "retry": 3 })).await;
        assert_eq!(r["error"]["data"]["attempts"], 1);
        // ui_tree takes it natively
        let r = c
            .tool(
                POCO,
                json!({ "tool": "ui_tree", "require": [{ "text": "Nope" }] }),
            )
            .await;
        assert_eq!(code(&r), DEVICE_ERROR);
        let r = c
            .tool(
                POCO,
                json!({ "tool": "ui_tree", "require": [{ "text": "Settings" }] }),
            )
            .await;
        assert!(r["result"]["text"]
            .as_str()
            .unwrap()
            .contains("require met"));
        // bad selectors and too many
        for bad in [
            json!([{}]),
            json!([{ "colour": "red" }]),
            json!(["text"]),
            json!([{ "rel": [2, 0] }]),
            json!([{ "text": "" }]),
        ] {
            assert_eq!(
                code(
                    &c.tool(
                        POCO,
                        json!({ "tool": "key", "name": "back", "require": bad })
                    )
                    .await
                ),
                INVALID_PARAMS
            );
        }
        let nine: Vec<Value> = (0..9).map(|i| json!({ "text": format!("t{i}") })).collect();
        assert_eq!(
            code(
                &c.tool(
                    POCO,
                    json!({ "tool": "key", "name": "back", "require": nine })
                )
                .await
            ),
            INVALID_PARAMS
        );
        assert_eq!(h.device.lock().unwrap().acted.len(), 1);
    });
}

#[test]
fn retry_repeats_transport_failures_of_an_idempotent_call_up_to_three_and_then_gives_up() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        let open = |extra: Value| {
            let mut v = json!({ "tool": "open_app", "package": "com.android.settings" });
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            v
        };
        // no retry: one attempt, one error
        h.fail_opens.store(1, Ordering::SeqCst);
        let r = c.tool(POCO, open(json!({}))).await;
        assert_eq!(code(&r), DEVICE_ERROR);
        assert_eq!(r["error"]["data"]["attempts"], 1);
        assert_eq!(h.open_attempts.load(Ordering::SeqCst), 1);
        // two failures then success with retry 2: three attempts, success
        h.open_attempts.store(0, Ordering::SeqCst);
        h.fail_opens.store(2, Ordering::SeqCst);
        let r = c.tool(POCO, open(json!({ "retry": 2 }))).await;
        assert_eq!(r["result"]["attempts"], 3, "{r}");
        assert_eq!(h.open_attempts.load(Ordering::SeqCst), 3);
        // exhaustion: always failing, retry 3 -> 4 attempts, then the error says so
        h.open_attempts.store(0, Ordering::SeqCst);
        h.fail_opens.store(100, Ordering::SeqCst);
        let r = c.tool(POCO, open(json!({ "retry": 3 }))).await;
        assert_eq!(code(&r), DEVICE_ERROR);
        assert_eq!(
            h.open_attempts.load(Ordering::SeqCst),
            4,
            "1 try + 3 retries, no more"
        );
        assert_eq!(r["error"]["data"]["attempts"], 4);
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("gave up after 4 attempts"),
            "{r}"
        );
        assert!(
            r["error"]["data"].get("not_retried").is_none(),
            "open_app is idempotent: it was retried"
        );
        health_of(&r);
        // more than three is refused before anything runs
        h.open_attempts.store(0, Ordering::SeqCst);
        let r = c.tool(POCO, open(json!({ "retry": 4 }))).await;
        assert_eq!(code(&r), INVALID_PARAMS);
        assert!(r["error"]["message"].as_str().unwrap().contains("0 to 3"));
        assert_eq!(
            h.open_attempts.load(Ordering::SeqCst),
            0,
            "retry 4 never reached the backend"
        );
        assert_eq!(
            code(&c.tool(POCO, open(json!({ "retry": -1 }))).await),
            INVALID_PARAMS
        );
    });
}

#[test]
fn retry_never_repeats_a_call_that_could_act_twice() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        // a tap that fails on the transport may still have landed: one attempt, however much retry is asked for
        h.fail_taps.store(100, Ordering::SeqCst);
        for call in [
            json!({ "tool": "tap", "x": 100, "y": 650, "space": "device", "retry": 3 }),
            json!({ "tool": "key", "name": "back", "retry": 3 }),
            json!({ "tool": "type_text", "text": "hi", "retry": 3 }),
        ] {
            h.tap_attempts.store(0, Ordering::SeqCst);
            let name = call["tool"].as_str().unwrap().to_string();
            let r = c.tool(POCO, call).await;
            if name == "tap" {
                assert_eq!(code(&r), DEVICE_ERROR, "{r}");
                assert_eq!(r["error"]["data"]["attempts"], 1, "{r}");
                assert_eq!(
                    h.tap_attempts.load(Ordering::SeqCst),
                    1,
                    "the tap was sent once, not four times"
                );
                assert_eq!(r["error"]["data"]["not_retried"], "tap is not idempotent");
                assert!(
                    r["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("not retried"),
                    "{r}"
                );
                health_of(&r);
            } else {
                assert!(
                    r.get("error").is_none(),
                    "{name} is not a tap, so it is not failed here: {r}"
                );
            }
        }
        assert_eq!(
            h.device.lock().unwrap().acted.len(),
            2,
            "only the key and the text reached the phone"
        );
    });
}

#[test]
fn verify_checks_the_screen_after_and_a_failed_verify_is_an_error_that_retry_repeats_only_for_a_read(
) {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "tap", "x": 100, "y": 650, "space": "device", "verify": { "text": "Settings" } })).await;
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .ends_with("verify: met (the screen changed)"),
            "the tap focused the search field: {r}"
        );
        // verify v2: the same tap again changes nothing (the field is focused already), so even though the
        // expected element is on screen the verify fails and says the screen did not change
        let r = c.tool(POCO, json!({ "tool": "tap", "x": 100, "y": 650, "space": "device", "verify": { "text": "Settings" } })).await;
        assert_eq!(code(&r), DEVICE_ERROR, "{r}");
        assert_eq!(r["error"]["data"]["reason"], "verify");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("did not change"),
            "{r}"
        );
        // a failed verify: the tap ran, and the error says the screen does not show the element
        let acted = h.device.lock().unwrap().acted.len();
        let r = c.tool(POCO, json!({ "tool": "tap", "x": 100, "y": 650, "space": "device", "verify": { "text": "Done!" } })).await;
        assert_eq!(code(&r), DEVICE_ERROR);
        assert_eq!(r["error"]["data"]["reason"], "verify");
        let m = r["error"]["message"].as_str().unwrap();
        assert!(m.contains("verify failed") && m.contains("Done!"), "{m}");
        assert_eq!(h.device.lock().unwrap().acted.len(), acted + 1);
        health_of(&r);
        // a tap whose verify failed ran, and may have toggled: retry does NOT tap it again
        let acted = h.device.lock().unwrap().acted.len();
        let r = c.tool(POCO, json!({ "tool": "tap", "x": 100, "y": 650, "space": "device", "verify": { "text": "Done!" }, "retry": 2 })).await;
        assert_eq!(r["error"]["data"]["attempts"], 1);
        assert_eq!(r["error"]["data"]["not_retried"], "tap is not idempotent");
        assert_eq!(h.device.lock().unwrap().acted.len(), acted + 1);
        // a read with a failing verify is repeated: three reads, then it gives up
        let r = c.tool(POCO, json!({ "tool": "getprop", "key": "ro.x", "verify": { "text": "Done!" }, "retry": 2 })).await;
        assert_eq!(r["error"]["data"]["attempts"], 3);
        // a verify on a read, and a malformed verify
        let r = c
            .tool(
                POCO,
                json!({ "tool": "getprop", "key": "ro.x", "verify": { "text": "Settings" } }),
            )
            .await;
        assert!(r.get("error").is_none());
        assert_eq!(
            code(
                &c.tool(POCO, json!({ "tool": "key", "name": "back", "verify": {} }))
                    .await
            ),
            INVALID_PARAMS
        );
    });
}

#[test]
fn verify_v2_reads_nothing_for_a_refused_tap_and_rereads_typed_text_at_two_delays() {
    // a refused tap: the gate comes before the fingerprint read
    run(async {
        let h = start(DeviceMode::ReadOnly);
        let mut c = Client::leased(&h, POCO).await;
        let reads = |h: &Harness| {
            h.device
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|c| **c == "tree_xml")
                .count()
        };
        let before = reads(&h);
        let r = c.tool(POCO, json!({ "tool": "tap", "x": 100, "y": 650, "space": "device", "verify": { "text": "Settings" } })).await;
        assert_eq!(code(&r), REFUSED, "{r}");
        assert_eq!(reads(&h), before, "a refused call reads nothing");
    });
    // typed text that stays: found at once, then again 150 and 600 ms later
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        let r = c
            .tool(
                POCO,
                json!({ "tool": "tap", "x": 100, "y": 650, "space": "device" }),
            )
            .await;
        assert!(r.get("error").is_none(), "{r}");
        let t0 = h.waited_ms.load(Ordering::SeqCst);
        let r = c
            .tool(
                POCO,
                json!({ "tool": "type_text", "text": "hi", "verify": { "text": "hi" } }),
            )
            .await;
        assert!(r.get("error").is_none(), "{r}");
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .ends_with("verify: met (still there 150 and 600 ms later)"),
            "{r}"
        );
        assert_eq!(
            h.waited_ms.load(Ordering::SeqCst) - t0,
            600,
            "the two re-reads are 150 and 600 ms after the first sighting"
        );
    });
    // a keyboard that rewrites the field after the first read: the verify fails and says when
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        c.tool(
            POCO,
            json!({ "tool": "tap", "x": 100, "y": 650, "space": "device" }),
        )
        .await;
        h.rewrite_field.store(1, Ordering::SeqCst);
        let r = c.tool(POCO, json!({ "tool": "type_text", "text": "hi", "verify": { "text": "hi" }, "retry": 2 })).await;
        assert_eq!(code(&r), DEVICE_ERROR, "{r}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not 150 ms later"),
            "{r}"
        );
        assert_eq!(
            r["error"]["data"]["attempts"], 1,
            "type_text is never repeated: {r}"
        );
    });
}

#[test]
fn health_comes_from_the_call_path_with_no_second_probe() {
    run(async {
        let h = start(DeviceMode::Full);
        h.daemon.set_alias(POCO, "pocket");
        let mut c = Client::leased(&h, POCO).await;
        // nothing known yet: identity only, never a guess
        let r = c
            .tool(
                POCO,
                json!({ "tool": "getprop", "key": "ro.product.model" }),
            )
            .await;
        let hl = health_of(&r);
        assert_eq!(hl["alias"], "pocket");
        assert_eq!(hl["model"], "23113RKC6G");
        assert_eq!(hl["mode"], "full");
        for k in [
            "battery",
            "powered",
            "temp_c",
            "lock",
            "foreground",
            "age_s",
        ] {
            assert!(hl.get(k).is_none(), "{k} was invented: {hl}");
        }
        // what the transport's replies carried is passed on
        h.device.lock().unwrap().hint = HealthHint {
            battery: Some(63),
            powered: Some(true),
            temp_c: Some(31.5),
            lock: Some("unlocked".into()),
            foreground_package: Some("com.android.settings".into()),
            foreground_title: Some("Wi-Fi".into()),
            age_s: 4.0,
            ..Default::default()
        };
        let before = h.device.lock().unwrap().calls.clone();
        let r = c
            .tool(
                POCO,
                json!({ "tool": "getprop", "key": "ro.product.model" }),
            )
            .await;
        let hl = health_of(&r).clone();
        assert_eq!(hl["battery"], 63);
        assert_eq!(hl["powered"], true);
        assert_eq!(hl["temp_c"], 31.5);
        assert_eq!(hl["lock"], "unlocked");
        assert_eq!(
            hl["foreground"],
            json!({ "package": "com.android.settings", "title": "Wi-Fi" })
        );
        assert_eq!(hl["age_s"], 4.0);
        // no second probe: the call added exactly the commands getprop itself needs (device list for the
        // lookup and the pick, one shell line) and health added nothing
        let after = h.device.lock().unwrap().calls.clone();
        let added: Vec<&str> = after[before.len()..].to_vec();
        assert!(
            added.iter().all(|c| matches!(*c, "devices" | "shell")),
            "extra phone traffic: {added:?}"
        );
        assert_eq!(added.iter().filter(|c| **c == "shell").count(), 1);
        assert!(h.device.lock().unwrap().hint_reads >= 1);
        // a look reads the screen state itself, and the header says so
        h.device.lock().unwrap().hint = HealthHint::default();
        h.device.lock().unwrap().asleep = true;
        let r = c.tool(POCO, json!({ "tool": "look" })).await;
        assert_eq!(health_of(&r)["screen"], "off");
        h.device.lock().unwrap().asleep = false;
        let r = c.tool(POCO, json!({ "tool": "ui_tree" })).await;
        assert_eq!(health_of(&r)["screen"], "on");
        // an unknown tool is rejected before a device is chosen: no health there, and nothing reached the phone
        let r = c.tool(POCO, json!({ "tool": "format_disk" })).await;
        assert_eq!(code(&r), INVALID_PARAMS);
        assert!(r["error"]["data"].is_null());
    });
}

#[test]
fn a_refusal_carries_the_health_member() {
    run(async {
        let h = start(DeviceMode::ReadOnly);
        let mut c = Client::leased(&h, POCO).await;
        h.device.lock().unwrap().hint = HealthHint {
            battery: Some(40),
            powered: Some(false),
            age_s: 2.0,
            ..Default::default()
        };
        let r = c
            .tool(
                POCO,
                json!({ "tool": "tap", "x": 1, "y": 1, "space": "device" }),
            )
            .await;
        assert_eq!(code(&r), REFUSED);
        let hl = health_of(&r);
        assert_eq!(hl["battery"], 40);
        assert_eq!(hl["powered"], false);
        assert_eq!(hl["mode"], "read_only");
        assert_eq!(
            r["error"]["data"]["tool"], "tap",
            "the refusal's own data is kept"
        );
    });
}

#[test]
fn health_is_on_every_answer_that_names_a_device_you_hold() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        let requests = [
            json!({ "tool": "look" }),
            json!({ "tool": "ui_tree", "interactive_only": true }),
            json!({ "tool": "tap" }),
            json!({ "tool": "tap", "index": 99 }),
            json!({ "tool": "key", "name": "no_such_key" }),
            json!({ "tool": "getprop", "key": "bad key;" }),
            json!({ "tool": "open_app", "package": "no.such.pkg" }),
            json!({ "tool": "scroll", "direction": "sideways" }),
            json!({ "tool": "wait", "seconds": 0 }),
            json!({ "tool": "tap", "x": 1, "y": 1, "retry": 9 }),
            json!({ "tool": "tap", "x": 1, "y": 1, "require": 5 }),
            json!({ "tool": "doctor" }),
            json!({ "tool": "push", "local": "a", "phone_path": "b" }),
            json!({ "tool": "pull_new", "dir": "d" }),
            json!({ "tool": "pixel_probe", "x": 1, "y": 1 }),
            json!({ "tool": "set_value", "index": 0, "value": 1 }),
        ];
        let mut n = 0;
        for call in requests {
            let r = c.tool(POCO, call.clone()).await;
            // `require: 5` fails to deserialize, which happens before the device is looked at: no health there
            if call["require"] == json!(5) {
                assert_eq!(code(&r), INVALID_PARAMS);
                continue;
            }
            health_of(&r);
            n += 1;
        }
        assert_eq!(n, 15);
        // a call by someone who does not hold the lease is refused before anything: that error's data is the lease
        let mut other = Client::new(&h);
        let r = other.tool(POCO, json!({ "tool": "look" })).await;
        assert_eq!(code(&r), NOT_HOLDER);
        assert!(r["error"]["data"].get("health").is_none());
    });
}

#[test]
fn require_and_verify_wait_with_the_backend_lock_free() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        // a require that never comes true: it waits its 5 s on the clock (virtual here), reading again each 200 ms
        let r = c.tool(POCO, json!({ "tool": "tap", "x": 1, "y": 1, "space": "device", "require": [{ "text": "No such thing" }] })).await;
        assert_eq!(code(&r), DEVICE_ERROR);
        let m = r["error"]["message"].as_str().unwrap();
        assert!(
            m.contains("require failed") && m.contains("No such thing") && m.contains("5000 ms"),
            "{m}"
        );
        let (all, free) = (
            h.sleeps.load(Ordering::SeqCst),
            h.sleeps_unlocked.load(Ordering::SeqCst),
        );
        assert_eq!(all, 25, "5000 ms in 200 ms pauses");
        assert_eq!(free, all, "every pause happened with the backend lock free");
        assert_eq!(h.waited_ms.load(Ordering::SeqCst), 5000);
        // verify waits 2 s the same way
        h.waited_ms.store(0, Ordering::SeqCst);
        h.sleeps.store(0, Ordering::SeqCst);
        h.sleeps_unlocked.store(0, Ordering::SeqCst);
        let r = c
            .tool(
                POCO,
                json!({ "tool": "key", "name": "back", "verify": { "text": "Done!" } }),
            )
            .await;
        assert_eq!(code(&r), DEVICE_ERROR);
        assert_eq!(h.sleeps.load(Ordering::SeqCst), 10);
        assert_eq!(h.sleeps_unlocked.load(Ordering::SeqCst), 10);
        // a require that is met at once does not wait at all
        h.sleeps.store(0, Ordering::SeqCst);
        let r = c
            .tool(
                POCO,
                json!({ "tool": "ui_tree", "require": [{ "text": "Settings" }] }),
            )
            .await;
        assert!(r["result"]["text"]
            .as_str()
            .unwrap()
            .contains("require met"));
        assert_eq!(h.sleeps.load(Ordering::SeqCst), 0);
    });
}

/// A list: `rows` rows from item `first`, each a clickable row with a title and a "Star" button (it was "Delete" before D3: the screen guard now asks before any Delete), `top` px down.
fn list_screen(first: usize, rows: usize, top: i32) -> String {
    let mut x = String::from("<hierarchy rotation=\"0\"><node class=\"androidx.recyclerview.widget.RecyclerView\" resource-id=\"a:id/list\" scrollable=\"true\" bounds=\"[0,0][1000,2000]\">");
    for (n, k) in (first..first + rows).enumerate() {
        let t = top + 300 * n as i32;
        x += &format!(
            "<node class=\"a.Row\" resource-id=\"a:id/row\" clickable=\"true\" bounds=\"[0,{t}][1000,{}]\"><node class=\"a.T\" text=\"Item {k}\" bounds=\"[0,{t}][500,{}]\"/><node class=\"a.B\" text=\"Star\" resource-id=\"a:id/star\" clickable=\"true\" bounds=\"[600,{t}][900,{}]\"/></node>",
            t + 280,
            t + 100,
            t + 100
        );
    }
    x + "</node></hierarchy>"
}

/// The ref printed at the end of the first listing line that contains `needle`.
fn ref_on_line(text: &str, needle: &str) -> String {
    let l = text
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?} in {text}"));
    l.rsplit(' ').next().unwrap().to_string()
}

#[test]
fn a_ref_from_the_tree_taps_the_same_row_after_a_scroll_and_since_returns_only_the_difference() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        h.device.lock().unwrap().xml_override = Some(list_screen(1, 6, 0));
        let r = c.tool(POCO, json!({ "tool": "ui_tree" })).await;
        let full = r["result"]["text"].as_str().unwrap().to_string();
        assert!(full.contains("snapshot s1"), "{full}");
        // the Star button of Item 3 (the third row's button), by its ref
        let del3 = full
            .lines()
            .filter(|l| l.contains("id=star"))
            .nth(2)
            .unwrap()
            .rsplit(' ')
            .next()
            .unwrap()
            .to_string();
        // the list scrolls: item 3 is now the first row, higher up the screen
        h.device.lock().unwrap().xml_override = Some(list_screen(3, 6, -80));
        let acted = h.device.lock().unwrap().acted.len();
        let r = c.tool(POCO, json!({ "tool": "tap", "ref": del3 })).await;
        assert!(r.get("error").is_none(), "{r}");
        let t = r["result"]["text"].as_str().unwrap();
        assert!(t.contains("matched by ref="), "{t}");
        assert!(
            t.contains("tapped device (750,"),
            "the button is at x 750 and the row is now first: {t}"
        );
        assert_eq!(h.device.lock().unwrap().acted.len(), acted + 1);
        // the same ref is gone once the row has scrolled out of the dump: nothing is touched
        h.device.lock().unwrap().xml_override = Some(list_screen(5, 6, 0));
        let r = c.tool(POCO, json!({ "tool": "tap", "ref": del3 })).await;
        assert_eq!(code(&r), INVALID_PARAMS, "{r}");
        assert_eq!(h.device.lock().unwrap().acted.len(), acted + 1);
        // since: read the new screen against s1 and get only the difference
        let r = c
            .tool(POCO, json!({ "tool": "ui_tree", "since": "s1" }))
            .await;
        let d = r["result"]["text"].as_str().unwrap();
        assert!(d.contains("(tree from") && d.contains("since s1:"), "{d}");
        assert!(
            d.lines()
                .any(|l| l.starts_with("+ ") && l.contains("Item 10")),
            "{d}"
        );
        assert!(
            d.lines()
                .any(|l| l.starts_with("- ") && l.contains("Item 1")),
            "{d}"
        );
        // a one-row scroll of a long list is a small diff next to the full tree
        h.device.lock().unwrap().xml_override = Some(list_screen(2, 30, 0));
        let long = c.tool(POCO, json!({ "tool": "ui_tree" })).await["result"]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let sid = long
            .split("snapshot ")
            .nth(1)
            .unwrap()
            .split(')')
            .next()
            .unwrap()
            .to_string();
        h.device.lock().unwrap().xml_override = Some(list_screen(3, 30, 0));
        let r = c
            .tool(POCO, json!({ "tool": "ui_tree", "since": sid }))
            .await;
        let small = r["result"]["text"].as_str().unwrap();
        assert!(
            small.len() * 100 <= long.len() * 30,
            "diff {} chars vs full {} chars",
            small.len(),
            long.len()
        );
        // look takes since too
        let r = c.tool(POCO, json!({ "tool": "look", "since": "s1" })).await;
        assert!(
            r["result"]["text"].as_str().unwrap().contains("since s1:"),
            "{r}"
        );
        // an unknown snapshot is an error; since with require is refused
        assert_eq!(
            code(
                &c.tool(POCO, json!({ "tool": "ui_tree", "since": "s99" }))
                    .await
            ),
            INVALID_PARAMS
        );
        assert_eq!(
            code(
                &c.tool(
                    POCO,
                    json!({ "tool": "ui_tree", "since": "s1", "require": [{ "text": "Item 5" }] })
                )
                .await
            ),
            INVALID_PARAMS
        );
        // a ref also works as a selector in require / verify / set_value's element
        let now = c.tool(POCO, json!({ "tool": "ui_tree" })).await;
        let del = now["result"]["text"]
            .as_str()
            .unwrap()
            .lines()
            .find(|l| l.contains("id=star"))
            .unwrap()
            .rsplit(' ')
            .next()
            .unwrap()
            .to_string();
        let r = c
            .tool(
                POCO,
                json!({ "tool": "key", "name": "back", "verify": { "ref": del } }),
            )
            .await;
        assert!(r.get("error").is_none(), "{r}");
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .contains("verify: met"),
            "{r}"
        );
        // a verify by a ref that is not on the screen fails, and a looser key beside it does not rescue it
        let r = c.tool(POCO, json!({ "tool": "key", "name": "back", "verify": { "ref": "r000000", "text": "Item 5" } })).await;
        assert!(r.get("error").is_some(), "{r}");
        // ref and index together are refused
        assert_eq!(
            code(
                &c.tool(POCO, json!({ "tool": "tap", "ref": "r000001", "index": 0 }))
                    .await
            ),
            INVALID_PARAMS
        );
    });
}

#[test]
fn type_text_by_ref_and_the_modes_still_gate_a_ref_tap() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        h.device.lock().unwrap().xml_override = Some(
            "<hierarchy rotation=\"0\"><node class=\"android.widget.EditText\" resource-id=\"a:id/q\" clickable=\"true\" bounds=\"[0,0][1000,100]\"/></hierarchy>".to_string(),
        );
        let r = c.tool(POCO, json!({ "tool": "ui_tree" })).await;
        let q = ref_on_line(r["result"]["text"].as_str().unwrap(), "EditText");
        let acted = h.device.lock().unwrap().acted.len();
        let r = c
            .tool(
                POCO,
                json!({ "tool": "type_text", "text": "hello", "ref": q }),
            )
            .await;
        assert!(r.get("error").is_none(), "{r}");
        assert!(h.device.lock().unwrap().acted.len() > acted, "typed");
        assert_eq!(
            code(
                &c.tool(
                    POCO,
                    json!({ "tool": "type_text", "text": "x", "ref": "r000001", "index": 0 })
                )
                .await
            ),
            INVALID_PARAMS
        );
        assert_eq!(
            code(
                &c.tool(
                    POCO,
                    json!({ "tool": "type_text", "text": "x", "ref": "r000001" })
                )
                .await
            ),
            INVALID_PARAMS
        );
    });
    // a read_only phone answers the read (ui_tree with since) and refuses the tap by ref
    run(async {
        let h = start(DeviceMode::ReadOnly);
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "ui_tree" })).await;
        assert!(r.get("error").is_none(), "{r}");
        let r = c
            .tool(POCO, json!({ "tool": "ui_tree", "since": "s1" }))
            .await;
        assert!(r.get("error").is_none(), "{r}");
        let r = c
            .tool(POCO, json!({ "tool": "tap", "ref": "r000001" }))
            .await;
        assert_eq!(code(&r), REFUSED, "{r}");
    });
}
