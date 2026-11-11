//! Bind refusals: the `apps/mcp/test/http.test.ts` table, the exact messages of
//! `apps/mcp/src/http-wiring.ts` `checkBindAddress`, and `Hub::start` refusing before it listens.

mod common;

use common::*;
use link_hub::bind::{check_bind_address, check_phone_hub_address, check_port};
use link_hub::hub::{HubConfig, HubStartError};
use link_hub::Hub;

const TAIL: &str = ". The listener binds only the laptop's tailnet IPv4 address (100.64.0.0/10, see `tailscale ip -4`) or 127.0.0.1 for tests.";

/// The refusal table of http.test.ts ("bind: refuses 0.0.0.0, ::, hostnames, LAN ..."), with the
/// pattern each message must match there. (Its non-string rows, `undefined` and a number, cannot
/// be written in Rust: the address is a `&str`.)
const REFUSED: [(&str, &str); 22] = [
    ("0.0.0.0", "every interface"),
    ("::", "IPv6"),
    ("::1", "IPv6"),
    ("[::]", "IPv6"),
    ("::ffff:100.64.0.1", "IPv6"),
    ("localhost", "hostname"),
    ("laptop.tail1234.ts.net", "hostname"),
    ("192.168.1.10", "LAN"),
    ("10.0.0.5", "LAN"),
    ("172.16.0.1", "LAN"),
    ("172.31.255.255", "LAN"),
    ("169.254.10.10", "link-local"),
    ("127.0.0.2", "only 127.0.0.1"),
    ("100.63.255.255", "not a tailnet address"),
    ("100.128.0.1", "not a tailnet address"),
    ("100.100.100.100", "Tailscale's own DNS"),
    ("8.8.8.8", "not a tailnet address"),
    ("100.064.0.1", "dotted-quad"),
    ("100.64.0", "dotted-quad"),
    ("100.64.0.256", "dotted-quad"),
    (" 100.64.0.1", "hostname"),
    ("", "no address"),
];

#[test]
fn refuses_everything_the_typescript_check_refuses_with_its_message() {
    for (host, why) in REFUSED {
        let e = check_bind_address(host).expect_err(host).0;
        assert!(e.contains(why), "{host:?}: {e}");
        assert!(e.contains("tailscale ip -4"), "{host:?}: {e}");
        assert!(
            e.starts_with(&format!(
                "refusing to listen on {}: ",
                serde_json::to_string(host).unwrap()
            )),
            "{e}"
        );
        assert!(e.ends_with(TAIL), "{e}");
    }
}

#[test]
fn the_messages_are_the_typescript_messages_word_for_word() {
    let ts = std::fs::read_to_string(
        scaffold()
            .join("apps")
            .join("mcp")
            .join("src")
            .join("http-wiring.ts"),
    )
    .unwrap();
    // the shared frame of every message
    assert!(ts.contains("refusing to listen on ${JSON.stringify(host)}: ${s}. The listener binds only the laptop's tailnet IPv4 address (100.64.0.0/10, see \\`tailscale ip -4\\`) or 127.0.0.1 for tests."));
    let reasons = [
        ("", "no address given"),
        ("::", "IPv6 addresses (including :: and ::1) are not accepted"),
        ("100.64.0", "not a well-formed dotted-quad IPv4 address"),
        ("localhost", "a hostname is not accepted, because a name can resolve to any interface; give the literal address"),
        ("0.0.0.0", "0.0.0.0 means every interface, the LAN and any public address included"),
        ("127.0.0.2", "of the loopback range only 127.0.0.1 is accepted (for tests)"),
        ("192.168.1.10", "a LAN or link-local address would expose the listener to the local network"),
        ("100.100.100.100", "100.100.100.100 is Tailscale's own DNS address, not this machine's"),
        ("8.8.8.8", "not a tailnet address"),
    ];
    for (host, reason) in reasons {
        assert!(
            ts.contains(&format!("why(\"{reason}\")")),
            "http-wiring.ts has no {reason:?}"
        );
        let want = format!(
            "refusing to listen on {}: {reason}{TAIL}",
            serde_json::to_string(host).unwrap()
        );
        assert_eq!(check_bind_address(host).unwrap_err().0, want);
    }
}

#[test]
fn accepts_127_0_0_1_and_literal_tailnet_addresses() {
    for h in [
        "127.0.0.1",
        "100.64.0.0",
        "100.64.0.1",
        "100.101.102.103",
        "100.127.255.254",
    ] {
        assert_eq!(check_bind_address(h).unwrap().to_string(), h);
    }
}

#[test]
fn ports_and_the_address_given_to_the_phone() {
    assert_eq!(check_port(8765), Ok(8765));
    assert_eq!(check_port(0), Ok(0));
    for p in [-1, 65536] {
        assert!(check_port(p).unwrap_err().0.contains("from 1 to 65535"));
    }
    assert_eq!(
        check_phone_hub_address("100.101.102.103:8765").unwrap().1,
        8765
    );
    for bad in [
        "127.0.0.1:8765",
        "0.0.0.0:8765",
        "192.168.1.10:8765",
        "100.101.102.103",
        "100.101.102.103:0",
        "100.101.102.103:08765",
        "100.101.102.103:70000",
        "laptop:8765",
    ] {
        assert!(check_phone_hub_address(bad).is_err(), "{bad}");
    }
}

/// Hub::start checks first: a refused address never reaches the socket layer. Only 127.0.0.1 is
/// ever bound in this file, and it is shut down.
#[tokio::test]
async fn hub_start_refuses_0000_ipv6_lan_and_hostnames_before_listening() {
    let dir = TempDir::new("bind");
    for host in [
        "0.0.0.0",
        "::",
        "192.168.1.10",
        "localhost",
        "127.0.0.2",
        "100.100.100.100",
    ] {
        // safety (as in http.test.ts): if the check were broken, stop here, before Hub::start
        // could bind a real interface
        assert!(
            check_bind_address(host).is_err(),
            "{host} must be refused before anything listens"
        );
        match Hub::start(HubConfig::new(dir.path(), host, 0, hub_key())).await {
            Err(HubStartError::Bind(e)) => assert!(e.0.starts_with("refusing to listen on"), "{e}"),
            Err(other) => panic!("{host}: {other}"),
            Ok(h) => {
                h.shutdown().await;
                panic!("{host} was bound");
            }
        }
    }
    let hub = Hub::start(HubConfig::new(dir.path(), "127.0.0.1", 0, hub_key()))
        .await
        .unwrap();
    assert!(hub.local_addr().ip().is_loopback());
    hub.shutdown().await;
}
