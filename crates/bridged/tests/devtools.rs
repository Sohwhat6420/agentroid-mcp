//! 2.4 daemon tests: `doctor` `diagnose`, `push`, `pull_new`. Fakes only: no adb, no device, no pipe.
//! The phone here is core's `FakeDevice` plus a little file store; every command that reaches it is recorded.

use agentroid_core::fake::{FakeClock, FakeDevice};
use agentroid_core::gate::DeviceMode;
use agentroid_core::transport::{
    Backend, Core, DeviceInfo, Input, Screenshot, Transport, TransportError,
};
use bridged::listener::{duplex_listener, DuplexConnector};
use bridged::proto::*;
use bridged::registry::Registry;
use bridged::server::{self, CoreBackend, Daemon};
use bridged::sha256::{file_sha256, Sha256};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
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
        tokio::time::timeout(Duration::from_secs(30), f)
            .await
            .expect("test timed out")
    })
}

#[derive(Default)]
struct Phone {
    dev: FakeDevice,
    files: HashMap<String, Vec<u8>>,
    /// every shell line, push and pull that reached the "adb"
    reached: Vec<String>,
    listing: Vec<String>,
    sizes: VecDeque<u64>,
    stats: usize,
    corrupt_push: bool,
    /// the pull flips the first byte (same size, another sha256)
    corrupt_pull: bool,
    /// the pull writes half the file, then fails (as an adb pull cut off mid-way)
    break_pull: bool,
    /// for every pull: the laptop path adb was given, and the names in its folder at that moment
    pulled_into: Vec<(PathBuf, Vec<String>)>,
}

#[derive(Clone)]
struct Shared(Arc<Mutex<Phone>>);

fn unquote(s: &str) -> String {
    match s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        Some(inner) => inner.replace("'\\''", "'"),
        None => s.to_string(),
    }
}

