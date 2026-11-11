//! Test support: temp dirs, link.json readers, and an in-process fake phone written to the
//! link.json messages. The fake phone ports `LinkClient.java`'s rules (hub-signature check,
//! epoch fence, seq == ack + 1, request-id cache, outbox + resume) and talks to the hub over a
//! real WebSocket on 127.0.0.1.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use link_hub::clock::Clock;
use link_hub::crypto::{
    b64, hub_challenge_message, key_id, phone_response_message, random_bytes, sign_der, spki_of,
    unb64, verify, HubKey,
};
use link_hub::hub::{Hub, HubConfig};
use link_hub::proto::validate;
use link_hub::store::StateDir;
use p256::ecdsa::SigningKey;
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

// ------------------------------------------------------------------ files

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "agentroid-link-hub-{name}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn scaffold() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

pub fn link_json() -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(scaffold().join("schemas").join("link.json")).unwrap(),
    )
    .unwrap()
}

/// The exact fix text for (from, reason), read from link.json itself (not from the crate).
pub fn schema_fix(from: &str, reason: &str) -> String {
    let s = link_json();
    for b in s["$defs"]["needs_repair"]["oneOf"].as_array().unwrap() {
        let p = &b["properties"];
        if p["from"]["const"] == from && p["reason"]["const"] == reason {
            return p["fix"]["const"].as_str().unwrap().to_string();
        }
    }
    panic!("no fix text for {from}/{reason} in link.json");
}

// ------------------------------------------------------------------ keys and the hub

pub const SERIAL: &str = "POCOX6TEST01";

pub struct PhoneKey {
    pub sk: SigningKey,
    pub spki: Vec<u8>,
    pub key_id: String,
}

pub fn phone_key(seed: u8) -> PhoneKey {
    let sk = SigningKey::from_slice(&[seed; 32]).unwrap();
    let spki = spki_of(sk.verifying_key());
    let key_id = key_id(&spki);
    PhoneKey { sk, spki, key_id }
}

pub fn hub_key() -> HubKey {
    HubKey::from_scalar(&[7u8; 32]).unwrap()
}

pub fn pair_direct(base: &Path, serial: &str, key: &PhoneKey) {
    StateDir::open(base)
        .unwrap()
        .bind_device(serial, &key.spki, 1)
        .unwrap();
}

pub async fn start_hub(base: &Path, key: HubKey, clock: Option<Arc<dyn Clock>>) -> Hub {
    let mut cfg = HubConfig::new(base, "127.0.0.1", 0, key);
    if let Some(c) = clock {
        cfg.clock = c;
    }
    Hub::start(cfg).await.expect("hub starts on 127.0.0.1")
}

