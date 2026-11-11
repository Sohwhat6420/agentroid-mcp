//! The hub: a WebSocket listener at `/helper` on one tailnet address, the challenge handshake,
//! and per-device sessions (epoch, sequenced requests with resume, request-id dedupe, heartbeat
//! bookkeeping). The reference it ports is `android/link/src/test/.../FakeHub.java`; the phone's
//! side is `LinkClient.java`.
//!
//! The daemon calls [`Hub::send_request`], [`Hub::status`] and [`Hub::events`]. Nothing here
//! dials a phone: the only socket is the listener, and the phone connects to it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{Map, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as WsRequest, Response as WsResponse,
};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::bind::{check_bind_address, check_bind_address_lan, BindRefused};
use crate::clock::{Clock, SystemClock};
use crate::crypto::{
    b64, hex, hub_challenge_message, new_nonce, phone_response_message, random_bytes, sha256,
    verify, HubKey,
};
use crate::proto::{
    self, fix_for_display, fix_text, is_method, is_request_id, Heartbeat, PhoneFrame,
};
use crate::store::{
    Binding, CachedResponse, ResponseCache, StateDir, StoreError, RESPONSE_CACHE_LIMIT,
    RESPONSE_MAX_AGE_MS,
};

/// The only path the listener answers.
pub const PATH: &str = "/helper";
/// A phone must send hello, and answer the challenge, within 15 s (LINK-REPORT item 3).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// No heartbeat for 10 minutes: the device is offline.
pub const OFFLINE_AFTER_MS: u64 = 10 * 60 * 1000;
/// Unacked requests in flight per device.
pub const DEFAULT_WINDOW: usize = 64;
/// Ack phone frames at least every 32 frames (and on a 250 ms tick; link.json asks for 64 / 1 s).
pub const ACK_EVERY: u64 = 32;
const ACK_TICK: Duration = Duration::from_millis(250);
/// Hub-side liveness: the slowest heartbeat is 60 s, so 2 x 60 s + 15 s of silence drops the socket.
const SILENCE_LIMIT: Duration = Duration::from_secs(135);
/// Connections that have not finished the handshake.
const MAX_HANDSHAKES: usize = 32;
/// untrusted_bssid reports kept per device.
const UNTRUSTED_KEEP: usize = 16;
/// Gate refusals mean "nothing ran": they are not cached, so the same id may run later
/// (`LinkClient.cacheable`: NEEDS_REPAIR, AUTH_REQUIRED, NOTIFICATION_HIDDEN). -32011 is the phone's TRANSIENT code
/// (`ErrorCodes.TRANSIENT`: runner busy, report not ready yet, runner did not answer in time): an answer that must
/// not pin its request id, so it is not cached here either.
const UNCACHEABLE_CODES: [i64; 4] = [-32001, -32002, -32003, -32011];

// ------------------------------------------------------------------ public types

/// Where a hub embedded in the daemon gets its epochs: the 2.8 lease registry, so a lease epoch
/// and the link epoch are one number (one epoch source). Without one, the hub allocates from
/// `hub-state/epochs.json` (the standalone `agentroid-link` CLI).
pub trait EpochSource: Send + Sync {
    /// The epoch this hub should use for `serial` now. It must be strictly greater than `floor`
    /// (the last epoch persisted for the serial in `hub-state/epochs.json`). The hub persists the
    /// answer there before using it.
    fn epoch_for(&self, serial: &str, floor: u64) -> Result<u64, String>;
}

/// Which cached answers must not be pruned: `(serial, request id)`. The daemon marks the answers
/// of segments that are still running.
pub type KeepCached = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// How the response (and so the segment) cache is pruned while the hub runs (wiring 2, 25 Oct):
/// by age AND by count, at hub start, when a new answer is written, and on the sweeper's tick, but
/// at most once per `every_ms` per phone (the hub's monotonic clock; the age is the wall clock's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    pub max_age_ms: u64,
    pub max_count: usize,
    pub every_ms: u64,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            max_age_ms: RESPONSE_MAX_AGE_MS,
            max_count: RESPONSE_CACHE_LIMIT,
            every_ms: 10 * 60 * 1000,
        }
    }
}

pub struct HubConfig {
    /// Base directory; `hub-state/` is created under it.
    pub state_dir: PathBuf,
    /// One literal tailnet IPv4 address (or 127.0.0.1 in tests): see [`check_bind_address`].
    pub bind: String,
    /// 0 picks a free port (tests).
    pub port: u16,
    pub key: HubKey,
    pub clock: Arc<dyn Clock>,
    pub handshake_timeout: Duration,
    pub window: usize,
    /// `None`: epochs come from `hub-state/epochs.json` alone.
    pub epochs: Option<Arc<dyn EpochSource>>,
    pub cache: CachePolicy,
    /// `None`: every old answer may be pruned.
    pub keep_cached: Option<KeepCached>,
    /// `Some`: serve `wss://` (TLS 1.3, the certificate the pairing QR pins). Required with `lan`.
    pub tls: Option<Arc<crate::tls::HubTls>>,
    /// `--link-lan`: also accept a private LAN IPv4 (RFC 1918) as the bind address; only together with `tls`.
    pub lan: bool,
}

impl HubConfig {
    pub fn new(
        state_dir: impl Into<PathBuf>,
        bind: impl Into<String>,
        port: u16,
        key: HubKey,
    ) -> Self {
        Self {
            state_dir: state_dir.into(),
            bind: bind.into(),
            port,
            key,
            clock: Arc::new(SystemClock::new()),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            window: DEFAULT_WINDOW,
            epochs: None,
            cache: CachePolicy::default(),
            keep_cached: None,
            tls: None,
            lan: false,
        }
    }
}

/// A phone's answer to one request.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: String,
    /// The JSON-RPC response object (`result` or `error`).
    pub rpc: Value,
    /// The phone answered from its request-id cache: nothing ran a second time.
    pub replayed: bool,
    /// The hub answered from its own request-id cache: nothing was even sent.
    pub from_hub_cache: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubError {
    /// No binding for this serial: pair it first.
    UnknownDevice(String),
    /// The payload is not `{"method": ..., "params"?: {...}, "id"?: ...}`.
    BadPayload(String),
    /// This request id was already used for a different call.
    IdReused(String),
    /// Epoch fencing: the caller's epoch is not the device's current one. Nothing was sent.
    StaleEpoch {
        requested: u64,
        current: u64,
    },
    /// A hub with a newer epoch owns this phone; this hub must stop (link.json stale_epoch).
    Superseded {
        phone_epoch: u64,
    },
    /// The phone process restarted before it answered: the request may or may not have run.
    OutcomeUnknown(String),
    /// The phone refused the request without running it.
    Refused(String),
    Store(String),
    /// The hub is shutting down.
    Shutdown,
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubError::UnknownDevice(s) => {
                write!(f, "no device {s:?} is paired; run: agentroid link pair {s}")
            }
            HubError::BadPayload(m) => write!(f, "bad request payload: {m}"),
            HubError::IdReused(id) => {
                write!(f, "request id {id:?} was already used for a different call")
            }
            HubError::StaleEpoch { requested, current } => {
                write!(
                    f,
                    "stale epoch {requested}: the device's epoch is {current}; nothing was sent"
                )
            }
            HubError::Superseded { phone_epoch } => {
                write!(
                    f,
                    "a newer hub (epoch {phone_epoch}) owns this phone; stop this hub process"
                )
            }
            HubError::OutcomeUnknown(id) => write!(
                f,
                "the phone restarted before answering {id:?}; it may or may not have run"
            ),
            HubError::Refused(m) => write!(f, "the phone refused the request: {m}"),
            HubError::Store(m) => write!(f, "{m}"),
            HubError::Shutdown => write!(f, "the hub is shutting down"),
        }
    }
}

impl std::error::Error for HubError {}