impl Transport for Shared {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        self.0.lock().unwrap().dev.devices()
    }
    fn screen_size(&mut self, s: &str) -> Result<(i32, i32), TransportError> {
        self.0.lock().unwrap().dev.screen_size(s)
    }
    fn screen_state(&mut self, s: &str) -> Result<String, TransportError> {
        self.0.lock().unwrap().dev.screen_state(s)
    }
    fn screenshot(&mut self, s: &str, w: i32) -> Result<Screenshot, TransportError> {
        self.0.lock().unwrap().dev.screenshot(s, w)
    }
    fn tree_xml(&mut self, s: &str, b: Backend) -> Result<String, TransportError> {
        self.0.lock().unwrap().dev.tree_xml(s, b)
    }
    fn input(&mut self, s: &str, i: &Input) -> Result<(), TransportError> {
        self.0.lock().unwrap().dev.input(s, i)
    }
    fn packages(&mut self, s: &str) -> Result<Vec<String>, TransportError> {
        self.0.lock().unwrap().dev.packages(s)
    }
    fn shell(&mut self, s: &str, l: &str) -> Result<String, TransportError> {
        let mut p = self.0.lock().unwrap();
        p.reached.push(format!("shell {l}"));
        if let Some(rest) = l.strip_prefix("sha256sum ") {
            let path = unquote(rest);
            return Ok(match p.files.get(&path) {
                Some(b) => {
                    let mut h = Sha256::new();
                    h.update(b);
                    format!("{}  {path}\n", h.finish_hex())
                }
                None => format!("sha256sum: {path}: No such file or directory\n"),
            });
        }
        if l.starts_with("ls -1tp ") {
            return Ok(p.listing.join("\n"));
        }
        if let Some(rest) = l.strip_prefix("stat -c %s ") {
            p.stats += 1;
            let n = if p.sizes.len() > 1 {
                p.sizes.pop_front()
            } else {
                p.sizes.front().copied()
            };
            return Ok(match n {
                Some(n) => format!("{n}\n"),
                None => format!("{}\n", p.files.get(&unquote(rest)).map_or(0, Vec::len)),
            });
        }
        p.dev.shell(s, l)
    }
    fn helper_status(&mut self, s: &str) -> Result<(), String> {
        self.0.lock().unwrap().dev.helper_status(s)
    }
    fn push(&mut self, _s: &str, local: &Path, remote: &str) -> Result<(), TransportError> {
        let mut p = self.0.lock().unwrap();
        p.reached.push(format!("push {remote}"));
        let mut b = std::fs::read(local).map_err(|e| TransportError(e.to_string()))?;
        if p.corrupt_push {
            b.push(b'!');
        }
        p.files.insert(remote.to_string(), b);
        Ok(())
    }
    fn pull(&mut self, _s: &str, remote: &str, local: &Path) -> Result<(), TransportError> {
        let mut p = self.0.lock().unwrap();
        p.reached.push(format!("pull {remote}"));
        let names = local
            .parent()
            .and_then(|d| std::fs::read_dir(d).ok())
            .map(|rd| {
                rd.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        p.pulled_into.push((local.to_path_buf(), names));
        let b = p
            .files
            .get(remote)
            .cloned()
            .ok_or_else(|| TransportError(format!("{remote}: no such file")))?;
        if p.break_pull {
            std::fs::write(local, &b[..b.len() / 2]).map_err(|e| TransportError(e.to_string()))?;
            return Err(TransportError("connection reset mid-pull".into()));
        }
        let mut b = b;
        if p.corrupt_pull && !b.is_empty() {
            b[0] ^= 0xff;
        }
        std::fs::write(local, b).map_err(|e| TransportError(e.to_string()))
    }
}

struct Harness {
    daemon: Arc<Daemon>,
    connector: DuplexConnector,
    _shutdown: watch::Sender<bool>,
    phone: Arc<Mutex<Phone>>,
}

fn start(poco_mode: DeviceMode) -> Harness {
    start_audited(poco_mode, None)
}

/// `start`, with core's audit log at `audit` (D3b: the file guard's decisions are logged there).
fn start_audited(poco_mode: DeviceMode, audit: Option<&Path>) -> Harness {
    start_full(poco_mode, audit, None)
}

/// `start`, with session stay-awake attached over `awake` (care's fake phone and a leases file).
fn start_awake(poco_mode: DeviceMode, awake: bridged::awake::AwakeConfig) -> Harness {
    start_full(poco_mode, None, Some(awake))
}

fn start_full(
    poco_mode: DeviceMode,
    audit: Option<&Path>,
    awake: Option<bridged::awake::AwakeConfig>,
) -> Harness {
    let phone = Arc::new(Mutex::new(Phone::default()));
    let mut core = Core::new(Shared(phone.clone()), FakeClock::new());
    if let Some(p) = audit {
        core.set_audit(agentroid_core::audit::AuditLog::open(p).unwrap());
    }
    let mut backend = CoreBackend::new(core);
    backend.set_mode(POCO, poco_mode);
    let daemon = Daemon::new(Registry::in_memory(), Box::new(backend));
    if let Some(cfg) = awake {
        daemon.attach_awake(cfg, Duration::from_secs(3600)).unwrap();
    }
    // the laptop folder of the test that called `tmp_dir` on this thread (outbox/ and inbox/)
    if let Some(files) = FILES.with(|f| f.borrow().clone()) {
        daemon.set_files_dir(files);
    }
    let (listener, connector) = duplex_listener();
    let (shutdown, rx) = watch::channel(false);
    tokio::spawn(server::serve(daemon.clone(), listener, rx));
    Harness {
        daemon,
        connector,
        _shutdown: shutdown,
        phone,
    }
}

struct Client {
    r: BufReader<ReadHalf<DuplexStream>>,
    w: WriteHalf<DuplexStream>,
    next: u64,
}

impl Client {
    /// A client holding the lease on `device`.
    async fn leased(h: &Harness, device: &str) -> Client {
        let (r, w) = tokio::io::split(h.connector.connect().unwrap());
        let mut c = Client {
            r: BufReader::new(r),
            w,
            next: 1,
        };
        c.call("hello", json!({ "session": "t" })).await;
        let r = c.call("lease.acquire", json!({ "device": device })).await;
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
    async fn tool(&mut self, device: &str, call: Value) -> Value {
        self.call("device.call", json!({ "device": device, "call": call }))
            .await
    }
    async fn diagnose(&mut self, device: &str, cmd: &str) -> Value {
        self.tool(device, json!({ "tool": "doctor", "diagnose": cmd }))
            .await
    }
}

fn code(r: &Value) -> i64 {
    r["error"]["code"]
        .as_i64()
        .unwrap_or_else(|| panic!("expected an error, got {r}"))
}

fn reached(h: &Harness) -> Vec<String> {
    h.phone.lock().unwrap().reached.clone()
}

fn no_command_reached(h: &Harness) {
    let p = h.phone.lock().unwrap();
    assert!(
        p.reached.is_empty(),
        "commands reached the fake adb: {:?}",
        p.reached
    );
    assert!(
        !p.dev.calls.contains(&"shell"),
        "the fake device saw a shell: {:?}",
        p.dev.calls
    );
    assert!(
        p.dev.acted.is_empty(),
        "the fake device acted: {:?}",
        p.dev.acted
    );
}

thread_local! {
    /// The daemon's files folder for the next `start` on this thread (set by `tmp_dir`).
    static FILES: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// A fresh test folder with `outbox/` and `inbox/`; the next daemon started on this thread uses it
/// as its files folder, so `push` reads `<dir>/outbox` and `pull_new` writes `<dir>/inbox`.
fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("outbox")).unwrap();
    std::fs::create_dir_all(dir.join("inbox")).unwrap();
    FILES.with(|f| *f.borrow_mut() = Some(dir.clone()));
    dir
}

/// The plan's 10 named forms (E2 fix-up 2, W1) and the verify-3 forms (a newline-joined pair among them).
const REFUSED_FORMS: [&str; 17] = [
    "dumpsys battery set level 5",
    "dumpsys battery reset",
    "dumpsys battery unplug",
    "dumpsys deviceidle force-idle",
    "dumpsys deviceidle unforce",
    "dumpsys deviceidle step",
    "dumpsys battery level",
    "settings put system screen_brightness 1",
    "cmd appops set com.x CAMERA allow",
    "pm enable com.x",
    "pm disable com.x",
    "dumpsys deviceidle whitelist +com.x",
    "wm size 720x1280",
    "wm density 240",
    "pm list packages; pm clear com.x",
    "getprop x && input tap 1 1",
    "getprop $(input tap 1 1)",
];
const REFUSED_MORE: [&str; 3] = [
    "settings get system a|settings put system a 1",
    "dumpsys battery\ndumpsys power",
    "rm -rf /sdcard",
];

#[test]
fn every_refused_form_is_refused_on_read_only_10_of_10_and_nothing_reaches_adb() {
    run(async {
        let h = start(DeviceMode::ReadOnly);
        let mut c = Client::leased(&h, POCO).await;
        for cmd in REFUSED_FORMS.iter().chain(REFUSED_MORE.iter()) {
            for attempt in 0..10 {
                let r = c.diagnose(POCO, cmd).await;
                assert_eq!(code(&r), REFUSED, "{cmd:?} attempt {attempt}: {r}");
            }
        }
        no_command_reached(&h);
        // R2 forms say so
        let r = c.diagnose(POCO, "dumpsys battery set level 5").await;
        assert_eq!(r["error"]["data"]["class"], "R2");
        let r = c.diagnose(POCO, "getprop x && input tap 1 1").await;
        assert_eq!(r["error"]["data"]["class"], "metachar");
    });
}

#[test]
fn the_same_forms_are_refused_on_a_full_device_too() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        for cmd in REFUSED_FORMS.iter().chain(REFUSED_MORE.iter()) {
            assert_eq!(code(&c.diagnose(POCO, cmd).await), REFUSED, "{cmd:?}");
        }
        no_command_reached(&h);
    });
}

