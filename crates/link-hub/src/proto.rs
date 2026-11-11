//! Link protocol v1 (`schemas/link.json` is the source of truth; `tests/interop.rs` checks this
//! module against it and against every fixture). A strict validator for all thirteen message types
//! (`segment`, hub -> phone, carries a signed 2.14 runner segment)
//! (unknown fields, wrong types, out-of-range numbers and the cross-field rules of
//! `test_schemas.py::link_cross_checks` are all refused), the exact fix texts, and builders for
//! the frames the hub sends.

use serde_json::{json, Map, Value};

use crate::crypto::{is_key_id, nonce_ok, unb64};

pub const VERSION: u64 = 1;
/// The 64 KiB JSON-RPC request cap plus room for the envelope (`LinkProto.MAX_FRAME_BYTES`).
pub const MAX_FRAME_BYTES: usize = 64 * 1024 + 1024;
/// Largest `rpc` the hub will put in a request.
pub const MAX_RPC_BYTES: usize = 64 * 1024;

/// Close codes (LINK-REPORT item 12).
pub const CLOSE_REPAIR: u16 = 4001;
pub const CLOSE_PROTOCOL: u16 = 4002;
pub const CLOSE_LIVENESS: u16 = 4003;
pub const CLOSE_OUTBOX: u16 = 4004;

pub const TYPES: [&str; 14] = [
    "hello",
    "challenge",
    "challenge_response",
    "ready",
    "needs_repair",
    "heartbeat",
    "request",
    "response",
    "ack",
    "resume",
    "refused",
    "untrusted_bssid",
    "segment",
    "human_touch",
];

/// The method a phone dispatches a `segment` frame to (the frame is answered like a request:
/// one `response` with the same id, cached by id, never run twice).
pub const SEGMENT_METHOD: &str = "runner.segment";

const PAIR: &str =
    "On the laptop, with the phone reachable over adb, run: agentroid link pair <serial>";

/// (from, reason, fix): the nine exact triples of link.json `$defs/needs_repair`.
pub const FIX_TEXTS: [(&str, &str, &str); 9] = [
    ("phone", "hub_key_missing", "This phone has no hub key yet. On the laptop, with the phone reachable over adb, run: agentroid link pair <serial>"),
    ("phone", "bad_hub_signature", "The hub's signature did not match the hub key this phone holds (the laptop's hub key changed, or another machine answered at the hub address). On the laptop, with the phone reachable over adb, run: agentroid link pair <serial>"),
    ("phone", "serial_mismatch", "The hub named a different device serial from the one this phone was paired as. On the laptop, with the phone reachable over adb, run: agentroid link pair <serial>"),
    ("phone", "stale_epoch", "A hub with an older epoch tried to take this phone. Stop the old hub process (only the newest hub may own a phone); the current hub then reconnects by itself."),
    ("phone", "bad_challenge", "The hub sent a challenge this phone could not read. Update the hub (bridged) and the helper app so both speak link protocol v1, then let the hub reconnect."),
    ("phone", "key_unavailable", "This phone's link key cannot sign (the Android Keystore refused it). On the laptop, with the phone reachable over adb, run: agentroid link pair <serial> --rekey"),
    ("hub", "unknown_device_key", "The hub has no record of this phone's link key. On the laptop, with the phone reachable over adb, run: agentroid link pair <serial>"),
    ("hub", "bad_phone_signature", "The phone's signature did not verify with the key the hub holds for it. On the laptop, with the phone reachable over adb, run: agentroid link pair <serial>"),
    ("hub", "key_revoked", "This phone's link key was revoked on the hub. To let the phone back in, on the laptop, with the phone reachable over adb, run: agentroid link pair <serial>"),
];

/// The exact fix text for (from, reason), with the literal `<serial>` as on the wire.
pub fn fix_text(from: &str, reason: &str) -> Option<&'static str> {
    FIX_TEXTS
        .iter()
        .find(|(f, r, _)| *f == from && *r == reason)
        .map(|(_, _, t)| *t)
}

