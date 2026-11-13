//! The link hub inside the daemon (task 2.12 hub half + 2.14 segments).
//!
//! * **The daemon owns the hub.** [`DaemonLink::start`] runs `link_hub::Hub` on a runtime of its
//!   own. The hub binds one literal address that link-hub's `check_bind_address` accepts: the
//!   laptop's tailnet IPv4 (100.64.0.0/10), or 127.0.0.1 in tests. Never `0.0.0.0`, never a LAN
//!   address: such a config is refused before anything is bound. No address configured: no hub.
//!   (The listener itself lives in `crates/link-hub`; this crate still opens none of its own for
//!   the link.)
//! * **One epoch source.** The hub's [`EpochSource`] is the 2.8 registry: a phone covered by a
//!   lease gets that lease's epoch, any other gets a fresh number from the same persisted counter
//!   ([`Registry::allocate_epoch_above`]); at start the registry is raised above every epoch the hub
//!   ever stored. A new `lease.acquire` moves a paired phone to the new epoch
//!   ([`Hub::adopt_epoch`]), so a request fenced with an old lease's epoch is refused unsent.
//! * **adb stays the fallback** under the same lease: `link.health` says which transport is in use
//!   and why; nothing here touches adb.
//! * **Segments** (`link.segment`): built and classified by `crates/segments` in Phase 2 (R2+ and
//!   `require_confirmation` refused: there is no approval channel), signed with the hub's own link
//!   key (one pinned key on the phone), admitted by the hub-side supervisor, then sent fenced on the
//!   link's `segment` frame (`schemas/link.json`). `link.segment_report` pulls the phone's report
//!   (`runner.report`) and checks it.
//!
//! * **Human touches from the overlay** (`human_touch` frames, the no-adb Detector 2) go to
//!   [`Daemon::phone_touch`](crate::server::Daemon) on the phone's elapsed clock, the same entry the
//!   take-over machine already dates touches with ([`DaemonLink::feed_human_touches`]).
//!
//! The daemon's dispatch is synchronous; every hub call is spawned on the hub's runtime and
//! awaited with a bound ([`CALL_TIMEOUT`]) on a std channel, never `block_on` inside async code.

use crate::proto::*;
use crate::registry::Registry;
use agentroid_segments::{
    build, sign_with, Phase, SafeClasses, Segment, SegmentRequest, Step, Supervisor,
};
use link_hub::crypto::HubKey;
use link_hub::hub::{EpochSource, Hub, HubConfig, HubError, HubEvent};
use link_hub::store::StateDir;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Longest the daemon waits for one hub call (a phone that is away answers when it is back).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A segment's life when the caller gives none.
pub const DEFAULT_SEGMENT_S: u64 = 300;

/// How to start the daemon's hub.
pub struct LinkConfig {
    /// `hub-state/` is created under it.
    pub state_dir: PathBuf,
    /// One literal address: the tailnet IPv4, or 127.0.0.1 for tests.
    pub bind: String,
    pub port: u16,
    pub key: HubKey,
    /// The hub's clock; default the system clock.
    pub clock: Option<Arc<dyn link_hub::clock::Clock>>,
    /// `Some`: serve `wss://` (TLS 1.3, the certificate the pairing QR pins). Required with `lan`.
    pub tls: Option<Arc<link_hub::tls::HubTls>>,
    /// `--link-lan`: `bind` may be a private home-LAN IPv4; the hub refuses this without `tls`.
    pub lan: bool,
}

/// The 2.8 registry as the hub's epoch source.
pub struct RegistryEpochs(pub Arc<Registry>);

impl EpochSource for RegistryEpochs {
    fn epoch_for(&self, serial: &str, floor: u64) -> Result<u64, String> {
        if let Some(l) = self.0.lease_of(serial) {
            if l.epoch > floor {
                return Ok(l.epoch);
            }
        }
        self.0
            .allocate_epoch_above(floor)
            .map_err(|e| format!("{e:?}"))
    }
}

