//! The daemon: JSON-RPC 2.0 framing (one JSON document per line), dispatch, and the per-connection
//! task. A connection that breaks mid-call ends only its own task; its leases go with it.

use crate::jobs::{Jobs, JobsConfig};
use crate::link::{DaemonLink, LinkConfig};
use crate::listener::Listener;
use crate::proto::*;
use crate::registry::{AcquireError, Registry, ReleaseOutcome};
use crate::runner::engine::Driver;
use agentroid_adb_link::runner::Clock;
use agentroid_core::fileguard::FileOp;
use agentroid_core::gate::{self, Call, DeviceMode};
use agentroid_core::settle::{Settle, SettleOpts};
use agentroid_core::transport::{
    Core, CoreError, DeviceInfo, HealthHint, LockTicket, Outcome, Transport, TransportError,
};
use agentroid_core::tree::Bounds;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::{poll_fn, Future};
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::task::Poll;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tokio::task::JoinSet;

/// Longest accepted request line.
pub const MAX_LINE: usize = 1 << 20;

/// What the daemon needs from the device side. `Core` over any transport implements it through
/// [`CoreBackend`]; tests use the fake device.
pub trait DeviceBackend: Send {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError>;
    /// The mode the gate will apply to this device (the protected phone is always `read_only`).
    fn mode_of(&self, d: &DeviceInfo) -> DeviceMode;
    /// One core call, through core's gate.
    fn call(&mut self, serial: &str, call: &Call) -> Result<Outcome, CoreError>;
    /// Milliseconds on the backend's clock (the job runner's caps and timestamps; 2.10b).
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
            .unwrap_or(0)
    }
    /// Settle the screen with core's contract (2.10b). Default: no screen.
    fn settle(&mut self, _serial: &str, _opts: SettleOpts) -> Result<Settle, CoreError> {
        Err(CoreError::Invalid(
            "this backend cannot read a screen".into(),
        ))
    }
    /// Core's gate only, nothing served (2.10b). Default: refuse to admit anything.
    fn admit(&mut self, _serial: &str, _call: &Call) -> Result<(), CoreError> {
        Err(CoreError::Invalid("this backend has no gate".into()))
    }
    /// On the allowlist (handed to the `run_script` sandbox's own gate check; 2.10b).
    fn allowed(&self, _d: &DeviceInfo) -> bool {
        false
    }
    /// What the calls already served say about the phone (2.4 `health` member). MUST NOT send anything
    /// to the phone: the protected phone rule. Default: nothing known.
    fn health(&mut self, _d: &DeviceInfo) -> HealthHint {
        HealthHint::default()
    }
    /// The latest tree a call stored for this phone, with its `interactive_only`; nothing is read.
    fn latest_tree(&mut self, _d: &DeviceInfo) -> Option<(String, bool)> {
        None
    }
    /// What changed in the latest tree since snapshot `since` (an id from an earlier read's header).
    /// Reads nothing from the phone. Default: this backend keeps no snapshots.
    fn snapshot_diff(&mut self, _d: &DeviceInfo, _since: &str) -> Result<String, CoreError> {
        Err(CoreError::Invalid("this backend keeps no snapshots".into()))
    }
    /// One pixel of the screen, `x`,`y` in `space` (the phone half: decoding a screenshot; CP-D).
    /// The daemon has already passed the gate (a read) when this runs.
    fn pixel(
        &mut self,
        _d: &DeviceInfo,
        _x: f64,
        _y: f64,
        _space: Option<&str>,
    ) -> Result<[u8; 3], CoreError> {
        Err(CoreError::Invalid(PHONE_HALF.into()))
    }
    /// Crop the screen to `bounds` (device pixels); with `ocr`, read the crop as text on the laptop and
    /// return only the text. The phone half (CP-D); the gate has already passed.
    fn crop(&mut self, _d: &DeviceInfo, _bounds: Bounds, _ocr: bool) -> Result<CropOut, CoreError> {
        Err(CoreError::Invalid(PHONE_HALF.into()))
    }
    /// Raw transport, no core gate: only `devtools.rs` (2.4 `doctor`, `push`, `pull_new`) calls these,
    /// after its own gate check.
    fn raw_shell(&mut self, _serial: &str, _line: &str) -> Result<String, TransportError> {
        Err(TransportError("this backend has no shell".into()))
    }
    fn raw_push(
        &mut self,
        _serial: &str,
        _local: &std::path::Path,
        _remote: &str,
    ) -> Result<(), TransportError> {
        Err(TransportError("this backend cannot push files".into()))
    }
    fn raw_pull(
        &mut self,
        _serial: &str,
        _remote: &str,
        _local: &std::path::Path,
    ) -> Result<(), TransportError> {
        Err(TransportError("this backend cannot pull files".into()))
    }
    /// A raw shell line through core's device gate, shell guard, approval channel and audit log
    /// (D3b). `Ok` means the caller may send it; `Ok(Some(ticket))` means the screen lock admitted
    /// it, and the ticket goes to [`DeviceBackend::lock_done`] once it has run. Default: nothing
    /// is admitted.
    fn shell_gate(&mut self, _serial: &str, _line: &str) -> Result<Option<LockTicket>, CoreError> {
        Err(CoreError::Invalid("this backend has no shell guard".into()))
    }
    /// A `push` / `pull` of `phone_path` through core's device gate, file guard, approval channel
    /// and audit log (D3b). Default: nothing is admitted.
    fn file_gate(
        &mut self,
        _serial: &str,
        _op: FileOp,
        _phone_path: &str,
    ) -> Result<(), CoreError> {
        Err(CoreError::Invalid("this backend has no file guard".into()))
    }
    /// Record a shell line the `run_script` sandbox refused before it reached the bridge (D3b), so
    /// the audit log holds every refused command. Default: not recorded.
    fn shell_refused(&mut self, _serial: &str, _line: &str, _reason: &str) {}
    /// Core's screen lock alone, for a link segment of `steps` steps asking to live `expires_s`
    /// seconds (core does not send segments itself). `Ok(Some(ticket))`: the phone was let through
    /// because it is not in use now, so the segment is held to the short limits
    /// ([`agentroid_core::screenlock::FREE_SEGMENT_MAX_S`]); stamp the ticket finished and give it
    /// to [`DeviceBackend::lock_done`] once the work has run, or drop it if it never ran.
    /// Default: no lock, so nothing is refused.
    fn lock_admit_segment(
        &mut self,
        _serial: &str,
        _steps: usize,
        _expires_s: Option<u64>,
    ) -> Result<Option<LockTicket>, CoreError> {
        Ok(None)
    }
    /// The work `ticket` names ([`DeviceBackend::lock_admit_segment`], or a shell line through
    /// [`DeviceBackend::shell_gate`]) has run: its end is the agent's own activity. Default: none.
    fn lock_done(&mut self, _ticket: LockTicket) {}
}