#[test]
fn read_only_answers_the_reads_with_the_exact_quoted_line() {
    run(async {
        let h = start(DeviceMode::ReadOnly);
        let mut c = Client::leased(&h, POCO).await;
        let r = c.diagnose(POCO, "dumpsys battery").await;
        assert_eq!(r["result"]["tool"], "doctor", "{r}");
        let r = c.diagnose(POCO, "getprop ro.product.model").await;
        assert_eq!(r["result"]["text"], "value-of-ro.product.model", "{r}");
        c.diagnose(POCO, "pm list packages -3").await;
        c.diagnose(POCO, "cmd appops get com.x.y CAMERA").await;
        assert_eq!(
            reached(&h),
            [
                "shell dumpsys battery",
                "shell getprop ro.product.model",
                "shell pm list packages -3",
                "shell cmd appops get com.x.y CAMERA"
            ]
        );
    });
}

#[test]
fn s24_answers_only_serial_and_the_approved_probes_through_diagnose() {
    run(async {
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, S24).await;
        for cmd in [
            "getprop ro.serialno",
            "getprop ro.build.version.sdk",
            "getprop ro.product.model",
            "wm size",
            "wm density",
            "pm list features",
            "dumpsys battery",
            "dumpsys thermalservice",
        ] {
            let r = c.diagnose(S24, cmd).await;
            assert!(r["result"].is_object(), "{cmd}: {r}");
        }
        let answered = reached(&h).len();
        assert_eq!(answered, 8);
        for cmd in [
            "getprop ro.product.manufacturer",
            "getprop ro.serial",
            "dumpsys power",
            "dumpsys battery set level 5",
            "settings get system a",
            "pm list packages",
            "getevent -pl",
            "getprop ro.serialno; id",
        ] {
            for _ in 0..10 {
                assert_eq!(code(&c.diagnose(S24, cmd).await), REFUSED, "{cmd}");
            }
        }
        assert_eq!(
            reached(&h).len(),
            answered,
            "a refused S24 command reached adb"
        );
        // push and pull_new never go near the protected phone
        let r = c
            .tool(
                S24,
                json!({ "tool": "push", "local": "x", "phone_path": "/sdcard/x" }),
            )
            .await;
        assert_eq!(code(&r), REFUSED);
        let r = c
            .tool(S24, json!({ "tool": "pull_new", "dir": "/sdcard/x" }))
            .await;
        assert_eq!(code(&r), REFUSED);
        assert_eq!(reached(&h).len(), answered);
    });
}

#[test]
fn a_device_without_the_lease_and_helper_mode_are_refused() {
    run(async {
        let h = start(DeviceMode::Helper);
        let mut c = Client::leased(&h, POCO).await;
        assert_eq!(code(&c.diagnose(POCO, "dumpsys battery").await), REFUSED);
        no_command_reached(&h);
        let h = start(DeviceMode::Full);
        let (r, w) = tokio::io::split(h.connector.connect().unwrap());
        let mut c = Client {
            r: BufReader::new(r),
            w,
            next: 1,
        };
        assert_eq!(code(&c.diagnose(POCO, "dumpsys battery").await), NOT_HOLDER);
        no_command_reached(&h);
    });
}

/// A file in the test folder's outbox (the only place `push` reads from).
fn local_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join("outbox").join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

