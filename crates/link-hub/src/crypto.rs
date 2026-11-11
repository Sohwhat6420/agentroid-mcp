//! Public-key helpers for the link, matching `android/link/.../net/LinkCrypto.java` byte for byte:
//! strict standard base64, key id = lower-case hex SHA-256 of the X.509 SubjectPublicKeyInfo DER,
//! strict P-256 SPKI parsing, ASN.1 DER SHA256withECDSA signatures, and the two signed layouts of
//! `schemas/link.json`. No encryption and no secret material leaves this module except into
//! Windows Credential Manager ([`HubKey::load_or_create`]).

use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use sha2::Digest;

pub const HUB_DOMAIN: &str = "agentroid-link/v1/hub-challenge";
pub const PHONE_DOMAIN: &str = "agentroid-link/v1/phone-response";
pub const NONCE_BYTES: usize = 32;
pub const NONCE_MIN_BYTES: usize = 32;
pub const NONCE_MAX_BYTES: usize = 64;

/// The fixed DER prefix of a P-256 `SubjectPublicKeyInfo` with an uncompressed point:
/// SEQUENCE { SEQUENCE { id-ecPublicKey, prime256v1 }, BIT STRING (66 bytes, 0 unused bits) }.
/// Java's `ECPublicKey.getEncoded()` (and AndroidKeyStore) produce exactly this form.
pub const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];
/// 26-byte prefix + 0x04 + X (32) + Y (32).
pub const P256_SPKI_LEN: usize = 91;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptoError(pub String);

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CryptoError {}

fn err<T>(m: impl Into<String>) -> Result<T, CryptoError> {
    Err(CryptoError(m.into()))
}

// ------------------------------------------------------------------ base64 (RFC 4648 section 4)

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
pub fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some(u32::from(c - b'A')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Strict standard base64, like `LinkCrypto.unb64`: it must decode, and the canonical
/// re-encoding must equal the input (no whitespace, no missing padding, no stray low bits).
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, quad) in bytes.chunks(4).enumerate() {
        let last = i + 1 == bytes.len() / 4;
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = (n << 6) | b64_value(c)?;
        }
        n <<= 6 * pad as u32;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    if b64(&out) == s {
        Some(out)
    } else {
        None
    }
}

// ------------------------------------------------------------------ hashing, key ids

pub fn sha256(data: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(data).into()
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Lower-case hex SHA-256 of the SPKI DER (link.json `$defs/key_id`).
pub fn key_id(spki: &[u8]) -> String {
    hex(&sha256(spki))
}

/// True for 64 lower-case hex characters.
pub fn is_key_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

// ------------------------------------------------------------------ keys and signatures

/// Parses a P-256 public key in the exact SPKI DER form Java produces; anything else (another
/// curve, a compressed point, a PKCS#8 private key, trailing bytes, a point off the curve) is refused.
pub fn parse_spki(spki: &[u8]) -> Result<VerifyingKey, CryptoError> {
    if spki.len() != P256_SPKI_LEN || spki[..26] != P256_SPKI_PREFIX || spki[26] != 0x04 {
        return err("not an EC P-256 public key (X.509 SPKI DER, uncompressed point)");
    }
    VerifyingKey::from_sec1_bytes(&spki[26..])
        .map_err(|_| CryptoError("the P-256 point is not on the curve".into()))
}

/// The SPKI DER of a verifying key (the inverse of [`parse_spki`]).
pub fn spki_of(vk: &VerifyingKey) -> Vec<u8> {
    let point = vk.to_sec1_point(false);
    let mut out = P256_SPKI_PREFIX.to_vec();
    out.extend_from_slice(point.as_bytes());
    out
}

/// SHA256withECDSA verify of a DER signature, like `LinkCrypto.verify` (8..=72 bytes of DER).
pub fn verify(spki: &[u8], msg: &[u8], der_sig: &[u8]) -> bool {
    if der_sig.len() < 8 || der_sig.len() > 72 {
        return false;
    }
    let Ok(vk) = parse_spki(spki) else {
        return false;
    };
    let Ok(sig) = Signature::from_der(der_sig) else {
        return false;
    };
    vk.verify(msg, &sig).is_ok()
}

/// SHA256withECDSA, DER encoded (what Java's `Signature.sign()` returns).
pub fn sign_der(key: &SigningKey, msg: &[u8]) -> Vec<u8> {
    let sig: Signature = key.sign(msg);
    sig.to_der().as_bytes().to_vec()
}

/// The bytes the HUB signs in a challenge (link.json `$defs/challenge`).
pub fn hub_challenge_message(
    nonce: &str,
    phone_nonce: &str,
    epoch: u64,
    serial: &str,
    phone_key_id: &str,
) -> Vec<u8> {
    format!("{HUB_DOMAIN}\nnonce={nonce}\nphone_nonce={phone_nonce}\nepoch={epoch}\nserial={serial}\nphone_key_id={phone_key_id}")
        .into_bytes()
}

/// The bytes the PHONE signs in a challenge_response (link.json `$defs/challenge_response`).
pub fn phone_response_message(
    nonce: &str,
    phone_nonce: &str,
    epoch: u64,
    serial: &str,
    hub_key_id: &str,
) -> Vec<u8> {
    format!("{PHONE_DOMAIN}\nnonce={nonce}\nphone_nonce={phone_nonce}\nepoch={epoch}\nserial={serial}\nhub_key_id={hub_key_id}")
        .into_bytes()
}

// ------------------------------------------------------------------ randomness

/// `N` bytes from the operating system's CSPRNG (getrandom; BCryptGenRandom-class on Windows).
pub fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b)
        .map_err(|e| CryptoError(format!("the OS random source failed: {e}")))?;
    Ok(b)
}

