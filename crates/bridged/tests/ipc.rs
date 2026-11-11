//! IPC tests over the in-memory transport. Fakes only: no adb, no device, no pipe.

use agentroid_core::fake::{FakeClock, FakeDevice};
use agentroid_core::gate::DeviceMode;
use agentroid_core::transport::{
    Backend, Core, DeviceInfo, Input, Screenshot, Transport, TransportError,
};
use bridged::listener::{duplex_listener, DuplexConnector};
use bridged::proto::*;
use bridged::registry::Registry;
use bridged::server::{self, CoreBackend, Daemon};
use serde_json::{json, Value};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::watch;
use tokio::task::JoinHandle;

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

/// A fake device the test can still inspect after the daemon owns the transport.
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
}

struct Harness {
    connector: DuplexConnector,
    shutdown: watch::Sender<bool>,
    daemon: Arc<Daemon>,
    device: Arc<Mutex<FakeDevice>>,
    task: JoinHandle<std::io::Result<()>>,
}

fn start(registry: Registry, poco_mode: DeviceMode) -> Harness {
    let device = Arc::new(Mutex::new(FakeDevice::new()));
    let mut backend = CoreBackend::new(Core::new(Shared(device.clone()), FakeClock::new()));
    backend.set_mode(POCO, poco_mode);
    let daemon = Daemon::new(registry, Box::new(backend));
    let (listener, connector) = duplex_listener();
    let (shutdown, rx) = watch::channel(false);
    let task = tokio::spawn(server::serve(daemon.clone(), listener, rx));
    Harness {
        connector,
        shutdown,
        daemon,
        device,
        task,
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
    async fn named(h: &Harness, session: &str) -> Client {
        let mut c = Client::new(h);
        let r = c.call("hello", json!({ "session": session })).await;
        assert_eq!(r["result"]["holder"], session);
        c
    }
    async fn send_raw(&mut self, text: &str) {
        self.w.write_all(text.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
    }
    async fn read(&mut self) -> Value {
        let mut line = String::new();
        let n = self.r.read_line(&mut line).await.unwrap();
        assert!(n > 0, "connection closed while waiting for a response");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad response {line:?}: {e}"))
    }
    async fn raw(&mut self, text: &str) -> Value {
        self.send_raw(text).await;
        self.read().await
    }
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let r = self
            .raw(
                &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
                    .to_string(),
            )
            .await;
        assert_eq!(r["id"], id);
        r
    }
}

fn code(r: &Value) -> i64 {
    r["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected an error, got {r}"))
}

fn tap_point() -> Value {
    json!({ "device": POCO, "call": { "tool": "tap", "x": 100, "y": 650, "space": "device" } })
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn hello_and_devices_list() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::Full);
        let mut a = Client::new(&h);
        let r = a.call("hello", json!({ "session": "claude-a" })).await;
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["result"]["name"], "bridged");
        assert_eq!(r["result"]["holder"], "claude-a");
        let caps: Vec<&str> = r["result"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        for m in METHODS {
            assert!(caps.contains(&m), "{m} missing from capabilities");
        }
        let r = a.call("devices.list", json!({})).await;
        let rows = r["result"]["devices"].as_array().unwrap();
        let by = |k: &str| {
            rows.iter()
                .find(|d| d["device"] == k)
                .unwrap_or_else(|| panic!("{k} not listed"))
                .clone()
        };
        assert_eq!(by(POCO)["mode"], "full");
        assert_eq!(
            by(S24)["mode"],
            "read_only",
            "the protected phone is always read_only"
        );
        assert!(by(POCO)["holder"].is_null());
        assert_eq!(
            by(POCO)["allowed"],
            true,
            "a device on the allowlist says so"
        );
        assert_eq!(
            by(S24)["allowed"],
            true,
            "the protected phone is leasable (read_only), so it is not flagged as refused"
        );
        a.call("lease.acquire", json!({ "device": POCO })).await;
        let r = a.call("devices.list", json!({})).await;
        let poco = r["result"]["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["device"] == POCO)
            .unwrap()
            .clone();
        assert_eq!(poco["holder"], "claude-a");
    });
}