#[test]
fn push_reads_back_the_same_sha256_and_a_path_with_a_space_arrives_unchanged() {
    run(async {
        let dir = tmp_dir("devtools-push");
        let local = local_file(&dir, "fixture.bin", b"hello fixture \x00\xff bytes");
        let (want, size) = file_sha256(&local).unwrap();
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "push", "local": local.to_string_lossy(), "phone_path": "/sdcard/Android/data/com.agentroid.testapp/files/my fixture.bin" })).await;
        assert_eq!(r["result"]["sha256"], want, "{r}");
        assert_eq!(r["result"]["size"], size);
        assert_eq!(
            r["result"]["path"],
            "/sdcard/Android/data/com.agentroid.testapp/files/my fixture.bin"
        );
        let p = h.phone.lock().unwrap();
        assert_eq!(
            p.files["/sdcard/Android/data/com.agentroid.testapp/files/my fixture.bin"],
            b"hello fixture \x00\xff bytes"
        );
        assert_eq!(
            p.reached[0],
            "push /sdcard/Android/data/com.agentroid.testapp/files/my fixture.bin"
        );
        assert_eq!(
            p.reached[1],
            "shell sha256sum '/sdcard/Android/data/com.agentroid.testapp/files/my fixture.bin'"
        );
    });
}

#[test]
fn push_that_reads_back_wrong_is_an_error() {
    run(async {
        let dir = tmp_dir("devtools-push-bad");
        let local = local_file(&dir, "f.bin", b"abc");
        let h = start(DeviceMode::Full);
        h.phone.lock().unwrap().corrupt_push = true;
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "push", "local": local.to_string_lossy(), "phone_path": "/sdcard/f.bin" })).await;
        assert_eq!(code(&r), DEVICE_ERROR);
        assert!(
            r["error"]["message"].as_str().unwrap().contains("mismatch"),
            "{r}"
        );
    });
}

#[test]
fn push_and_pull_new_are_refused_on_read_only_and_helper_with_nothing_reaching_adb() {
    run(async {
        let dir = tmp_dir("devtools-push-ro");
        let local = local_file(&dir, "f.bin", b"abc");
        for mode in [DeviceMode::ReadOnly, DeviceMode::Helper] {
            let h = start(mode);
            let mut c = Client::leased(&h, POCO).await;
            for _ in 0..10 {
                let r = c.tool(POCO, json!({ "tool": "push", "local": local.to_string_lossy(), "phone_path": "/sdcard/f.bin" })).await;
                assert_eq!(code(&r), REFUSED, "{r}");
                let r = c
                    .tool(POCO, json!({ "tool": "pull_new", "dir": "/sdcard/DCIM" }))
                    .await;
                assert_eq!(code(&r), REFUSED, "{r}");
            }
            no_command_reached(&h);
        }
        let h = start(DeviceMode::ReadOnly);
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "push", "local": local.to_string_lossy(), "phone_path": "/sdcard/f.bin" })).await;
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("read_only"),
            "{r}"
        );
    });
}

#[test]
fn push_rejects_bad_paths_before_touching_the_phone() {
    run(async {
        let dir = tmp_dir("devtools-push-paths");
        let local = local_file(&dir, "f.bin", b"abc");
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        for p in ["relative/x", "/sdcard/../data/x", "/sdcard/a\nb"] {
            let r = c
                .tool(
                    POCO,
                    json!({ "tool": "push", "local": local.to_string_lossy(), "phone_path": p }),
                )
                .await;
            assert_eq!(code(&r), INVALID_PARAMS, "{p:?}");
        }
        let r = c.tool(POCO, json!({ "tool": "push", "local": dir.join("missing").to_string_lossy(), "phone_path": "/sdcard/x" })).await;
        assert_eq!(code(&r), INVALID_PARAMS);
        no_command_reached(&h);
    });
}

/// The laptop side is fenced: `push` reads only the outbox and `pull_new` writes only the inbox.
/// A file elsewhere, a `..` name, or a pull into another folder is REFUSED (class `outbox` /
/// `inbox`) and nothing reaches the phone.
#[test]
fn push_reads_only_the_outbox_and_pull_new_writes_only_the_inbox() {
    run(async {
        let dir = tmp_dir("devtools-fence");
        let secret = dir.join("id_ed25519");
        std::fs::write(&secret, b"not a real key").unwrap();
        let _ = local_file(&dir, "ok.bin", b"abc");
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        let outside = secret.to_string_lossy().into_owned();
        for local in [outside.as_str(), "../id_ed25519", "sub/../../id_ed25519"] {
            let r = c
                .tool(
                    POCO,
                    json!({ "tool": "push", "local": local, "phone_path": "/sdcard/Download/k" }),
                )
                .await;
            assert_eq!(code(&r), REFUSED, "{local:?}: {r}");
            assert_eq!(r["error"]["data"]["class"], "outbox", "{r}");
        }
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        for local_dir in [
            elsewhere.to_string_lossy().into_owned(),
            "../elsewhere".to_string(),
        ] {
            let r = c
                .tool(POCO, json!({ "tool": "pull_new", "dir": "/sdcard/DCIM", "poll_ms": 0, "local_dir": local_dir }))
                .await;
            assert_eq!(code(&r), REFUSED, "{local_dir:?}: {r}");
            assert_eq!(r["error"]["data"]["class"], "inbox", "{r}");
        }
        no_command_reached(&h);
        // a plain name is taken inside the outbox
        let r = c
            .tool(
                POCO,
                json!({ "tool": "push", "local": "ok.bin", "phone_path": "/sdcard/ok.bin" }),
            )
            .await;
        assert_eq!(r["result"]["size"], 3, "{r}");
    });
}