pub async fn wait_until(what: &str, secs: u64, f: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f() {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ------------------------------------------------------------------ the fake phone

pub enum PhoneCmd {
    Heartbeat {
        adb_port: Option<u16>,
    },
    Untrusted {
        bssid: Option<String>,
        reason: String,
    },
    HumanTouch {
        touch_ms: u64,
        sent_ms: u64,
    },
    Raw(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum End {
    Stopped,
    Dropped,
    Closed(Option<u16>),
    Repair(String),
    ConnectFailed,
}

pub struct PhoneCfg {
    pub serial: String,
    pub key: PhoneKey,
    /// The hub public key the phone was paired with.
    pub hub_spki: Vec<u8>,
    /// Sign challenge responses with this key instead (bad-signature test).
    pub sign_with: Option<SigningKey>,
    /// Put this in hello.epoch_seen instead of the real max epoch.
    pub lie_epoch_seen: Option<u64>,
    /// Record every hub frame (off for the 10k test).
    pub record: bool,
    /// For `wss://` URLs: the certificate SHA-256 the pairing QR carried (the phone pins it).
    pub tls_fp: Option<[u8; 32]>,
}

#[derive(Default)]
pub struct PhoneState {
    pub instance: String,
    pub max_epoch: u64,
    pub epoch: u64,
    pub rx_acked: u64,
    pub tx_seq: u64,
    pub outbox: BTreeMap<u64, String>,
    pub cache: HashMap<String, (String, Value)>,
    pub executed: HashMap<String, u32>,
    pub executed_total: u64,
    pub replayed: u64,
    pub refused_stale: u64,
    pub requests_seen: u64,
    pub hub_frames: Vec<Value>,
    pub hub_repair: Option<Value>,
    pub close_code: Option<u16>,
    /// Drop the connection right after the n-th execution (odd drops before the response is
    /// sent, even drops after it).
    pub drop_at: VecDeque<u64>,
    pub drops: u32,
    pub handshakes: u32,
    /// Run these, keep the response in the outbox, do not send it.
    pub withhold: HashSet<String>,
    pub resent: u64,
    /// Every `segment` frame's signed segment, in arrival order.
    pub segments_seen: Vec<Value>,
    /// Answer the request with this id with this `result` instead of the echo (a phone that refuses a segment,
    /// a runner.report that returns a report).
    pub canned: HashMap<String, Value>,
    /// Answer the request with this id with these fragments, one per execution, first: each is `{"result": ..}` or
    /// `{"error": {..}}`. A phone's transient answer (error code -32011) is not remembered, as on the real phone, so
    /// the same id can be asked again and gets the next fragment.
    pub scripted: HashMap<String, VecDeque<Value>>,
    /// Answer a `segment` frame that has no canned answer like a phone that took it (`accepted: true`), not with the echo.
    pub accept_segments: bool,
}

pub struct FakePhone {
    pub cfg: PhoneCfg,
    pub st: Mutex<PhoneState>,
    cmd_tx: mpsc::UnboundedSender<PhoneCmd>,
    cmd_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<PhoneCmd>>,
}

fn new_instance() -> String {
    link_hub::crypto::hex(&random_bytes::<16>().unwrap())
}

enum Drop2 {
    No,
    BeforeSend,
    AfterSend,
}

impl FakePhone {
    pub fn new(cfg: PhoneCfg) -> Arc<Self> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let st = PhoneState {
            instance: new_instance(),
            ..Default::default()
        };
        Arc::new(Self {
            cfg,
            st: Mutex::new(st),
            cmd_tx,
            cmd_rx: tokio::sync::Mutex::new(cmd_rx),
        })
    }

    pub fn paired(key_seed: u8, hub: &HubKey) -> Arc<Self> {
        Self::new(PhoneCfg {
            serial: SERIAL.into(),
            key: phone_key(key_seed),
            hub_spki: hub.spki().to_vec(),
            sign_with: None,
            lie_epoch_seen: None,
            record: true,
            tls_fp: None,
        })
    }

    pub fn st(&self) -> MutexGuard<'_, PhoneState> {
        self.st.lock().unwrap()
    }

    pub fn cmd(&self, c: PhoneCmd) {
        self.cmd_tx.send(c).unwrap();
    }

    /// The phone process restarts: a new instance; stream, outbox and cache are gone; the
    /// persisted max epoch stays (TrustStore).
    pub fn restart_process(&self) {
        let mut st = self.st();
        st.instance = new_instance();
        st.epoch = 0;
        st.rx_acked = 0;
        st.tx_seq = 0;
        st.outbox.clear();
        st.cache.clear();
    }

    fn sequenced(st: &mut PhoneState, mut body: Value) -> String {
        st.tx_seq += 1;
        body["epoch"] = json!(st.epoch);
        body["seq"] = json!(st.tx_seq);
        body["ack"] = json!(st.rx_acked);
        let text = body.to_string();
        st.outbox.insert(st.tx_seq, text.clone());
        text
    }

    fn trim(st: &mut PhoneState, acked: u64) {
        while st
            .outbox
            .first_key_value()
            .is_some_and(|(k, _)| *k <= acked)
        {
            st.outbox.pop_first();
        }
    }

    /// `LinkClient.onRequest`, ported.
    fn on_request(st: &mut PhoneState, v: &Value) -> (Vec<String>, Drop2) {
        st.requests_seen += 1;
        let id = v["id"].as_str().unwrap().to_string();
        let seq = v["seq"].as_u64().unwrap();
        let req_epoch = v["epoch"].as_u64().unwrap();
        if req_epoch != st.epoch {
            if req_epoch < st.epoch {
                st.refused_stale += 1;
            }
            let reason = if req_epoch < st.epoch {
                "stale_epoch"
            } else {
                "epoch_ahead"
            };
            let f = json!({"v": 1, "t": "refused", "epoch": st.epoch, "reason": reason, "id": id, "req_seq": seq, "req_epoch": req_epoch});
            return (vec![f.to_string()], Drop2::No);
        }
        if seq <= st.rx_acked {
            return (
                vec![
                    json!({"v": 1, "t": "ack", "epoch": st.epoch, "ack": st.rx_acked}).to_string(),
                ],
                Drop2::No,
            );
        }
        if seq != st.rx_acked + 1 {
            let r = json!({"v": 1, "t": "refused", "epoch": st.epoch, "reason": "out_of_order", "id": id, "req_seq": seq, "req_epoch": req_epoch});
            let s = json!({"v": 1, "t": "resume", "epoch": st.epoch, "last_acked": st.rx_acked});
            return (vec![r.to_string(), s.to_string()], Drop2::No);
        }
        st.rx_acked = seq;
        // a segment frame is dispatched like a request to runner.segment (LinkProto.java does the same)
        let synth;
        let rpc = if v["t"] == "segment" {
            st.segments_seen.push(v["segment"].clone());
            synth = json!({"jsonrpc": "2.0", "id": id.clone(), "method": link_hub::proto::SEGMENT_METHOD, "params": v["segment"]});
            &synth
        } else {
            &v["rpc"]
        };
        let rpc_text = rpc.to_string();
        if let Some((t, resp)) = st.cache.get(&id).cloned() {
            let (out, replayed) = if t == rpc_text {
                st.replayed += 1;
                (resp, true)
            } else {
                (
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": "id already used by a different request"}}),
                    false,
                )
            };
            let f = Self::sequenced(
                st,
                json!({"v": 1, "t": "response", "id": id, "replayed": replayed, "rpc": out}),
            );
            return (vec![f], Drop2::No);
        }
        st.executed_total += 1;
        let n = {
            let e = st.executed.entry(id.clone()).or_insert(0);
            *e += 1;
            *e
        };
        let scripted = st.scripted.get_mut(&id).and_then(VecDeque::pop_front);
        let result = if let Some(frag) = scripted {
            let mut o = json!({"jsonrpc": "2.0", "id": id});
            for (k, v) in frag.as_object().expect("a scripted fragment is an object") {
                o[k.as_str()] = v.clone();
            }
            o
        } else {
            let accepted = st.accept_segments && rpc["method"] == link_hub::proto::SEGMENT_METHOD;
            let body = st.canned.get(&id).cloned().unwrap_or_else(|| {
                if accepted {
                    json!({"accepted": true, "outcome": "started", "segment_id": rpc["params"]["segment"]["segment_id"]})
                } else {
                    json!({"method": rpc["method"], "params": rpc.get("params").cloned().unwrap_or(Value::Null), "run": n})
                }
            });
            json!({"jsonrpc": "2.0", "id": id, "result": body})
        };
        if result.pointer("/error/code").and_then(Value::as_i64) != Some(-32011) {
            st.cache.insert(id.clone(), (rpc_text, result.clone()));
        }
        let f = Self::sequenced(
            st,
            json!({"v": 1, "t": "response", "id": id, "replayed": false, "rpc": result}),
        );
        let mut out = vec![];
        if !st.withhold.contains(&id) {
            out.push(f);
        }
        let mut mode = Drop2::No;
        if st.drop_at.front() == Some(&st.executed_total) {
            st.drop_at.pop_front();
            st.drops += 1;
            mode = if st.drops % 2 == 1 {
                Drop2::BeforeSend
            } else {
                Drop2::AfterSend
            };
        }
        (out, mode)
    }

    fn heartbeat(st: &PhoneState, adb_port: Option<u16>) -> String {
        json!({
            "v": 1, "t": "heartbeat", "epoch": st.epoch, "ts_ms": 1_760_001_000_000u64, "period_s": 15, "ack": st.rx_acked,
            "tx_seq": st.tx_seq, "charging": true, "screen_on": false, "battery_pct": 80, "adb_port": adb_port,
            "adb_port_source": if adb_port.is_some() { "nsd" } else { "none" }, "wifi_adb": true, "a11y_enabled": true,
            "keeper": "ok", "bssid_trusted": true
        })
        .to_string()
    }

    /// One connection: hello, check the hub's challenge, sign, ready, resume, then serve requests
    /// until the hub closes, a drop is due, or `stop` fires.
    pub async fn session(&self, url: &str, stop: &mut watch::Receiver<bool>) -> End {
        if let Some(rest) = url.strip_prefix("wss://") {
            // --link-lan: TLS 1.3 to exactly the pinned certificate, then the same session inside it
            let Some(fp) = self.cfg.tls_fp else {
                return End::ConnectFailed;
            };
            let host = rest.split('/').next().unwrap_or("");
            let Ok(tcp) = tokio::net::TcpStream::connect(host).await else {
                return End::ConnectFailed;
            };
            let (c, name) = link_hub::tls::pinned_connector(fp).unwrap();
            let Ok(tls) = c.connect(name, tcp).await else {
                return End::ConnectFailed;
            };
            let Ok((ws, _)) = tokio_tungstenite::client_async(url, tls).await else {
                return End::ConnectFailed;
            };
            return self.session_ws(ws, stop).await;
        }
        let Ok((ws, _)) = tokio_tungstenite::connect_async(url).await else {
            return End::ConnectFailed;
        };
        self.session_ws(ws, stop).await
    }

    async fn session_ws<S>(
        &self,
        ws: tokio_tungstenite::WebSocketStream<S>,
        stop: &mut watch::Receiver<bool>,
    ) -> End
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let (mut sink, mut source) = ws.split();
        let me = &self.cfg.key;
        let phone_nonce = b64(&random_bytes::<32>().unwrap());
        let (instance, max_epoch) = {
            let st = self.st();
            (st.instance.clone(), st.max_epoch)
        };
        let hello = json!({
            "v": 1, "t": "hello", "key_id": me.key_id, "alg": "SHA256withECDSA", "phone_nonce": phone_nonce,
            "instance": instance, "helper_version": "0.2.0-test", "sdk": 34, "state": "pending_auth",
            "epoch_seen": self.cfg.lie_epoch_seen.unwrap_or(max_epoch)
        });
        if sink.send(Message::text(hello.to_string())).await.is_err() {
            return End::ConnectFailed;
        }

        // challenge
        let ch = match self.read(&mut source).await {
            Ok(v) => v,
            Err(code) => return End::Closed(code),
        };
        if ch["t"] == "needs_repair" {
            return self.hub_repaired(ch, &mut source).await;
        }
        assert_eq!(
            validate(&ch),
            Ok(()),
            "the hub sent an invalid challenge: {ch}"
        );
        assert_eq!(ch["t"], "challenge");
        let epoch = ch["epoch"].as_u64().unwrap();
        let nonce = ch["nonce"].as_str().unwrap().to_string();
        let serial = ch["serial"].as_str().unwrap().to_string();
        let hub_kid = key_id(&self.cfg.hub_spki);
        let good_sig = ch["phone_nonce"] == phone_nonce.as_str()
            && ch["hub_key_id"] == hub_kid.as_str()
            && verify(
                &self.cfg.hub_spki,
                &hub_challenge_message(&nonce, &phone_nonce, epoch, &serial, &me.key_id),
                &unb64(ch["hub_sig"].as_str().unwrap()).unwrap(),
            );
        let phone_reason = if !good_sig {
            Some("bad_hub_signature")
        } else if serial != self.cfg.serial {
            Some("serial_mismatch")
        } else if epoch < max_epoch {
            Some("stale_epoch")
        } else {
            None
        };
        if let Some(r) = phone_reason {
            let fix = schema_fix("phone", r);
            let f = json!({"v": 1, "t": "needs_repair", "from": "phone", "reason": r, "fix": fix, "epoch": epoch});
            let _ = sink.send(Message::text(f.to_string())).await;
            let _ = sink.close().await;
            return End::Repair(r.to_string());
        }
        let signer = self.cfg.sign_with.as_ref().unwrap_or(&me.sk);
        let sig = sign_der(
            signer,
            &phone_response_message(&nonce, &phone_nonce, epoch, &self.cfg.serial, &hub_kid),
        );
        let resp = json!({"v": 1, "t": "challenge_response", "epoch": epoch, "nonce": nonce, "key_id": me.key_id, "signature": b64(&sig)});
        if sink.send(Message::text(resp.to_string())).await.is_err() {
            return End::Closed(None);
        }

        // ready
        let ready = match self.read(&mut source).await {
            Ok(v) => v,
            Err(code) => return End::Closed(code),
        };
        if ready["t"] == "needs_repair" {
            return self.hub_repaired(ready, &mut source).await;
        }
        assert_eq!(ready["t"], "ready", "{ready}");
        assert_eq!(ready["epoch"], epoch);
        assert_eq!(ready["key_id"], me.key_id.as_str());
        let resume = {
            let mut st = self.st();
            if epoch != st.epoch {
                st.epoch = epoch;
                st.rx_acked = 0;
                st.tx_seq = 0;
                st.outbox.clear();
            }
            st.max_epoch = st.max_epoch.max(epoch);
            st.handshakes += 1;
            json!({"v": 1, "t": "resume", "epoch": st.epoch, "last_acked": st.rx_acked}).to_string()
        };
        if sink.send(Message::text(resume)).await.is_err() {
            return End::Closed(None);
        }

        let mut cmds = self.cmd_rx.lock().await;
        loop {
            tokio::select! {
                m = self.read(&mut source) => {
                    let v = match m {
                        Ok(v) => v,
                        Err(code) => return End::Closed(code),
                    };
                    assert_eq!(validate(&v), Ok(()), "the hub sent an invalid frame: {v}");
                    let (outs, mode) = {
                        let mut st = self.st();
                        match v["t"].as_str().unwrap() {
                            "request" | "segment" => Self::on_request(&mut st, &v),
                            "ack" => {
                                if v["epoch"] == st.epoch {
                                    Self::trim(&mut st, v["ack"].as_u64().unwrap());
                                }
                                (vec![], Drop2::No)
                            }
                            "resume" => {
                                let last = v["last_acked"].as_u64().unwrap();
                                if v["epoch"] == st.epoch {
                                    if last > st.tx_seq {
                                        st.tx_seq = last;
                                    }
                                    Self::trim(&mut st, last);
                                    let again: Vec<String> = st.outbox.values().cloned().collect();
                                    st.resent += again.len() as u64;
                                    (again, Drop2::No)
                                } else {
                                    (vec![], Drop2::No)
                                }
                            }
                            "needs_repair" => {
                                drop(st);
                                return self.hub_repaired(v, &mut source).await;
                            }
                            other => panic!("unexpected hub frame {other}: {v}"),
                        }
                    };
                    if matches!(mode, Drop2::BeforeSend) {
                        return End::Dropped; // the response stays in the outbox; sink and source drop: TCP closes
                    }
                    for o in outs {
                        if sink.feed(Message::text(o)).await.is_err() {
                            return End::Closed(None);
                        }
                    }
                    if sink.flush().await.is_err() {
                        return End::Closed(None);
                    }
                    if matches!(mode, Drop2::AfterSend) {
                        return End::Dropped;
                    }
                }
                c = cmds.recv() => {
                    let text = {
                        let mut st = self.st();
                        match c {
                            Some(PhoneCmd::Heartbeat { adb_port }) => Self::heartbeat(&st, adb_port),
                            Some(PhoneCmd::Untrusted { bssid, reason }) => Self::sequenced(&mut st, json!({
                                "v": 1, "t": "untrusted_bssid", "bssid": bssid, "reason": reason,
                                "action": "wifi_adb_not_reasserted", "ts_ms": 1_760_001_000_105u64
                            })),
                            Some(PhoneCmd::HumanTouch { touch_ms, sent_ms }) => Self::sequenced(&mut st, json!({
                                "v": 1, "t": "human_touch", "touch_ms": touch_ms, "sent_ms": sent_ms
                            })),
                            Some(PhoneCmd::Raw(t)) => t,
                            None => return End::Stopped,
                        }
                    };
                    if sink.send(Message::text(text)).await.is_err() {
                        return End::Closed(None);
                    }
                }
                _ = stop.changed() => {
                    let _ = sink.close().await;
                    return End::Stopped;
                }
            }
        }
    }

    async fn hub_repaired<S>(&self, v: Value, source: &mut S) -> End
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        assert_eq!(
            validate(&v),
            Ok(()),
            "the hub sent an invalid needs_repair: {v}"
        );
        let reason = v["reason"].as_str().unwrap().to_string();
        self.st().hub_repair = Some(v);
        // the hub closes with 4001 after a needs_repair
        let code = match self.read(source).await {
            Err(c) => c,
            Ok(other) => panic!("frame after needs_repair: {other}"),
        };
        self.st().close_code = code;
        End::Repair(reason)
    }

    /// The next text frame as JSON, or Err(close code) when the connection ends.
    async fn read<S>(&self, source: &mut S) -> Result<Value, Option<u16>>
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        loop {
            match source.next().await {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(t.as_str()).expect("hub frames are JSON");
                    let mut st = self.st();
                    if self.cfg.record {
                        st.hub_frames.push(v.clone());
                    }
                    return Ok(v);
                }
                Some(Ok(Message::Close(f))) => {
                    let code = f.map(|f| u16::from(f.code));
                    self.st().close_code = code;
                    return Err(code);
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return Err(None),
            }
        }
    }
}

/// A phone that keeps redialling `url` (which a test may change, e.g. after a hub restart).
pub struct PhoneRunner {
    pub url: Arc<Mutex<String>>,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Vec<End>>,
}

impl PhoneRunner {
    pub fn start(phone: Arc<FakePhone>, url: String) -> Self {
        let url = Arc::new(Mutex::new(url));
        let (stop, mut stop_rx) = watch::channel(false);
        let u = url.clone();
        let task = tokio::spawn(async move {
            let mut ends = vec![];
            loop {
                if *stop_rx.borrow() {
                    break;
                }
                let target = u.lock().unwrap().clone();
                let end = phone.session(&target, &mut stop_rx).await;
                let done = matches!(end, End::Stopped | End::Repair(_));
                if end == End::ConnectFailed || matches!(end, End::Closed(_)) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                ends.push(end);
                if done {
                    break;
                }
            }
            ends
        });
        Self { url, stop, task }
    }

    pub fn set_url(&self, url: String) {
        *self.url.lock().unwrap() = url;
    }

    pub async fn stop(self) -> Vec<End> {
        let _ = self.stop.send(true);
        tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("phone stops")
            .unwrap()
    }
}