/// What the user is shown: the fix text with `<serial>` replaced (link.json `$defs/needs_repair`).
pub fn fix_for_display(fix: &str, serial: Option<&str>) -> String {
    match serial {
        Some(s) => fix.replace("<serial>", s),
        None => fix.to_string(),
    }
}

/// The shared tail of most fix texts, exposed so tests can check the CLI name.
pub fn pair_command_text() -> &'static str {
    PAIR
}

// ------------------------------------------------------------------ validation

type Obj = Map<String, Value>;

fn fields(o: &Obj, required: &[&str], optional: &[&str]) -> Result<(), String> {
    for k in required {
        if !o.contains_key(*k) {
            return Err(format!("missing field {k}"));
        }
    }
    for k in o.keys() {
        if !required.contains(&k.as_str()) && !optional.contains(&k.as_str()) {
            return Err(format!("unknown field {k}"));
        }
    }
    Ok(())
}

/// An integer >= min (floats such as 1.0 are refused: the Java side only writes integers).
fn int(o: &Obj, k: &str, min: i64) -> Result<i64, String> {
    // as_i64 is None for floats and for integers above i64::MAX
    match o.get(k).and_then(Value::as_i64) {
        Some(n) if n >= min => Ok(n),
        _ => Err(format!("{k} must be an integer >= {min}")),
    }
}

fn int_or_null(o: &Obj, k: &str, min: i64, max: i64) -> Result<(), String> {
    match o.get(k) {
        Some(Value::Null) => Ok(()),
        Some(v) if v.as_i64().is_some_and(|n| (min..=max).contains(&n)) => Ok(()),
        _ => Err(format!("{k} must be null or an integer {min}..={max}")),
    }
}

fn boolean(o: &Obj, k: &str) -> Result<bool, String> {
    o.get(k)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{k} must be a boolean"))
}

fn bool_or_null(o: &Obj, k: &str) -> Result<(), String> {
    match o.get(k) {
        Some(Value::Null) | Some(Value::Bool(_)) => Ok(()),
        _ => Err(format!("{k} must be a boolean or null")),
    }
}

fn string<'a>(o: &'a Obj, k: &str) -> Result<&'a str, String> {
    o.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{k} must be a string"))
}

fn one_of(o: &Obj, k: &str, allowed: &[&str]) -> Result<(), String> {
    let s = string(o, k)?;
    if allowed.contains(&s) {
        Ok(())
    } else {
        Err(format!("{k} {s:?} is not one of {allowed:?}"))
    }
}

fn charset(s: &str, min: usize, max: usize, ok: impl Fn(char) -> bool) -> bool {
    let n = s.chars().count();
    n >= min && n <= max && s.chars().all(ok)
}

pub fn is_serial(s: &str) -> bool {
    charset(s, 1, 64, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')
    })
}

pub fn is_request_id(s: &str) -> bool {
    charset(s, 1, 128, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')
    })
}

fn is_instance(s: &str) -> bool {
    charset(s, 16, 64, |c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
    })
}

/// `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`, at most 64 characters.
pub fn is_method(s: &str) -> bool {
    s.len() <= 64
        && s.split('.').all(|part| {
            let mut c = part.chars();
            matches!(c.next(), Some('a'..='z'))
                && c.all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == '_')
        })
}