/// D3b: the file guard sits behind the path check. A push outside shared storage, a pull from an
/// app's private data and a push of an APK (asked, no approval channel) are REFUSED with class
/// `file_guard`, nothing reaches the fake adb, and every decision is in the hash-chained log.
#[test]
fn push_and_pull_new_go_through_the_file_guard_and_the_audit_log() {
    run(async {
        let dir = tmp_dir("devtools-file-guard");
        let local = local_file(&dir, "f.bin", b"abc");
        let apk = local_file(&dir, "evil.apk", b"PK");
        let log = dir.join("audit.jsonl");
        let h = start_audited(DeviceMode::Full, Some(&log));
        let mut c = Client::leased(&h, POCO).await;
        for (p, word) in [
            ("/system/etc/hosts", "system or app state"),
            (
                "/data/data/com.whatsapp/shared_prefs/x.xml",
                "system or app state",
            ),
            (
                "/data/system/users/0/settings_secure.xml",
                "system or app state",
            ),
        ] {
            let r = c
                .tool(
                    POCO,
                    json!({ "tool": "push", "local": local.to_string_lossy(), "phone_path": p }),
                )
                .await;
            assert_eq!(code(&r), REFUSED, "{p}: {r}");
            assert_eq!(r["error"]["data"]["class"], "file_guard", "{r}");
            assert!(
                r["error"]["message"].as_str().unwrap().contains(word),
                "{r}"
            );
        }
        // the APK needs a human yes; no channel is configured, so it is refused
        let r = c
            .tool(
                POCO,
                json!({ "tool": "push", "local": apk.to_string_lossy(), "phone_path": "/sdcard/Download/evil.apk" }),
            )
            .await;
        assert_eq!(code(&r), REFUSED, "{r}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no approval channel is configured"),
            "{r}"
        );
        for d in [
            "/data/data/com.whatsapp/databases",
            "/data/misc/wifi",
            "/sdcard/Android/data/com.whatsapp/files",
        ] {
            let r = c
                .tool(POCO, json!({ "tool": "pull_new", "dir": d, "poll_ms": 0 }))
                .await;
            assert_eq!(code(&r), REFUSED, "{d}: {r}");
            assert_eq!(r["error"]["data"]["class"], "file_guard", "{r}");
        }
        no_command_reached(&h);
        // every refusal is in the log, which verifies
        let v = agentroid_core::audit::verify_file(&log).unwrap();
        assert_eq!(v.records, 7, "{v:?}");
        // 5 blocked; 2 asked (the APK push, and the pull from another app's Android/data)
        assert_eq!((v.blocked, v.asked), (5, 2));
        // an allowed push is logged as allowed before the bytes move
        let r = c
            .tool(
                POCO,
                json!({ "tool": "push", "local": local.to_string_lossy(), "phone_path": "/sdcard/Download/f.bin" }),
            )
            .await;
        assert!(r["result"].is_object(), "{r}");
        let v = agentroid_core::audit::verify_file(&log).unwrap();
        assert_eq!((v.records, v.allowed), (8, 1));
        assert!(reached(&h)
            .iter()
            .any(|l| l == "push /sdcard/Download/f.bin"));
    });
}

#[test]
fn pull_new_waits_for_a_stable_size_then_returns_path_size_and_sha256() {
    run(async {
        let dir = tmp_dir("devtools-pull");
        let h = start(DeviceMode::Full);
        {
            let mut p = h.phone.lock().unwrap();
            p.listing = vec![
                ".pending-1.mp4".into(),
                "new shot.png".into(),
                "old.png".into(),
            ];
            p.files.insert(
                "/sdcard/DCIM/new shot.png".into(),
                b"0123456789abcdefghij".to_vec(),
            );
            p.sizes = VecDeque::from([4, 12, 20, 20]);
        }
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "pull_new", "dir": "/sdcard/DCIM/", "poll_ms": 0, "local_dir": dir.join("inbox").to_string_lossy() })).await;
        let res = &r["result"];
        assert_eq!(res["size"], 20, "{r}");
        let (want, _) = file_sha256(Path::new(res["path"].as_str().unwrap())).unwrap();
        assert_eq!(res["sha256"], want);
        assert!(res["path"].as_str().unwrap().ends_with("new shot.png"));
        assert_eq!(
            std::fs::read(res["path"].as_str().unwrap()).unwrap(),
            b"0123456789abcdefghij"
        );
        let p = h.phone.lock().unwrap();
        assert_eq!(
            p.stats, 4,
            "4 polls: 4, 12, 20, 20; stable only when the same size is seen twice running"
        );
        assert!(
            p.reached
                .contains(&"shell ls -1tp /sdcard/DCIM/".to_string())
                || p.reached.iter().any(|l| l.starts_with("shell ls -1tp "))
        );
        assert!(
            p.reached
                .contains(&"pull /sdcard/DCIM/new shot.png".to_string()),
            "{:?}",
            p.reached
        );
    });
}

