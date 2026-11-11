//! The laptop half of the no-ADB pairing QR (ship/NO-ADB-PLAN.md, stage 3).
//!
//! [`PairCodes`] issues one-time 128-bit codes that live [`CODE_LIFETIME_MS`] (5 minutes), and
//! [`pair_uri`] builds the `agentroid://pair?...` link the laptop shows as a QR. The phone parses it
//! with `dev.agentroid.helper.link.net.PairUri`, which applies the same rules, so the two halves
//! agree on every byte (the test vectors below are shared with `PairUriTest`).
//!
//! Rules, each with a test:
//! * a code is redeemed at most once, and never after it expires;
//! * only one code is outstanding at a time: issuing a new one cancels the old;
//! * three wrong guesses cancel the outstanding code, so a LAN neighbour cannot grind it;
//! * the hub address must be a private (10/8, 172.16/12, 192.168/16) or tailnet (100.64/10) IPv4
//!   literal: never 0.0.0.0, loopback, link-local or public.
//!
//! The code only proves the phone saw this screen. It is not the whole pairing: the phone still pins
//! the hub's TLS certificate hash from the same QR, and the laptop still asks the user to approve.

use std::net::Ipv4Addr;
use std::sync::Mutex;

pub const CODE_LIFETIME_MS: u64 = 5 * 60 * 1000;
pub const MAX_WRONG_GUESSES: u32 = 3;

#[derive(Debug, PartialEq, Eq)]
pub enum Redeem {
    Ok,
    /// No code outstanding, or the one there was cancelled.
    NoCode,
    Expired,
    Wrong,
    /// That guess was the last one allowed: the code is gone.
    WrongAndCancelled,
}

struct Outstanding {
    code: [u8; 16],
    expires_ms: u64,
    wrong: u32,
}

/// One-time pairing codes. Thread-safe; the clock is passed in so tests can move time.
#[derive(Default)]
pub struct PairCodes {
    cur: Mutex<Option<Outstanding>>,
}

impl PairCodes {
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh random code, cancelling any outstanding one. Returns the code and its expiry (ms).
    pub fn issue(&self, now_ms: u64) -> Result<([u8; 16], u64), String> {
        let mut code = [0u8; 16];
        getrandom::fill(&mut code).map_err(|e| format!("no system randomness: {e}"))?;
        Ok(self.issue_with(code, now_ms))
    }

    /// `issue` with a given code (tests only use this to pin the shared vectors).
    pub fn issue_with(&self, code: [u8; 16], now_ms: u64) -> ([u8; 16], u64) {
        let expires_ms = now_ms + CODE_LIFETIME_MS;
        *self.cur.lock().unwrap() = Some(Outstanding {
            code,
            expires_ms,
            wrong: 0,
        });
        (code, expires_ms)
    }

    pub fn redeem(&self, guess: &[u8], now_ms: u64) -> Redeem {
        let mut cur = self.cur.lock().unwrap();
        let Some(o) = cur.as_mut() else {
            return Redeem::NoCode;
        };
        if now_ms >= o.expires_ms {
            *cur = None;
            return Redeem::Expired;
        }
        if guess.len() == 16 && ct_eq(&o.code, guess) {
            *cur = None;
            return Redeem::Ok;
        }
        o.wrong += 1;
        if o.wrong >= MAX_WRONG_GUESSES {
            *cur = None;
            return Redeem::WrongAndCancelled;
        }
        Redeem::Wrong
    }
}

fn ct_eq(a: &[u8; 16], b: &[u8]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Private IPv4 or the tailnet: the only hub addresses a pairing QR may carry.
pub fn qr_host_allowed(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 10
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 100 && (64..=127).contains(&o[1]))
}