/// The running hub plus the segment supervisors (one per device and epoch).
pub struct DaemonLink {
    rt: Mutex<Option<tokio::runtime::Runtime>>,
    handle: tokio::runtime::Handle,
    hub: Arc<Hub>,
    sups: Arc<Mutex<HashMap<String, Supervisor>>>,
    classes: SafeClasses,
}

/// What the daemon remembers of segments it sent: finished ones older than this are forgotten, and
/// at most [`SEGMENTS_KEPT`] are remembered (a running segment is never forgotten).
const SEGMENT_MEMORY_S: i64 = 7 * 24 * 3600;
const SEGMENTS_KEPT: usize = 1024;

/// True while `id` (a segment id, or the `report-<segment id>` request that pulls its report) names
/// a segment that `device` is still running or may still report on. The hub's response cache never
/// prunes such an answer, by age or by count.
fn is_running_answer(
    sups: &Mutex<HashMap<String, Supervisor>>,
    device: &str,
    id: &str,
    now: i64,
) -> bool {
    let segment_id = id.strip_prefix("report-").unwrap_or(id);
    sups.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(device)
        .is_some_and(|s| s.is_running(segment_id, now))
}

/// The phone's TRANSIENT code (`ErrorCodes.TRANSIENT`): an answer nothing caches ("busy", "no report yet", "the runner
/// did not answer in time"). `data.reason` says which.
const PHONE_TRANSIENT: i64 = -32011;

/// What the phone's answer to `runner.segment` means for the segment.
enum SegmentAnswer {
    /// `accepted: true`: the phone is running it; its report follows.
    Taken,
    /// The phone says nothing ran: the supervisor must not count the segment as running.
    NothingRan(RpcError),
    /// The phone's answer does not say whether it started: it may be running, its report will tell.
    Unknown(RpcError),
    /// The phone has already started this segment id (a repeat): refused, but the first run's state is not touched.
    Duplicate(RpcError),
}

fn read_segment_answer(rpc: &Value, segment_id: &str) -> SegmentAnswer {
    let unknown =
        |what: String, phone: Value| {
            SegmentAnswer::Unknown(RpcError::new(DEVICE_ERROR, what).with_data(
                json!({"segment_id": segment_id, "outcome_unknown": true, "phone": phone}),
            ))
        };
    if let Some(err) = rpc.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = err.get("message").and_then(Value::as_str).unwrap_or("");
        let reason = err
            .pointer("/data/reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        return match (code, reason) {
            // the auth gate or the request shape said no before any handler ran
            (-32003..=-32001 | -32601 | -32602, _) => SegmentAnswer::NothingRan(RpcError::new(REFUSED, format!("the phone refused the call ({code}): {message}; nothing ran")).with_data(json!({"phone": err, "segment_id": segment_id}))),
            // another segment is running: nothing ran and nothing was remembered, so this id may be sent again
            (PHONE_TRANSIENT, "busy") => SegmentAnswer::NothingRan(RpcError::new(REFUSED, "the phone's runner is busy with another segment: nothing ran; send this one again when it has finished").with_data(json!({"phone": {"code": "busy", "reason": message}, "segment_id": segment_id, "retry": true}))),
            (PHONE_TRANSIENT, "runner_timeout") => unknown("the phone's runner did not answer in time: the segment may have started anyway; pull its report to find out".into(), err.clone()),
            _ => unknown(format!("the phone answered runner.segment with an error ({code}: {message}); the segment may be running: pull its report to find out"), err.clone()),
        };
    }
    match rpc.pointer("/result/accepted") {
        Some(Value::Bool(true)) => SegmentAnswer::Taken,
        Some(Value::Bool(false)) => {
            let phone = &rpc["result"];
            let (code, reason) = (phone["code"].as_str().unwrap_or("unknown"), phone["reason"].as_str().unwrap_or(""));
            let e = RpcError::new(REFUSED, format!("the phone refused the segment ({code}): {reason}; nothing ran")).with_data(json!({"phone": phone, "segment_id": segment_id}));
            // a repeat of a segment the phone already started: this call ran nothing, but the first run is still its own
            if code == "duplicate_segment" { SegmentAnswer::Duplicate(e) } else { SegmentAnswer::NothingRan(e) }
        }
        // no `accepted` at all (an older phone, or something that is not the runner answering): not known to be running
        _ => unknown("the phone's answer to runner.segment is not an admission (no `accepted`): the segment may or may not be running; pull its report to find out".into(), rpc.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// True when a hub error means the segment frame never left the hub.
fn nothing_was_sent(e: &HubError) -> bool {
    matches!(
        e,
        HubError::UnknownDevice(_)
            | HubError::StaleEpoch { .. }
            | HubError::Superseded { .. }
            | HubError::Refused(_)
            | HubError::BadPayload(_)
    )
}

fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

fn hub_err(e: &HubError) -> RpcError {
    let code = match e {
        HubError::UnknownDevice(_) => UNKNOWN_DEVICE,
        HubError::BadPayload(_) | HubError::IdReused(_) => INVALID_PARAMS,
        HubError::StaleEpoch { .. } | HubError::Superseded { .. } => NOT_HOLDER,
        HubError::Refused(_) => REFUSED,
        HubError::Store(_) => STATE_ERROR,
        HubError::OutcomeUnknown(_) | HubError::Shutdown => DEVICE_ERROR,
    };
    RpcError::new(code, e.to_string())
}

/// The loop behind [`DaemonLink::feed_human_touches`] (a free function so it can be tested alone).
async fn pump_human_touches(
    mut rx: tokio::sync::broadcast::Receiver<HubEvent>,
    on_touch: impl Fn(&str, u64, u64),
    lagged: Arc<std::sync::atomic::AtomicU64>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(HubEvent::HumanTouch {
                serial,
                touch_ms,
                sent_ms,
            }) => on_touch(
                &serial,
                touch_ms.saturating_mul(1000),
                sent_ms.saturating_mul(1000),
            ),
            Ok(_) => {}
            Err(RecvError::Lagged(n)) => {
                lagged.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
            }
            Err(RecvError::Closed) => return,
        }
    }
}