#[test]
fn pull_new_is_stable_on_two_equal_polls_and_fails_when_the_size_keeps_changing() {
    run(async {
        let dir = tmp_dir("devtools-pull2");
        let h = start(DeviceMode::Full);
        {
            let mut p = h.phone.lock().unwrap();
            p.listing = vec!["a.bin".into()];
            p.files.insert("/sdcard/d/a.bin".into(), vec![7; 5]);
            p.sizes = VecDeque::from([5, 5]);
        }
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "pull_new", "dir": "/sdcard/d", "poll_ms": 0, "local_dir": dir.join("inbox").to_string_lossy() })).await;
        assert_eq!(r["result"]["size"], 5, "{r}");
        assert_eq!(h.phone.lock().unwrap().stats, 2);
        // growing forever
        {
            let mut p = h.phone.lock().unwrap();
            p.stats = 0;
            p.sizes = (1..=40).collect();
        }
        let r = c.tool(POCO, json!({ "tool": "pull_new", "dir": "/sdcard/d", "poll_ms": 0, "local_dir": dir.join("inbox").to_string_lossy() })).await;
        assert_eq!(code(&r), DEVICE_ERROR, "{r}");
        assert!(r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("still changing"));
        // an empty directory
        h.phone.lock().unwrap().listing = vec![".hidden".into(), "x.pending".into(), "d/".into()];
        let r = c
            .tool(
                POCO,
                json!({ "tool": "pull_new", "dir": "/sdcard/d", "poll_ms": 0 }),
            )
            .await;
        assert_eq!(code(&r), DEVICE_ERROR);
    });
}

/// The phone chooses the size and the name of its newest file. Neither may hurt the laptop: a file
/// over the limit is refused before the pull, and a pull never replaces a file already in the inbox.
#[test]
fn pull_new_refuses_a_file_over_the_limit_and_never_overwrites_an_inbox_file() {
    run(async {
        let dir = tmp_dir("devtools-pull-limits");
        let h = start(DeviceMode::Full);
        h.daemon.set_max_pull_bytes(16);
        {
            let mut p = h.phone.lock().unwrap();
            p.listing = vec!["big.bin".into()];
            p.files.insert("/sdcard/d/big.bin".into(), vec![1; 17]);
        }
        let mut c = Client::leased(&h, POCO).await;
        let pull = json!({ "tool": "pull_new", "dir": "/sdcard/d", "poll_ms": 0 });
        // one byte over: refused, and adb pull never ran
        let r = c.tool(POCO, pull.clone()).await;
        assert_eq!(code(&r), REFUSED, "{r}");
        assert_eq!(r["error"]["data"]["class"], "inbox_size", "{r}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("at most 16 bytes"),
            "{r}"
        );
        assert!(
            !reached(&h).iter().any(|l| l.starts_with("pull ")),
            "{:?}",
            reached(&h)
        );
        assert_eq!(std::fs::read_dir(dir.join("inbox")).unwrap().count(), 0);
        // exactly the limit is taken
        {
            let mut p = h.phone.lock().unwrap();
            p.listing = vec!["report.txt".into()];
            p.files
                .insert("/sdcard/d/report.txt".into(), b"0123456789abcdef".to_vec());
        }
        let r = c.tool(POCO, pull.clone()).await;
        assert_eq!(r["result"]["size"], 16, "{r}");
        let first = PathBuf::from(r["result"]["path"].as_str().unwrap());
        assert_eq!(first, dir.join("inbox").join("report.txt"));
        // the phone now holds another file with the same name: the first pull is kept as it was
        h.phone.lock().unwrap().files.insert(
            "/sdcard/d/report.txt".into(),
            b"replaced by phone".to_vec()[..16].to_vec(),
        );
        let r = c.tool(POCO, pull.clone()).await;
        let second = PathBuf::from(r["result"]["path"].as_str().unwrap());
        assert_eq!(second, dir.join("inbox").join("report (1).txt"), "{r}");
        assert_eq!(std::fs::read(&first).unwrap(), b"0123456789abcdef");
        assert_eq!(std::fs::read(&second).unwrap(), b"replaced by phon");
        let r = c.tool(POCO, pull).await;
        assert!(
            r["result"]["path"]
                .as_str()
                .unwrap()
                .ends_with("report (2).txt"),
            "{r}"
        );
    });
}

/// The size limit is checked against what the phone says. A phone that says 4 bytes and then sends
/// 5000 (the file grew after the check, or the phone lies) gets an error, and nothing stays in the inbox.
#[test]
fn a_pull_bigger_than_the_phone_said_is_refused_and_deleted() {
    run(async {
        let dir = tmp_dir("devtools-pull-grew");
        let h = start(DeviceMode::Full);
        h.daemon.set_max_pull_bytes(16);
        {
            let mut p = h.phone.lock().unwrap();
            p.listing = vec!["big.bin".into()];
            p.files.insert("/sdcard/d/big.bin".into(), vec![1; 5000]);
            p.sizes = VecDeque::from([4, 4]);
        }
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "pull_new", "dir": "/sdcard/d", "poll_ms": 0, "local_dir": dir.join("inbox").to_string_lossy() })).await;
        assert_eq!(code(&r), DEVICE_ERROR, "{r}");
        assert!(
            r["error"]["message"]
                .as_str()
                .unwrap()
                .contains("the laptop copy was deleted"),
            "{r}"
        );
        assert!(
            reached(&h).iter().any(|l| l.starts_with("pull ")),
            "the pull ran: {:?}",
            reached(&h)
        );
        assert_eq!(
            std::fs::read_dir(dir.join("inbox")).unwrap().count(),
            0,
            "nothing is left in the inbox"
        );
    });
}