#[test]
fn two_clients_one_device_second_acquire_is_refused_naming_the_holder() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        let mut b = Client::named(&h, "claude-b").await;

        let r = a.call("lease.acquire", json!({ "device": POCO })).await;
        assert_eq!(
            r["result"],
            json!({ "device": POCO, "holder": "claude-a", "epoch": 1 }),
            "the record is exactly {{device, holder, epoch}}"
        );

        let r = b.call("lease.acquire", json!({ "device": POCO })).await;
        assert_eq!(code(&r), LEASE_HELD);
        assert!(
            r["error"]["message"].as_str().unwrap().contains("claude-a"),
            "{r}"
        );
        assert_eq!(
            r["error"]["data"],
            json!({ "device": POCO, "holder": "claude-a", "epoch": 1 })
        );

        // The adb serial names the same device.
        let r = b
            .call("lease.acquire", json!({ "device": "192.0.2.50:37123" }))
            .await;
        assert_eq!(code(&r), LEASE_HELD);

        // B can see the lease, but cannot drive or release the device.
        let r = b.call("lease.status", json!({ "device": POCO })).await;
        assert_eq!(r["result"]["leases"][0]["holder"], "claude-a");
        assert_eq!(code(&b.call("device.call", tap_point()).await), NOT_HOLDER);
        assert_eq!(
            code(&b.call("lease.release", json!({ "device": POCO })).await),
            NOT_HOLDER
        );
        assert!(
            h.device.lock().unwrap().acted.is_empty(),
            "B's refused tap reached the phone"
        );

        // A drives the shared device through the path: tree read, look, tap.
        let r = a
            .call(
                "device.call",
                json!({ "device": POCO, "call": { "tool": "ui_tree" } }),
            )
            .await;
        assert!(
            r["result"]["text"].as_str().unwrap().contains("Settings"),
            "{r}"
        );
        let r = a
            .call(
                "device.call",
                json!({ "device": POCO, "call": { "tool": "look" } }),
            )
            .await;
        assert_eq!(r["result"]["tool"], "look");
        assert!(r["result"]["image_bytes"].as_u64().unwrap() > 0);
        let r = a.call("device.call", tap_point()).await;
        assert!(
            r["result"]["text"].as_str().unwrap().starts_with("tapped"),
            "{r}"
        );
        assert_eq!(h.device.lock().unwrap().acted.len(), 1);

        // Same label, different connection: still a different holder.
        let mut b2 = Client::named(&h, "claude-a").await;
        assert_eq!(
            code(&b2.call("lease.acquire", json!({ "device": POCO })).await),
            LEASE_HELD
        );

        // Release hands it over, with a higher epoch.
        assert_eq!(
            a.call("lease.release", json!({ "device": POCO })).await["result"]["released"],
            true
        );
        let r = b.call("lease.acquire", json!({ "device": POCO })).await;
        assert_eq!(
            r["result"],
            json!({ "device": POCO, "holder": "claude-b", "epoch": 2 })
        );
        assert_eq!(
            code(&a.call("device.call", tap_point()).await),
            NOT_HOLDER,
            "A lost the device"
        );
    });
}

