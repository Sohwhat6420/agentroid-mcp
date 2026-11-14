//! The protected-device lease through the daemon (`docs/design/protected-device-lease.md`): the
//! daemon's lease names the holder, a connection that goes or a release closes the protected lease,
//! a job's run id is named on its ask, and push and pull reach core's file gate. Fakes only: no adb,
//! no phone, no real pipe.

use agentroid_adb_link::guard::S24_SERIAL;
use agentroid_adb_link::lease::{LeaseBook, ManualClock};
use agentroid_core::approval::{Approver, Channel, Inbound, Request, Style};
use agentroid_core::fake::FakeClock;
use agentroid_core::frame::Shot;
use agentroid_core::protected::{Protected, ProtectedConfig};
use agentroid_core::transport::{
    Backend, Core, DeviceInfo, Input, Screenshot, Transport, TransportError,
};
use bridged::jobs::JobsConfig;
use bridged::listener::{duplex_listener, DuplexConnector};
use bridged::registry::Registry;
use bridged::server::{self, CoreBackend, Daemon};
use serde_json::{json, Value};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::watch;

const PROT: &str = "192.0.2.60:5555";
const APP: &str = "com.android.settings";
const T0: u64 = 1_800_000_000_000;

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

// ---------------------------------------------------------------------------------------------
// the fake phone and channel
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Phone {
    reached: Vec<String>,
}

#[derive(Clone)]
struct SharedPhone(Arc<Mutex<Phone>>);

/// A Settings screen whose nodes carry the app's package (the engine's foreground check).
fn tree() -> String {
    let mut s = String::from("<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation=\"0\"><node class=\"android.widget.FrameLayout\" package=\"\" text=\"\" content-desc=\"\" resource-id=\"\" clickable=\"false\" bounds=\"[0,0][1080,2400]\">");
    for (text, b) in [
        ("Settings", "[48,200][400,280]"),
        ("Bluetooth", "[0,720][1080,880]"),
        ("Display", "[0,900][1080,1060]"),
    ] {
        s += &format!("<node class=\"android.widget.Button\" package=\"{APP}\" text=\"{text}\" content-desc=\"\" resource-id=\"\" clickable=\"true\" bounds=\"{b}\"/>");
    }
    s + "</node></hierarchy>"
}

impl Transport for SharedPhone {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        Ok(vec![DeviceInfo {
            serial: PROT.into(),
            model: "protected".into(),
            hw_serial: S24_SERIAL.into(),
        }])
    }
    fn screen_size(&mut self, _: &str) -> Result<(i32, i32), TransportError> {
        Ok((1080, 2400))
    }
    fn screen_state(&mut self, _: &str) -> Result<String, TransportError> {
        Ok("Awake".into())
    }
    fn screenshot(&mut self, _: &str, _: i32) -> Result<Screenshot, TransportError> {
        Ok(Screenshot {
            jpeg: vec![0xff; 16],
            shot: Shot {
                dev_w: 1080,
                dev_h: 2400,
                img_w: 1080,
                img_h: 2400,
            },
        })
    }
    fn tree_xml(&mut self, _: &str, _: Backend) -> Result<String, TransportError> {
        Ok(tree())
    }
    fn input(&mut self, s: &str, i: &Input) -> Result<(), TransportError> {
        self.0
            .lock()
            .unwrap()
            .reached
            .push(format!("input {s} {i:?}"));
        Ok(())
    }
    fn packages(&mut self, _: &str) -> Result<Vec<String>, TransportError> {
        Ok(vec![APP.into()])
    }
    fn shell(&mut self, s: &str, l: &str) -> Result<String, TransportError> {
        self.0
            .lock()
            .unwrap()
            .reached
            .push(format!("shell {s} {l}"));
        if l.starts_with("dumpsys window") {
            return Ok(format!("mCurrentFocus=Window{{1 u0 {APP}/.Settings}}\n"));
        }
        Ok(String::new())
    }
    fn helper_status(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn push(&mut self, _s: &str, _local: &Path, remote: &str) -> Result<(), TransportError> {
        self.0
            .lock()
            .unwrap()
            .reached
            .push(format!("push {remote}"));
        Ok(())
    }
    fn pull(&mut self, _s: &str, remote: &str, _local: &Path) -> Result<(), TransportError> {
        self.0
            .lock()
            .unwrap()
            .reached
            .push(format!("pull {remote}"));
        Err(TransportError(format!("{remote}: no such file")))
    }
}