/// Why a backend without the phone half answers `pixel_probe` and element crops with an error.
pub const PHONE_HALF: &str = "this needs the phone half (screenshot pixels and the OCR engine), which is not built into this bridged yet (CP-D)";

/// The answer of [`DeviceBackend::crop`].
#[derive(Debug, Clone, Default)]
pub struct CropOut {
    /// The recognised text (OCR crops).
    pub text: Option<String>,
    /// Size of the cropped image, for a crop without OCR.
    pub image_bytes: Option<usize>,
}

pub struct CoreBackend<T: Transport, C: Clock> {
    pub core: Core<T, C>,
    modes: HashMap<String, DeviceMode>,
}

impl<T: Transport, C: Clock> CoreBackend<T, C> {
    pub fn new(core: Core<T, C>) -> Self {
        CoreBackend {
            core,
            modes: HashMap::new(),
        }
    }
    /// Configure a device's mode (hardware or adb serial). The protected phone stays `read_only` regardless.
    pub fn set_mode(&mut self, serial: &str, mode: DeviceMode) {
        self.modes.insert(serial.to_string(), mode);
        self.core.set_mode(serial, mode);
    }
}

impl<T: Transport + Send, C: Clock + Send> DeviceBackend for CoreBackend<T, C> {
    fn devices(&mut self) -> Result<Vec<DeviceInfo>, TransportError> {
        self.core.transport.devices()
    }
    fn mode_of(&self, d: &DeviceInfo) -> DeviceMode {
        let configured = self
            .modes
            .get(&d.hw_serial)
            .or_else(|| self.modes.get(&d.serial))
            .copied()
            .unwrap_or_default();
        gate::effective_mode(&[&d.serial, &d.hw_serial], configured)
    }
    fn call(&mut self, serial: &str, call: &Call) -> Result<Outcome, CoreError> {
        self.core.call(Some(serial), call)
    }
    fn now_ms(&self) -> u64 {
        self.core.clock().now_ms()
    }
    fn settle(&mut self, serial: &str, opts: SettleOpts) -> Result<Settle, CoreError> {
        self.core.settle(Some(serial), opts)
    }
    fn admit(&mut self, serial: &str, call: &Call) -> Result<(), CoreError> {
        self.core.admit(Some(serial), call)
    }
    fn allowed(&self, d: &DeviceInfo) -> bool {
        self.core.allowed(d)
    }
    fn health(&mut self, d: &DeviceInfo) -> HealthHint {
        self.core.health_of(d)
    }
    fn latest_tree(&mut self, d: &DeviceInfo) -> Option<(String, bool)> {
        self.core.latest_tree(&d.serial)
    }
    fn snapshot_diff(&mut self, d: &DeviceInfo, since: &str) -> Result<String, CoreError> {
        self.core.diff_since(&d.serial, since)
    }
    fn raw_shell(&mut self, serial: &str, line: &str) -> Result<String, TransportError> {
        self.core.transport.shell(serial, line)
    }
    fn raw_push(
        &mut self,
        serial: &str,
        local: &std::path::Path,
        remote: &str,
    ) -> Result<(), TransportError> {
        self.core.transport.push(serial, local, remote)
    }
    fn raw_pull(
        &mut self,
        serial: &str,
        remote: &str,
        local: &std::path::Path,
    ) -> Result<(), TransportError> {
        self.core.transport.pull(serial, remote, local)
    }
    fn shell_gate(&mut self, serial: &str, line: &str) -> Result<Option<LockTicket>, CoreError> {
        self.core.shell_gate(Some(serial), line)
    }
    fn file_gate(&mut self, serial: &str, op: FileOp, phone_path: &str) -> Result<(), CoreError> {
        self.core.file_gate(Some(serial), op, phone_path)
    }
    fn shell_refused(&mut self, serial: &str, line: &str, reason: &str) {
        self.core.record_shell_refusal(Some(serial), line, reason);
    }
    fn lock_admit_segment(
        &mut self,
        serial: &str,
        steps: usize,
        expires_s: Option<u64>,
    ) -> Result<Option<LockTicket>, CoreError> {
        self.core.lock_admit_segment(Some(serial), steps, expires_s)
    }
    fn lock_done(&mut self, ticket: LockTicket) {
        self.core.lock_done(ticket);
    }
}

