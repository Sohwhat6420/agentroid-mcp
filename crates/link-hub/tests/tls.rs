//! `--link-lan` (home Wi-Fi): the hub serves `wss://` with a self-signed certificate the pairing QR pins.
//! The whole phone session (hello, challenge, signature, requests) runs inside TLS 1.3; a wrong pin, a plain
//! `ws://` client and `--link-lan` without TLS are all refused. ship/NO-ADB-PLAN.md stage 2.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use link_hub::bind::check_bind_address_lan;
use link_hub::hub::{Hub, HubConfig};
use link_hub::tls::HubTls;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;

async fn tls_hub(dir: &TempDir) -> (Hub, Arc<HubTls>) {
    let (tls, _) = HubTls::generate().unwrap();
    let tls = Arc::new(tls);
    let mut cfg = HubConfig::new(dir.path(), "127.0.0.1", 0, hub_key());
    cfg.tls = Some(tls.clone());
    (Hub::start(cfg).await.expect("tls hub starts"), tls)
}

fn wss(hub: &Hub) -> String {
    hub.url().replacen("ws://", "wss://", 1)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_with_the_pinned_certificate_completes_the_session_and_answers_over_wss() {
    let dir = TempDir::new("tls-ok");
    let (hub, tls) = tls_hub(&dir).await;
    let mut p = FakePhone::paired(31, hub.hub_key());
    Arc::get_mut(&mut p).unwrap().cfg.tls_fp = Some(tls.fingerprint());
    pair_direct(dir.path(), SERIAL, &p.cfg.key);
    let runner = PhoneRunner::start(p.clone(), wss(&hub));
    wait_until("the phone is connected over TLS", 20, || {
        hub.status(SERIAL)
            .is_some_and(|s| s.connected && s.counters.handshakes >= 1)
    })
    .await;
    let r = tokio::time::timeout(
        Duration::from_secs(20),
        hub.send_request(
            SERIAL,
            json!({"method": "bulk.op", "params": {"i": 1}, "id": "tls-1"}),
        ),
    )
    .await
    .expect("an answer in time")
    .expect("the phone answered");
    assert_eq!(r.id, "tls-1");
    runner.stop().await;
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_pinning_another_certificate_never_reaches_the_hello() {
    let dir = TempDir::new("tls-wrong");
    let (hub, tls) = tls_hub(&dir).await;
    let mut p = FakePhone::paired(32, hub.hub_key());
    let mut fp = tls.fingerprint();
    fp[31] ^= 0x80;
    Arc::get_mut(&mut p).unwrap().cfg.tls_fp = Some(fp);
    pair_direct(dir.path(), SERIAL, &p.cfg.key);
    let (_stop, mut rx) = tokio::sync::watch::channel(false);
    assert!(matches!(
        p.session(&wss(&hub), &mut rx).await,
        End::ConnectFailed
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(hub
        .status(SERIAL)
        .is_none_or(|s| !s.connected && s.counters.handshakes == 0));
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_websocket_client_is_refused_by_a_tls_hub() {
    let dir = TempDir::new("tls-plain");
    let (hub, _tls) = tls_hub(&dir).await;
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(hub.url()),
    )
    .await;
    assert!(
        matches!(r, Ok(Err(_)) | Err(_)),
        "a plain ws:// upgrade must not succeed"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn link_lan_without_tls_is_refused_before_anything_listens() {
    let dir = TempDir::new("tls-lan-plain");
    let mut cfg = HubConfig::new(dir.path(), "192.0.2.147", 0, hub_key());
    cfg.lan = true;
    let err = match Hub::start(cfg).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("--link-lan without TLS started"),
    };
    assert!(err.contains("without TLS"), "{err}");
}

#[tokio::test]
async fn without_link_lan_a_lan_address_is_still_refused() {
    let dir = TempDir::new("tls-nolan");
    let (tls, _) = HubTls::generate().unwrap();
    let mut cfg = HubConfig::new(dir.path(), "192.0.2.147", 0, hub_key());
    cfg.tls = Some(Arc::new(tls));
    let err = match Hub::start(cfg).await {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a LAN address was accepted without --link-lan"),
    };
    assert!(err.contains("LAN or link-local"), "{err}");
}

#[test]
fn the_lan_rule_adds_exactly_the_private_ranges() {
    for ok in [
        "10.0.0.5",
        "10.255.255.254",
        "172.16.0.1",
        "172.31.255.1",
        "192.0.2.147",
        "127.0.0.1",
        "100.64.0.35",
    ] {
        assert!(
            check_bind_address_lan(ok).is_ok(),
            "{ok} should be accepted"
        );
    }
    for bad in [
        "0.0.0.0",
        "8.8.8.8",
        "169.254.1.1",
        "172.15.0.1",
        "172.32.0.1",
        "192.169.0.1",
        "127.0.0.2",
        "::",
        "::1",
        "laptop.local",
        "",
        "192.168.050.1",
        "100.100.100.100",
    ] {
        assert!(
            check_bind_address_lan(bad).is_err(),
            "{bad} should be refused"
        );
    }
}

// Raw TLS against the acceptor alone (moved here from src/tls.rs: client code stays out of src/, which
// interop.rs::the_hub_never_dials_out checks).

async fn serve_once(tls: &HubTls) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let acc = tls.acceptor();
    tokio::spawn(async move {
        if let Ok((s, _)) = l.accept().await {
            if let Ok(mut t) = acc.accept(s).await {
                let _ = t.write_all(b"hub").await;
                let _ = t.shutdown().await;
            }
        }
    });
    addr
}

async fn dial(addr: std::net::SocketAddr, fp: [u8; 32]) -> Result<Vec<u8>, String> {
    let c = TlsConnector::from(Arc::new(link_hub::tls::pinned_client_config(fp).unwrap()));
    let s = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from("agentroid-hub").unwrap();
    let mut t = c.connect(name, s).await.map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    t.read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
    Ok(buf)
}

#[tokio::test]
async fn raw_tls_the_pinned_fingerprint_connects_and_any_other_is_refused() {
    let (tls, _) = HubTls::generate().unwrap();
    assert_eq!(tls.fingerprint_hex().len(), 64);
    let addr = serve_once(&tls).await;
    assert_eq!(dial(addr, tls.fingerprint()).await.unwrap(), b"hub");

    let addr = serve_once(&tls).await;
    let mut wrong = tls.fingerprint();
    wrong[0] ^= 1;
    let err = dial(addr, wrong).await.unwrap_err();
    assert!(err.contains("fingerprint does not match"), "{err}");
}

#[tokio::test]
async fn a_second_certificate_with_the_same_name_is_not_trusted() {
    let (real, _) = HubTls::generate().unwrap();
    let (impostor, _) = HubTls::generate().unwrap();
    assert_ne!(real.fingerprint(), impostor.fingerprint());
    let addr = serve_once(&impostor).await;
    assert!(dial(addr, real.fingerprint()).await.is_err());
}

#[tokio::test]
async fn a_plain_client_gets_no_frames_from_a_tls_listener() {
    let (tls, _) = HubTls::generate().unwrap();
    let addr = serve_once(&tls).await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /helper HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), s.read_to_end(&mut buf)).await;
    assert!(
        !buf.windows(3).any(|w| w == b"hub"),
        "plaintext leaked: {buf:?}"
    );
}