#[derive(Default)]
struct Inbox {
    sent: Vec<Request>,
    events: Vec<Inbound>,
}

struct Ch(Arc<Mutex<Inbox>>);

impl Channel for Ch {
    fn name(&self) -> &'static str {
        "test"
    }
    fn send(&mut self, req: &Request) -> Result<(), String> {
        self.0.lock().unwrap().sent.push(req.clone());
        Ok(())
    }
    fn poll(&mut self, _: &Request) -> Result<Vec<Inbound>, String> {
        Ok(self.0.lock().unwrap().events.clone())
    }
}

// ---------------------------------------------------------------------------------------------
// the daemon
// ---------------------------------------------------------------------------------------------

struct Harness {
    connector: DuplexConnector,
    _shutdown: watch::Sender<bool>,
    _daemon: Arc<Daemon>,
    book: Arc<LeaseBook>,
    wall: Arc<ManualClock>,
    inbox: Arc<Mutex<Inbox>>,
    phone: Arc<Mutex<Phone>>,
    audit: PathBuf,
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("protected-lease")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("outbox")).unwrap();
    std::fs::create_dir_all(dir.join("inbox")).unwrap();
    dir
}

fn start(name: &str) -> Harness {
    let dir = tmp_dir(name);
    let phone = Arc::new(Mutex::new(Phone::default()));
    let wall = Arc::new(ManualClock::new(T0));
    let book = Arc::new(LeaseBook::new(wall.clone()));
    let inbox = Arc::new(Mutex::new(Inbox::default()));
    let mut core = Core::new(SharedPhone(phone.clone()), FakeClock::new());
    let audit = dir.join("audit.log");
    core.set_audit(agentroid_core::audit::AuditLog::open(&audit).unwrap());
    core.set_approver(Approver::new(Box::new(Ch(inbox.clone()))));
    core.set_protected(Protected::new(
        ProtectedConfig::new(30, 10, None),
        book.clone(),
    ));
    let backend = CoreBackend::new(core);
    let daemon = Daemon::with_jobs(
        Registry::in_memory(),
        Box::new(backend),
        Some(JobsConfig::new(dir.join("runs"))),
    );
    daemon.set_files_dir(dir.clone());
    daemon.attach_protected(book.clone()).unwrap();
    assert!(
        daemon.attach_protected(book.clone()).is_err(),
        "attached once"
    );
    let (listener, connector) = duplex_listener();
    let (shutdown, rx) = watch::channel(false);
    tokio::spawn(server::serve(daemon.clone(), listener, rx));
    Harness {
        connector,
        _shutdown: shutdown,
        _daemon: daemon,
        book,
        wall,
        inbox,
        phone,
        audit,
    }
}

struct Client {
    r: BufReader<ReadHalf<DuplexStream>>,
    w: WriteHalf<DuplexStream>,
    next: u64,
}