impl DaemonLink {
    /// Start the hub. The address is checked by link-hub before anything is bound.
    pub fn start(cfg: LinkConfig, registry: Arc<Registry>) -> Result<DaemonLink, String> {
        // the registry's counter goes above every epoch the hub ever handed out
        let stored = StateDir::open(&cfg.state_dir)
            .and_then(|s| s.epochs())
            .map_err(|e| format!("hub state: {e:?}"))?;
        if let Some(max) = stored.values().copied().max() {
            registry
                .raise_floor(max)
                .map_err(|e| format!("registry: {e:?}"))?;
        }
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("link-hub")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let mut hcfg = HubConfig::new(&cfg.state_dir, cfg.bind.as_str(), cfg.port, cfg.key);
        if let Some(c) = cfg.clock {
            hcfg.clock = c;
        }
        hcfg.epochs = Some(Arc::new(RegistryEpochs(registry)));
        hcfg.tls = cfg.tls.clone();
        hcfg.lan = cfg.lan;
        // the response cache is pruned by age and count while the hub runs; a running segment's answer stays
        let sups: Arc<Mutex<HashMap<String, Supervisor>>> = Arc::new(Mutex::new(HashMap::new()));
        let for_hub = sups.clone();
        hcfg.keep_cached = Some(Arc::new(move |device, id| {
            is_running_answer(&for_hub, device, id, now_s())
        }));
        // `block_on` from a plain thread: the caller may itself be inside a runtime
        let started = std::thread::scope(|s| s.spawn(|| rt.block_on(Hub::start(hcfg))).join());
        let hub = match started {
            Ok(Ok(hub)) => hub,
            // a runtime may not be dropped inside async code: shut it down without blocking
            Ok(Err(e)) => {
                rt.shutdown_background();
                return Err(format!("link hub not started: {e:?}"));
            }
            Err(_) => {
                rt.shutdown_background();
                return Err("the hub start panicked".into());
            }
        };
        let handle = rt.handle().clone();
        Ok(DaemonLink {
            rt: Mutex::new(Some(rt)),
            handle,
            hub: Arc::new(hub),
            sups,
            classes: SafeClasses::bundled(),
        })
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    /// Hand every overlay `human_touch` to `on_touch(serial, touch_us, sent_us)`, both on the phone's
    /// elapsed clock in microseconds. Runs on the hub's runtime until the hub stops. Events the
    /// receiver fell behind on are counted, not silently lost: a dropped touch is a person ignored.
    pub fn feed_human_touches(
        &self,
        on_touch: impl Fn(&str, u64, u64) + Send + 'static,
    ) -> Arc<std::sync::atomic::AtomicU64> {
        let lagged = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let rx = self.hub.subscribe();
        self.handle
            .spawn(pump_human_touches(rx, on_touch, lagged.clone()));
        lagged
    }

    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.hub.local_addr()
    }