#[test]
fn a_client_killed_mid_call_only_ends_its_own_connection() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        let mut b = Client::named(&h, "claude-b").await;
        assert_eq!(
            a.call("lease.acquire", json!({ "device": POCO })).await["result"]["epoch"],
            1
        );
        assert_eq!(
            code(&b.call("lease.acquire", json!({ "device": POCO })).await),
            LEASE_HELD
        );

        // A fires a call and vanishes before reading the answer (the daemon's write hits a dead pipe),
        // with a torn half-request behind it.
        a.send_raw(
            &json!({ "jsonrpc": "2.0", "id": 99, "method": "device.call", "params": tap_point() })
                .to_string(),
        )
        .await;
        a.w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":100,\"meth")
            .await
            .unwrap();
        drop(a);

        // B keeps getting answers straight away...
        let r = b.call("devices.list", json!({})).await;
        assert_eq!(r["result"]["devices"].as_array().unwrap().len(), 2);
        let r = b.call("hello", json!({})).await;
        assert_eq!(r["result"]["holder"], "claude-b");

        // ...and acquires as soon as the daemon has noticed A is gone.
        let mut got = None;
        for _ in 0..200 {
            let r = b.call("lease.acquire", json!({ "device": POCO })).await;
            if r.get("result").is_some() {
                got = Some(r);
                break;
            }
            assert_eq!(code(&r), LEASE_HELD);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let r = got.expect("A's lease was never released after A vanished");
        assert_eq!(
            r["result"],
            json!({ "device": POCO, "holder": "claude-b", "epoch": 2 })
        );

        // The daemon still accepts new clients.
        let mut c = Client::named(&h, "claude-c").await;
        assert_eq!(
            c.call("lease.status", json!({})).await["result"]["leases"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    });
}

#[test]
fn epoch_survives_a_restart_and_never_goes_backwards() {
    run(async {
        let dir = tmp_dir("epoch-restart");
        let state = dir.join("state.json");
        let mut seen = vec![];
        for round in 0..3 {
            let h = start(Registry::open(&state).unwrap(), DeviceMode::Full);
            let mut a = Client::named(&h, "claude-a").await;
            for _ in 0..2 {
                let r = a.call("lease.acquire", json!({ "device": POCO })).await;
                seen.push(r["result"]["epoch"].as_u64().unwrap());
                a.call("lease.release", json!({ "device": POCO })).await;
            }
            // Stop the daemon: graceful.
            h.shutdown.send(true).unwrap();
            h.task.await.unwrap().unwrap();
            assert_eq!(h.daemon.registry.epoch(), 2 * (round + 1));
        }
        assert_eq!(
            seen,
            vec![1, 2, 3, 4, 5, 6],
            "strictly increasing across restarts"
        );
        assert!(seen.windows(2).all(|w| w[0] < w[1]));
        let text = std::fs::read_to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["epoch"], 6);
        assert!(
            !dir.join("state.json.tmp").exists(),
            "temp file renamed away"
        );
    });
}

#[test]
fn a_corrupt_state_file_is_an_error_not_a_reset_to_zero() {
    let dir = tmp_dir("epoch-corrupt");
    let state = dir.join("state.json");
    std::fs::write(&state, b"{ nope").unwrap();
    assert!(Registry::open(&state).is_err());
    std::fs::write(&state, b"{\"epoch\":-3}").unwrap();
    assert!(Registry::open(&state).is_err());
    std::fs::write(&state, b"{\"epoch\":41}").unwrap();
    let r = Registry::open(&state).unwrap();
    assert_eq!(r.acquire("D", 1, "a").unwrap().epoch, 42);
}

