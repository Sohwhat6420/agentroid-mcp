//! TLS 1.3 for the hub on home Wi-Fi (`--link-lan`, ship/NO-ADB-PLAN.md stage 2).
//!
//! On Tailscale, WireGuard hides the frames; on a home LAN anyone on the network can read and inject them. So with
//! `--link-lan` the hub serves `wss://`: TLS 1.3 only, a long-lived self-signed P-256 certificate, and **no CA**.
//! The phone trusts exactly one certificate: the SHA-256 of its DER, carried in the pairing QR
//! (`agentroid://pair?...&fp=<64 hex>`, see [`crate::paircode`]). The existing P-256 challenge handshake still runs
//! inside the TLS session (defence in depth): a stolen TLS key alone cannot pose as the hub.
//!
//! * The certificate's private key lives in Windows Credential Manager (through `credstore`, like the hub key);
//!   the certificate itself (public) is `hub-state/tls-cert.der`.
//! * [`pinned_client_config`] is the phone-side rule in Rust (tests, and any laptop-side client): accept only the
//!   certificate whose SHA-256 matches, verify the TLS 1.3 signature with it, nothing else.

use std::path::Path;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring, verify_tls13_signature, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use tokio_rustls::TlsAcceptor;

use crate::crypto::{b64, hex, sha256, unb64};
use crate::store::write_atomic;

/// The Credential Manager user field for the TLS key (the target is chosen by the caller).
pub const TLS_KEY_USER: &str = "agentroid-link-hub-tls";
/// The default Credential Manager target for the TLS key (beside `crypto::HUB_KEY_TARGET`).
pub const TLS_KEY_TARGET: &str = "agentroid:link-hub-tls:v1";

/// The TLS key target that goes with a hub key target: the default pair, or `<target>-tls` for a custom one
/// (tests and second hubs never share a TLS key with the real hub).
pub fn tls_target_for(hub_key_target: &str) -> String {
    if hub_key_target == crate::crypto::HUB_KEY_TARGET {
        TLS_KEY_TARGET.to_string()
    } else {
        format!("{hub_key_target}-tls")
    }
}
pub const CERT_FILE: &str = "tls-cert.der";

#[derive(Debug)]
pub struct TlsError(pub String);

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TLS: {}", self.0)
    }
}

impl std::error::Error for TlsError {}

fn e<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> TlsError + '_ {
    move |x| TlsError(format!("{what}: {x}"))
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring::default_provider())
}

/// The hub's certificate and the acceptor built from it.
pub struct HubTls {
    acceptor: TlsAcceptor,
    cert_der: Vec<u8>,
    fingerprint: [u8; 32],
}

impl HubTls {
    /// From a PKCS#8 P-256 key and its certificate.
    pub fn from_parts(key_pkcs8: Vec<u8>, cert_der: Vec<u8>) -> Result<Self, TlsError> {
        let cfg = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(e("protocol versions"))?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pkcs8)),
            )
            .map_err(e("certificate and key"))?;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(cfg)),
            fingerprint: sha256(&cert_der),
            cert_der,
        })
    }

    /// A fresh self-signed P-256 certificate (CN "agentroid-hub"). Returns the hub TLS and the PKCS#8 key to store.
    pub fn generate() -> Result<(Self, Vec<u8>), TlsError> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(e("key"))?;
        let mut params =
            rcgen::CertificateParams::new(Vec::<String>::new()).map_err(e("params"))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "agentroid-hub");
        params.not_before = rcgen::date_time_ymd(2026, 1, 1);
        params.not_after = rcgen::date_time_ymd(2036, 1, 1);
        let cert = params.self_signed(&key).map_err(e("self-sign"))?;
        let pkcs8 = key.serialize_der();
        let me = Self::from_parts(pkcs8.clone(), cert.der().to_vec())?;
        Ok((me, pkcs8))
    }

    /// The hub's TLS identity: the key from Credential Manager `target` and the certificate from
    /// `<hub_state>/tls-cert.der`; both are made on first use. A key with no certificate (or the reverse) is
    /// replaced as a pair, which changes the fingerprint: paired phones must re-pair, and the log says so.
    pub fn load_or_create(target: &str, hub_state: &Path) -> Result<(Self, bool), TlsError> {
        let cert_path = hub_state.join(CERT_FILE);
        let key = credstore::read(target).ok().and_then(|s| unb64(&s));
        let cert = std::fs::read(&cert_path).ok();
        if let (Some(k), Some(c)) = (key, cert) {
            return Ok((Self::from_parts(k, c)?, false));
        }
        let (me, pkcs8) = Self::generate()?;
        std::fs::create_dir_all(hub_state).map_err(e("hub-state"))?;
        credstore::write(target, &b64(&pkcs8), TLS_KEY_USER).map_err(e("store the TLS key"))?;
        write_atomic(&cert_path, &me.cert_der).map_err(e("write the certificate"))?;
        Ok((me, true))
    }

    pub fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// The SHA-256 of the certificate's DER: what the QR carries as `fp`.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub fn fingerprint_hex(&self) -> String {
        hex(&self.fingerprint)
    }
}

/// Trusts exactly one certificate, by the SHA-256 of its DER. No CA, no host name, no expiry check (the pin is the
/// whole of the trust; the hub's certificate is self-signed and long-lived on purpose).
#[derive(Debug)]
struct Pinned {
    fp: [u8; 32],
    algs: WebPkiSupportedAlgorithms,
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        if !intermediates.is_empty() {
            return Err(RustlsError::General(
                "the hub sends one self-signed certificate, not a chain".into(),
            ));
        }
        if same(&sha256(end_entity.as_ref()), &self.fp) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(RustlsError::General(
                "certificate fingerprint does not match the pairing QR".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Err(RustlsError::General("TLS 1.2 is not accepted".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// The TLS server name the hub's certificate carries (its only name; the pin, not the name, is the trust).
pub const HUB_TLS_NAME: &str = "agentroid-hub";

/// A ready connector for [`pinned_client_config`] plus the server name to give it, so client code outside this
/// crate (the fake phone in tests, bridged's tests) needs no direct rustls dependency. This builds; it never dials.
pub fn pinned_connector(
    fp: [u8; 32],
) -> Result<(tokio_rustls::TlsConnector, ServerName<'static>), TlsError> {
    let name = ServerName::try_from(HUB_TLS_NAME).map_err(e("server name"))?;
    Ok((
        tokio_rustls::TlsConnector::from(Arc::new(pinned_client_config(fp)?)),
        name,
    ))
}

/// A TLS 1.3 client config that trusts only the certificate with SHA-256 `fp` (the phone's rule, in Rust).
pub fn pinned_client_config(fp: [u8; 32]) -> Result<rustls::ClientConfig, TlsError> {
    let p = provider();
    let algs = p.signature_verification_algorithms;
    Ok(rustls::ClientConfig::builder_with_provider(p)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(e("protocol versions"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { fp, algs }))
        .with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_round_trip_keeps_the_fingerprint() {
        let (a, pkcs8) = HubTls::generate().unwrap();
        let b = HubTls::from_parts(pkcs8, a.cert_der().to_vec()).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
    }
}