pub struct Daemon {
    /// Shared with the link hub, whose epoch source it is (one epoch counter).
    pub registry: Arc<Registry>,
    backend: Mutex<Box<dyn DeviceBackend>>,
    next_conn: AtomicU64,
    /// Jobs and `run_script` (2.10b). `None` config: the job methods answer `STATE_ERROR`.
    pub(crate) jobs: Jobs,
    pub(crate) jobs_cfg: Option<JobsConfig>,
    /// Human names for phones (the header's `alias`), by device key.
    pub(crate) aliases: Mutex<HashMap<String, String>>,
    /// The link hub (2.12), once attached. Not attached: the `link.*` methods answer `STATE_ERROR`.
    pub(crate) link: OnceLock<DaemonLink>,
    /// How `require` and `verify` wait between reads (the backend lock is free while they do). Real time unless a test sets it.
    pub(crate) waits: OnceLock<crate::devcall::Waits>,
    /// The laptop folder `push` reads from (`outbox`) and `pull_new` writes to (`inbox`).
    /// Unset: `devtools::default_files_dir()`. `bridged` sets `<state dir>/files`.
    pub(crate) files_dir: OnceLock<PathBuf>,
    /// Session stay-awake (`crate::awake`), once attached (live adb only).
    pub(crate) awake: OnceLock<Arc<crate::awake::SessionAwake>>,
    /// The `Arc` this daemon lives in, so a job thread can own a handle to it.
    me: Weak<Daemon>,
}