    /// Run one hub future on the hub's runtime and wait for it (bounded) from sync code.
    fn call<T: Send + 'static>(
        &self,
        f: impl std::future::Future<Output = T> + Send + 'static,
    ) -> Result<T, RpcError> {
        let (tx, rx) = mpsc::channel();
        self.handle.spawn(async move {
            let _ = tx.send(f.await);
        });
        rx.recv_timeout(CALL_TIMEOUT).map_err(|_| {
            RpcError::new(
                DEVICE_ERROR,
                format!(
                    "the link gave no answer within {} s; the call may still run on the phone",
                    CALL_TIMEOUT.as_secs()
                ),
            )
        })
    }

    /// Stop the hub and its runtime.
    pub fn shutdown(&self) {
        let hub = self.hub.clone();
        let _ = self.call(async move { hub.shutdown().await });
        if let Some(rt) = self
            .rt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            rt.shutdown_background();
        }
    }

    /// A new lease on a paired phone: the link moves to the lease's epoch.
    pub fn on_lease(&self, device: &str, epoch: u64) {
        if self.hub.status(device).is_some() {
            let _ = self.hub.adopt_epoch(device, epoch);
        }
    }

    pub fn pair_status(&self, device: Option<&str>) -> Result<LinkPairStatusResult, RpcError> {
        let names = match device {
            Some(d) => vec![d.to_string()],
            None => self.hub.devices().map_err(|e| hub_err(&e))?,
        };
        let devices = names
            .iter()
            .filter_map(|d| self.hub.status(d))
            .map(|s| LinkPairRow {
                device: s.serial,
                key_id: s.key_id,
                revoked: s.revoked,
                epoch: s.epoch,
                connected: s.connected,
                online: s.online,
                last_seen_ms: s.last_seen_ms,
                pending_requests: s.pending_requests,
                superseded: s.superseded,
                needs_repair: s.repair.is_some(),
            })
            .collect();
        Ok(LinkPairStatusResult { devices })
    }

    pub fn health(&self, device: &str, lease_epoch: Option<u64>) -> LinkHealthResult {
        let st = self.hub.status(device);
        let (transport, reason) = match &st {
            None => (
                "adb",
                "not paired with this hub: adb under the same lease".to_string(),
            ),
            Some(s) if s.revoked => (
                "adb",
                "the pairing was revoked: adb until it is paired again".to_string(),
            ),
            Some(s) if s.superseded => ("adb", "a newer hub owns the phone: adb".to_string()),
            Some(s) if s.connected => (
                "link",
                format!("the link is up at epoch {}", s.epoch.unwrap_or(0)),
            ),
            Some(_) => (
                "adb",
                "paired, but the link is down: adb until it reconnects".to_string(),
            ),
        };
        LinkHealthResult {
            device: device.to_string(),
            transport: transport.into(),
            paired: st.is_some(),
            connected: st.as_ref().is_some_and(|s| s.connected),
            online: st.as_ref().is_some_and(|s| s.online),
            link_epoch: st.as_ref().and_then(|s| s.epoch),
            lease_epoch,
            reason,
        }
    }

    /// Build, classify (Phase 2), sign and send one segment, fenced with the lease epoch.
    pub fn segment(
        &self,
        key: &str,
        lease_epoch: u64,
        p: LinkSegmentParams,
    ) -> Result<LinkSegmentResult, RpcError> {
        let steps: Vec<Step> = p
            .steps
            .iter()
            .map(|s| serde_json::from_value(s.clone()))
            .collect::<Result<_, _>>()
            .map_err(|e| {
                RpcError::new(
                    INVALID_PARAMS,
                    format!("a step is not a segment.json step: {e}"),
                )
            })?;
        let now = now_s();
        let expires = now + i64::try_from(p.expires_in_s.unwrap_or(DEFAULT_SEGMENT_S)).unwrap_or(0);
        let req = SegmentRequest {
            segment_id: p.segment_id.clone(),
            run_id: p.run_id,
            device: key.to_string(),
            epoch: lease_epoch,
            owned: p.owned.unwrap_or(false),
            expires,
            steps,
        };
        // Phase 2: anything that is not R0/R1, or needs a confirmation, refuses the whole segment
        let seg: Segment = build(req, &self.classes, Phase::Two, now).map_err(|r| match &r {
            agentroid_segments::BuildRefusal::NotSafe(ds) => RpcError::new(
                REFUSED,
                format!("{r}; Phase 2 has no approval channel, nothing was sent"),
            )
            .with_data(json!({"decisions": ds})),
            agentroid_segments::BuildRefusal::Invalid(_) => {
                RpcError::new(INVALID_PARAMS, r.to_string())
            }
        })?;
        let class = seg.classification.as_str().to_string();
        // the link moves to the lease's epoch first (one epoch source)
        if self.hub.epoch(key).ok() != Some(lease_epoch) {
            self.hub
                .adopt_epoch(key, lease_epoch)
                .map_err(|e| hub_err(&e))?;
        }
        let key_der = self.hub.hub_key().spki().to_vec();
        let hub = self.hub.clone();
        let signed = sign_with(seg, &key_der, |m| hub.hub_key().sign(m))
            .map_err(|e| RpcError::new(INTERNAL_ERROR, format!("segment not signed: {e}")))?;
        {
            let mut sups = self
                .sups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let sup = sups
                .entry(key.to_string())
                .or_insert_with(|| Supervisor::new(lease_epoch, self.classes.clone(), Phase::Two));
            if sup.epoch() != lease_epoch {
                *sup = Supervisor::new(lease_epoch, self.classes.clone(), Phase::Two);
            }
            sup.admit(&signed, &key_der, key, now).map_err(|r| {
                RpcError::new(
                    REFUSED,
                    format!(
                        "the hub's own check refused the segment ({}): nothing was sent",
                        r.code()
                    ),
                )
            })?;
            // bound what the supervisor remembers: finished segments age out, and only so many are kept
            sup.prune(now, SEGMENT_MEMORY_S, SEGMENTS_KEPT);
        }
        let value = serde_json::to_value(&signed)
            .map_err(|e| RpcError::new(INTERNAL_ERROR, e.to_string()))?;
        let (h, k, id) = (self.hub.clone(), key.to_string(), p.segment_id.clone());
        // an outer error is the link giving no answer in time: the outcome is unknown and the segment stays "running"
        let sent =
            self.call(async move { h.send_segment_fenced(&k, lease_epoch, &id, value).await })?;
        let r = match sent {
            Ok(r) => r,
            Err(e) => {
                if nothing_was_sent(&e) {
                    self.refuse(key, &p.segment_id);
                }
                return Err(hub_err(&e));
            }
        };
        // Only `accepted: true` is a segment that is running. Everything else is told apart: a definite refusal
        // (nothing ran: the supervisor is told, so the segment is not "running" and its answer is not pinned), or an
        // outcome that is not known (the segment may be running: it stays "running", and its report decides).
        match read_segment_answer(&r.rpc, &p.segment_id) {
            SegmentAnswer::Taken => {}
            SegmentAnswer::NothingRan(e) => {
                self.refuse(key, &p.segment_id);
                return Err(e);
            }
            SegmentAnswer::Unknown(e) | SegmentAnswer::Duplicate(e) => return Err(e),
        }
        Ok(LinkSegmentResult {
            segment_id: p.segment_id,
            epoch: lease_epoch,
            classification: class,
            from_hub_cache: r.from_hub_cache,
            response: r.rpc,
        })
    }

    /// True while `segment_id` (sent to `key` by this daemon) counts as running: admitted, not reported, not refused,
    /// not expired. The hub's response cache keeps such a segment's answer.
    pub fn segment_is_running(&self, key: &str, segment_id: &str) -> bool {
        is_running_answer(&self.sups, key, segment_id, now_s())
    }

    /// The supervisor is told that nothing ran for this segment (refused by the phone, or never sent).
    fn refuse(&self, key: &str, segment_id: &str) {
        if let Some(sup) = self
            .sups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(key)
        {
            sup.refuse(segment_id);
        }
    }

    /// Pull the phone's report for a segment and check it. The request id is fixed (`report-<segment id>`), which is
    /// safe because the phone answers "not ready" with a transient code that neither the phone nor the hub caches:
    /// asking again after the segment has finished gets the report.
    pub fn segment_report(
        &self,
        key: &str,
        lease_epoch: u64,
        segment_id: &str,
    ) -> Result<LinkSegmentReportResult, RpcError> {
        let payload = json!({"method": "runner.report", "params": {"segment_id": segment_id}, "id": format!("report-{segment_id}")});
        let (h, k) = (self.hub.clone(), key.to_string());
        let r = self
            .call(async move { h.send_request_fenced(&k, lease_epoch, payload).await })?
            .map_err(|e| hub_err(&e))?;
        if let Some(err) = r.rpc.get("error") {
            let not_ready = err.get("code").and_then(Value::as_i64) == Some(PHONE_TRANSIENT)
                && err.pointer("/data/reason").and_then(Value::as_str) == Some("no_report");
            let what = if not_ready {
                format!("the phone has no report for {segment_id} yet (it is still running, or it never took the segment): ask again later")
            } else {
                format!("the phone did not report: {err}")
            };
            return Err(RpcError::new(DEVICE_ERROR, what)
                .with_data(json!({"segment_id": segment_id, "ready": false, "phone_error": err})));
        }
        let report_v = r.rpc.get("result").cloned().ok_or_else(|| {
            RpcError::new(
                DEVICE_ERROR,
                "the phone's answer has neither a result nor an error",
            )
        })?;
        let report: agentroid_segments::Report =
            serde_json::from_value(report_v.clone()).map_err(|e| {
                RpcError::new(
                    DEVICE_ERROR,
                    format!("the phone's report is not a report: {e}"),
                )
            })?;
        let verdict = {
            let mut sups = self
                .sups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let sup = sups.get_mut(key).ok_or_else(|| {
                RpcError::new(
                    STATE_ERROR,
                    format!("no segment was sent to {key} by this daemon"),
                )
            })?;
            sup.check_report(&report)
        };
        Ok(LinkSegmentReportResult {
            segment_id: segment_id.to_string(),
            ok: verdict.ok(),
            outcome: verdict.outcome.clone(),
            findings: verdict.findings.iter().map(|f| format!("{f:?}")).collect(),
            report: report_v,
        })
    }
}