impl From<StoreError> for HubError {
    fn from(e: StoreError) -> Self {
        HubError::Store(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HubEvent {
    /// The phone passed the challenge on a new connection.
    Connected {
        serial: String,
        epoch: u64,
        instance: String,
    },
    Disconnected {
        serial: String,
    },
    /// A new phone process: its stream and cache are gone; these requests' outcomes are unknown.
    PhoneRestarted {
        serial: String,
        outcome_unknown: Vec<String>,
    },
    /// A failed challenge (either side). `fix` has `<serial>` replaced when the serial is known.
    NeedsRepair {
        serial: Option<String>,
        key_id: String,
        from: String,
        reason: String,
        fix: String,
    },
    Heartbeat {
        serial: String,
        heartbeat: Heartbeat,
    },
    AdbPortChanged {
        serial: String,
        port: Option<u16>,
    },
    UntrustedBssid {
        serial: String,
        bssid: Option<String>,
        reason: String,
        ts_ms: u64,
    },
    /// The phone's overlay saw a human touch. Both times are the phone's elapsedRealtime (ms):
    /// when it happened and when the frame was made. The daemon dates it on its own clock
    /// (`Daemon::phone_touch`, `PhoneClock::Elapsed`).
    HumanTouch {
        serial: String,
        touch_ms: u64,
        sent_ms: u64,
    },
    /// No heartbeat for 10 minutes.
    Offline {
        serial: String,
    },
    /// A newer hub owns the phone.
    Superseded {
        serial: String,
        phone_epoch: u64,
    },
    ProtocolError {
        serial: Option<String>,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repair {
    pub from: String,
    pub reason: String,
    /// The exact fix text with `<serial>` replaced.
    pub fix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedReport {
    pub bssid: Option<String>,
    pub reason: String,
    pub ts_ms: u64,
    /// Hub clock when it arrived.
    pub seen_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counters {
    pub handshakes: u64,
    pub requests_sent: u64,
    pub frames_resent: u64,
    pub responses: u64,
    pub wire_duplicates: u64,
    pub app_duplicates: u64,
    pub replayed_by_phone: u64,
    pub hub_cache_hits: u64,
    pub refused_stale_by_phone: u64,
    pub fenced_by_hub: u64,
    pub stale_frames_ignored: u64,
    /// Cached answers dropped while the hub ran (by age or by count), not counting the start-up prune.
    pub cache_pruned: u64,
    /// human_touch frames accepted from the overlay.
    pub human_touches: u64,
}

/// Per-device status for the registry.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceStatus {
    pub serial: String,
    pub key_id: String,
    /// The binding was revoked (`agentroid link revoke`): the phone gets needs_repair until re-paired.
    pub revoked: bool,
    pub epoch: Option<u64>,
    /// The link is up and authenticated.
    pub connected: bool,
    /// A heartbeat arrived within the last 10 minutes.
    pub online: bool,
    pub last_seen_ms: Option<u64>,
    pub last_heartbeat_ms: Option<u64>,
    pub last_heartbeat: Option<Heartbeat>,
    /// Wireless-debugging port from the beacon (NSD), for adb over Wi-Fi.
    pub adb_port: Option<u16>,
    pub untrusted_bssid: Vec<UntrustedReport>,
    pub repair: Option<Repair>,
    pub superseded: bool,
    pub instance: Option<String>,
    pub pending_requests: usize,
    pub counters: Counters,
}

#[derive(Debug)]
pub enum HubStartError {
    Bind(BindRefused),
    Io(std::io::Error),
    Store(StoreError),
}

impl std::fmt::Display for HubStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubStartError::Bind(e) => write!(f, "{e}"),
            HubStartError::Io(e) => write!(f, "cannot listen: {e}"),
            HubStartError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for HubStartError {}

// ------------------------------------------------------------------ internal state

enum Out {
    Text(String),
    Close(u16, &'static str),
}

struct Conn {
    id: u64,
    tx: mpsc::UnboundedSender<Out>,
}

type Waiter = oneshot::Sender<Result<Response, HubError>>;

/// What a pending id carries on the wire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Carry {
    /// A `request` frame with the JSON-RPC object.
    Rpc,
    /// A `segment` frame with a signed runner segment (`schemas/segment.json`, 2.14).
    Segment,
}

struct Pending {
    /// The JSON-RPC request, or the signed segment for [`Carry::Segment`].
    rpc: Value,
    hash: String,
    waiters: Vec<Waiter>,
    carry: Carry,
}

struct Device {
    binding: Binding,
    epoch: Option<u64>,
    instance: Option<String>,
    conn: Option<Conn>,
    /// The phone has sent its resume on this connection: requests may flow.
    streaming: bool,
    tx_seq: u64,
    rx_acked: u64,
    ack_sent: u64,
    /// Sent and not yet acked, by seq: (request id, frame text).
    outbox: BTreeMap<u64, (String, String)>,
    /// Accepted and not yet sequenced.
    queue: VecDeque<String>,
    pending: HashMap<String, Pending>,
    /// Ids whose outcome became unknown (phone restart), with their request hash, so a late
    /// answer can still be cached.
    unknown: HashMap<String, String>,
    cache: Option<ResponseCache>,
    /// Hub monotonic time of the last cache prune (0: never).
    last_prune_ms: u64,
    last_seen_ms: Option<u64>,
    last_heartbeat_ms: Option<u64>,
    last_heartbeat: Option<Heartbeat>,
    offline_reported: bool,
    untrusted: VecDeque<UntrustedReport>,
    repair: Option<Repair>,
    superseded: Option<u64>,
    counters: Counters,
}

impl Device {
    fn new(binding: Binding) -> Self {
        Self {
            binding,
            epoch: None,
            instance: None,
            conn: None,
            streaming: false,
            tx_seq: 0,
            rx_acked: 0,
            ack_sent: 0,
            outbox: BTreeMap::new(),
            queue: VecDeque::new(),
            pending: HashMap::new(),
            unknown: HashMap::new(),
            cache: None,
            last_prune_ms: 0,
            last_seen_ms: None,
            last_heartbeat_ms: None,
            last_heartbeat: None,
            offline_reported: false,
            untrusted: VecDeque::new(),
            repair: None,
            superseded: None,
            counters: Counters::default(),
        }
    }

    fn send(&self, text: String) {
        if let Some(c) = &self.conn {
            let _ = c.tx.send(Out::Text(text));
        }
    }

    fn close(&self, code: u16, reason: &'static str) {
        if let Some(c) = &self.conn {
            let _ = c.tx.send(Out::Close(code, reason));
        }
    }

    fn trim(&mut self, acked: u64) {
        while let Some((&seq, _)) = self.outbox.first_key_value() {
            if seq > acked {
                break;
            }
            self.outbox.pop_first();
        }
    }

    fn fail_all(&mut self, e: &HubError) {
        for (_, p) in self.pending.drain() {
            for w in p.waiters {
                let _ = w.send(Err(e.clone()));
            }
        }
        self.queue.clear();
        self.outbox.clear();
    }
}

struct State {
    devices: HashMap<String, Device>,
    next_conn: u64,
    stopped: bool,
}

struct Shared {
    state: Mutex<State>,
    store: StateDir,
    key: HubKey,
    clock: Arc<dyn Clock>,
    events: broadcast::Sender<HubEvent>,
    window: usize,
    handshake_timeout: Duration,
    id_prefix: String,
    id_counter: AtomicU64,
    handshakes: AtomicUsize,
    epochs: Option<Arc<dyn EpochSource>>,
    policy: CachePolicy,
    keep_cached: Option<KeepCached>,
    tls: Option<tokio_rustls::TlsAcceptor>,
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What to do after a hello.
enum HelloPlan {
    Challenge {
        ctx: HandshakeCtx,
        frame: String,
    },
    Refuse {
        frame: Option<String>,
        code: u16,
        reason: &'static str,
    },
}

struct HandshakeCtx {
    serial: String,
    epoch: u64,
    nonce: String,
    phone_nonce: String,
    key_id: String,
    spki: Vec<u8>,
    instance: String,
}

fn rpc_error_code(rpc: &Value) -> Option<i64> {
    rpc.get("error")
        .and_then(|e| e.get("code"))
        .and_then(Value::as_i64)
}

impl Shared {
    fn emit(&self, e: HubEvent) {
        let _ = self.events.send(e);
    }

    fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The device entry for a bound serial (bindings are re-read from devices.json on a miss,
    /// so a phone paired by the CLI while the hub runs is picked up).
    fn device<'a>(&self, st: &'a mut State, serial: &str) -> Result<&'a mut Device, HubError> {
        if !st.devices.contains_key(serial) {
            let b = self
                .store
                .load_devices()?
                .into_iter()
                .find(|b| b.serial == serial)
                .ok_or_else(|| HubError::UnknownDevice(serial.to_string()))?;
            st.devices.insert(serial.to_string(), Device::new(b));
        }
        Ok(st.devices.get_mut(serial).expect("inserted above"))
    }

    /// This process's epoch for the device: allocated (persisted, strictly greater than any
    /// earlier one) on first use, then fixed until [`Hub::adopt_epoch`] moves it up. With an
    /// [`EpochSource`] (bridged's registry) the number comes from there and is persisted here.
    fn ensure_epoch(&self, dev: &mut Device) -> Result<u64, HubError> {
        if let Some(e) = dev.epoch {
            return Ok(e);
        }
        let serial = dev.binding.serial.clone();
        let e = match &self.epochs {
            None => self.store.next_epoch(&serial)?,
            Some(src) => {
                let floor = self.store.current_epoch(&serial)?;
                let e = src.epoch_for(&serial, floor).map_err(HubError::Store)?;
                if e <= floor {
                    return Err(HubError::Store(format!(
                        "epoch source gave {e} for {serial}, not above the stored {floor}"
                    )));
                }
                self.store.raise_epoch(&serial, e)?;
                e
            }
        };
        dev.epoch = Some(e);
        Ok(e)
    }

    fn record_repair(&self, dev: Option<&mut Device>, key_id: &str, from: &str, reason: &str) {
        let serial = dev.as_ref().map(|d| d.binding.serial.clone());
        let fix = fix_for_display(
            fix_text(from, reason).unwrap_or_default(),
            serial.as_deref(),
        );
        if let Some(d) = dev {
            d.repair = Some(Repair {
                from: from.into(),
                reason: reason.into(),
                fix: fix.clone(),
            });
        }
        self.emit(HubEvent::NeedsRepair {
            serial,
            key_id: key_id.into(),
            from: from.into(),
            reason: reason.into(),
            fix,
        });
    }

    fn supersede(&self, dev: &mut Device, phone_epoch: u64) {
        dev.superseded = Some(phone_epoch);
        dev.fail_all(&HubError::Superseded { phone_epoch });
        let serial = dev.binding.serial.clone();
        self.emit(HubEvent::Superseded {
            serial,
            phone_epoch,
        });
    }

    // -------------------------------------------------------------- handshake

    fn on_hello(
        &self,
        key_id: &str,
        phone_nonce: &str,
        instance: &str,
        epoch_seen: u64,
        phone_state: &str,
        phone_reason: Option<&str>,
    ) -> HelloPlan {
        let bindings = match self.store.load_devices() {
            Ok(b) => b,
            Err(e) => {
                self.emit(HubEvent::ProtocolError {
                    serial: None,
                    detail: e.to_string(),
                });
                return HelloPlan::Refuse {
                    frame: None,
                    code: 1011,
                    reason: "hub state unreadable",
                };
            }
        };
        let Some(binding) = bindings.into_iter().find(|b| b.key_id == key_id) else {
            self.record_repair(None, key_id, "hub", "unknown_device_key");
            return HelloPlan::Refuse {
                frame: Some(proto::needs_repair_hub("unknown_device_key", 0)),
                code: proto::CLOSE_REPAIR,
                reason: "needs_repair",
            };
        };
        let mut st = lock(&self.state);
        let serial = binding.serial.clone();
        let dev = st
            .devices
            .entry(serial.clone())
            .or_insert_with(|| Device::new(binding.clone()));
        dev.binding = binding.clone();
        if binding.revoked {
            let epoch = dev.epoch.unwrap_or(0);
            self.record_repair(Some(dev), key_id, "hub", "key_revoked");
            return HelloPlan::Refuse {
                frame: Some(proto::needs_repair_hub("key_revoked", epoch)),
                code: proto::CLOSE_REPAIR,
                reason: "needs_repair",
            };
        }
        if dev.superseded.is_some() {
            return HelloPlan::Refuse {
                frame: None,
                code: proto::CLOSE_REPAIR,
                reason: "superseded by a newer hub",
            };
        }
        let epoch = match self.ensure_epoch(dev) {
            Ok(e) => e,
            Err(e) => {
                self.emit(HubEvent::ProtocolError {
                    serial: Some(serial),
                    detail: e.to_string(),
                });
                return HelloPlan::Refuse {
                    frame: None,
                    code: 1011,
                    reason: "hub state unwritable",
                };
            }
        };
        if epoch_seen > epoch {
            // The phone already accepted a newer hub. This hub is the old one: it must not try to
            // take the phone back (the phone would answer needs_repair stale_epoch).
            self.record_repair(Some(dev), key_id, "phone", "stale_epoch");
            self.supersede(dev, epoch_seen);
            return HelloPlan::Refuse {
                frame: None,
                code: proto::CLOSE_REPAIR,
                reason: "stale epoch",
            };
        }
        if phone_state == "needs_repair" {
            if let Some(r) = phone_reason {
                let from = if fix_text("phone", r).is_some() {
                    "phone"
                } else {
                    "hub"
                };
                let fix = fix_for_display(fix_text(from, r).unwrap_or_default(), Some(&serial));
                dev.repair = Some(Repair {
                    from: from.into(),
                    reason: r.into(),
                    fix,
                });
            }
        }
        let nonce = match new_nonce() {
            Ok(n) => n,
            Err(e) => {
                self.emit(HubEvent::ProtocolError {
                    serial: Some(serial),
                    detail: e.0,
                });
                return HelloPlan::Refuse {
                    frame: None,
                    code: 1011,
                    reason: "no randomness",
                };
            }
        };
        let sig = self.key.sign(&hub_challenge_message(
            &nonce,
            phone_nonce,
            epoch,
            &serial,
            key_id,
        ));
        let frame = proto::challenge(
            epoch,
            &nonce,
            phone_nonce,
            &serial,
            self.key.key_id(),
            &b64(&sig),
        );
        let ctx = HandshakeCtx {
            serial,
            epoch,
            nonce,
            phone_nonce: phone_nonce.into(),
            key_id: key_id.into(),
            spki: binding.spki,
            instance: instance.into(),
        };
        HelloPlan::Challenge { ctx, frame }
    }

    /// Verifies the phone's signature; on success the connection becomes the device's link.
    fn on_challenge_response(
        &self,
        ctx: &HandshakeCtx,
        epoch: u64,
        nonce: &str,
        key_id: &str,
        signature: &[u8],
    ) -> Result<(u64, mpsc::UnboundedReceiver<Out>), String> {
        let msg = phone_response_message(
            &ctx.nonce,
            &ctx.phone_nonce,
            ctx.epoch,
            &ctx.serial,
            self.key.key_id(),
        );
        let good = epoch == ctx.epoch
            && nonce == ctx.nonce
            && key_id == ctx.key_id
            && verify(&ctx.spki, &msg, signature);
        let mut st = lock(&self.state);
        let next = st.next_conn + 1;
        st.next_conn = next;
        let Some(dev) = st.devices.get_mut(&ctx.serial) else {
            return Err(proto::needs_repair_hub("unknown_device_key", ctx.epoch));
        };
        if !good {
            self.record_repair(Some(dev), &ctx.key_id, "hub", "bad_phone_signature");
            return Err(proto::needs_repair_hub("bad_phone_signature", ctx.epoch));
        }
        // A new phone process: its outbox and request cache are gone. Unanswered requests may or
        // may not have run; they are failed as outcome-unknown, never replayed blindly.
        if dev.instance.as_deref().is_some_and(|i| i != ctx.instance) {
            // every request already sent (in the outbox, or acked by the old process but never
            // answered); queued ones were never sent and simply flow on the new stream
            let queued: std::collections::HashSet<&String> = dev.queue.iter().collect();
            let mut ids: Vec<String> = dev
                .pending
                .keys()
                .filter(|id| !queued.contains(id))
                .cloned()
                .collect();
            ids.sort();
            for id in &ids {
                if let Some(p) = dev.pending.remove(id) {
                    dev.unknown.insert(id.clone(), p.hash);
                    for w in p.waiters {
                        let _ = w.send(Err(HubError::OutcomeUnknown(id.clone())));
                    }
                }
            }
            dev.outbox.clear();
            dev.tx_seq = 0;
            dev.rx_acked = 0;
            dev.ack_sent = 0;
            self.emit(HubEvent::PhoneRestarted {
                serial: ctx.serial.clone(),
                outcome_unknown: ids,
            });
        }
        if let Some(old) = dev.conn.take() {
            let _ = old
                .tx
                .send(Out::Close(1000, "superseded by a new connection"));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(Out::Text(proto::ready(ctx.epoch, &ctx.key_id)));
        let _ = tx.send(Out::Text(proto::resume(ctx.epoch, dev.rx_acked)));
        dev.ack_sent = dev.rx_acked;
        dev.conn = Some(Conn { id: next, tx });
        dev.streaming = false;
        dev.instance = Some(ctx.instance.clone());
        dev.repair = None;
        dev.last_seen_ms = Some(self.now());
        dev.counters.handshakes += 1;
        self.emit(HubEvent::Connected {
            serial: ctx.serial.clone(),
            epoch: ctx.epoch,
            instance: ctx.instance.clone(),
        });
        Ok((next, rx))
    }

    /// The phone answered the challenge with needs_repair (bad hub signature, serial mismatch,
    /// stale epoch, ...). A stale epoch means a newer hub owns the phone: this one stops.
    fn on_handshake_repair(&self, ctx: &HandshakeCtx, reason: &str) {
        let mut st = lock(&self.state);
        let dev = st.devices.get_mut(&ctx.serial);
        if reason == "stale_epoch" {
            if let Some(d) = dev {
                self.record_repair(Some(&mut *d), &ctx.key_id, "phone", reason);
                self.supersede(d, ctx.epoch.saturating_add(1));
            }
        } else {
            self.record_repair(dev, &ctx.key_id, "phone", reason);
        }
    }

    // -------------------------------------------------------------- the stream

    fn pump(&self, dev: &mut Device) {
        if dev.conn.is_none() || !dev.streaming || dev.superseded.is_some() {
            return;
        }
        let Some(epoch) = dev.epoch else { return };
        while dev.outbox.len() < self.window {
            let Some(id) = dev.queue.pop_front() else {
                break;
            };
            let Some(p) = dev.pending.get(&id) else {
                continue;
            };
            dev.tx_seq += 1;
            let text = match p.carry {
                Carry::Rpc => proto::request(epoch, dev.tx_seq, &id, &p.rpc),
                Carry::Segment => proto::segment(epoch, dev.tx_seq, &id, &p.rpc),
            };
            dev.outbox.insert(dev.tx_seq, (id, text.clone()));
            dev.counters.requests_sent += 1;
            dev.send(text);
        }
    }

    /// In-order delivery of a sequenced phone frame. Returns true when `seq` is the next one.
    fn accept_seq(&self, dev: &mut Device, epoch: u64, seq: u64) -> bool {
        if seq <= dev.rx_acked {
            dev.counters.wire_duplicates += 1;
            dev.send(proto::ack(epoch, dev.rx_acked));
            dev.ack_sent = dev.rx_acked;
            return false;
        }
        if seq != dev.rx_acked + 1 {
            dev.send(proto::resume(epoch, dev.rx_acked)); // a gap: ask for a resend
            return false;
        }
        dev.rx_acked = seq;
        if dev.rx_acked - dev.ack_sent >= ACK_EVERY {
            dev.send(proto::ack(epoch, dev.rx_acked));
            dev.ack_sent = dev.rx_acked;
        }
        true
    }

    fn deliver_response(&self, dev: &mut Device, id: String, rpc: Value, replayed: bool) {
        let cacheable = !rpc_error_code(&rpc).is_some_and(|c| UNCACHEABLE_CODES.contains(&c));
        let hash = match dev.pending.remove(&id) {
            Some(p) => {
                dev.counters.responses += 1;
                if replayed {
                    dev.counters.replayed_by_phone += 1;
                }
                let r = Response {
                    id: id.clone(),
                    rpc: rpc.clone(),
                    replayed,
                    from_hub_cache: false,
                };
                for w in p.waiters {
                    let _ = w.send(Ok(r.clone()));
                }
                Some(p.hash)
            }
            None => {
                if dev.cache.as_ref().is_some_and(|c| c.get(&id).is_some()) {
                    dev.counters.app_duplicates += 1;
                    None
                } else {
                    dev.unknown.remove(&id)
                }
            }
        };
        if let (Some(hash), true) = (hash, cacheable) {
            let ts_ms = self.clock.wall_ms();
            if let Ok(cache) = self.cache_of(dev) {
                if let Err(e) = cache.insert(
                    &id,
                    CachedResponse {
                        request_hash: hash,
                        rpc,
                        replayed,
                        ts_ms,
                    },
                ) {
                    let serial = dev.binding.serial.clone();
                    self.emit(HubEvent::ProtocolError {
                        serial: Some(serial),
                        detail: format!("response cache: {e}"),
                    });
                }
            }
            self.prune_if_due(dev, false);
        }
    }

    /// Prunes one phone's response cache by age and count, unless one ran within `policy.every_ms`
    /// (or `force`). Answers `keep_cached` names are never dropped. Returns how many went.
    fn prune_if_due(&self, dev: &mut Device, force: bool) -> usize {
        let now = self.now();
        if !force
            && dev.last_prune_ms != 0
            && now.saturating_sub(dev.last_prune_ms) < self.policy.every_ms
        {
            return 0;
        }
        dev.last_prune_ms = now.max(1);
        let serial = dev.binding.serial.clone();
        let wall = self.clock.wall_ms();
        let keep = |id: &str| self.keep_cached.as_ref().is_some_and(|k| k(&serial, id));
        let p = self.policy;
        let n = match self.cache_of(dev).and_then(|c| {
            c.prune(wall, p.max_age_ms, p.max_count, &keep)
                .map_err(HubError::from)
        }) {
            Ok(r) => r.dropped(),
            Err(e) => {
                self.emit(HubEvent::ProtocolError {
                    serial: Some(serial),
                    detail: format!("response cache prune: {e}"),
                });
                0
            }
        };
        dev.counters.cache_pruned += n as u64;
        n
    }

    fn cache_of<'a>(&self, dev: &'a mut Device) -> Result<&'a mut ResponseCache, HubError> {
        if dev.cache.is_none() {
            dev.cache = Some(self.store.response_cache(&dev.binding.serial)?);
        }
        Ok(dev.cache.as_mut().expect("opened above"))
    }

    /// One frame on an authenticated connection. Returns a close to perform, if any.
    fn on_frame(
        &self,
        serial: &str,
        conn_id: u64,
        frame: PhoneFrame,
    ) -> Option<(u16, &'static str)> {
        let mut st = lock(&self.state);
        let now = self.now();
        let dev = st.devices.get_mut(serial)?;
        if dev.conn.as_ref().map(|c| c.id) != Some(conn_id) {
            return Some((1000, "superseded by a new connection"));
        }
        let epoch = dev.epoch?;
        dev.last_seen_ms = Some(now);
        // Every sequenced or acking frame names its epoch: anything from another epoch is a
        // leftover and is ignored (it neither acks nor delivers).
        let frame_epoch = match &frame {
            PhoneFrame::Heartbeat(h) => Some(h.epoch),
            PhoneFrame::Response { epoch, .. }
            | PhoneFrame::Ack { epoch, .. }
            | PhoneFrame::Resume { epoch, .. }
            | PhoneFrame::UntrustedBssid { epoch, .. }
            | PhoneFrame::HumanTouch { epoch, .. } => Some(*epoch),
            _ => None,
        };
        if frame_epoch.is_some_and(|e| e != epoch) {
            dev.counters.stale_frames_ignored += 1;
            return None;
        }
        match frame {
            PhoneFrame::Heartbeat(h) => {
                dev.trim(h.ack);
                dev.send(proto::ack(epoch, dev.rx_acked)); // every heartbeat is acked
                dev.ack_sent = dev.rx_acked;
                let port_changed =
                    dev.last_heartbeat.as_ref().map(|o| o.adb_port) != Some(h.adb_port);
                dev.last_heartbeat_ms = Some(now);
                dev.last_heartbeat = Some(h.clone());
                dev.offline_reported = false;
                if port_changed {
                    self.emit(HubEvent::AdbPortChanged {
                        serial: serial.into(),
                        port: h.adb_port,
                    });
                }
                self.emit(HubEvent::Heartbeat {
                    serial: serial.into(),
                    heartbeat: h,
                });
                self.pump(dev);
            }
            PhoneFrame::Response {
                seq,
                ack,
                id,
                replayed,
                rpc,
                ..
            } => {
                dev.trim(ack);
                if self.accept_seq(dev, epoch, seq) {
                    self.deliver_response(dev, id, rpc, replayed);
                }
                self.pump(dev);
            }
            PhoneFrame::UntrustedBssid {
                seq,
                ack,
                bssid,
                reason,
                ts_ms,
                ..
            } => {
                dev.trim(ack);
                if self.accept_seq(dev, epoch, seq) {
                    dev.untrusted.push_back(UntrustedReport {
                        bssid: bssid.clone(),
                        reason: reason.clone(),
                        ts_ms,
                        seen_ms: now,
                    });
                    while dev.untrusted.len() > UNTRUSTED_KEEP {
                        dev.untrusted.pop_front();
                    }
                    self.emit(HubEvent::UntrustedBssid {
                        serial: serial.into(),
                        bssid,
                        reason,
                        ts_ms,
                    });
                }
                self.pump(dev);
            }
            PhoneFrame::HumanTouch {
                seq,
                ack,
                touch_ms,
                sent_ms,
                ..
            } => {
                dev.trim(ack);
                if self.accept_seq(dev, epoch, seq) {
                    dev.counters.human_touches += 1;
                    self.emit(HubEvent::HumanTouch {
                        serial: serial.into(),
                        touch_ms,
                        sent_ms,
                    });
                }
                self.pump(dev);
            }
            PhoneFrame::Ack { ack, .. } => {
                dev.trim(ack);
                self.pump(dev);
            }
            PhoneFrame::Resume { last_acked, .. } => {
                dev.trim(last_acked);
                let resend: Vec<String> = dev.outbox.values().map(|(_, t)| t.clone()).collect();
                dev.counters.frames_resent += resend.len() as u64;
                for t in resend {
                    dev.send(t);
                }
                dev.streaming = true;
                self.pump(dev);
            }
            PhoneFrame::Refused {
                epoch: phone_epoch,
                reason,
                id,
                ..
            } => match reason.as_str() {
                "stale_epoch" => {
                    // The phone holds a newer epoch than ours: another hub owns it. Stop.
                    dev.counters.refused_stale_by_phone += 1;
                    let key_id = dev.binding.key_id.clone();
                    self.record_repair(Some(dev), &key_id, "phone", "stale_epoch");
                    self.supersede(dev, phone_epoch);
                    return Some((proto::CLOSE_REPAIR, "superseded by a newer hub"));
                }
                "epoch_ahead" | "not_ready" => return Some((1000, "re-handshake")),
                "bad_request" => {
                    if let Some(p) = id.as_ref().and_then(|i| dev.pending.remove(i)) {
                        for w in p.waiters {
                            let _ = w.send(Err(HubError::Refused("bad_request".into())));
                        }
                    }
                }
                _ => {} // out_of_order: the phone's resume follows
            },
            PhoneFrame::NeedsRepair { reason, .. } => {
                let key_id = dev.binding.key_id.clone();
                self.record_repair(Some(dev), &key_id, "phone", &reason);
                return Some((proto::CLOSE_REPAIR, "needs_repair"));
            }
            PhoneFrame::Hello { .. } | PhoneFrame::ChallengeResponse { .. } => {
                self.emit(HubEvent::ProtocolError {
                    serial: Some(serial.into()),
                    detail: "handshake frame after ready".into(),
                });
                return Some((proto::CLOSE_PROTOCOL, "protocol violation"));
            }
        }
        None
    }

    fn flush_ack(&self, serial: &str, conn_id: u64) {
        let mut st = lock(&self.state);
        let Some(dev) = st.devices.get_mut(serial) else {
            return;
        };
        if dev.conn.as_ref().map(|c| c.id) != Some(conn_id) {
            return;
        }
        if let (Some(epoch), true) = (dev.epoch, dev.rx_acked > dev.ack_sent) {
            dev.send(proto::ack(epoch, dev.rx_acked));
            dev.ack_sent = dev.rx_acked;
        }
    }

    fn on_disconnect(&self, serial: &str, conn_id: u64) {
        let mut st = lock(&self.state);
        let Some(dev) = st.devices.get_mut(serial) else {
            return;
        };
        if dev.conn.as_ref().map(|c| c.id) == Some(conn_id) {
            dev.conn = None;
            dev.streaming = false;
            self.emit(HubEvent::Disconnected {
                serial: serial.into(),
            });
        }
    }

    fn new_request_id(&self) -> String {
        format!(
            "{}-{}",
            self.id_prefix,
            self.id_counter.fetch_add(1, Ordering::Relaxed) + 1
        )
    }
}

/// Turns the caller's payload into the carried JSON-RPC request.
fn build_rpc(payload: Value, new_id: impl FnOnce() -> String) -> Result<(String, Value), HubError> {
    let Value::Object(mut o) = payload else {
        return Err(HubError::BadPayload(
            "the payload must be a JSON object".into(),
        ));
    };
    for k in o.keys() {
        if !matches!(k.as_str(), "method" | "params" | "id" | "jsonrpc") {
            return Err(HubError::BadPayload(format!("unknown field {k}")));
        }
    }
    if o.get("jsonrpc").is_some_and(|v| v != "2.0") {
        return Err(HubError::BadPayload("jsonrpc must be \"2.0\"".into()));
    }
    let method = o
        .get("method")
        .and_then(Value::as_str)
        .filter(|m| is_method(m))
        .map(str::to_string)
        .ok_or_else(|| {
            HubError::BadPayload(
                "method must be a dotted lower-case name of at most 64 characters".into(),
            )
        })?;
    let id = match o.remove("id") {
        None => new_id(),
        Some(Value::String(s)) if is_request_id(&s) => s,
        Some(_) => {
            return Err(HubError::BadPayload(
                "id must be 1..128 of [A-Za-z0-9._:-]".into(),
            ))
        }
    };
    let mut rpc = Map::new();
    rpc.insert("jsonrpc".into(), Value::from("2.0"));
    rpc.insert("id".into(), Value::from(id.clone()));
    rpc.insert("method".into(), Value::from(method));
    match o.remove("params") {
        None => {}
        Some(p @ Value::Object(_)) => {
            rpc.insert("params".into(), p);
        }
        Some(_) => return Err(HubError::BadPayload("params must be an object".into())),
    }
    let rpc = Value::Object(rpc);
    if rpc.to_string().len() > proto::MAX_RPC_BYTES {
        return Err(HubError::BadPayload(
            "the request is larger than 64 KiB".into(),
        ));
    }
    Ok((id, rpc))
}

// ------------------------------------------------------------------ the hub

/// The running hub. Dropping it (or [`Hub::shutdown`]) stops the listener and every connection.
pub struct Hub {
    shared: Arc<Shared>,
    addr: SocketAddr,
    stop: watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Hub({}, key_id={})", self.addr, self.shared.key.key_id())
    }
}

enum Submitted {
    Done(Response),
    Wait(oneshot::Receiver<Result<Response, HubError>>),
}

impl Hub {
    /// Checks the bind address (nothing is bound if it is refused), opens `hub-state/`, binds the
    /// listener and starts accepting phones. Must run inside a tokio runtime.
    pub async fn start(cfg: HubConfig) -> Result<Hub, HubStartError> {
        if cfg.lan && cfg.tls.is_none() {
            return Err(HubStartError::Bind(BindRefused(
                "refusing --link-lan without TLS: on a home network anyone could read and inject the frames".into(),
            )));
        }
        let ip = if cfg.lan {
            check_bind_address_lan(&cfg.bind)
        } else {
            check_bind_address(&cfg.bind)
        }
        .map_err(HubStartError::Bind)?;
        let store = StateDir::open(&cfg.state_dir).map_err(HubStartError::Store)?;
        // the age limit and the per-device cap, before any device's cache is opened
        store
            .prune_responses(cfg.clock.wall_ms(), cfg.cache.max_age_ms)
            .map_err(HubStartError::Store)?;
        let listener = TcpListener::bind(SocketAddrV4::new(ip, cfg.port))
            .await
            .map_err(HubStartError::Io)?;
        let addr = listener.local_addr().map_err(HubStartError::Io)?;
        let prefix = random_bytes::<6>()
            .map(|b| format!("h{}", hex(&b)))
            .unwrap_or_else(|_| format!("h{}", cfg.clock.wall_ms()));
        let (events, _) = broadcast::channel(4096);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                devices: HashMap::new(),
                next_conn: 0,
                stopped: false,
            }),
            store,
            key: cfg.key,
            clock: cfg.clock,
            events,
            window: cfg.window.max(1),
            handshake_timeout: cfg.handshake_timeout,
            id_prefix: prefix,
            id_counter: AtomicU64::new(0),
            handshakes: AtomicUsize::new(0),
            epochs: cfg.epochs,
            policy: cfg.cache,
            keep_cached: cfg.keep_cached,
            tls: cfg.tls.as_ref().map(|t| t.acceptor()),
        });
        let (stop, stop_rx) = watch::channel(false);
        let accept = tokio::spawn(accept_loop(shared.clone(), listener, stop_rx.clone()));
        let sweeper = tokio::spawn(sweep_loop(shared.clone(), stop_rx));
        Ok(Hub {
            shared,
            addr,
            stop,
            tasks: Mutex::new(vec![accept, sweeper]),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Prunes every connected-or-seen phone's response cache now (age and count, `keep_cached`
    /// honoured), whatever the interval says. The sweeper and every cache write do the same on
    /// their own, at most once per [`CachePolicy::every_ms`]. Returns how many answers went.
    pub fn prune_caches(&self) -> usize {
        let mut st = lock(&self.shared.state);
        st.devices
            .values_mut()
            .map(|dev| self.shared.prune_if_due(dev, true))
            .sum()
    }

    /// `ws://<addr>/helper`, what the phone dials.
    pub fn url(&self) -> String {
        format!("ws://{}{}", self.addr, PATH)
    }

    pub fn hub_key(&self) -> &HubKey {
        &self.shared.key
    }

    pub fn state_dir(&self) -> &std::path::Path {
        self.shared.store.path()
    }

    /// Stops accepting, closes every connection (1001), fails every waiting request with
    /// [`HubError::Shutdown`], and waits until the listener socket is closed.
    pub async fn shutdown(&self) {
        let _ = self.stop.send(true);
        {
            let mut st = lock(&self.shared.state);
            st.stopped = true;
            for dev in st.devices.values_mut() {
                dev.close(1001, "hub shutting down");
                dev.conn = None;
                for (_, p) in dev.pending.drain() {
                    for w in p.waiters {
                        let _ = w.send(Err(HubError::Shutdown));
                    }
                }
                dev.queue.clear();
            }
        }
        let tasks: Vec<_> =
            std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|e| e.into_inner()));
        for t in tasks {
            let _ = t.await;
        }
    }

    /// Sends one request to a device and waits for its response. `payload` is
    /// `{"method": "...", "params": {...}?, "id": "..."?}`; give `id` to make a retry idempotent
    /// (the same id is answered from the hub's cache, or the phone's, and never runs twice).
    /// Requests wait while the phone is away and flow, in order, when it is back.
    pub async fn send_request(&self, device: &str, payload: Value) -> Result<Response, HubError> {
        match self.submit(device, None, payload)? {
            Submitted::Done(r) => Ok(r),
            Submitted::Wait(rx) => rx.await.map_err(|_| HubError::Shutdown)?,
        }
    }

    /// Like [`Hub::send_request`], fenced: refused (and nothing sent) unless `epoch` is the
    /// device's current epoch, e.g. the lease epoch the caller was granted.
    pub async fn send_request_fenced(
        &self,
        device: &str,
        epoch: u64,
        payload: Value,
    ) -> Result<Response, HubError> {
        match self.submit(device, Some(epoch), payload)? {
            Submitted::Done(r) => Ok(r),
            Submitted::Wait(rx) => rx.await.map_err(|_| HubError::Shutdown)?,
        }
    }

    /// Sends one signed runner segment (`schemas/segment.json`, task 2.14) in a `segment` frame,
    /// fenced like [`Hub::send_request_fenced`]. `id` is the request id the phone answers with a
    /// normal `response` frame (its acceptance or refusal); the same id with the same segment is
    /// answered once and never runs twice. The caller signs and admits the segment first; the
    /// hub only carries it.
    pub async fn send_segment_fenced(
        &self,
        device: &str,
        epoch: u64,
        id: &str,
        signed: Value,
    ) -> Result<Response, HubError> {
        if !is_request_id(id) {
            return Err(HubError::BadPayload(
                "id must be 1..128 of [A-Za-z0-9._:-]".into(),
            ));
        }
        if !signed.is_object()
            || !signed.get("segment").is_some_and(Value::is_object)
            || !signed.get("signature").is_some_and(Value::is_object)
        {
            return Err(HubError::BadPayload(
                "a segment frame carries {segment, signature}".into(),
            ));
        }
        if signed.to_string().len() > proto::MAX_RPC_BYTES {
            return Err(HubError::BadPayload(
                "the segment is larger than 64 KiB".into(),
            ));
        }
        match self.submit_carry(device, Some(epoch), id.to_string(), signed, Carry::Segment)? {
            Submitted::Done(r) => Ok(r),
            Submitted::Wait(rx) => rx.await.map_err(|_| HubError::Shutdown)?,
        }
    }

    /// Moves the device to `epoch`, a new lease epoch from the daemon's registry (one epoch
    /// source). Only ever raises: a lower or equal epoch changes nothing and returns false.
    /// Requests already sent under the old epoch fail as [`HubError::OutcomeUnknown`] (they may
    /// have run); queued ones fail as [`HubError::StaleEpoch`] (they never left). The stream is
    /// reset and a connected phone is closed (1000) so it re-handshakes and learns the new epoch.
    pub fn adopt_epoch(&self, device: &str, epoch: u64) -> Result<bool, HubError> {
        let sh = &self.shared;
        let mut st = lock(&sh.state);
        let dev = sh.device(&mut st, device)?;
        let old = dev.epoch.unwrap_or(0);
        if epoch <= old {
            return Ok(false);
        }
        sh.store.raise_epoch(&dev.binding.serial, epoch)?;
        let queued: std::collections::HashSet<String> = dev.queue.drain(..).collect();
        let ids: Vec<String> = dev.pending.keys().cloned().collect();
        for id in ids {
            if let Some(p) = dev.pending.remove(&id) {
                let err = if queued.contains(&id) {
                    HubError::StaleEpoch {
                        requested: old,
                        current: epoch,
                    }
                } else {
                    dev.unknown.insert(id.clone(), p.hash.clone());
                    HubError::OutcomeUnknown(id.clone())
                };
                for w in p.waiters {
                    let _ = w.send(Err(err.clone()));
                }
            }
        }
        dev.outbox.clear();
        dev.tx_seq = 0;
        dev.rx_acked = 0;
        dev.ack_sent = 0;
        dev.streaming = false;
        dev.epoch = Some(epoch);
        dev.close(1000, "re-handshake");
        Ok(true)
    }

    fn submit(
        &self,
        device: &str,
        fence: Option<u64>,
        payload: Value,
    ) -> Result<Submitted, HubError> {
        let sh = &self.shared;
        let (id, rpc) = build_rpc(payload, || sh.new_request_id())?;
        self.submit_carry(device, fence, id, rpc, Carry::Rpc)
    }

    fn submit_carry(
        &self,
        device: &str,
        fence: Option<u64>,
        id: String,
        rpc: Value,
        carry: Carry,
    ) -> Result<Submitted, HubError> {
        let sh = &self.shared;
        let hash = hex(&sha256(rpc.to_string().as_bytes()));
        let mut st = lock(&sh.state);
        if st.stopped {
            return Err(HubError::Shutdown);
        }
        let dev = sh.device(&mut st, device)?;
        if let Some(e) = fence {
            let current = sh.ensure_epoch(dev)?;
            if e != current {
                dev.counters.fenced_by_hub += 1;
                return Err(HubError::StaleEpoch {
                    requested: e,
                    current,
                });
            }
        }
        if let Some(phone_epoch) = dev.superseded {
            return Err(HubError::Superseded { phone_epoch });
        }
        // request-id dedupe: an id answered before (even before a hub restart) is answered from
        // the cache and never sent again
        if let Some(c) = sh.cache_of(dev)?.get(&id) {
            if c.request_hash != hash {
                return Err(HubError::IdReused(id));
            }
            let r = Response {
                id: id.clone(),
                rpc: c.rpc.clone(),
                replayed: c.replayed,
                from_hub_cache: true,
            };
            dev.counters.hub_cache_hits += 1;
            return Ok(Submitted::Done(r));
        }
        let (tx, rx) = oneshot::channel();
        if let Some(p) = dev.pending.get_mut(&id) {
            if p.hash != hash {
                return Err(HubError::IdReused(id));
            }
            p.waiters.push(tx); // the same call already in flight: wait for its one answer
            return Ok(Submitted::Wait(rx));
        }
        let hash_for_unknown = dev.unknown.remove(&id);
        if hash_for_unknown.as_ref().is_some_and(|h| *h != hash) {
            return Err(HubError::IdReused(id));
        }
        dev.pending.insert(
            id.clone(),
            Pending {
                rpc,
                hash,
                waiters: vec![tx],
                carry,
            },
        );
        dev.queue.push_back(id);
        sh.pump(dev);
        Ok(Submitted::Wait(rx))
    }

    /// The device's current epoch in this hub process (allocated on first use).
    pub fn epoch(&self, device: &str) -> Result<u64, HubError> {
        let mut st = lock(&self.shared.state);
        let dev = self.shared.device(&mut st, device)?;
        self.shared.ensure_epoch(dev)
    }

    /// Status for the registry; `None` if the serial is not paired.
    pub fn status(&self, device: &str) -> Option<DeviceStatus> {
        let mut st = lock(&self.shared.state);
        let now = self.shared.now();
        let dev = self.shared.device(&mut st, device).ok()?;
        Some(DeviceStatus {
            serial: dev.binding.serial.clone(),
            key_id: dev.binding.key_id.clone(),
            revoked: dev.binding.revoked,
            epoch: dev.epoch,
            connected: dev.conn.is_some(),
            online: dev
                .last_heartbeat_ms
                .is_some_and(|t| now.saturating_sub(t) < OFFLINE_AFTER_MS),
            last_seen_ms: dev.last_seen_ms,
            last_heartbeat_ms: dev.last_heartbeat_ms,
            last_heartbeat: dev.last_heartbeat.clone(),
            adb_port: dev.last_heartbeat.as_ref().and_then(|h| h.adb_port),
            untrusted_bssid: dev.untrusted.iter().cloned().collect(),
            repair: dev.repair.clone(),
            superseded: dev.superseded.is_some(),
            instance: dev.instance.clone(),
            pending_requests: dev.pending.len(),
            counters: dev.counters.clone(),
        })
    }

    /// Every paired serial.
    pub fn devices(&self) -> Result<Vec<String>, HubError> {
        Ok(self
            .shared
            .store
            .load_devices()?
            .into_iter()
            .map(|b| b.serial)
            .collect())
    }

    /// Marks devices with no heartbeat for 10 minutes offline (one [`HubEvent::Offline`] each).
    /// Runs every 15 s on its own; public so a test with a fake clock can call it.
    pub fn sweep(&self) {
        sweep_once(&self.shared);
    }

    /// Revokes a device's key: it gets `needs_repair key_revoked` now (if connected) and at every
    /// later handshake, until it is paired again.
    pub fn revoke(&self, device: &str) -> Result<bool, HubError> {
        let found = self.shared.store.revoke(device)?;
        let mut st = lock(&self.shared.state);
        if let Some(dev) = st.devices.get_mut(device) {
            dev.binding.revoked = true;
            let epoch = dev.epoch.unwrap_or(0);
            dev.send(proto::needs_repair_hub("key_revoked", epoch));
            dev.close(proto::CLOSE_REPAIR, "needs_repair");
            let key_id = dev.binding.key_id.clone();
            self.shared
                .record_repair(Some(dev), &key_id, "hub", "key_revoked");
        }
        Ok(found)
    }

    /// A receiver of every hub event from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.shared.events.subscribe()
    }