/// [`Driver`] over the daemon's shared backend: the lock is taken per engine call, so other
/// connections are served between the steps of a running job.
pub(crate) struct SharedDriver<'a> {
    pub daemon: &'a Daemon,
}

fn need_serial(serial: Option<&str>) -> Result<&str, CoreError> {
    serial
        .ok_or_else(|| CoreError::Invalid("the daemon's job runner always names its device".into()))
}

impl Driver for SharedDriver<'_> {
    fn now_ms(&self) -> u64 {
        self.daemon.backend().now_ms()
    }
    fn settle(&mut self, serial: Option<&str>, opts: SettleOpts) -> Result<Settle, CoreError> {
        let s = need_serial(serial)?;
        self.daemon.backend().settle(s, opts)
    }
    fn admit(&mut self, serial: Option<&str>, call: &Call) -> Result<(), CoreError> {
        let s = need_serial(serial)?;
        self.daemon.backend().admit(s, call)
    }
    fn call(&mut self, serial: Option<&str>, call: &Call) -> Result<Outcome, CoreError> {
        let s = need_serial(serial)?;
        self.daemon.backend().call(s, call)
    }
}

/// Per-connection state.
struct Conn {
    id: u64,
    session: Option<String>,
}

impl Conn {
    fn holder(&self) -> String {
        self.session
            .clone()
            .unwrap_or_else(|| format!("conn-{}", self.id))
    }
}

/// Releases a connection's leases however its task ends (EOF, broken pipe, abort, panic).
struct ConnGuard<'a> {
    daemon: &'a Daemon,
    id: u64,
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        self.daemon.registry.release_conn(self.id);
        if let Some(a) = self.daemon.awake.get() {
            // the restore talks to the phone: queued on stay-awake's worker, after this session's hold
            a.leave_later(self.id);
        }
    }
}

pub(crate) fn device_key(d: &DeviceInfo) -> String {
    if d.hw_serial.is_empty() {
        d.serial.clone()
    } else {
        d.hw_serial.clone()
    }
}

/// Can this device be leased: on the allowlist, or the protected phone (leasable because its mode is always
/// read_only and only its ruled `getprop` reads get through). `devices.list` reports the same answer.
fn may_lease(d: &DeviceInfo, on_allowlist: bool) -> bool {
    gate::is_s24(&d.serial) || gate::is_s24(&d.hw_serial) || on_allowlist
}

pub(crate) fn parse_params<P: DeserializeOwned>(params: &Value) -> Result<P, RpcError> {
    serde_json::from_value(params.clone())
        .map_err(|e| RpcError::new(INVALID_PARAMS, format!("invalid params: {e}")))
}

pub(crate) fn to_value<R: serde::Serialize>(r: &R) -> Result<Value, RpcError> {
    serde_json::to_value(r).map_err(|e| RpcError::new(INTERNAL_ERROR, e.to_string()))
}

fn lease_data(l: &Lease) -> Value {
    serde_json::to_value(l).unwrap_or(Value::Null)
}

impl Daemon {
    /// A daemon without a jobs directory (the job methods answer `STATE_ERROR`).
    pub fn new(registry: Registry, backend: Box<dyn DeviceBackend>) -> Arc<Daemon> {
        Daemon::with_jobs(registry, backend, None)
    }

    /// A daemon whose jobs keep their journals under `jobs.dir` (2.10b).
    pub fn with_jobs(
        registry: Registry,
        backend: Box<dyn DeviceBackend>,
        jobs: Option<JobsConfig>,
    ) -> Arc<Daemon> {
        Arc::new_cyclic(|me| Daemon {
            registry: Arc::new(registry),
            backend: Mutex::new(backend),
            next_conn: AtomicU64::new(1),
            jobs: Jobs::default(),
            jobs_cfg: jobs,
            aliases: Mutex::new(HashMap::new()),
            link: OnceLock::new(),
            waits: OnceLock::new(),
            files_dir: OnceLock::new(),
            awake: OnceLock::new(),
            me: me.clone(),
        })
    }