/// A copy whose sha256 does not match the phone's, a pull cut off half-way, and a pull that fails
/// before writing anything each end in an error and leave nothing in the inbox (not the partial
/// copy, not the reserved empty name). A file already in the inbox is left alone.
#[test]
fn a_sha256_mismatch_or_a_failed_pull_leaves_nothing_in_the_inbox() {
    run(async {
        let dir = tmp_dir("devtools-pull-fails");
        let inbox = dir.join("inbox");
        std::fs::write(inbox.join("keep.txt"), b"mine").unwrap();
        let h = start(DeviceMode::Full);
        let mut c = Client::leased(&h, POCO).await;
        let pull = json!({ "tool": "pull_new", "dir": "/sdcard/d", "poll_ms": 0 });
        let only_keep = || {
            let names: Vec<_> = std::fs::read_dir(&inbox)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, ["keep.txt"], "only the earlier file is left");
            assert_eq!(std::fs::read(inbox.join("keep.txt")).unwrap(), b"mine");
        };
        // the same size, another sha256
        {
            let mut p = h.phone.lock().unwrap();
            p.listing = vec!["shot.png".into()];
            p.files
                .insert("/sdcard/d/shot.png".into(), b"0123456789".to_vec());
            p.corrupt_pull = true;
        }
        let r = c.tool(POCO, pull.clone()).await;
        assert_eq!(code(&r), DEVICE_ERROR, "{r}");
        let m = r["error"]["message"].as_str().unwrap();
        assert!(
            m.contains("sha256 mismatch") && m.contains("the laptop copy was deleted"),
            "{r}"
        );
        only_keep();
        // cut off half-way: the partial copy goes too
        {
            let mut p = h.phone.lock().unwrap();
            p.corrupt_pull = false;
            p.break_pull = true;
        }
        let r = c.tool(POCO, pull.clone()).await;
        assert_eq!(code(&r), DEVICE_ERROR, "{r}");
        let m = r["error"]["message"].as_str().unwrap();
        assert!(
            m.contains("pull failed") && m.contains("the laptop copy was deleted"),
            "{r}"
        );
        only_keep();
        // the phone file is gone by the time adb pulls it: nothing was written, and the reserved
        // name is not left behind either
        {
            let mut p = h.phone.lock().unwrap();
            p.break_pull = false;
            p.files.clear();
            p.listing = vec!["gone.bin".into()];
            p.sizes = VecDeque::from([3, 3]);
        }
        let r = c.tool(POCO, pull).await;
        assert_eq!(code(&r), DEVICE_ERROR, "{r}");
        let m = r["error"]["message"].as_str().unwrap();
        assert!(
            m.contains("pull failed") && m.contains("nothing was left on the laptop"),
            "{r}"
        );
        assert!(!m.contains("was deleted"), "{r}");
        only_keep();
    });
}

/// adb pulls into a temporary name of the call's own, never into the reserved name: adb deletes
/// and re-creates its destination, and in that gap another pull could reserve the same name. The
/// reservation stays in place while adb writes; the checked copy is then renamed over it, and the
/// temporary file is gone.
#[test]
fn pull_new_pulls_into_a_temporary_name_and_renames_it_over_the_reservation() {
    run(async {
        let dir = tmp_dir("devtools-pull-temp");
        let inbox = dir.join("inbox");
        let h = start(DeviceMode::Full);
        {
            let mut p = h.phone.lock().unwrap();
            p.listing = vec!["shot.png".into()];
            p.files
                .insert("/sdcard/d/shot.png".into(), b"pixels".to_vec());
            p.sizes = VecDeque::from([6, 6]);
        }
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "pull_new", "dir": "/sdcard/d", "poll_ms": 0, "local_dir": inbox.to_string_lossy() })).await;
        let path = PathBuf::from(
            r["result"]["path"]
                .as_str()
                .unwrap_or_else(|| panic!("{r}")),
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"pixels");
        let (into, names_then) = h.phone.lock().unwrap().pulled_into[0].clone();
        assert_ne!(into, path, "adb was given the reserved name itself");
        assert_eq!(
            into.parent(),
            path.parent(),
            "the temporary file is in the inbox"
        );
        assert!(
            into.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".agentroid-pull-"),
            "{into:?}"
        );
        assert!(
            names_then.iter().any(|n| n == "shot.png"),
            "the reservation was in place while adb wrote: {names_then:?}"
        );
        assert!(!into.exists(), "the temporary file is gone");
        let names: Vec<_> = std::fs::read_dir(&inbox)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["shot.png"]);
    });
}