    /// The events as a stream (a slow reader skips what it missed rather than blocking the hub).
    pub fn events(&self) -> impl Stream<Item = HubEvent> + Send + 'static {
        futures_util::stream::unfold(self.subscribe(), |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(e) => return Some((e, rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

fn sweep_once(sh: &Shared) {
    let now = sh.now();
    let mut st = lock(&sh.state);
    for dev in st.devices.values_mut() {
        if let Some(t) = dev.last_heartbeat_ms {
            if now.saturating_sub(t) >= OFFLINE_AFTER_MS && !dev.offline_reported {
                dev.offline_reported = true;
                sh.emit(HubEvent::Offline {
                    serial: dev.binding.serial.clone(),
                });
            }
        }
        sh.prune_if_due(dev, false);
    }
}

async fn sweep_loop(sh: Arc<Shared>, mut stop: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = tick.tick() => sweep_once(&sh),
            _ = stop.changed() => break,
        }
    }
}

async fn accept_loop(sh: Arc<Shared>, listener: TcpListener, mut stop: watch::Receiver<bool>) {
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            r = listener.accept() => {
                let Ok((stream, _peer)) = r else { continue };
                if sh.handshakes.load(Ordering::SeqCst) >= MAX_HANDSHAKES {
                    drop(stream); // too many half-open handshakes: shed load before any work
                    continue;
                }
                let _ = stream.set_nodelay(true);
                conns.spawn(connection(sh.clone(), stream, stop.clone()));
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
            _ = stop.changed() => break,
        }
    }
    drop(listener); // the listening socket is closed before shutdown() returns
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    conns.abort_all();
}

type Ws = WebSocketStream<HubStream>;

/// The phone's byte stream: plain TCP on the tailnet (WireGuard already encrypts it), TLS 1.3 with `--link-lan`.
enum HubStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl tokio::io::AsyncRead for HubStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            HubStream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            HubStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for HubStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            HubStream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            HubStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            HubStream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            HubStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            HubStream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            HubStream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

enum Incoming {
    Text(String),
    Other,
    Closed,
}

async fn next_frame(source: &mut SplitStream<Ws>) -> Incoming {
    loop {
        match source.next().await {
            Some(Ok(Message::Text(t))) => return Incoming::Text(t.as_str().to_string()),
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            Some(Ok(Message::Binary(_))) | Some(Ok(Message::Frame(_))) => return Incoming::Other,
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Incoming::Closed,
        }
    }
}

async fn close_with(sink: &mut SplitSink<Ws, Message>, code: u16, reason: &'static str) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: Utf8Bytes::from_static(reason),
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        sink.send(Message::Close(Some(frame))),
    )
    .await;
}

