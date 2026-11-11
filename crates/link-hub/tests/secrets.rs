//! The hub private key lives in Windows Credential Manager (crates/credstore) and never in a
//! file: a full session's state dir is scanned for it in every encoding. The credential is a
//! throwaway under `agentroid:test:` and is deleted afterwards (also when the test fails).

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use link_hub::crypto::{b64, unhex, HubKey};
use serde_json::json;

struct ThrowawayCredential(String);

impl ThrowawayCredential {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(format!(
            "agentroid:test:link-hub-{}-{nanos}",
            std::process::id()
        ))
    }
}

impl Drop for ThrowawayCredential {
    fn drop(&mut self) {
        let _ = HubKey::delete_stored(&self.0);
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hub_private_key_never_appears_in_any_file_under_the_state_dir() {
    let cred = ThrowawayCredential::new();
    assert!(cred.0.starts_with("agentroid:test:"));
    let key = HubKey::load_or_create(&cred.0).expect("Credential Manager is usable");
    let again = HubKey::load_or_create(&cred.0).unwrap();
    assert_eq!(
        again.key_id(),
        key.key_id(),
        "the same key comes back from Credential Manager"
    );
    let secret_hex = key.secret_hex_for_leak_test();
    let secret = unhex(&secret_hex).unwrap();
    assert_eq!(secret.len(), 32);
    assert!(
        !format!("{key:?}").contains(&secret_hex),
        "Debug does not print the key"
    );

    // a full session: pairing record, epochs, response cache, a restart
    let dir = TempDir::new("secrets");
    let phone = FakePhone::paired(41, &key);
    pair_direct(dir.path(), SERIAL, &phone.cfg.key);
    let hub = start_hub(dir.path(), key, None).await;
    let runner = PhoneRunner::start(phone.clone(), hub.url());
    for i in 0..50 {
        hub.send_request(
            SERIAL,
            json!({"method": "ui.tap", "params": {"i": i}, "id": format!("s-{i}")}),
        )
        .await
        .unwrap();
    }
    runner.stop().await;
    hub.shutdown().await;
    drop(hub);
    let hub = start_hub(dir.path(), HubKey::load_or_create(&cred.0).unwrap(), None).await;
    assert_eq!(hub.epoch(SERIAL).unwrap(), 2);
    hub.shutdown().await;
    drop(hub);
    tokio::time::sleep(Duration::from_millis(20)).await;

    let files = files_under(dir.path());
    assert!(files.len() >= 3, "{files:?}");
    let forms: Vec<(&str, Vec<u8>)> = vec![
        ("raw", secret.clone()),
        ("hex", secret_hex.to_lowercase().into_bytes()),
        ("HEX", secret_hex.to_uppercase().into_bytes()),
        (
            "base64",
            b64(&secret).trim_end_matches('=').as_bytes().to_vec(),
        ),
        (
            "base64url",
            b64(&secret)
                .trim_end_matches('=')
                .replace('+', "-")
                .replace('/', "_")
                .into_bytes(),
        ),
    ];
    for f in &files {
        let bytes = std::fs::read(f).unwrap();
        for (name, form) in &forms {
            assert!(
                !contains(&bytes, form),
                "the hub private key ({name}) is in {}",
                f.display()
            );
        }
    }
    eprintln!(
        "scanned {} files under the state dir: no private key",
        files.len()
    );

    // the credential is removed and gone
    let target = cred.0.clone();
    drop(cred);
    assert!(matches!(
        credstore::read(&target),
        Err(credstore::Error::NotFound)
    ));
}
