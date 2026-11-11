//! Segments: the types of `schemas/segment.json`, the hub key, building (with classification), signing and verifying.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature as EcdsaSignature, SigningKey, VerifyingKey};
use p256::pkcs8::{DecodePublicKey, EncodePublicKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;

use crate::canonical::{self, CanonError};
use crate::classes::{Decision, Phase, Risk, SafeClasses, SafetyDecision, StepView};
use crate::time;

pub const FORMAT_VERSION: u32 = 1;
pub const ALG: &str = "ECDSA-P256-SHA256";
/// Longest a segment may live. The phone refuses anything that expires further ahead than this plus [`CLOCK_SKEW_S`],
/// so a hub with a wrong clock cannot mint a segment that never expires.
pub const MAX_TTL_S: i64 = 3600;
pub const CLOCK_SKEW_S: i64 = 300;
pub const MAX_STEPS: usize = 500;
/// `setting_equals` reads the setting back (settings_set only); the others read the screen (as the 2.10 engine).
pub const POST_KINDS: [&str; 5] = ["none", "present", "absent", "text_equals", "setting_equals"];
/// Kinds that change something on the phone and so must carry a real post-condition.
pub const NEEDS_POST: [&str; 2] = ["tap", "settings_set"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Post {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Post {
    pub fn none() -> Post {
        Post {
            kind: "none".into(),
            select: None,
            text: None,
        }
    }
    pub fn present(select: Value) -> Post {
        Post {
            kind: "present".into(),
            select: Some(select),
            text: None,
        }
    }
    pub fn absent(select: Value) -> Post {
        Post {
            kind: "absent".into(),
            select: Some(select),
            text: None,
        }
    }
    pub fn text_equals(select: Value, text: &str) -> Post {
        Post {
            kind: "text_equals".into(),
            select: Some(select),
            text: Some(text.into()),
        }
    }
    /// The setting named by the step reads back as the step's value.
    pub fn setting_equals() -> Post {
        Post {
            kind: "setting_equals".into(),
            select: None,
            text: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub kind: String,
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<Value>,
    pub post: Post,
    /// The hub's label. The phone never relies on it alone (it can only raise a step's class).
    pub risk: Risk,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
}

impl Step {
    /// A bare step; set the optional fields directly.
    pub fn new(id: &str, kind: &str, package: &str, risk: Risk, post: Post) -> Step {
        Step {
            id: id.into(),
            kind: kind.into(),
            package: package.into(),
            select: None,
            post,
            risk,
            value: None,
            timeout_ms: None,
            direction: None,
            namespace: None,
            key: None,
            scope: None,
            lease: None,
        }
    }
    pub fn with_select(mut self, select: Value) -> Step {
        self.select = Some(select);
        self
    }
    pub fn view(&self) -> StepView<'_> {
        StepView {
            id: &self.id,
            kind: &self.kind,
            package: &self.package,
            select: self.select.as_ref(),
            label: self.risk,
            value: self.value.as_ref(),
            namespace: self.namespace.as_deref(),
            scope: self.scope.as_deref(),
            lease: self.lease.as_deref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub v: u32,
    pub segment_id: String,
    pub run_id: String,
    /// `ro.serialno` of the one phone that may run it.
    pub device: String,
    /// The hub epoch that minted it (link `ready` epoch).
    pub epoch: u64,
    /// The hub's registry says this is an owned phone (gates the Play Store Update token).
    pub owned: bool,
    /// The highest effective class of its steps; R0 or R1 only.
    pub classification: Risk,
    pub expires: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signature {
    pub alg: String,
    /// Lower-case hex SHA-256 of the hub's SPKI DER (the link's key id).
    pub key_id: String,
    /// Base64 (standard, padded) of the ASN.1 DER ECDSA signature, as Java's SHA256withECDSA produces and expects.
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedSegment {
    pub segment: Segment,
    pub signature: Signature,
}

impl SignedSegment {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("segment serialises")
    }
    pub fn from_json(text: &str) -> Result<SignedSegment, Refusal> {
        serde_json::from_str(text).map_err(|e| Refusal::Malformed(e.to_string()))
    }
}

// ------------------------------------------------------------------------------------------------ hub key

/// The hub's signing key. Debug prints the key id only.
pub struct HubKey {
    sk: SigningKey,
}

impl fmt::Debug for HubKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HubKey(key_id={})", self.key_id())
    }
}

impl HubKey {
    /// A fresh key: 32 bytes from the OS RNG (getrandom) into `SigningKey::from_slice`, retried in the (2^-128)
    /// case that the bytes are zero or not below the group order.
    pub fn generate() -> Result<HubKey, SegError> {
        for _ in 0..8 {
            let mut b = [0u8; 32];
            getrandom::fill(&mut b).map_err(|e| SegError::Rng(e.to_string()))?;
            let k = SigningKey::from_slice(&b);
            b.fill(0);
            if let Ok(sk) = k {
                return Ok(HubKey { sk });
            }
        }
        Err(SegError::Rng("no valid scalar in 8 draws".into()))
    }

    /// A key from a stored 32-byte scalar (for the hub's own key file, and for the test fixtures).
    pub fn from_scalar(bytes: &[u8]) -> Result<HubKey, SegError> {
        SigningKey::from_slice(bytes)
            .map(|sk| HubKey { sk })
            .map_err(|_| SegError::BadKey)
    }

    /// X.509 SubjectPublicKeyInfo DER: what `agentroid link pair` sends to the phone.
    pub fn spki_der(&self) -> Vec<u8> {
        self.sk
            .verifying_key()
            .to_public_key_der()
            .expect("P-256 SPKI encodes")
            .as_bytes()
            .to_vec()
    }

    pub fn key_id(&self) -> String {
        key_id(&self.spki_der())
    }

    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        let sig: EcdsaSignature = self.sk.sign(msg);
        sig.to_der().to_bytes().to_vec()
    }
}

/// Lower-case hex SHA-256 of an SPKI DER (same as `LinkCrypto.keyId`).
pub fn key_id(spki: &[u8]) -> String {
    Sha256::digest(spki)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegError {
    Rng(String),
    BadKey,
    Canonical(CanonError),
}

impl fmt::Display for SegError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegError::Rng(e) => write!(f, "OS random source failed: {e}"),
            SegError::BadKey => f.write_str("not a valid P-256 scalar"),
            SegError::Canonical(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SegError {}

// ------------------------------------------------------------------------------------------------ build

/// What the hub asks for. The classification is computed, never supplied.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentRequest {
    pub segment_id: String,
    pub run_id: String,
    pub device: String,
    pub epoch: u64,
    pub owned: bool,
    /// Unix seconds.
    pub expires: i64,
    pub steps: Vec<Step>,
}

/// Why the hub refused to build a segment.
#[derive(Debug, Clone, PartialEq)]
pub enum BuildRefusal {
    /// At least one step's final decision is not `allowed`. Every step's decision is included.
    NotSafe(Vec<SafetyDecision>),
    /// Breaks a structural rule (one line per rule).
    Invalid(Vec<String>),
}

impl fmt::Display for BuildRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildRefusal::NotSafe(ds) => {
                let bad: Vec<String> = ds
                    .iter()
                    .filter(|d| d.final_decision != Decision::Allowed)
                    .map(|d| {
                        format!(
                            "{}: {} ({})",
                            d.step_id,
                            d.final_decision.as_str(),
                            d.explanation
                        )
                    })
                    .collect();
                write!(f, "segment refused: {}", bad.join("; "))
            }
            BuildRefusal::Invalid(v) => write!(f, "segment invalid: {}", v.join("; ")),
        }
    }
}

impl std::error::Error for BuildRefusal {}

/// Structural rules shared by the builder and the verifier (Java twin: `Segment.check`).
pub fn structural_errors(seg: &Segment, now: i64) -> Vec<String> {
    let mut e = Vec::new();
    if seg.v != FORMAT_VERSION {
        e.push(format!("v must be {FORMAT_VERSION}"));
    }
    for (name, val) in [
        ("segment_id", &seg.segment_id),
        ("run_id", &seg.run_id),
        ("device", &seg.device),
    ] {
        if val.is_empty() {
            e.push(format!("{name} is empty"));
        }
    }
    if seg.epoch > i64::MAX as u64 {
        e.push("epoch is out of range".into());
    }
    if seg.steps.is_empty() {
        e.push("a segment needs at least one step".into());
    }
    if seg.steps.len() > MAX_STEPS {
        e.push(format!("more than {MAX_STEPS} steps"));
    }
    match time::parse_utc(&seg.expires) {
        None => e.push("expires must be YYYY-MM-DDTHH:MM:SSZ".into()),
        Some(t) if t > now + MAX_TTL_S + CLOCK_SKEW_S => {
            e.push(format!("expires is more than {MAX_TTL_S} s ahead"))
        }
        Some(_) => {}
    }
    let mut ids = BTreeSet::new();
    for (i, s) in seg.steps.iter().enumerate() {
        let p = format!("steps/{i}");
        if s.id.is_empty() {
            e.push(format!("{p}: id is empty"));
        } else if !ids.insert(s.id.as_str()) {
            e.push(format!("{p}: id {} repeats", s.id));
        }
        if s.kind.is_empty() {
            e.push(format!("{p}: kind is empty"));
        }
        if s.package.is_empty() {
            e.push(format!("{p}: package is empty"));
        }
        if let Some(sel) = &s.select {
            if !select_ok(sel) {
                e.push(format!(
                    "{p}: select must be one of id/text/desc/near_text (a non-empty string)"
                ));
            }
        }
        if matches!(s.kind.as_str(), "tap" | "read" | "assert") && s.select.is_none() {
            e.push(format!("{p}: {} needs select", s.kind));
        }
        if s.kind == "settings_set"
            && (s.key.as_deref().unwrap_or("").is_empty()
                || s.namespace.is_none()
                || s.value.is_none()
                || s.scope.is_none())
        {
            e.push(format!(
                "{p}: settings_set needs namespace, key, value and scope"
            ));
        }
        if !POST_KINDS.contains(&s.post.kind.as_str()) {
            e.push(format!(
                "{p}/post: kind {:?} is not one of {POST_KINDS:?}",
                s.post.kind
            ));
        }
        let screen_post = matches!(s.post.kind.as_str(), "present" | "absent" | "text_equals");
        if screen_post && !s.post.select.as_ref().is_some_and(select_ok) {
            e.push(format!("{p}/post: {} needs a select", s.post.kind));
        }
        if !screen_post && (s.post.select.is_some() || s.post.text.is_some()) {
            e.push(format!("{p}/post: {} takes no select or text", s.post.kind));
        }
        if s.post.kind == "setting_equals" && s.kind != "settings_set" {
            e.push(format!(
                "{p}/post: setting_equals is for settings_set steps only"
            ));
        }
        if s.post.kind == "text_equals" && s.post.text.is_none() {
            e.push(format!("{p}/post: text_equals needs text"));
        }
        if NEEDS_POST.contains(&s.kind.as_str()) && s.post.kind == "none" {
            e.push(format!(
                "{p}: {} changes the phone, so it needs a post-condition",
                s.kind
            ));
        }
    }
    e
}

/// A selector in a segment: exactly one of id/text/desc/near_text, a non-empty string. Coordinates (`rel`) never
/// travel in a segment (they are not unique and they would need floats in the canonical form).
pub fn select_ok(v: &Value) -> bool {
    v.as_object().is_some_and(|m| {
        m.len() == 1
            && m.iter().all(|(k, v)| {
                matches!(k.as_str(), "id" | "text" | "desc" | "near_text")
                    && v.as_str().is_some_and(|s| !s.is_empty())
            })
    })
}

/// Classify every step; Ok(the decisions and the segment's class) only when every final decision is `allowed`.
pub fn classify(
    steps: &[Step],
    owned: bool,
    classes: &SafeClasses,
    phase: Phase,
) -> Result<(Vec<SafetyDecision>, Risk), Vec<SafetyDecision>> {
    let ds: Vec<SafetyDecision> = steps
        .iter()
        .map(|s| classes.decide(&s.view(), owned, phase))
        .collect();
    if ds.iter().all(|d| d.final_decision == Decision::Allowed) {
        let class = ds.iter().map(|d| d.effective).max().unwrap_or(Risk::R0);
        Ok((ds, class))
    } else {
        Err(ds)
    }
}

/// Build (classify, check) the segment. Any step that is not R0/R1, or whose decision is not `allowed` after the
/// phase rule, refuses the WHOLE segment.
pub fn build(
    req: SegmentRequest,
    classes: &SafeClasses,
    phase: Phase,
    now: i64,
) -> Result<Segment, BuildRefusal> {
    let (_, class) =
        classify(&req.steps, req.owned, classes, phase).map_err(BuildRefusal::NotSafe)?;
    let seg = Segment {
        v: FORMAT_VERSION,
        segment_id: req.segment_id,
        run_id: req.run_id,
        device: req.device,
        epoch: req.epoch,
        owned: req.owned,
        classification: class,
        expires: time::format_utc(req.expires),
        steps: req.steps,
    };
    let mut errs = structural_errors(&seg, now);
    if req.expires <= now {
        errs.push("expires is not in the future".into());
    }
    if !errs.is_empty() {
        return Err(BuildRefusal::Invalid(errs));
    }
    Ok(seg)
}

/// Sign a built segment.
pub fn sign(seg: Segment, key: &HubKey) -> Result<SignedSegment, SegError> {
    let msg = signed_bytes(&seg).map_err(SegError::Canonical)?;
    Ok(SignedSegment {
        signature: Signature {
            alg: ALG.into(),
            key_id: key.key_id(),
            value: B64.encode(key.sign(&msg)),
        },
        segment: seg,
    })
}

/// Signs with a key held elsewhere: the daemon's link hub key (Credential Manager), so the phone
/// pins one hub key for the link handshake and for segments. `signer` gets the exact signed bytes
/// ([`signed_bytes`]) and must return an ASN.1 DER ECDSA P-256/SHA-256 signature by the private
/// half of `hub_spki`. The signature is checked against `hub_spki` before it is returned, so a
/// signer holding another key cannot produce a segment that names this key id.
pub fn sign_with(
    seg: Segment,
    hub_spki: &[u8],
    signer: impl FnOnce(&[u8]) -> Vec<u8>,
) -> Result<SignedSegment, SegError> {
    let msg = signed_bytes(&seg).map_err(SegError::Canonical)?;
    let der = signer(&msg);
    let vk = VerifyingKey::from_public_key_der(hub_spki).map_err(|_| SegError::BadKey)?;
    let sig = EcdsaSignature::from_der(&der).map_err(|_| SegError::BadKey)?;
    vk.verify(&msg, &sig).map_err(|_| SegError::BadKey)?;
    Ok(SignedSegment {
        signature: Signature {
            alg: ALG.into(),
            key_id: key_id(hub_spki),
            value: B64.encode(der),
        },
        segment: seg,
    })
}

pub fn build_and_sign(
    req: SegmentRequest,
    classes: &SafeClasses,
    phase: Phase,
    now: i64,
    key: &HubKey,
) -> Result<SignedSegment, BuildRefusal> {
    let seg = build(req, classes, phase, now)?;
    sign(seg, key).map_err(|e| BuildRefusal::Invalid(vec![e.to_string()]))
}

/// The bytes signed for this segment.
pub fn signed_bytes(seg: &Segment) -> Result<Vec<u8>, CanonError> {
    canonical::signed_bytes(&serde_json::to_value(seg).expect("segment serialises"))
}

/// Signs without classifying. Test-only: it is how the fixtures carry a hub that mislabels a step, which the phone
/// must still refuse. Not reachable from outside the crate.
#[cfg(test)]
pub(crate) fn sign_unchecked(seg: Segment, key: &HubKey) -> SignedSegment {
    sign(seg, key).expect("canonical")
}

// ------------------------------------------------------------------------------------------------ verify

/// What the verifier needs from the link: the pinned hub key, the phone's serial and the current epoch.
#[derive(Debug, Clone, Copy)]
pub struct VerifyContext<'a> {
    pub hub_spki: &'a [u8],
    pub device: &'a str,
    pub current_epoch: u64,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    Malformed(String),
    Unsigned,
    BadAlg(String),
    WrongKey,
    BadSignature,
    WrongDevice {
        segment: String,
        phone: String,
    },
    StaleEpoch {
        segment: u64,
        current: u64,
    },
    EpochAhead {
        segment: u64,
        current: u64,
    },
    Expired {
        expires: String,
    },
    Invalid(Vec<String>),
    NotClassifiable(Risk),
    NotSafe(Vec<SafetyDecision>),
    ClassificationTooLow {
        claimed: Risk,
        actual: Risk,
    },
    /// The hub's supervisor already sent a segment with this id and it is not known to have been refused: it is
    /// running, finished, or its outcome is unknown (a timeout). A segment id runs once.
    Duplicate(String),
}

impl Refusal {
    /// Stable reason code (the phone uses the same strings).
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Malformed(_) => "malformed",
            Refusal::Unsigned => "unsigned",
            Refusal::BadAlg(_) => "bad_alg",
            Refusal::WrongKey => "wrong_hub_key",
            Refusal::BadSignature => "bad_signature",
            Refusal::WrongDevice { .. } => "wrong_device",
            Refusal::StaleEpoch { .. } => "stale_epoch",
            Refusal::EpochAhead { .. } => "epoch_ahead",
            Refusal::Expired { .. } => "expired",
            Refusal::Invalid(_) => "invalid",
            Refusal::NotClassifiable(_) => "classification_not_r0_r1",
            Refusal::NotSafe(_) => "step_not_safe",
            Refusal::ClassificationTooLow { .. } => "classification_too_low",
            Refusal::Duplicate(_) => "duplicate_segment",
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {self:?}", self.code())
    }
}