fn is_bssid(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6
        && parts.iter().all(|p| {
            p.len() == 2
                && p.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
}

fn pattern(o: &Obj, k: &str, ok: fn(&str) -> bool) -> Result<(), String> {
    if ok(string(o, k)?) {
        Ok(())
    } else {
        Err(format!("{k} has the wrong form"))
    }
}

fn nonce(o: &Obj, k: &str) -> Result<(), String> {
    let s = string(o, k)?;
    if (44..=88).contains(&s.len()) && nonce_ok(s) {
        Ok(())
    } else {
        Err(format!("{k} is not canonical base64 of 32..64 bytes"))
    }
}

fn sig(o: &Obj, k: &str) -> Result<(), String> {
    let s = string(o, k)?;
    if (12..=96).contains(&s.len()) && unb64(s).is_some() {
        Ok(())
    } else {
        Err(format!("{k} is not a base64 DER signature"))
    }
}

const REPAIR_REASONS: [&str; 9] = [
    "hub_key_missing",
    "bad_hub_signature",
    "serial_mismatch",
    "stale_epoch",
    "bad_challenge",
    "key_unavailable",
    "unknown_device_key",
    "bad_phone_signature",
    "key_revoked",
];

/// The envelope of `schemas/segment.json` (`{segment, signature}`): the frame carries it
/// opaquely; the full segment rules are the phone's verifier and `test_schemas.py`'s.
fn signed_segment(o: &Obj) -> Result<(), String> {
    let s = o
        .get("segment")
        .and_then(Value::as_object)
        .ok_or("segment must be an object")?;
    fields(s, &["segment", "signature"], &[])?;
    let body = s["segment"]
        .as_object()
        .ok_or("segment.segment must be an object")?;
    if body.get("v").and_then(Value::as_u64) != Some(1) {
        return Err("segment.segment.v must be 1".into());
    }
    if !body
        .get("segment_id")
        .and_then(Value::as_str)
        .is_some_and(|x| !x.is_empty())
    {
        return Err("segment.segment.segment_id must be a string".into());
    }
    let sig = s["signature"]
        .as_object()
        .ok_or("segment.signature must be an object")?;
    fields(sig, &["alg", "key_id", "value"], &[])?;
    one_of(sig, "alg", &["ECDSA-P256-SHA256"])?;
    pattern(sig, "key_id", is_key_id)?;
    if string(sig, "value").ok().and_then(unb64).is_none() {
        return Err("segment.signature.value must be base64".into());
    }
    Ok(())
}

fn rpc_request(o: &Obj, id: &str) -> Result<(), String> {
    let rpc = o
        .get("rpc")
        .and_then(Value::as_object)
        .ok_or("rpc must be an object")?;
    fields(rpc, &["jsonrpc", "id", "method"], &["params"])?;
    if rpc["jsonrpc"] != "2.0" {
        return Err("rpc.jsonrpc must be \"2.0\"".into());
    }
    pattern(rpc, "id", is_request_id)?;
    pattern(rpc, "method", is_method)?;
    if rpc.get("params").is_some_and(|p| !p.is_object()) {
        return Err("rpc.params must be an object".into());
    }
    if rpc["id"] != id {
        return Err("rpc.id differs from id".into());
    }
    Ok(())
}

fn rpc_response(o: &Obj, id: &str) -> Result<(), String> {
    let rpc = o
        .get("rpc")
        .and_then(Value::as_object)
        .ok_or("rpc must be an object")?;
    fields(rpc, &["jsonrpc", "id"], &["result", "error"])?;
    if rpc["jsonrpc"] != "2.0" {
        return Err("rpc.jsonrpc must be \"2.0\"".into());
    }
    pattern(rpc, "id", is_request_id)?;
    match (rpc.contains_key("result"), rpc.get("error")) {
        (true, None) => {}
        (false, Some(Value::Object(e))) => {
            fields(e, &["code", "message"], &["data"])?;
            if !e["code"].is_i64() && !e["code"].is_u64() {
                return Err("rpc.error.code must be an integer".into());
            }
            string(e, "message")?;
        }
        (false, Some(_)) => return Err("rpc.error must be an object".into()),
        _ => return Err("rpc needs exactly one of result and error".into()),
    }
    if rpc["id"] != id {
        return Err("rpc.id differs from id".into());
    }
    Ok(())
}

/// Validates one frame against link.json plus the cross-field checks. Direction-agnostic.
pub fn validate(v: &Value) -> Result<(), String> {
    let o = v.as_object().ok_or("a frame is one JSON object")?;
    if o.get("v").and_then(Value::as_u64) != Some(VERSION) {
        return Err("v must be 1".into());
    }
    let t = string(o, "t")?;
    let base = ["v", "t"];
    let req = |extra: &[&'static str]| -> Vec<&'static str> {
        base.iter().copied().chain(extra.iter().copied()).collect()
    };
    match t {
        "hello" => {
            fields(
                o,
                &req(&[
                    "key_id",
                    "alg",
                    "phone_nonce",
                    "instance",
                    "helper_version",
                    "sdk",
                    "state",
                    "epoch_seen",
                ]),
                &["repair_reason"],
            )?;
            pattern(o, "key_id", is_key_id)?;
            one_of(o, "alg", &["SHA256withECDSA"])?;
            nonce(o, "phone_nonce")?;
            pattern(o, "instance", is_instance)?;
            if !charset(string(o, "helper_version")?, 1, 32, |_| true) {
                return Err("helper_version must be 1..32 characters".into());
            }
            int(o, "sdk", 29)?;
            one_of(o, "state", &["pending_auth", "needs_repair"])?;
            if o.contains_key("repair_reason") {
                one_of(o, "repair_reason", &REPAIR_REASONS)?;
            }
            int(o, "epoch_seen", 0)?;
        }
        "challenge" => {
            fields(
                o,
                &req(&[
                    "epoch",
                    "nonce",
                    "phone_nonce",
                    "serial",
                    "hub_key_id",
                    "hub_sig",
                ]),
                &[],
            )?;
            int(o, "epoch", 1)?;
            nonce(o, "nonce")?;
            nonce(o, "phone_nonce")?;
            pattern(o, "serial", is_serial)?;
            pattern(o, "hub_key_id", is_key_id)?;
            sig(o, "hub_sig")?;
        }
        "challenge_response" => {
            fields(o, &req(&["epoch", "nonce", "key_id", "signature"]), &[])?;
            int(o, "epoch", 1)?;
            nonce(o, "nonce")?;
            pattern(o, "key_id", is_key_id)?;
            sig(o, "signature")?;
        }
        "ready" => {
            fields(o, &req(&["epoch", "key_id"]), &[])?;
            int(o, "epoch", 1)?;
            pattern(o, "key_id", is_key_id)?;
        }
        "needs_repair" => {
            fields(o, &req(&["from", "reason", "fix"]), &["epoch"])?;
            one_of(o, "from", &["phone", "hub"])?;
            one_of(o, "reason", &REPAIR_REASONS)?;
            let fix = string(o, "fix")?;
            if o.contains_key("epoch") {
                int(o, "epoch", 0)?;
            }
            match fix_text(string(o, "from")?, string(o, "reason")?) {
                Some(exact) if exact == fix => {}
                Some(_) => return Err("fix is not the exact text for this reason".into()),
                None => return Err("this reason is not sent from this side".into()),
            }
        }
        "heartbeat" => {
            fields(
                o,
                &req(&[
                    "epoch",
                    "ts_ms",
                    "period_s",
                    "ack",
                    "tx_seq",
                    "charging",
                    "screen_on",
                    "battery_pct",
                    "adb_port",
                    "adb_port_source",
                    "wifi_adb",
                    "a11y_enabled",
                    "keeper",
                    "bssid_trusted",
                ]),
                &[],
            )?;
            int(o, "epoch", 1)?;
            int(o, "ts_ms", 0)?;
            match o["period_s"].as_u64() {
                Some(15) | Some(60) => {}
                _ => return Err("period_s must be 15 or 60".into()),
            }
            int(o, "ack", 0)?;
            int(o, "tx_seq", 0)?;
            boolean(o, "charging")?;
            boolean(o, "screen_on")?;
            int_or_null(o, "battery_pct", 0, 100)?;
            int_or_null(o, "adb_port", 1, 65535)?;
            one_of(o, "adb_port_source", &["nsd", "none"])?;
            bool_or_null(o, "wifi_adb")?;
            bool_or_null(o, "a11y_enabled")?;
            one_of(
                o,
                "keeper",
                &[
                    "ok",
                    "untrusted_bssid",
                    "no_wifi",
                    "no_permission",
                    "absent",
                ],
            )?;
            bool_or_null(o, "bssid_trusted")?;
        }
        "request" => {
            fields(o, &req(&["epoch", "seq", "id", "rpc"]), &[])?;
            int(o, "epoch", 1)?;
            int(o, "seq", 1)?;
            pattern(o, "id", is_request_id)?;
            rpc_request(o, string(o, "id")?)?;
        }
        "segment" => {
            fields(o, &req(&["epoch", "seq", "id", "segment"]), &[])?;
            int(o, "epoch", 1)?;
            int(o, "seq", 1)?;
            pattern(o, "id", is_request_id)?;
            signed_segment(o)?;
        }
        "response" => {
            fields(
                o,
                &req(&["epoch", "seq", "ack", "id", "replayed", "rpc"]),
                &[],
            )?;
            int(o, "epoch", 1)?;
            int(o, "seq", 1)?;
            int(o, "ack", 0)?;
            pattern(o, "id", is_request_id)?;
            boolean(o, "replayed")?;
            rpc_response(o, string(o, "id")?)?;
        }
        "ack" => {
            fields(o, &req(&["epoch", "ack"]), &[])?;
            int(o, "epoch", 1)?;
            int(o, "ack", 0)?;
        }
        "resume" => {
            fields(o, &req(&["epoch", "last_acked"]), &[])?;
            int(o, "epoch", 1)?;
            int(o, "last_acked", 0)?;
        }
        "refused" => {
            fields(
                o,
                &req(&["epoch", "reason"]),
                &["id", "req_seq", "req_epoch"],
            )?;
            let epoch = int(o, "epoch", 0)?;
            one_of(
                o,
                "reason",
                &[
                    "stale_epoch",
                    "epoch_ahead",
                    "not_ready",
                    "out_of_order",
                    "bad_request",
                ],
            )?;
            if o.contains_key("id") {
                pattern(o, "id", is_request_id)?;
            }
            if o.contains_key("req_seq") {
                int(o, "req_seq", 0)?;
            }
            let req_epoch = if o.contains_key("req_epoch") {
                Some(int(o, "req_epoch", 0)?)
            } else {
                None
            };
            match (string(o, "reason")?, req_epoch) {
                ("stale_epoch" | "epoch_ahead", None) => {
                    return Err("req_epoch is required for stale_epoch and epoch_ahead".into())
                }
                ("stale_epoch", Some(r)) if r >= epoch => {
                    return Err("stale_epoch needs req_epoch < epoch".into())
                }
                ("epoch_ahead", Some(r)) if r <= epoch => {
                    return Err("epoch_ahead needs req_epoch > epoch".into())
                }
                _ => {}
            }
        }
        "untrusted_bssid" => {
            fields(
                o,
                &req(&["epoch", "seq", "ack", "bssid", "reason", "action", "ts_ms"]),
                &[],
            )?;
            int(o, "epoch", 1)?;
            int(o, "seq", 1)?;
            int(o, "ack", 0)?;
            one_of(
                o,
                "reason",
                &["not_in_trusted_list", "no_trusted_list", "bssid_unreadable"],
            )?;
            one_of(o, "action", &["wifi_adb_not_reasserted"])?;
            int(o, "ts_ms", 0)?;
            let unreadable = o["reason"] == "bssid_unreadable";
            match &o["bssid"] {
                Value::Null if unreadable => {}
                Value::String(b) if !unreadable && is_bssid(b) => {}
                _ => return Err("bssid must be null exactly when the reason is bssid_unreadable, else a lower-case bssid".into()),
            }
        }
        "human_touch" => {
            fields(
                o,
                &req(&["epoch", "seq", "ack", "touch_ms", "sent_ms"]),
                &[],
            )?;
            int(o, "epoch", 1)?;
            int(o, "seq", 1)?;
            int(o, "ack", 0)?;
            let touch = int(o, "touch_ms", 0)?;
            if int(o, "sent_ms", 0)? < touch {
                return Err("sent_ms is before touch_ms".into());
            }
        }
        other => return Err(format!("unknown type {other:?}")),
    }
    Ok(())
}

// ------------------------------------------------------------------ frames from the phone

/// The beacon fields merged into a heartbeat.
#[derive(Debug, Clone, PartialEq)]
pub struct Heartbeat {
    pub epoch: u64,
    pub ts_ms: u64,
    pub period_s: u64,
    pub ack: u64,
    pub tx_seq: u64,
    pub charging: bool,
    pub screen_on: bool,
    pub battery_pct: Option<u8>,
    pub adb_port: Option<u16>,
    pub adb_port_source: String,
    pub wifi_adb: Option<bool>,
    pub a11y_enabled: Option<bool>,
    pub keeper: String,
    pub bssid_trusted: Option<bool>,
}

/// A frame the phone may send, after [`validate`].
#[derive(Debug, Clone, PartialEq)]
pub enum PhoneFrame {
    Hello {
        key_id: String,
        phone_nonce: String,
        instance: String,
        helper_version: String,
        sdk: u64,
        state: String,
        repair_reason: Option<String>,
        epoch_seen: u64,
    },
    ChallengeResponse {
        epoch: u64,
        nonce: String,
        key_id: String,
        signature: Vec<u8>,
    },
    Heartbeat(Heartbeat),
    Response {
        epoch: u64,
        seq: u64,
        ack: u64,
        id: String,
        replayed: bool,
        rpc: Value,
    },
    Ack {
        epoch: u64,
        ack: u64,
    },
    Resume {
        epoch: u64,
        last_acked: u64,
    },
    Refused {
        epoch: u64,
        reason: String,
        id: Option<String>,
        req_seq: Option<u64>,
        req_epoch: Option<u64>,
    },
    UntrustedBssid {
        epoch: u64,
        seq: u64,
        ack: u64,
        bssid: Option<String>,
        reason: String,
        ts_ms: u64,
    },
    /// The overlay saw a human touch: `touch_ms` and `sent_ms` on the phone's elapsedRealtime.
    HumanTouch {
        epoch: u64,
        seq: u64,
        ack: u64,
        touch_ms: u64,
        sent_ms: u64,
    },
    NeedsRepair {
        reason: String,
        fix: String,
        epoch: Option<u64>,
    },
}

/// Why a phone frame was refused (the connection is then closed with 4002).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoError(pub String);

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol violation: {}", self.0)
    }
}

impl std::error::Error for ProtoError {}

fn u(v: &Value, k: &str) -> u64 {
    v[k].as_u64().unwrap_or(0)
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

/// Parses and validates one text frame from the phone. Hub-only types (challenge, ready, request),
/// needs_repair claiming `from: hub`, and anything link.json refuses are protocol errors.
pub fn parse_from_phone(text: &str) -> Result<PhoneFrame, ProtoError> {
    if text.len() > MAX_FRAME_BYTES {
        return Err(ProtoError("frame too large".into()));
    }
    let v: Value = serde_json::from_str(text).map_err(|_| ProtoError("invalid JSON".into()))?;
    validate(&v).map_err(ProtoError)?;
    let t = v["t"].as_str().unwrap_or_default();
    Ok(match t {
        "hello" => PhoneFrame::Hello {
            key_id: s(&v, "key_id"),
            phone_nonce: s(&v, "phone_nonce"),
            instance: s(&v, "instance"),
            helper_version: s(&v, "helper_version"),
            sdk: u(&v, "sdk"),
            state: s(&v, "state"),
            repair_reason: v
                .get("repair_reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            epoch_seen: u(&v, "epoch_seen"),
        },
        "challenge_response" => PhoneFrame::ChallengeResponse {
            epoch: u(&v, "epoch"),
            nonce: s(&v, "nonce"),
            key_id: s(&v, "key_id"),
            signature: unb64(v["signature"].as_str().unwrap_or_default()).unwrap_or_default(),
        },
        "heartbeat" => PhoneFrame::Heartbeat(Heartbeat {
            epoch: u(&v, "epoch"),
            ts_ms: u(&v, "ts_ms"),
            period_s: u(&v, "period_s"),
            ack: u(&v, "ack"),
            tx_seq: u(&v, "tx_seq"),
            charging: v["charging"] == true,
            screen_on: v["screen_on"] == true,
            battery_pct: v["battery_pct"].as_u64().map(|n| n as u8),
            adb_port: v["adb_port"].as_u64().map(|n| n as u16),
            adb_port_source: s(&v, "adb_port_source"),
            wifi_adb: v["wifi_adb"].as_bool(),
            a11y_enabled: v["a11y_enabled"].as_bool(),
            keeper: s(&v, "keeper"),
            bssid_trusted: v["bssid_trusted"].as_bool(),
        }),
        "response" => PhoneFrame::Response {
            epoch: u(&v, "epoch"),
            seq: u(&v, "seq"),
            ack: u(&v, "ack"),
            id: s(&v, "id"),
            replayed: v["replayed"] == true,
            rpc: v["rpc"].clone(),
        },
        "ack" => PhoneFrame::Ack {
            epoch: u(&v, "epoch"),
            ack: u(&v, "ack"),
        },
        "resume" => PhoneFrame::Resume {
            epoch: u(&v, "epoch"),
            last_acked: u(&v, "last_acked"),
        },
        "refused" => PhoneFrame::Refused {
            epoch: u(&v, "epoch"),
            reason: s(&v, "reason"),
            id: v.get("id").and_then(Value::as_str).map(str::to_string),
            req_seq: v.get("req_seq").and_then(Value::as_u64),
            req_epoch: v.get("req_epoch").and_then(Value::as_u64),
        },
        "untrusted_bssid" => PhoneFrame::UntrustedBssid {
            epoch: u(&v, "epoch"),
            seq: u(&v, "seq"),
            ack: u(&v, "ack"),
            bssid: v["bssid"].as_str().map(str::to_string),
            reason: s(&v, "reason"),
            ts_ms: u(&v, "ts_ms"),
        },
        "human_touch" => PhoneFrame::HumanTouch {
            epoch: u(&v, "epoch"),
            seq: u(&v, "seq"),
            ack: u(&v, "ack"),
            touch_ms: u(&v, "touch_ms"),
            sent_ms: u(&v, "sent_ms"),
        },
        "needs_repair" if v["from"] == "phone" => PhoneFrame::NeedsRepair {
            reason: s(&v, "reason"),
            fix: s(&v, "fix"),
            epoch: v.get("epoch").and_then(Value::as_u64),
        },
        other => {
            return Err(ProtoError(format!(
                "the phone may not send {other:?} frames"
            )))
        }
    })
}

// ------------------------------------------------------------------ frames to the phone

fn text(v: Value) -> String {
    debug_assert_eq!(validate(&v), Ok(()), "hub built an invalid frame: {v}");
    v.to_string()
}

pub fn challenge(
    epoch: u64,
    nonce: &str,
    phone_nonce: &str,
    serial: &str,
    hub_key_id: &str,
    hub_sig_b64: &str,
) -> String {
    text(
        json!({"v": 1, "t": "challenge", "epoch": epoch, "nonce": nonce, "phone_nonce": phone_nonce, "serial": serial, "hub_key_id": hub_key_id, "hub_sig": hub_sig_b64}),
    )
}

pub fn ready(epoch: u64, key_id: &str) -> String {
    text(json!({"v": 1, "t": "ready", "epoch": epoch, "key_id": key_id}))
}

/// `needs_repair` from the hub: one of the three hub-side reasons, with its exact fix text.
pub fn needs_repair_hub(reason: &str, epoch: u64) -> String {
    let fix = fix_text("hub", reason).expect("a hub-side repair reason");
    text(
        json!({"v": 1, "t": "needs_repair", "from": "hub", "reason": reason, "fix": fix, "epoch": epoch}),
    )
}

pub fn ack(epoch: u64, ack: u64) -> String {
    text(json!({"v": 1, "t": "ack", "epoch": epoch, "ack": ack}))
}

pub fn resume(epoch: u64, last_acked: u64) -> String {
    text(json!({"v": 1, "t": "resume", "epoch": epoch, "last_acked": last_acked}))
}

/// A request frame. `rpc` must already carry `jsonrpc`, `id` (== `id`) and `method`.
pub fn request(epoch: u64, seq: u64, id: &str, rpc: &Value) -> String {
    text(json!({"v": 1, "t": "request", "epoch": epoch, "seq": seq, "id": id, "rpc": rpc}))
}

/// A segment frame: `signed` is a `schemas/segment.json` object (`{segment, signature}`).
pub fn segment(epoch: u64, seq: u64, id: &str, signed: &Value) -> String {
    text(json!({"v": 1, "t": "segment", "epoch": epoch, "seq": seq, "id": id, "segment": signed}))
}