    /// Start the link hub and attach it (once). Its epochs come from this daemon's registry.
    pub fn attach_link(&self, cfg: LinkConfig) -> Result<&DaemonLink, String> {
        if self.link.get().is_some() {
            return Err("a link hub is already attached".into());
        }
        let link = DaemonLink::start(cfg, self.registry.clone())?;
        // the overlay's human touches (no adb needed) go to the take-over machine of the job there
        let me = self.me.clone();
        let _lagged = link.feed_human_touches(move |serial, touch_us, sent_us| {
            if let Some(d) = me.upgrade() {
                let _ = d.phone_touch(
                    serial,
                    handover::touch::PhoneClock::Elapsed,
                    touch_us,
                    sent_us,
                    handover::mode::Source::Overlay,
                );
            }
        });
        self.link
            .set(link)
            .map_err(|_| "a link hub is already attached".to_string())?;
        self.link
            .get()
            .ok_or_else(|| "link not attached".to_string())
    }

    /// Set the laptop folder for `push` (its `outbox`) and `pull_new` (its `inbox`). Once; later calls are ignored.
    pub fn set_files_dir(&self, dir: PathBuf) {
        let _ = self.files_dir.set(dir);
    }

    /// The attached link hub.
    pub fn link(&self) -> Option<&DaemonLink> {
        self.link.get()
    }

    /// care's settings leases in force on a phone (`leases.json`; empty without care or stay-awake).
    pub fn changes_in_force(&self, key: &str) -> Vec<care::settings::ChangeInForce> {
        let path = match (
            self.jobs_cfg.as_ref().and_then(|c| c.care.as_ref()),
            self.awake.get(),
        ) {
            (Some(care), _) => care.leases.clone(),
            (None, Some(a)) => a.leases_path().to_path_buf(),
            (None, None) => return vec![],
        };
        crate::jobcare::changes_in_force(&path, key).unwrap_or_default()
    }

    /// Attach session stay-awake (once) and start its idle sweep.
    pub fn attach_awake(
        &self,
        cfg: crate::awake::AwakeConfig,
        sweep_every: std::time::Duration,
    ) -> Result<(), String> {
        let a = Arc::new(crate::awake::SessionAwake::new(cfg));
        a.start_worker();
        crate::awake::spawn_sweeper(Arc::downgrade(&a), sweep_every);
        self.awake
            .set(a)
            .map_err(|_| "stay-awake is already attached".to_string())
    }

    /// Stay-awake, if attached.
    pub fn awake(&self) -> Option<&Arc<crate::awake::SessionAwake>> {
        self.awake.get()
    }

    fn change_rows(&self, key: &str) -> Vec<ChangeRow> {
        self.changes_in_force(key)
            .iter()
            .map(|c| ChangeRow {
                key: c.key.clone(),
                ns: c.ns.clone(),
                before: c.before.clone(),
                applied: c.applied.clone(),
                owner: ChangeOwner {
                    pid: c.owner.pid,
                    job_id: c.owner.job_id.clone(),
                    session: c.owner.session.clone(),
                },
                scope: c.scope.as_str().to_string(),
                expires: c.expires.clone(),
                ts: c.ts.clone(),
            })
            .collect()
    }

    /// An owning handle to this daemon (for job threads).
    pub(crate) fn handle(&self) -> Option<Arc<Daemon>> {
        self.me.upgrade()
    }

    pub(crate) fn backend(&self) -> std::sync::MutexGuard<'_, Box<dyn DeviceBackend>> {
        self.backend
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether no call holds the backend right now (`require` and `verify` wait with it free). A test hook.
    #[doc(hidden)]
    pub fn backend_is_free(&self) -> bool {
        self.backend.try_lock().is_ok()
    }

    fn list(&self) -> Result<Vec<DeviceInfo>, RpcError> {
        self.backend()
            .devices()
            .map_err(|e| RpcError::new(DEVICE_ERROR, e.to_string()))
    }

    /// The lease rule every device and job method shares: `conn` must hold `key`.
    pub(crate) fn require_holder(&self, key: &str, conn: u64) -> Result<(), RpcError> {
        match self.registry.check_holder(key, conn) {
            Ok(()) => Ok(()),
            Err(Some(l)) => Err(RpcError::new(
                NOT_HOLDER,
                format!("device {} is held by {}, not by you", l.device, l.holder),
            )
            .with_data(lease_data(&l))),
            Err(None) => Err(RpcError::new(
                NOT_HOLDER,
                format!("you do not hold a lease on {key}; call lease.acquire first"),
            )),
        }
    }