impl std::error::Error for Refusal {}

/// Signature over the canonical bytes with the pinned key, then epoch, device, expiry, structure, and a fresh
/// classification of every step (the hub's labels are not trusted).
pub fn verify(
    signed: &SignedSegment,
    ctx: &VerifyContext<'_>,
    classes: &SafeClasses,
    phase: Phase,
) -> Result<(), Refusal> {
    let sig = &signed.signature;
    if sig.value.is_empty() {
        return Err(Refusal::Unsigned);
    }
    if sig.alg != ALG {
        return Err(Refusal::BadAlg(sig.alg.clone()));
    }
    if sig.key_id != key_id(ctx.hub_spki) {
        return Err(Refusal::WrongKey);
    }
    let der = B64.decode(&sig.value).map_err(|_| Refusal::BadSignature)?;
    let ecdsa = EcdsaSignature::from_der(&der).map_err(|_| Refusal::BadSignature)?;
    let vk = VerifyingKey::from_public_key_der(ctx.hub_spki).map_err(|_| Refusal::WrongKey)?;
    let msg = signed_bytes(&signed.segment).map_err(|e| Refusal::Invalid(vec![e.to_string()]))?;
    vk.verify(&msg, &ecdsa).map_err(|_| Refusal::BadSignature)?;

    let seg = &signed.segment;
    if seg.epoch < ctx.current_epoch {
        return Err(Refusal::StaleEpoch {
            segment: seg.epoch,
            current: ctx.current_epoch,
        });
    }
    if seg.epoch > ctx.current_epoch {
        return Err(Refusal::EpochAhead {
            segment: seg.epoch,
            current: ctx.current_epoch,
        });
    }
    if seg.device != ctx.device {
        return Err(Refusal::WrongDevice {
            segment: seg.device.clone(),
            phone: ctx.device.into(),
        });
    }
    match time::parse_utc(&seg.expires) {
        Some(t) if t > ctx.now => {}
        _ => {
            return Err(Refusal::Expired {
                expires: seg.expires.clone(),
            })
        }
    }
    let errs = structural_errors(seg, ctx.now);
    if !errs.is_empty() {
        return Err(Refusal::Invalid(errs));
    }
    if seg.classification > Risk::R1 {
        return Err(Refusal::NotClassifiable(seg.classification));
    }
    let (_, actual) = classify(&seg.steps, seg.owned, classes, phase).map_err(Refusal::NotSafe)?;
    if actual > seg.classification {
        return Err(Refusal::ClassificationTooLow {
            claimed: seg.classification,
            actual,
        });
    }
    Ok(())
}