struct HandshakeSlot<'a>(&'a AtomicUsize);

impl Drop for HandshakeSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[allow(clippy::result_large_err)]
fn only_helper_path(req: &WsRequest, resp: WsResponse) -> Result<WsResponse, ErrorResponse> {
    if req.uri().path() == PATH {
        return Ok(resp);
    }
    let mut e = ErrorResponse::new(Some("not found".into()));
    *e.status_mut() = StatusCode::NOT_FOUND;
    Err(e)
}

async fn connection(sh: Arc<Shared>, stream: TcpStream, mut stop: watch::Receiver<bool>) {
    let hs = sh.handshake_timeout;
    let slot = {
        sh.handshakes.fetch_add(1, Ordering::SeqCst);
        HandshakeSlot(&sh.handshakes)
    };
    // TLS first (inside the same handshake slot and timeout): a client that cannot finish it gets nothing
    let stream = match &sh.tls {
        None => HubStream::Plain(stream),
        Some(acc) => match tokio::time::timeout(hs, acc.accept(stream)).await {
            Ok(Ok(t)) => HubStream::Tls(Box::new(t)),
            _ => return,
        },
    };
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(proto::MAX_FRAME_BYTES))
        .max_frame_size(Some(proto::MAX_FRAME_BYTES));
    let ws = match tokio::time::timeout(
        hs,
        tokio_tungstenite::accept_hdr_async_with_config(stream, only_helper_path, Some(cfg)),
    )
    .await
    {
        Ok(Ok(ws)) => ws,
        _ => return,
    };
    let (mut sink, mut source) = ws.split();

    // hello
    let hello = match tokio::time::timeout(hs, next_frame(&mut source)).await {
        Ok(Incoming::Text(t)) => proto::parse_from_phone(&t),
        Ok(Incoming::Closed) => return,
        Ok(Incoming::Other) => Err(proto::ProtoError("binary frame".into())),
        Err(_) => return close_with(&mut sink, proto::CLOSE_LIVENESS, "handshake timeout").await,
    };
    let ctx = match hello {
        Ok(PhoneFrame::Hello {
            key_id,
            phone_nonce,
            instance,
            state,
            repair_reason,
            epoch_seen,
            ..
        }) => {
            match sh.on_hello(
                &key_id,
                &phone_nonce,
                &instance,
                epoch_seen,
                &state,
                repair_reason.as_deref(),
            ) {
                HelloPlan::Challenge { ctx, frame } => {
                    if sink.send(Message::text(frame)).await.is_err() {
                        return;
                    }
                    ctx
                }
                HelloPlan::Refuse {
                    frame,
                    code,
                    reason,
                } => {
                    if let Some(f) = frame {
                        let _ = sink.send(Message::text(f)).await;
                    }
                    return close_with(&mut sink, code, reason).await;
                }
            }
        }
        Ok(_) => return close_with(&mut sink, proto::CLOSE_PROTOCOL, "hello expected").await,
        Err(e) => {
            sh.emit(HubEvent::ProtocolError {
                serial: None,
                detail: e.0,
            });
            return close_with(&mut sink, proto::CLOSE_PROTOCOL, "protocol violation").await;
        }
    };

    // challenge_response
    let answer = match tokio::time::timeout(hs, next_frame(&mut source)).await {
        Ok(Incoming::Text(t)) => proto::parse_from_phone(&t),
        Ok(Incoming::Closed) => return,
        Ok(Incoming::Other) => Err(proto::ProtoError("binary frame".into())),
        Err(_) => return close_with(&mut sink, proto::CLOSE_LIVENESS, "handshake timeout").await,
    };
    let (conn_id, mut rx) = match answer {
        Ok(PhoneFrame::ChallengeResponse {
            epoch,
            nonce,
            key_id,
            signature,
        }) => match sh.on_challenge_response(&ctx, epoch, &nonce, &key_id, &signature) {
            Ok(c) => c,
            Err(frame) => {
                let _ = sink.send(Message::text(frame)).await;
                return close_with(&mut sink, proto::CLOSE_REPAIR, "needs_repair").await;
            }
        },
        Ok(PhoneFrame::NeedsRepair { reason, .. }) => {
            sh.on_handshake_repair(&ctx, &reason);
            return close_with(&mut sink, proto::CLOSE_REPAIR, "needs_repair").await;
        }
        Ok(_) => {
            return close_with(
                &mut sink,
                proto::CLOSE_PROTOCOL,
                "challenge_response expected",
            )
            .await
        }
        Err(e) => {
            sh.emit(HubEvent::ProtocolError {
                serial: Some(ctx.serial.clone()),
                detail: e.0,
            });
            return close_with(&mut sink, proto::CLOSE_PROTOCOL, "protocol violation").await;
        }
    };
    drop(slot);

    // the authenticated link
    let serial = ctx.serial.clone();
    let mut tick = tokio::time::interval(ACK_TICK);
    let mut last_rx = Instant::now();
    loop {
        tokio::select! {
            m = source.next() => {
                match m {
                    Some(Ok(Message::Text(t))) => {
                        last_rx = Instant::now();
                        match proto::parse_from_phone(t.as_str()) {
                            Ok(f) => {
                                if let Some((code, reason)) = sh.on_frame(&serial, conn_id, f) {
                                    flush_out(&mut sink, &mut rx).await;
                                    close_with(&mut sink, code, reason).await;
                                    break;
                                }
                            }
                            Err(e) => {
                                sh.emit(HubEvent::ProtocolError { serial: Some(serial.clone()), detail: e.0 });
                                close_with(&mut sink, proto::CLOSE_PROTOCOL, "protocol violation").await;
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => last_rx = Instant::now(),
                    Some(Ok(Message::Binary(_))) | Some(Ok(Message::Frame(_))) => {
                        close_with(&mut sink, proto::CLOSE_PROTOCOL, "protocol violation").await;
                        break;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                }
            }
            out = rx.recv() => {
                let Some(first) = out else { break };
                if !write_batch(&mut sink, &mut rx, first).await {
                    break;
                }
            }
            _ = tick.tick() => {
                if last_rx.elapsed() > SILENCE_LIMIT {
                    close_with(&mut sink, proto::CLOSE_LIVENESS, "phone silent").await;
                    break;
                }
                sh.flush_ack(&serial, conn_id);
            }
            _ = stop.changed() => {
                close_with(&mut sink, 1001, "hub shutting down").await;
                break;
            }
        }
    }
    sh.on_disconnect(&serial, conn_id);
}

/// Writes `first` and whatever else is queued, then flushes once. False when the socket is gone
/// or a close was written.
async fn write_batch(
    sink: &mut SplitSink<Ws, Message>,
    rx: &mut mpsc::UnboundedReceiver<Out>,
    first: Out,
) -> bool {
    let mut next = Some(first);
    let mut n = 0;
    while let Some(o) = next.take() {
        match o {
            Out::Text(t) => {
                if sink.feed(Message::text(t)).await.is_err() {
                    return false;
                }
            }
            Out::Close(code, reason) => {
                let _ = sink.flush().await;
                close_with(sink, code, reason).await;
                return false;
            }
        }
        n += 1;
        if n < 512 {
            next = rx.try_recv().ok();
        }
    }
    sink.flush().await.is_ok()
}

/// Sends what is already queued (for example a needs_repair) before a close.
async fn flush_out(sink: &mut SplitSink<Ws, Message>, rx: &mut mpsc::UnboundedReceiver<Out>) {
    while let Ok(Out::Text(t)) = rx.try_recv() {
        if sink.feed(Message::text(t)).await.is_err() {
            return;
        }
    }
    let _ = sink.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn payloads_become_json_rpc_requests() {
        let (id, rpc) = build_rpc(json!({"method": "helper.status"}), || "gen-1".into()).unwrap();
        assert_eq!(id, "gen-1");
        assert_eq!(
            rpc,
            json!({"jsonrpc": "2.0", "id": "gen-1", "method": "helper.status"})
        );
        let (id, rpc) = build_rpc(
            json!({"method": "act.tap", "params": {"x": 1}, "id": "job7.step3"}),
            || unreachable!(),
        )
        .unwrap();
        assert_eq!(id, "job7.step3");
        assert_eq!(rpc["params"], json!({"x": 1}));
        for bad in [
            json!([]),
            json!({}),
            json!({"method": "Bad.Name"}),
            json!({"method": "a.b", "params": [1]}),
            json!({"method": "a.b", "id": "has space"}),
            json!({"method": "a.b", "extra": 1}),
            json!({"method": "a.b", "jsonrpc": "1.0"}),
            json!({"method": "a.b", "params": {"blob": "x".repeat(70_000)}}),
        ] {
            assert!(
                matches!(
                    build_rpc(bad.clone(), || "x".into()),
                    Err(HubError::BadPayload(_))
                ),
                "{bad}"
            );
        }
    }
}