#[test]
fn the_s24_refuses_tap_even_with_a_lease() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        let r = a.call("lease.acquire", json!({ "device": S24 })).await;
        assert_eq!(r["result"]["device"], S24, "a lease alone grants nothing");
        let r = a.call("device.call", json!({ "device": S24, "call": { "tool": "tap", "x": 10, "y": 10, "space": "device" } })).await;
        assert_eq!(code(&r), REFUSED, "{r}");
        let m = r["error"]["message"].as_str().unwrap();
        assert!(m.contains("S24") || m.contains("protected phone"), "{r}");
        let r = a
            .call(
                "device.call",
                json!({ "device": S24, "call": { "tool": "tap", "index": 0 } }),
            )
            .await;
        assert_eq!(code(&r), REFUSED);
        let r = a
            .call(
                "device.call",
                json!({ "device": S24, "call": { "tool": "look" } }),
            )
            .await;
        assert_eq!(code(&r), REFUSED, "only the ruled serial read is answered");
        let r = a
            .call(
                "device.call",
                json!({ "device": S24, "call": { "tool": "getprop", "key": "ro.product.brand" } }),
            )
            .await;
        assert_eq!(code(&r), REFUSED, "{r}");
        {
            let d = h.device.lock().unwrap();
            assert!(d.acted.is_empty(), "nothing reached any device");
            assert!(
                !d.touched(),
                "not even a read reached the transport beyond listing: {:?}",
                d.calls
            );
        }
        // the serial read passes the gate (whatever the fake answers)
        let r = a
            .call(
                "device.call",
                json!({ "device": S24, "call": { "tool": "getprop", "key": "ro.serialno" } }),
            )
            .await;
        assert!(r["error"]["code"].as_i64() != Some(REFUSED), "{r}");
    });
}

#[test]
fn a_read_only_device_refuses_acting_calls_but_answers_reads() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::ReadOnly);
        let mut a = Client::named(&h, "claude-a").await;
        let r = a.call("devices.list", json!({})).await;
        assert_eq!(
            r["result"]["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["device"] == POCO)
                .unwrap()["mode"],
            "read_only"
        );
        a.call("lease.acquire", json!({ "device": POCO })).await;
        assert_eq!(code(&a.call("device.call", tap_point()).await), REFUSED);
        assert_eq!(
            code(
                &a.call(
                    "device.call",
                    json!({ "device": POCO, "call": { "tool": "tap", "index": 0 } })
                )
                .await
            ),
            REFUSED
        );
        assert!(h.device.lock().unwrap().acted.is_empty());
        let r = a
            .call(
                "device.call",
                json!({ "device": POCO, "call": { "tool": "ui_tree" } }),
            )
            .await;
        assert!(
            r.get("result").is_some(),
            "reads are allowed on read_only: {r}"
        );
    });
}

#[test]
fn calls_need_a_lease_and_a_known_device() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        assert_eq!(code(&a.call("device.call", tap_point()).await), NOT_HOLDER);
        assert_eq!(
            code(&a.call("lease.acquire", json!({ "device": "NOPE" })).await),
            UNKNOWN_DEVICE
        );
        let r = a
            .call(
                "device.call",
                json!({ "device": POCO, "call": { "tool": "tap" } }),
            )
            .await;
        assert_eq!(code(&r), NOT_HOLDER, "lease is checked before arguments");
        a.call("lease.acquire", json!({ "device": POCO })).await;
        let r = a
            .call(
                "device.call",
                json!({ "device": POCO, "call": { "tool": "tap" } }),
            )
            .await;
        assert_eq!(code(&r), INVALID_PARAMS);
        let r = a
            .call(
                "device.call",
                json!({ "device": POCO, "call": { "tool": "format_disk" } }),
            )
            .await;
        assert_eq!(code(&r), INVALID_PARAMS);
        assert!(h.device.lock().unwrap().acted.is_empty());
    });
}

#[test]
fn malformed_json_rpc_gets_the_spec_error_codes() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::Full);
        let mut c = Client::new(&h);

        let r = c.raw("{not json").await;
        assert_eq!((code(&r), &r["id"]), (PARSE_ERROR, &Value::Null));
        let r = c.raw("[]").await;
        assert_eq!(code(&r), INVALID_REQUEST);
        let r = c
            .raw("{\"jsonrpc\":\"2.0\",\"method\":\"nope\",\"id\":3}")
            .await;
        assert_eq!((code(&r), &r["id"]), (METHOD_NOT_FOUND, &json!(3)));
        // Blank lines are ignored: the next answer is the ping's.
        c.send_raw("").await;
        let r = c.call("hello", json!({})).await;
        assert!(r["result"].is_object());
    });
}

