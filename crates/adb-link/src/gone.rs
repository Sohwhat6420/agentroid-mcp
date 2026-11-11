//! `device_gone`: a structured result with the likely cause of a dropped phone and the exact
//! human fix in plain words.

use crate::runner::Reach;

/// Likely cause of a drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cause {
    WifiOff,
    WirelessDebuggingOff,
    StaleMdnsIp,
    Unauthorised,
    Offline,
    Unknown,
}

impl Cause {
    /// Stable snake_case name used in the JSON result.
    pub fn as_str(self) -> &'static str {
        match self {
            Cause::WifiOff => "wifi_off",
            Cause::WirelessDebuggingOff => "wireless_debugging_off",
            Cause::StaleMdnsIp => "stale_mdns_ip",
            Cause::Unauthorised => "unauthorised",
            Cause::Offline => "offline",
            Cause::Unknown => "unknown",
        }
    }

    /// The exact fix, in plain words, for the person holding the phone.
    pub fn human_fix(self) -> &'static str {
        match self {
            Cause::WifiOff => "The phone cannot be reached on the network. On the phone, turn Wi-Fi on and check it is joined to the same network as this laptop. The watcher reconnects by itself within a few seconds of Wi-Fi coming back.",
            Cause::WirelessDebuggingOff => "The phone is on the network but wireless debugging is closed. On the phone open Settings, Developer options, Wireless debugging, and switch it on (if it already shows on, switch it off and on again). The watcher reconnects by itself afterwards.",
            Cause::StaleMdnsIp => "The network announcement points at an old address, but the phone is still at its last working address. Nothing to do on the phone; the watcher is using the last working address. If it is still gone after a minute, switch Wi-Fi off and on on the phone.",
            Cause::Unauthorised => "The phone has not approved this laptop. Unlock the phone and tap Allow on the 'Allow USB debugging?' prompt, ticking 'Always allow from this computer'. If no prompt appears, open Developer options, tap 'Revoke USB debugging authorisations', then connect again.",
            Cause::Offline => "adb sees the phone but it is not answering. Unlock the phone and wake the screen. If it stays offline, switch Wireless debugging off and on, or unplug and replug the USB cable.",
            Cause::Unknown => "The cause is not clear. Check the phone is awake, unlocked and on Wi-Fi, switch Wireless debugging off and on, then wait for the next check (every 15 seconds).",
        }
    }
}

/// What the watcher saw when the phone was missing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Evidence {
    /// State adb lists for the target when it is listed but not `device` (unauthorized, offline, ...).
    pub listed_state: Option<String>,
    /// mDNS announcement for the phone: (address, reachability), best reachability if several.
    pub mdns: Option<(String, Reach)>,
    /// Last working address and whether it answers now.
    pub last: Option<(String, Reach)>,
}

/// Work out the likely cause from the evidence.
pub fn classify(e: &Evidence) -> Cause {
    match e.listed_state.as_deref() {
        Some("unauthorized") | Some("authorizing") => return Cause::Unauthorised,
        Some("offline") => return Cause::Offline,
        _ => {}
    }
    let reaches: Vec<Reach> = e
        .mdns
        .iter()
        .chain(e.last.iter())
        .map(|(_, r)| *r)
        .collect();
    if reaches.is_empty() {
        return Cause::Unknown;
    }
    if reaches.contains(&Reach::Open) {
        // An address answers, yet the phone is not listed and a connect has not (yet) fixed it.
        return match (&e.mdns, &e.last) {
            (Some((_, r)), Some((_, Reach::Open))) if *r != Reach::Open => Cause::StaleMdnsIp,
            _ => Cause::Unknown,
        };
    }
    if reaches.contains(&Reach::Refused) {
        return Cause::WirelessDebuggingOff;
    }
    Cause::WifiOff
}

/// The structured `device_gone` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceGone {
    pub target: String,
    pub cause: Cause,
    pub human_fix: String,
    pub evidence: Evidence,
}

impl DeviceGone {
    pub fn from_evidence(target: &str, evidence: Evidence) -> Self {
        let cause = classify(&evidence);
        Self {
            target: target.to_string(),
            cause,
            human_fix: cause.human_fix().to_string(),
            evidence,
        }
    }

    /// JSON text: `{"error":"device_gone","target":..,"cause":..,"human_fix":..}`.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"error\":\"device_gone\",\"target\":{},\"cause\":{},\"human_fix\":{}}}",
            json_str(&self.target),
            json_str(self.cause.as_str()),
            json_str(&self.human_fix)
        )
    }
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(state: Option<&str>, mdns: Option<Reach>, last: Option<Reach>) -> Evidence {
        Evidence {
            listed_state: state.map(str::to_string),
            mdns: mdns.map(|r| ("198.51.100.246:37000".to_string(), r)),
            last: last.map(|r| ("198.51.100.78:37000".to_string(), r)),
        }
    }

    #[test]
    fn every_down_state_has_a_cause_and_a_fix() {
        let cases = [
            (
                ev(None, None, Some(Reach::Unreachable)),
                Cause::WifiOff,
                "wifi_off",
                "turn Wi-Fi on",
            ),
            (
                ev(None, Some(Reach::Unreachable), Some(Reach::Unreachable)),
                Cause::WifiOff,
                "wifi_off",
                "turn Wi-Fi on",
            ),
            (
                ev(None, None, Some(Reach::Refused)),
                Cause::WirelessDebuggingOff,
                "wireless_debugging_off",
                "Wireless debugging",
            ),
            (
                ev(None, Some(Reach::Unreachable), Some(Reach::Refused)),
                Cause::WirelessDebuggingOff,
                "wireless_debugging_off",
                "switch it on",
            ),
            (
                ev(None, Some(Reach::Unreachable), Some(Reach::Open)),
                Cause::StaleMdnsIp,
                "stale_mdns_ip",
                "last working address",
            ),
            (
                ev(Some("unauthorized"), None, None),
                Cause::Unauthorised,
                "unauthorised",
                "tap Allow",
            ),
            (
                ev(Some("offline"), None, Some(Reach::Open)),
                Cause::Offline,
                "offline",
                "unplug and replug",
            ),
            (ev(None, None, None), Cause::Unknown, "unknown", "not clear"),
            (
                ev(None, Some(Reach::Open), None),
                Cause::Unknown,
                "unknown",
                "not clear",
            ),
        ];
        for (e, cause, name, words) in cases {
            let d = DeviceGone::from_evidence("T", e.clone());
            assert_eq!(d.cause, cause, "{e:?}");
            assert_eq!(d.cause.as_str(), name);
            assert!(d.human_fix.contains(words), "{name}: {}", d.human_fix);
            assert!(d.human_fix.len() > 40);
            let j = d.to_json();
            assert!(j.contains("\"error\":\"device_gone\""));
            assert!(j.contains(&format!("\"cause\":\"{name}\"")));
        }
    }

    #[test]
    fn json_escapes_quotes() {
        let d = DeviceGone::from_evidence("a\"b", ev(Some("unauthorized"), None, None));
        let j = d.to_json();
        assert!(j.contains("\"target\":\"a\\\"b\""));
        assert!(j.contains("\\u0027") || j.contains("'Allow USB debugging?'"));
        assert!(!j.contains('\n'));
    }
}