#[test]
fn a_phone_file_name_becomes_a_plain_laptop_file_name() {
    use bridged::devtools::local_name;
    // ordinary names are kept
    for n in [
        "IMG_0001.jpg",
        "new shot.png",
        "notes.tar.gz",
        "Makefile",
        "résumé.pdf",
    ] {
        assert_eq!(local_name(n), n);
    }
    // separators, drive and stream characters, control characters
    assert_eq!(
        local_name("..\\..\\Startup\\run.bat"),
        ".._.._Startup_run.bat"
    );
    assert_eq!(local_name("a/b:c*d?e\"f<g>h|i"), "a_b_c_d_e_f_g_h_i");
    assert_eq!(local_name("a\nb\tc"), "a_b_c");
    // invisible characters, bidi controls among them: this one would show as `invoiceexe.pdf`
    assert_eq!(local_name("invoice\u{202E}fdp.exe"), "invoice_fdp.exe");
    for c in [
        '\u{061C}',
        '\u{200B}',
        '\u{200E}',
        '\u{200F}',
        '\u{202A}',
        '\u{2066}',
        '\u{2069}',
        '\u{FEFF}',
        '\u{FE0F}',
        '\u{180B}',
        '\u{E0041}',
    ] {
        assert_eq!(
            local_name(&format!("a{c}b.txt")),
            "a_b.txt",
            "U+{:04X}",
            c as u32
        );
    }
    // Windows drops trailing dots and spaces, so they are dropped here first
    assert_eq!(local_name("report.txt. . "), "report.txt");
    assert_eq!(local_name(" . "), "pulled_file");
    for n in ["", ".", "..", "...", "   "] {
        assert_eq!(local_name(n), "pulled_file", "{n:?}");
    }
    // Windows device names never reach a device
    for (n, want) in [
        ("NUL", "_NUL"),
        ("nul.txt", "_nul.txt"),
        ("CON", "_CON"),
        ("Aux.log", "_Aux.log"),
        ("PRN.tar.gz", "_PRN.tar.gz"),
        ("COM1", "_COM1"),
        ("lpt9.txt", "_lpt9.txt"),
        ("NUL .txt", "_NUL .txt"),
    ] {
        assert_eq!(local_name(n), want);
    }
    for n in ["NULL", "CONSOLE.txt", "COM0", "COM10", "LPT", "command.com"] {
        assert_eq!(local_name(n), n, "{n} is not a device name");
    }
    // a very long name is cut and keeps its extension
    let long = format!("{}.jpeg", "x".repeat(400));
    let cut = local_name(&long);
    assert_eq!(cut.chars().count(), 120);
    assert!(cut.ends_with(".jpeg"));
}

#[test]
fn doctor_without_a_command_asks_the_phone_nothing_and_every_call_is_journalled() {
    run(async {
        let h = start(DeviceMode::ReadOnly);
        let mut c = Client::leased(&h, POCO).await;
        let r = c.tool(POCO, json!({ "tool": "doctor" })).await;
        assert!(
            r["result"]["text"]
                .as_str()
                .unwrap()
                .contains("mode=read_only"),
            "{r}"
        );
        c.diagnose(POCO, "settings put system a 1").await;
        no_command_reached(&h);
        let log = bridged::devtools::devtools_log();
        assert!(
            log.iter().any(|l| l.contains("doctor")
                && l.contains(POCO)
                && l.contains("refused")
                && l.contains("settings put")),
            "{log:?}"
        );
    });
}

/// Session stay-awake counts the devtools route too (12 Nov 2026: on the protected phone, a session of `doctor`
/// calls alone never held the screen, because only the tool route told stay-awake).
#[test]
fn a_doctor_call_holds_the_screen_awake_and_the_session_ending_restores_it() {
    run(async {
        let dir = tmp_dir("awake-doctor");
        let phone = care::fake::FakePhone::new(POCO);
        phone.state().tokens.push("192.0.2.50:37123".into()); // the fake core's adb serial for the POCO
        phone.set("system", "screen_off_timeout", "30000");
        phone.set("global", "stay_on_while_plugged_in", "0");
        let h = start_awake(
            DeviceMode::Full,
            bridged::awake::AwakeConfig {
                adb: bridged::jobs::DynRunner(Arc::new(phone.clone())),
                s24: true,
                leases: dir.join("leases.json"),
                clock: Arc::new(care::clock::FakeClock::new(care::clock::TEST_EPOCH_MS)),
                pids: Arc::new(care::pid::FakePids::default()),
                idle_ms: 60_000,
            },
        );
        let mut c = Client::leased(&h, POCO).await;
        let held = || {
            (
                phone.get("system", "screen_off_timeout"),
                phone.get("global", "stay_on_while_plugged_in"),
            )
        };
        assert_eq!(
            held(),
            (Some("30000".into()), Some("0".into())),
            "the lease alone holds nothing"
        );
        let r = c.diagnose(POCO, "dumpsys battery").await;
        assert!(r["result"].is_object(), "{r}");
        // the hold runs on stay-awake's worker, so the call did not wait for it
        for _ in 0..200 {
            if held().1.as_deref() == Some("7") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let notes = h.daemon.awake().unwrap().take_notes();
        assert_eq!(
            held(),
            (Some("1800000".into()), Some("7".into())),
            "{notes:?}"
        );
        drop(c); // the session ends: the restore runs on its own thread
        for _ in 0..200 {
            if held().0.as_deref() == Some("30000") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(held(), (Some("30000".into()), Some("0".into())));
    });
}