/// Unpadded base64url of 16 bytes (22 characters), as `PairUri` expects.
pub fn b64url16(b: &[u8; 16]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(22);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &x in b {
        acc = (acc << 8) | x as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(A[((acc >> bits) & 63) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(A[((acc << (6 - bits)) & 63) as usize] as char);
    }
    out
}

/// The `agentroid://pair` link for the QR. `name` is display-only on the phone and is limited to the
/// characters `PairUri` accepts (letters, digits, space . _ ' -; 1 to 64), spaces sent as `%20`.
pub fn pair_uri(
    ip: Ipv4Addr,
    port: u16,
    cert_sha256: &[u8; 32],
    code: &[u8; 16],
    expires_ms: u64,
    name: Option<&str>,
) -> Result<String, String> {
    if !qr_host_allowed(ip) {
        return Err(format!(
            "{ip} is not a home-network (10/8, 172.16/12, 192.168/16) or tailnet address"
        ));
    }
    if port == 0 {
        return Err("port 0".into());
    }
    let fp: String = cert_sha256.iter().map(|b| format!("{b:02x}")).collect();
    let mut s = format!(
        "agentroid://pair?h={ip}:{port}&fp={fp}&k={}&e={}",
        b64url16(code),
        expires_ms / 1000
    );
    if let Some(n) = name {
        let ok = (1..=64).contains(&n.chars().count())
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || " ._'-".contains(c));
        if !ok {
            return Err("name must be 1..64 of letters, digits, space . _ ' -".into());
        }
        s.push_str("&n=");
        s.push_str(&n.replace(' ', "%20"));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;
    const FP: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn bytes16() -> [u8; 16] {
        std::array::from_fn(|i| i as u8)
    }

    fn fp() -> [u8; 32] {
        std::array::from_fn(|i| {
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff,
            ][i % 16]
        })
    }

    /// The same vectors as PairUriTest: bytes 0..15 and the 0011..ff pin.
    #[test]
    fn the_uri_matches_the_phone_parser_vectors() {
        assert_eq!(b64url16(&bytes16()), "AAECAwQFBgcICQoLDA0ODw");
        assert_eq!(b64url16(&[0xff; 16]), format!("{}w", "_".repeat(21)));
        let u = pair_uri(
            Ipv4Addr::new(192, 168, 1, 20),
            8765,
            &fp(),
            &bytes16(),
            NOW + 300_000,
            Some("Alex PC"),
        )
        .unwrap();
        assert_eq!(
            u,
            format!("agentroid://pair?h=192.168.1.20:8765&fp={FP}&k=AAECAwQFBgcICQoLDA0ODw&e=1800000300&n=Alex%20PC")
        );
        assert!(u.len() < 512, "PairUri.MAX_LEN");
    }

    #[test]
    fn only_private_and_tailnet_hosts_go_in_a_qr() {
        for ok in [
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.0.1",
            "100.64.0.1",
            "100.127.1.1",
        ] {
            assert!(qr_host_allowed(ok.parse().unwrap()), "{ok}");
        }
        for bad in [
            "8.8.8.8",
            "127.0.0.1",
            "0.0.0.0",
            "169.254.1.1",
            "172.32.0.1",
            "172.15.0.1",
            "100.128.0.1",
            "192.169.0.1",
            "255.255.255.255",
        ] {
            assert!(!qr_host_allowed(bad.parse().unwrap()), "{bad}");
            assert!(
                pair_uri(bad.parse().unwrap(), 8765, &fp(), &bytes16(), NOW, None).is_err(),
                "{bad}"
            );
        }
        assert!(pair_uri(Ipv4Addr::new(10, 0, 0, 1), 0, &fp(), &bytes16(), NOW, None).is_err());
        for bad in ["", "<script>", "a\nb", "Ä", &"x".repeat(65)] {
            assert!(
                pair_uri(
                    Ipv4Addr::new(10, 0, 0, 1),
                    1,
                    &fp(),
                    &bytes16(),
                    NOW,
                    Some(bad)
                )
                .is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_code_is_single_use_and_expires() {
        let c = PairCodes::new();
        assert_eq!(c.redeem(&bytes16(), NOW), Redeem::NoCode);
        let (code, exp) = c.issue_with(bytes16(), NOW);
        assert_eq!(exp, NOW + CODE_LIFETIME_MS);
        assert_eq!(c.redeem(&code, NOW + 1), Redeem::Ok);
        assert_eq!(
            c.redeem(&code, NOW + 2),
            Redeem::NoCode,
            "a second phone with the same code"
        );
        c.issue_with(bytes16(), NOW);
        assert_eq!(c.redeem(&code, NOW + CODE_LIFETIME_MS), Redeem::Expired);
        assert_eq!(
            c.redeem(&code, NOW + 1),
            Redeem::NoCode,
            "an expired code is gone"
        );
    }

    #[test]
    fn a_new_code_cancels_the_old_and_wrong_guesses_run_out() {
        let c = PairCodes::new();
        let (old, _) = c.issue_with(bytes16(), NOW);
        let (new, _) = c.issue(NOW).unwrap();
        assert_ne!(old, new, "random code");
        assert_eq!(c.redeem(&old, NOW), Redeem::Wrong);
        assert_eq!(c.redeem(&old[..15], NOW), Redeem::Wrong, "short guess");
        assert_eq!(c.redeem(&old, NOW), Redeem::WrongAndCancelled);
        assert_eq!(
            c.redeem(&new, NOW),
            Redeem::NoCode,
            "the right code after 3 wrong guesses"
        );
    }
}