impl crate::server::Daemon {
    /// A device name as the hub knows it (the hardware serial when adb sees the phone; else as given).
    fn link_key(&self, name: &str) -> String {
        self.resolve(name)
            .map(|d| crate::server::device_key(&d))
            .unwrap_or_else(|_| name.to_string())
    }

    /// The `link.*` methods.
    pub(crate) fn link_dispatch(
        &self,
        conn: u64,
        method: &str,
        params: &Value,
    ) -> Result<Value, RpcError> {
        use crate::server::{parse_params, to_value};
        let link = self.link.get().ok_or_else(|| RpcError::new(STATE_ERROR, "no link hub in this daemon (start bridged with --link-bind <tailnet address>); adb is the transport"))?;
        match method {
            "link.pair_status" => {
                let p: LinkPairStatusParams = parse_params(params)?;
                let key = p.device.map(|d| self.link_key(&d));
                to_value(&link.pair_status(key.as_deref())?)
            }
            "link.health" => {
                let p: LinkHealthParams = parse_params(params)?;
                let key = self.link_key(&p.device);
                to_value(&link.health(&key, self.registry.lease_of(&key).map(|l| l.epoch)))
            }
            "link.segment" | "link.segment_report" => {
                let device = params
                    .get("device")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let d = self.resolve(&device)?;
                self.require_allowed(&d)?;
                let key = crate::server::device_key(&d);
                self.require_holder(&key, conn)?;
                let mode = self.backend().mode_of(&d);
                if mode.as_str() == "read_only" {
                    return Err(RpcError::new(
                        REFUSED,
                        format!("{key} is read_only: no segment runs on it"),
                    )
                    .with_data(json!({"mode": mode.as_str()})));
                }
                let epoch = self
                    .registry
                    .lease_of(&key)
                    .map(|l| l.epoch)
                    .ok_or_else(|| RpcError::new(NOT_HOLDER, format!("no lease on {key}")))?;
                if method == "link.segment" {
                    let p: LinkSegmentParams = parse_params(params)?;
                    let id = p.segment_id.clone();
                    // the screen lock, before anything is built, signed or sent: a segment
                    // changes the screen like any tap
                    let ticket = self
                        .backend()
                        .lock_admit(&d.serial, "link.segment")
                        .map_err(|e| {
                            RpcError::new(REFUSED, e.to_string())
                                .with_data(json!({"rule": "screen_lock"}))
                        })?;
                    let r = link.segment(&key, epoch, p);
                    // the phone's answer is in (or the segment may be running): the end of the
                    // agent's own action. A segment that failed before it ran cancels its ticket
                    // (dropped: nothing it did is the agent's)
                    match ticket {
                        Some(t) if r.is_ok() || link.segment_is_running(&key, &id) => {
                            self.backend().lock_done(t)
                        }
                        cancelled => drop(cancelled),
                    }
                    to_value(&r?)
                } else {
                    let p: LinkSegmentReportParams = parse_params(params)?;
                    to_value(&link.segment_report(&key, epoch, &p.segment_id)?)
                }
            }
            other => Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            )),
        }
    }
}