#[test]
fn json_rpc_envelope_rules() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::Full);
        let mut c = Client::new(&h);

        let r = c
            .raw("{\"jsonrpc\":\"2.0\",\"method\":\"hello\",\"id\":1")
            .await; // torn JSON that did get a newline
        assert_eq!(code(&r), PARSE_ERROR);
        assert_eq!(r["id"], Value::Null);

        let r = c.raw("[]").await;
        assert_eq!(code(&r), INVALID_REQUEST);
        let r = c.raw("123").await;
        assert_eq!(code(&r), INVALID_REQUEST);
        let r = c.raw("\"hello\"").await;
        assert_eq!(code(&r), INVALID_REQUEST);
        let r = c
            .raw("{\"jsonrpc\":\"1.0\",\"method\":\"hello\",\"id\":5}")
            .await;
        assert_eq!((code(&r), &r["id"]), (INVALID_REQUEST, &json!(5)));
        let r = c.raw("{\"method\":\"hello\",\"id\":6}").await;
        assert_eq!((code(&r), &r["id"]), (INVALID_REQUEST, &json!(6)));
        let r = c.raw("{\"jsonrpc\":\"2.0\",\"id\":7}").await;
        assert_eq!((code(&r), &r["id"]), (INVALID_REQUEST, &json!(7)));
        let r = c.raw("{\"jsonrpc\":\"2.0\",\"method\":42,\"id\":8}").await;
        assert_eq!((code(&r), &r["id"]), (INVALID_REQUEST, &json!(8)));
        let r = c
            .raw("{\"jsonrpc\":\"2.0\",\"method\":\"hello\",\"id\":[1]}")
            .await;
        assert_eq!((code(&r), &r["id"]), (INVALID_REQUEST, &Value::Null));

        let r = c
            .raw("{\"jsonrpc\":\"2.0\",\"method\":\"no.such\",\"id\":\"x\"}")
            .await;
        assert_eq!((code(&r), &r["id"]), (METHOD_NOT_FOUND, &json!("x")));
        let r = c
            .raw("{\"jsonrpc\":\"2.0\",\"method\":\"hello\",\"id\":null}")
            .await;
        assert!(r["result"].is_object(), "null id is allowed: {r}");

        let r = c.raw("{\"jsonrpc\":\"2.0\",\"method\":\"lease.acquire\",\"params\":{\"device\":5},\"id\":9}").await;
        assert_eq!((code(&r), &r["id"]), (INVALID_PARAMS, &json!(9)));
        let r = c
            .raw("{\"jsonrpc\":\"2.0\",\"method\":\"lease.acquire\",\"params\":[\"x\"],\"id\":10}")
            .await;
        assert_eq!(code(&r), INVALID_PARAMS);
        let r = c
            .raw("{\"jsonrpc\":\"2.0\",\"method\":\"lease.acquire\",\"id\":11}")
            .await;
        assert_eq!(code(&r), INVALID_PARAMS, "missing required device");

        // Notifications get no response, even when they fail; the next answer is the ping's.
        c.send_raw("{\"jsonrpc\":\"2.0\",\"method\":\"hello\",\"params\":{\"session\":\"n\"}}")
            .await;
        c.send_raw("{\"jsonrpc\":\"2.0\",\"method\":\"no.such\"}")
            .await;
        let r = c
            .raw("{\"jsonrpc\":\"2.0\",\"method\":\"hello\",\"id\":12}")
            .await;
        assert_eq!(r["id"], 12);
        assert_eq!(
            r["result"]["holder"], "n",
            "the notification still took effect"
        );

        // Batches: one response per request that has an id.
        let r = c.raw("[{\"jsonrpc\":\"2.0\",\"method\":\"hello\",\"id\":20},{\"jsonrpc\":\"2.0\",\"method\":\"hello\"},{\"jsonrpc\":\"2.0\",\"method\":\"zzz\",\"id\":21},5]").await;
        let arr = r.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert!(arr[0]["result"].is_object());
        assert_eq!(code(&arr[1]), METHOD_NOT_FOUND);
        assert_eq!(code(&arr[2]), INVALID_REQUEST);

        // The connection survived all of it.
        let r = c.call("devices.list", json!({})).await;
        assert!(r["result"]["devices"].is_array());
    });
}

