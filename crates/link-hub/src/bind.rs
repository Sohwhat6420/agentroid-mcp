//! The listener's address check, ported rule for rule and message for message from
//! `apps/mcp/src/http-wiring.ts` `checkBindAddress` / `checkPort` (the
//! hub-mode exception to "no TCP listener": the tailnet interface only).

use std::net::Ipv4Addr;

/// Why an address or port was refused. Nothing is bound when this is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindRefused(pub String);

impl std::fmt::Display for BindRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BindRefused {}

/// `(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)`: a decimal octet without leading zeros.
fn octet(s: &str) -> Option<u8> {
    let b = s.as_bytes();
    let ok = match b.len() {
        1 => b[0].is_ascii_digit(),
        2 => (b'1'..=b'9').contains(&b[0]) && b[1].is_ascii_digit(),
        3 => b.iter().all(u8::is_ascii_digit) && b[0] != b'0',
        _ => false,
    };
    if !ok {
        return None;
    }
    s.parse::<u16>().ok().filter(|n| *n <= 255).map(|n| n as u8)
}

/// `IPV4_RE`: four octets, nothing else.
fn dotted_quad(host: &str) -> Option<[u8; 4]> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut o = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        o[i] = octet(p)?;
    }
    Some(o)
}

/// JSON.stringify of a string, as the TypeScript message prints it.
fn quoted(host: &str) -> String {
    serde_json::to_string(host).unwrap_or_else(|_| format!("{host:?}"))
}

/// The bind address, or [`BindRefused`] with the reason. Accepts 127.0.0.1 and literal IPv4
/// addresses in 100.64.0.0/10 (Tailscale's CGNAT range), and nothing else.
pub fn check_bind_address(host: &str) -> Result<Ipv4Addr, BindRefused> {
    let why = |s: &str| {
        BindRefused(format!(
            "refusing to listen on {}: {s}. The listener binds only the laptop's tailnet IPv4 address (100.64.0.0/10, see `tailscale ip -4`) or 127.0.0.1 for tests.",
            quoted(host)
        ))
    };
    if host.is_empty() {
        return Err(why("no address given"));
    }
    if host == "127.0.0.1" {
        return Ok(Ipv4Addr::LOCALHOST);
    }
    let Some(o) = dotted_quad(host) else {
        if host.contains(':') {
            return Err(why(
                "IPv6 addresses (including :: and ::1) are not accepted",
            ));
        }
        if host.bytes().all(|c| c.is_ascii_digit() || c == b'.') {
            return Err(why("not a well-formed dotted-quad IPv4 address"));
        }
        return Err(why("a hostname is not accepted, because a name can resolve to any interface; give the literal address"));
    };
    let (a, b) = (o[0], o[1]);
    if host == "0.0.0.0" {
        return Err(why(
            "0.0.0.0 means every interface, the LAN and any public address included",
        ));
    }
    if a == 127 {
        return Err(why(
            "of the loopback range only 127.0.0.1 is accepted (for tests)",
        ));
    }
    if a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 169 && b == 254)
    {
        return Err(why(
            "a LAN or link-local address would expose the listener to the local network",
        ));
    }
    if a == 100 && (64..=127).contains(&b) {
        if host == "100.100.100.100" {
            return Err(why(
                "100.100.100.100 is Tailscale's own DNS address, not this machine's",
            ));
        }
        return Ok(Ipv4Addr::from(o));
    }
    Err(why("not a tailnet address"))
}

/// The bind rule with `--link-lan` (home Wi-Fi, TLS required by the hub): everything [`check_bind_address`] accepts,
/// plus exactly one more class, a literal private IPv4 (10/8, 172.16/12, 192.168/16). 0.0.0.0, link-local, other
/// loopback, public addresses, IPv6 and host names stay refused.
pub fn check_bind_address_lan(host: &str) -> Result<Ipv4Addr, BindRefused> {
    if let Some(o) = dotted_quad(host) {
        let (a, b) = (o[0], o[1]);
        if a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168) {
            return Ok(Ipv4Addr::from(o));
        }
    }
    check_bind_address(host)
}

/// `checkPort`: 0..=65535 (0 picks a free port, for tests).
pub fn check_port(port: i64) -> Result<u16, BindRefused> {
    u16::try_from(port).map_err(|_| {
        BindRefused(format!("refusing port {port}: give an integer from 1 to 65535 in the config (0 picks a free port, for tests)"))
    })
}

/// The address a phone is told to dial (`am broadcast ... --es hub <host:port>`): a tailnet
/// address and a real port. Stricter than [`check_bind_address`]: the phone refuses loopback
/// (`TailnetGuard`), so 127.0.0.1 is refused here too.
pub fn check_phone_hub_address(addr: &str) -> Result<(Ipv4Addr, u16), BindRefused> {
    let (host, port) = addr.rsplit_once(':').ok_or_else(|| {
        BindRefused(format!(
            "hub address {} must be <tailnet IPv4>:<port>",
            quoted(addr)
        ))
    })?;
    let ip = check_bind_address(host)?;
    if ip.is_loopback() {
        return Err(BindRefused(
            "refusing 127.0.0.1 as the hub address for a phone: the phone dials only 100.64.0.0/10 (see `tailscale ip -4`)".into(),
        ));
    }
    let port = match port.parse::<u16>() {
        Ok(p) if p >= 1 && !port.starts_with('0') => p,
        _ => {
            return Err(BindRefused(format!(
                "hub port {} must be an integer from 1 to 65535",
                quoted(port)
            )))
        }
    };
    Ok((ip, port))
}