    /// A lease, and so a link segment, is for a device on the allowlist. The protected phone is leasable
    /// because its mode is always read_only; every other unlisted device is refused.
    pub(crate) fn require_allowed(&self, d: &DeviceInfo) -> Result<(), RpcError> {
        if may_lease(d, self.backend().allowed(d)) {
            return Ok(());
        }
        Err(RpcError::new(REFUSED, format!("{} is not on the allowlist. Start bridged with --allow-serial <hardware serial> (or list it in allow.txt) to drive it.", d.serial)))
    }

    /// Match a device by adb serial or hardware serial.
    pub(crate) fn resolve(&self, name: &str) -> Result<DeviceInfo, RpcError> {
        let devices = self.list()?;
        devices
            .into_iter()
            .find(|d| d.serial == name || d.hw_serial == name)
            .ok_or_else(|| {
                RpcError::new(
                    UNKNOWN_DEVICE,
                    format!("no connected device matches {name:?}"),
                )
            })
    }

    /// Run one request line; `None` when nothing is to be sent back (notifications).
    fn handle_line(&self, conn: &mut Conn, line: &[u8]) -> Option<String> {
        let parsed: Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(encode(&RpcResponse::err(
                    Value::Null,
                    RpcError::new(PARSE_ERROR, format!("parse error: {e}")),
                )))
            }
        };
        match parsed {
            Value::Array(items) if items.is_empty() => Some(encode(&RpcResponse::err(
                Value::Null,
                RpcError::new(INVALID_REQUEST, "empty batch"),
            ))),
            Value::Array(items) => {
                let responses: Vec<RpcResponse> = items
                    .into_iter()
                    .filter_map(|v| self.handle_one(conn, v))
                    .collect();
                if responses.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&responses).unwrap_or_else(|_| "[]".into()))
                }
            }
            other => self.handle_one(conn, other).map(|r| encode(&r)),
        }
    }

    fn handle_one(&self, conn: &mut Conn, v: Value) -> Option<RpcResponse> {
        let Value::Object(obj) = v else {
            return Some(RpcResponse::err(
                Value::Null,
                RpcError::new(INVALID_REQUEST, "request must be a JSON object"),
            ));
        };
        let id = match obj.get("id") {
            None => None,
            Some(i @ (Value::Null | Value::String(_) | Value::Number(_))) => Some(i.clone()),
            Some(_) => {
                return Some(RpcResponse::err(
                    Value::Null,
                    RpcError::new(INVALID_REQUEST, "id must be a string, a number or null"),
                ))
            }
        };
        let reply_id = id.clone().unwrap_or(Value::Null);
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Some(RpcResponse::err(
                reply_id,
                RpcError::new(INVALID_REQUEST, "jsonrpc must be \"2.0\""),
            ));
        }
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            return Some(RpcResponse::err(
                reply_id,
                RpcError::new(INVALID_REQUEST, "method must be a string"),
            ));
        };
        let params = match obj.get("params") {
            None | Some(Value::Null) => json!({}),
            Some(p @ Value::Object(_)) => p.clone(),
            Some(_) => {
                return id.map(|id| {
                    RpcResponse::err(
                        id,
                        RpcError::new(INVALID_PARAMS, "params must be an object (by-name)"),
                    )
                });
            }
        };
        let result = self.dispatch(conn, method, &params);
        let id = id?; // a notification gets no response, whatever happened
        Some(match result {
            Ok(v) => RpcResponse::ok(id, v),
            Err(e) => RpcResponse::err(id, e),
        })
    }

    fn dispatch(&self, conn: &mut Conn, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "hello" => {
                let p: HelloParams = parse_params(params)?;
                if let Some(s) = p.session.filter(|s| !s.trim().is_empty()) {
                    conn.session = Some(s.chars().take(80).collect());
                }
                let mut capabilities: Vec<String> =
                    METHODS.iter().map(|m| (*m).to_string()).collect();
                capabilities.extend(["lease:single-holder".to_string(), "gate:device-mode".to_string(), "tools:look,screenshot,ui_tree,tap,long_press,swipe,scroll,type_text,key,unlock,open_app,open_url,start_activity,list_apps,wait,getprop,set_value,pixel_probe".to_string(), "call:require,retry,verify,health".to_string(),"jobs:journal,resume,receipt,run_script".to_string()]);
                capabilities.extend([
                    "care:heat,floor,leases".to_string(),
                    "handover:take_over".to_string(),
                ]);
                if self.link.get().is_some() {
                    capabilities.push("link:hub,segments".to_string());
                }
                to_value(&HelloResult {
                    name: "bridged".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    protocol: "2.0".into(),
                    capabilities,
                    holder: conn.holder(),
                })
            }
            "devices.list" => {
                let _: DevicesListParams = parse_params(params)?;
                let mut backend = self.backend();
                let devices = backend
                    .devices()
                    .map_err(|e| RpcError::new(DEVICE_ERROR, e.to_string()))?;
                let rows = devices
                    .iter()
                    .map(|d| {
                        let key = device_key(d);
                        let changes_in_force = self.change_rows(&key);
                        DeviceRow {
                            holder: self.registry.lease_of(&key).map(|l| l.holder),
                            device: key,
                            serial: d.serial.clone(),
                            hw_serial: d.hw_serial.clone(),
                            model: d.model.clone(),
                            mode: backend.mode_of(d).as_str().to_string(),
                            allowed: may_lease(d, backend.allowed(d)),
                            changes_in_force,
                        }
                    })
                    .collect();
                to_value(&DevicesListResult { devices: rows })
            }
            "lease.acquire" => {
                let p: LeaseAcquireParams = parse_params(params)?;
                let d = self.resolve(&p.device)?;
                self.require_allowed(&d)?;
                let key = device_key(&d);
                match self.registry.acquire(&key, conn.id, &conn.holder()) {
                    Ok(lease) => {
                        // one epoch source: a paired phone's link moves to the new lease's epoch
                        if let Some(link) = self.link.get() {
                            link.on_lease(&key, lease.epoch);
                        }
                        to_value(&lease)
                    }
                    Err(AcquireError::Held(l)) => Err(RpcError::new(
                        LEASE_HELD,
                        format!(
                            "device {} is held by {} (epoch {})",
                            l.device, l.holder, l.epoch
                        ),
                    )
                    .with_data(lease_data(&l))),
                    Err(AcquireError::Persist(e)) => Err(RpcError::new(
                        STATE_ERROR,
                        format!("lease not granted, state file not writable: {e}"),
                    )),
                }
            }
            "lease.release" => {
                let p: LeaseReleaseParams = parse_params(params)?;
                // A device that has since vanished can still be released by the name it was leased under.
                let key = self
                    .resolve(&p.device)
                    .map(|d| device_key(&d))
                    .unwrap_or(p.device);
                match self.registry.release(&key, conn.id) {
                    ReleaseOutcome::Released => to_value(&LeaseReleaseResult { released: true }),
                    ReleaseOutcome::NotLeased => to_value(&LeaseReleaseResult { released: false }),
                    ReleaseOutcome::NotHolder(l) => Err(RpcError::new(
                        NOT_HOLDER,
                        format!("device {} is held by {}, not by you", l.device, l.holder),
                    )
                    .with_data(lease_data(&l))),
                }
            }
            "lease.status" => {
                let p: LeaseStatusParams = parse_params(params)?;
                let key = p
                    .device
                    .map(|d| self.resolve(&d).map(|i| device_key(&i)).unwrap_or(d));
                to_value(&LeaseStatusResult {
                    leases: self.registry.status(key.as_deref()),
                })
            }
            "device.call" if crate::devtools::claims(params) => self.devtools_call(conn.id, params),
            "device.call" => self.device_call(conn.id, params),
            m if m.starts_with("job.") && METHODS.contains(&m) => {
                self.job_dispatch(conn.id, m, params)
            }
            m if m.starts_with("link.") && METHODS.contains(&m) => {
                self.link_dispatch(conn.id, m, params)
            }
            other => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            )),
        }
    }
}