#[test]
fn an_oversized_line_is_refused_and_closes_only_that_connection() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::Full);
        let mut big = Client::new(&h);
        let mut ok = Client::named(&h, "claude-b").await;
        big.w
            .write_all(&vec![b'a'; server::MAX_LINE + 10])
            .await
            .unwrap();
        big.w.write_all(b"\n").await.unwrap();
        let r = big.read().await;
        assert_eq!(code(&r), INVALID_REQUEST);
        let mut rest = String::new();
        assert_eq!(
            big.r.read_line(&mut rest).await.unwrap(),
            0,
            "closed after the oversized line"
        );
        assert!(ok.call("devices.list", json!({})).await["result"].is_object());
    });
}

#[test]
fn graceful_stop_ends_connections_and_frees_leases() {
    run(async {
        let h = start(Registry::in_memory(), DeviceMode::Full);
        let mut a = Client::named(&h, "claude-a").await;
        a.call("lease.acquire", json!({ "device": POCO })).await;
        h.shutdown.send(true).unwrap();
        h.task.await.unwrap().unwrap();
        let mut rest = String::new();
        assert_eq!(
            a.r.read_line(&mut rest).await.unwrap(),
            0,
            "client sees EOF"
        );
        assert!(
            h.daemon.registry.status(None).is_empty(),
            "leases released on stop"
        );
    });
}

#[test]
fn schema_is_generated_and_matches_the_lease_schema() {
    let text = bridged::proto::ipc_schema();
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../generated/ipc.schema.json");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, &text).unwrap();

    let schema: Value = serde_json::from_str(&text).unwrap();
    let defs = schema["definitions"].as_object().expect("definitions");
    for t in [
        "RpcRequest",
        "RpcResponse",
        "RpcError",
        "HelloParams",
        "HelloResult",
        "DevicesListResult",
        "DeviceRow",
        "Lease",
        "LeaseAcquireParams",
        "LeaseReleaseResult",
        "LeaseStatusResult",
        "ToolCall",
        "DeviceCallParams",
        "DeviceCallResult",
    ] {
        assert!(
            defs.contains_key(t),
            "{t} missing from the generated schema"
        );
    }

    // The Lease type agrees with schemas/registry-lease.json.
    let hand: Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/registry-lease.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&defs["Lease"]["properties"]),
        keys(&hand["properties"])
    );
    let mut req: Vec<String> = defs["Lease"]["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    req.sort();
    let mut hand_req: Vec<String> = hand["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect();
    hand_req.sort();
    assert_eq!(req, hand_req);
    assert_eq!(
        defs["Lease"]["additionalProperties"],
        hand["additionalProperties"]
    );
}

#[test]
fn the_crate_has_no_tcp_listener() {
    let needle = ["Tcp", "Listener"].concat();
    let bind = ["bind(\"", "0.0.0.0"].concat();
    // recursive: src/ has subdirectories (src/runner/, task 2.10)
    let mut dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
    let mut files = 0;
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            // the job view (src/jobview.rs) is the one listener of the
            // crate's own, and it checks every address first; nothing binds every interface
            let is_view = p.file_name().is_some_and(|n| n == "jobview.rs");
            assert!(!text.contains(&bind), "{} binds 0.0.0.0", p.display());
            assert!(
                is_view || !text.contains(&needle),
                "{} mentions a TCP listener",
                p.display()
            );
            if is_view {
                assert!(
                    text.contains("check_view_addr(*ip, cfg.hub_mode)?;")
                        && text.contains("SocketAddr::new(ip, port)"),
                    "the view checks each address and binds exactly that one"
                );
            }
            files += 1;
        }
    }
    assert!(files > 8, "scanned {files} files");
}