impl Client {
    async fn leased(h: &Harness, session: &str) -> Client {
        let (r, w) = tokio::io::split(h.connector.connect().unwrap());
        let mut c = Client {
            r: BufReader::new(r),
            w,
            next: 1,
        };
        c.call("hello", json!({ "session": session })).await;
        let r = c.call("lease.acquire", json!({ "device": PROT })).await;
        assert!(r["result"].is_object(), "{r}");
        c
    }
    async fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let line =
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string();
        self.w.write_all(line.as_bytes()).await.unwrap();
        self.w.write_all(b"\n").await.unwrap();
        let mut out = String::new();
        assert!(self.r.read_line(&mut out).await.unwrap() > 0);
        serde_json::from_str(&out).unwrap()
    }
    async fn tap(&mut self) -> Value {
        self.call(
            "device.call",
            json!({ "device": PROT, "call": { "tool": "tap", "x": 100, "y": 980, "space": "device" } }),
        )
        .await
    }
}

impl Harness {
    fn sent(&self) -> Vec<Request> {
        self.inbox.lock().unwrap().sent.clone()
    }
    /// The lease asks sent (not the lease-open notices).
    fn asks(&self) -> Vec<Request> {
        self.sent()
            .into_iter()
            .filter(|r| r.style == Style::Lease)
            .collect()
    }
    /// The audit log's lease closes so far.
    fn closes(&self) -> Vec<String> {
        std::fs::read_to_string(&self.audit)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| agentroid_core::audit::Record::from_json(l).ok())
            .filter(|r| r.entry.rule == "lease:closed")
            .map(|r| r.entry.reason)
            .collect()
    }
    fn allow_last(&self) {
        let mut g = self.inbox.lock().unwrap();
        let id = g
            .sent
            .iter()
            .rev()
            .find(|r| r.style == Style::Lease)
            .unwrap()
            .id
            .clone();
        let n = g.events.len();
        g.events.push(Inbound {
            event_id: format!("ev{n}"),
            time_ms: T0,
            message: format!("allow {id}"),
        });
    }
    fn taps(&self) -> usize {
        self.phone
            .lock()
            .unwrap()
            .reached
            .iter()
            .filter(|l| l.starts_with("input"))
            .count()
    }
    /// Wait (briefly) for the daemon to notice a connection that went.
    async fn until_closed(&self) {
        let t = Instant::now();
        while self.book.active().is_some() {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "the protected lease outlived its connection: {:?}",
                self.book.active()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// An ask, an Allow, and the tap that rides on the new lease.
async fn open_lease(h: &Harness, c: &mut Client) {
    let r = c.tap().await;
    assert!(r["error"].is_object(), "the first tap asks: {r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Allow"),
        "{r}"
    );
    assert_eq!(h.taps(), 0);
    h.allow_last();
    let r = c.tap().await;
    assert!(r["result"].is_object(), "the lease opens on Allow: {r}");
    assert_eq!(h.taps(), 1);
}

// ---------------------------------------------------------------------------------------------
// the tests
// ---------------------------------------------------------------------------------------------

#[test]
fn the_ask_names_the_daemon_lease_holder_and_the_lease_is_theirs() {
    run(async {
        let h = start("holder");
        let mut c = Client::leased(&h, "lane-a").await;
        open_lease(&h, &mut c).await;
        let ask = h.sent()[0].clone();
        assert!(ask.reason.contains("lane-a"), "{}", ask.reason);
        let lease = h.book.active().expect("open");
        assert_eq!(lease.holder.name, "lane-a");
        assert!(h.book.admits(PROT));
        // the burst rides on it
        for _ in 0..3 {
            assert!(c.tap().await["result"].is_object());
        }
        assert_eq!(h.asks().len(), 1, "one Allow");
        assert_eq!(h.sent().len(), 2, "and its lease-open notice");
    });
}

#[test]
fn a_connection_that_goes_closes_its_protected_lease() {
    run(async {
        let h = start("conn");
        let mut c = Client::leased(&h, "lane-a").await;
        open_lease(&h, &mut c).await;
        drop(c);
        h.until_closed().await;
        assert!(!h.book.admits(PROT), "the adb guard refuses at once");
        // the next holder starts from nothing: an ask of its own, once the ask window has passed
        let mut d = Client::leased(&h, "lane-b").await;
        let r = d.tap().await;
        assert!(r["error"].is_object(), "{r}");
        assert_eq!(h.asks().len(), 1, "one ask per ask window");
        h.wall.advance(10 * 60_000);
        let r = d.tap().await;
        assert!(r["error"].is_object(), "{r}");
        assert_eq!(h.asks().len(), 2);
        assert!(h.asks()[1].reason.contains("lane-b"));
        assert_eq!(h.taps(), 1);
        // the close was logged
        let closes = h.closes();
        assert!(
            closes
                .iter()
                .any(|c| c.ends_with("closed: connection closed")),
            "{closes:?}"
        );
    });
}

#[test]
fn releasing_the_protected_phone_closes_its_protected_lease() {
    run(async {
        let h = start("release");
        let mut c = Client::leased(&h, "lane-a").await;
        open_lease(&h, &mut c).await;
        let r = c.call("lease.release", json!({ "device": PROT })).await;
        assert_eq!(r["result"]["released"], true, "{r}");
        assert!(h.book.active().is_none(), "closed with the release");
        assert!(!h.book.admits(PROT));
        // logged at the release, before any later call
        let closes = h.closes();
        assert_eq!(closes.len(), 1, "{closes:?}");
        assert!(closes[0].ends_with("closed: released"), "{closes:?}");
    });
}

#[test]
fn a_job_on_the_protected_phone_is_named_on_its_ask_and_forgotten_when_it_ends() {
    run(async {
        let h = start("job");
        let mut c = Client::leased(&h, "lane-a").await;
        let flow = json!({ "flow": "plp-test", "version": 1, "app": { "package": APP, "version_range": "*" }, "steps": [{ "id": "disp", "kind": "tap", "select": { "text": "Display" } }] });
        let r = c
            .call("job.start", json!({ "device": PROT, "flow": flow }))
            .await;
        let run_id = r["result"]["run_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{r}"))
            .to_string();
        let t = Instant::now();
        loop {
            let s = c.call("job.status", json!({ "run_id": run_id })).await;
            if s["result"]["status"] != "running" {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(30), "{s}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let asks = h.sent();
        assert!(!asks.is_empty(), "the job's tap asked");
        assert!(
            asks[0].reason.contains(&run_id),
            "the ask names the job: {}",
            asks[0].reason
        );
        assert_eq!(h.taps(), 0, "nothing acted before the Allow");
        // the job has ended: an Allow now opens a lease that names no job
        h.allow_last();
        assert!(c.tap().await["result"].is_object());
        assert_eq!(h.book.active().unwrap().job, None);
    });
}

#[test]
fn push_and_pull_on_the_protected_phone_reach_core_s_file_gate_under_the_lease() {
    run(async {
        let h = start("files");
        let mut c = Client::leased(&h, "lane-a").await;
        // a protected path is refused before any ask or lease
        let r = c
            .call(
                "device.call",
                json!({ "device": PROT, "call": { "tool": "pull_new", "dir": "/sdcard/Android/media/com.whatsapp/WhatsApp/Media" } }),
            )
            .await;
        assert!(r["error"].is_object(), "{r}");
        let msg = r["error"]["message"].as_str().unwrap_or("").to_string();
        assert!(msg.contains("protected"), "{msg}");
        assert!(h.sent().is_empty(), "no ask for a refused path");
        assert!(
            !h.phone
                .lock()
                .unwrap()
                .reached
                .iter()
                .any(|l| l.contains("whatsapp")),
            "nothing reached the phone"
        );
        // an ordinary folder goes to core, which asks for the lease (no daemon-side wall)
        let r = c
            .call(
                "device.call",
                json!({ "device": PROT, "call": { "tool": "pull_new", "dir": "/sdcard/Download" } }),
            )
            .await;
        assert!(r["error"].is_object(), "{r}");
        assert_eq!(h.sent().len(), 1, "core asked: {r}");
    });
}