/// A fresh challenge nonce: 32 random bytes, standard base64 with padding.
pub fn new_nonce() -> Result<String, CryptoError> {
    Ok(b64(&random_bytes::<NONCE_BYTES>()?))
}

/// True if `s` is canonical base64 of 32..=64 bytes (link.json `$defs/nonce` plus the byte check).
pub fn nonce_ok(s: &str) -> bool {
    matches!(unb64(s), Some(b) if (NONCE_MIN_BYTES..=NONCE_MAX_BYTES).contains(&b.len()))
}

/// Overwrites a buffer that held key material.
fn wipe(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: b is a valid, aligned, exclusive reference; the volatile write keeps the store.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
}

// ------------------------------------------------------------------ the hub key pair

/// Credential Manager target of the hub's private key (CRED_TYPE_GENERIC, this user, this machine).
pub const HUB_KEY_TARGET: &str = "agentroid:link-hub-key:v1";
const HUB_KEY_USER: &str = "agentroid-link";

/// The hub's P-256 key pair. The private scalar lives in this struct (zeroised on drop by p256)
/// and in Windows Credential Manager; it is never written to a file, logged or put in a frame.
/// `Debug` prints the key id only.
#[derive(Clone)]
pub struct HubKey {
    signing: SigningKey,
    spki: Vec<u8>,
    key_id: String,
}

impl std::fmt::Debug for HubKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HubKey(key_id={})", self.key_id)
    }
}

impl HubKey {
    fn from_signing(signing: SigningKey) -> Self {
        let spki = spki_of(signing.verifying_key());
        let key_id = key_id(&spki);
        Self {
            signing,
            spki,
            key_id,
        }
    }

    /// A new key from 32 bytes of getrandom, retried in the (about 2^-32) case that the bytes are
    /// zero or not below the group order.
    pub fn generate() -> Result<Self, CryptoError> {
        for _ in 0..64 {
            let mut b = random_bytes::<32>()?;
            let k = SigningKey::from_slice(&b);
            wipe(&mut b);
            if let Ok(k) = k {
                return Ok(Self::from_signing(k));
            }
        }
        err("the OS random source gave 64 invalid P-256 scalars in a row")
    }

    /// A key from a 32-byte big-endian scalar (tests use fixed seeds; production keys come from
    /// [`HubKey::load_or_create`]).
    pub fn from_scalar(bytes: &[u8]) -> Result<Self, CryptoError> {
        SigningKey::from_slice(bytes)
            .map(Self::from_signing)
            .map_err(|_| CryptoError("not a valid P-256 private scalar".into()))
    }