fn encode(r: &RpcResponse) -> String {
    serde_json::to_string(r).unwrap_or_else(|_| {
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"encode failed"}}"#.into()
    })
}

// ---- small select without tokio's `macros` feature ----

enum Either<A, B> {
    Left(A),
    Right(B),
}

/// First of two futures to finish (the first is polled first).
async fn race<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let mut a = pin!(a);
    let mut b = pin!(b);
    poll_fn(|cx| {
        if let Poll::Ready(v) = a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(v));
        }
        if let Poll::Ready(v) = b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(v));
        }
        Poll::Pending
    })
    .await
}

/// Completes once shutdown was requested (or the trigger is gone).
pub async fn wait_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

enum Line {
    Eof,
    TooLong,
    Data,
}

async fn read_line<R: AsyncBufReadExt + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Line> {
    buf.clear();
    let n = (&mut *r)
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', buf)
        .await?;
    if n == 0 {
        return Ok(Line::Eof);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        return Ok(if buf.len() > MAX_LINE {
            Line::TooLong
        } else {
            Line::Data
        });
    }
    // No newline: either over the limit, or the peer vanished mid-line (a torn request is dropped).
    Ok(if buf.len() > MAX_LINE {
        Line::TooLong
    } else {
        Line::Eof
    })
}