impl Drop for DaemonLink {
    fn drop(&mut self) {
        if let Some(rt) = self
            .rt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod human_touch_tests {
    use super::*;

    #[test]
    fn overlay_touches_reach_the_daemon_in_microseconds_and_nothing_else_does() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = got.clone();
        let lagged = Arc::new(std::sync::atomic::AtomicU64::new(0));
        tx.send(HubEvent::Disconnected { serial: "x".into() })
            .unwrap();
        tx.send(HubEvent::HumanTouch {
            serial: "POCO".into(),
            touch_ms: 86_400_000,
            sent_ms: 86_400_020,
        })
        .unwrap();
        drop(tx);
        rt.block_on(pump_human_touches(
            rx,
            move |s: &str, t, sent| g.lock().unwrap().push((s.to_string(), t, sent)),
            lagged.clone(),
        ));
        assert_eq!(
            *got.lock().unwrap(),
            vec![("POCO".to_string(), 86_400_000_000, 86_400_020_000)]
        );
        assert_eq!(lagged.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_receiver_that_falls_behind_counts_what_it_missed() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = tokio::sync::broadcast::channel(2);
        for i in 0..5 {
            tx.send(HubEvent::HumanTouch {
                serial: "POCO".into(),
                touch_ms: i,
                sent_ms: i,
            })
            .unwrap();
        }
        drop(tx);
        let lagged = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let n = Arc::new(Mutex::new(0));
        let c = n.clone();
        rt.block_on(pump_human_touches(
            rx,
            move |_: &str, _, _| *c.lock().unwrap() += 1,
            lagged.clone(),
        ));
        assert_eq!(lagged.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(*n.lock().unwrap(), 2);
    }
}