    /// The hub key from Credential Manager `target`, creating and storing a new one when there is
    /// none. The secret is the scalar in hex; it never touches the file system.
    pub fn load_or_create(target: &str) -> Result<Self, CryptoError> {
        match credstore::read(target) {
            Ok(secret) => {
                let mut raw = secret.into_bytes();
                let parsed = std::str::from_utf8(&raw).ok().and_then(unhex);
                wipe(&mut raw);
                let Some(mut scalar) = parsed else {
                    return err(format!(
                        "the credential {target} does not hold a hub key; delete it and run again"
                    ));
                };
                let k = Self::from_scalar(&scalar);
                wipe(&mut scalar);
                k
            }
            Err(credstore::Error::NotFound) => {
                let k = Self::generate()?;
                let mut secret = hex(&k.signing.to_bytes());
                let w = credstore::write(target, &secret, HUB_KEY_USER);
                // SAFETY: zero bytes are valid UTF-8, so the String stays valid.
                wipe(unsafe { secret.as_bytes_mut() });
                w.map_err(|e| {
                    CryptoError(format!(
                        "cannot store the hub key in Credential Manager: {e}"
                    ))
                })?;
                // read back, so a key that could not be persisted is never used
                Self::load_existing(target, &k)
            }
            Err(e) => err(format!(
                "cannot read the hub key from Credential Manager: {e}"
            )),
        }
    }

    fn load_existing(target: &str, expect: &Self) -> Result<Self, CryptoError> {
        let again = Self::load_or_create(target)?;
        if again.key_id != expect.key_id {
            return err("Credential Manager returned a different hub key than the one just stored");
        }
        Ok(again)
    }

    /// Deletes the stored key (re-keying; tests clean their throwaway target with it).
    pub fn delete_stored(target: &str) -> Result<bool, CryptoError> {
        credstore::delete(target).map_err(|e| CryptoError(e.to_string()))
    }

    pub fn spki(&self) -> &[u8] {
        &self.spki
    }

    pub fn spki_b64(&self) -> String {
        b64(&self.spki)
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// SHA256withECDSA DER signature by the hub key.
    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        sign_der(&self.signing, msg)
    }

    /// The private scalar in hex, for the key-never-in-a-file test only.
    #[doc(hidden)]
    pub fn secret_hex_for_leak_test(&self) -> String {
        hex(&self.signing.to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_is_strict_and_round_trips() {
        for n in 0..70usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(unb64(&b64(&data)).unwrap(), data, "{n}");
        }
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(unb64("Zm8="), Some(b"fo".to_vec()));
        for bad in [
            "Zm8", "Zm9=", "Zm8=Zm8=", "Zm 8", "Zm8==", "Z===", "Zm8-", "Zg=", "Zh==",
        ] {
            assert_eq!(unb64(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn sha256_known_answer() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn spki_round_trip_and_refusals() {
        let k = HubKey::generate().unwrap();
        assert_eq!(k.spki().len(), P256_SPKI_LEN);
        let vk = parse_spki(k.spki()).unwrap();
        assert_eq!(spki_of(&vk), k.spki());
        let mut bad = k.spki().to_vec();
        bad[60] ^= 1; // point off the curve (almost surely)
        assert!(parse_spki(&bad).is_err() || key_id(&bad) != k.key_id());
        assert!(parse_spki(&k.spki()[..90]).is_err());
        let mut longer = k.spki().to_vec();
        longer.push(0);
        assert!(parse_spki(&longer).is_err());
    }

    #[test]
    fn sign_verify_and_domain_separation() {
        let k = HubKey::generate().unwrap();
        let m = hub_challenge_message("n", "p", 3, "S1", "id");
        let s = k.sign(&m);
        assert!(verify(k.spki(), &m, &s));
        assert!(!verify(
            k.spki(),
            &hub_challenge_message("n", "p", 4, "S1", "id"),
            &s
        ));
        assert!(!verify(k.spki(), &[m.as_slice(), b"\n"].concat(), &s));
        assert!(!verify(k.spki(), &m, &s[..s.len() - 1]));
        assert_eq!(
            String::from_utf8(phone_response_message("N", "P", 9, "SER", "HID")).unwrap(),
            "agentroid-link/v1/phone-response\nnonce=N\nphone_nonce=P\nepoch=9\nserial=SER\nhub_key_id=HID"
        );
    }

    #[test]
    fn nonces_are_fresh_32_bytes() {
        let a = new_nonce().unwrap();
        let b = new_nonce().unwrap();
        assert_ne!(a, b);
        assert!(nonce_ok(&a));
        assert_eq!(unb64(&a).unwrap().len(), 32);
        assert!(!nonce_ok("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==")); // 31 bytes
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let k = HubKey::generate().unwrap();
        let d = format!("{k:?}");
        assert!(d.contains(k.key_id()));
        assert!(!d.contains(&k.secret_hex_for_leak_test()));
    }
}