/// Serve one client until it goes away or shutdown is requested. Never panics on I/O errors.
pub async fn serve_conn<S: AsyncRead + AsyncWrite + Unpin>(
    daemon: &Daemon,
    stream: S,
    mut shutdown: watch::Receiver<bool>,
) {
    let id = daemon.next_conn.fetch_add(1, Ordering::Relaxed);
    let _guard = ConnGuard { daemon, id };
    let mut conn = Conn { id, session: None };
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    let mut buf = Vec::new();
    loop {
        let line = match race(wait_shutdown(&mut shutdown), read_line(&mut rd, &mut buf)).await {
            Either::Left(()) => break,
            Either::Right(Ok(l)) => l,
            Either::Right(Err(_)) => break, // pipe broken: this connection only
        };
        let reply = match line {
            Line::Eof => break,
            Line::TooLong => {
                let r = RpcResponse::err(
                    Value::Null,
                    RpcError::new(
                        INVALID_REQUEST,
                        format!("request longer than {MAX_LINE} bytes"),
                    ),
                );
                let _ = wr.write_all((encode(&r) + "\n").as_bytes()).await;
                break; // cannot resync inside an oversized line
            }
            Line::Data if buf.iter().all(u8::is_ascii_whitespace) => continue,
            Line::Data => daemon.handle_line(&mut conn, &buf),
        };
        if let Some(mut text) = reply {
            text.push('\n');
            if wr.write_all(text.as_bytes()).await.is_err() || wr.flush().await.is_err() {
                break; // client vanished mid-call; the guard frees its leases
            }
        }
    }
}

/// Accept clients until shutdown, one task per connection. On shutdown the connections get a short
/// grace period, then are aborted (their guards still release the leases).
pub async fn serve<L: Listener>(
    daemon: Arc<Daemon>,
    mut listener: L,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let mut conns: JoinSet<()> = JoinSet::new();
    let mut failures = 0u32;
    loop {
        while conns.try_join_next().is_some() {}
        match race(wait_shutdown(&mut shutdown), listener.accept()).await {
            Either::Left(()) => break,
            Either::Right(Ok(stream)) => {
                failures = 0;
                let d = daemon.clone();
                let rx = shutdown.clone();
                conns.spawn(async move { serve_conn(&d, stream, rx).await });
            }
            Either::Right(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break, // listener closed
            Either::Right(Err(e)) => {
                failures += 1;
                if failures >= 5 {
                    conns.abort_all();
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    let drain = async { while conns.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(2), drain)
        .await
        .is_err()
    {
        conns.abort_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(serial: &str) -> DeviceInfo {
        DeviceInfo {
            serial: serial.to_string(),
            model: "m".to_string(),
            hw_serial: serial.to_string(),
        }
    }

    #[test]
    fn may_lease_is_the_allowlist_or_the_protected_phone() {
        assert!(
            may_lease(&dev("PROTECTED0000EXAMPLE"), false),
            "the protected phone is leasable (read_only) without being listed"
        );
        assert!(
            may_lease(&dev("DEVICE0000EXAMPLE"), true),
            "a listed phone is leasable"
        );
        assert!(
            !may_lease(&dev("DEVICE0000EXAMPLE"), false),
            "an unlisted phone is refused"
        );
    }
}
