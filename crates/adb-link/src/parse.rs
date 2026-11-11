//! Parsers: `adb devices -l`, `adb mdns services`, phone id from an mDNS name, and `choose`.
//!
//! Faithful port of phone-guard `devices.py` (parse_devices, phone_id, choose, parse_mdns).
//! No regex dependency: the two regexes of the original are hand-matched with the same
//! lazy/greedy behaviour.

use std::collections::BTreeSet;

/// States `adb devices` can print (the `_STATES` alternation of the Python original).
pub const STATES: [&str; 10] = [
    "device",
    "offline",
    "unauthorized",
    "authorizing",
    "connecting",
    "recovery",
    "sideload",
    "bootloader",
    "host",
    "no permissions",
];

/// One line of `adb devices -l`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub serial: String,
    pub state: String,
    pub model: Option<String>,
    pub usb: bool,
    /// `transport_id:` value; adb never reuses one within a server's life.
    pub tid: Option<String>,
}

impl Listed {
    /// True for a wireless entry (mDNS name or `ip:port`).
    pub fn network(&self) -> bool {
        is_network(&self.serial)
    }
}

fn is_network(serial: &str) -> bool {
    serial.contains("._adb-tls-connect.") || serial.contains(':')
}

/// Lazy `^(.+?)\s+(STATES)(\s.*)?$` on an already trimmed line. Returns (serial, state, rest).
fn match_line(l: &str) -> Option<(&str, &'static str, &str)> {
    for (i, c) in l.char_indices() {
        if i == 0 || !c.is_whitespace() {
            continue;
        }
        let after = l[i..].trim_start();
        for st in STATES {
            if let Some(r) = after.strip_prefix(st) {
                if r.is_empty() || r.starts_with(char::is_whitespace) {
                    return Some((&l[..i], st, r));
                }
            }
        }
    }
    None
}

/// Parse `adb devices` (with or without `-l`). Tab separated, or space padded in `-l` mode;
/// mDNS serials may hold spaces.
pub fn parse_devices(out: &str) -> Vec<Listed> {
    let mut res = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        let (serial, state, rest): (String, String, String) =
            if let Some((s, r)) = line.split_once('\t') {
                let words: Vec<&str> = r.split_whitespace().collect();
                if words.is_empty() {
                    continue;
                }
                (s.to_string(), words[0].to_string(), words[1..].join(" "))
            } else {
                match match_line(line.trim()) {
                    Some((s, st, r)) => (s.to_string(), st.to_string(), r.to_string()),
                    None => continue,
                }
            };
        let words: Vec<&str> = rest.split_whitespace().collect();
        let model = words
            .iter()
            .find_map(|w| w.strip_prefix("model:"))
            .map(|m| m.replace('_', " "));
        let tid = words
            .iter()
            .find_map(|w| {
                w.strip_prefix("transport_id:")
                    .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()))
            })
            .map(str::to_string);
        let net = is_network(&serial);
        let usb = words.iter().any(|w| w.starts_with("usb:")) || !net;
        res.push(Listed {
            serial: serial.trim().to_string(),
            state,
            model,
            usb,
            tid,
        });
    }
    res
}

/// The phone's own serial number, from an mDNS name (`adb-SERIALNO-ID (2)._adb-tls-connect._tcp`).
///
/// Any other token is returned as it is. Splits at the LAST dash so serials with dashes survive.
pub fn phone_id(serial: &str) -> String {
    let Some(s) = serial.strip_prefix("adb-") else {
        return serial.to_string();
    };
    let s = s.split("._adb-").next().unwrap_or(s);
    let s = strip_dup_suffix(s);
    match s.rfind('-') {
        Some(idx) if idx > 0 => s[..idx].to_string(),
        _ => s.to_string(),
    }
}

/// `re.sub(r"\s*\(\d+\)$", "", s)`
fn strip_dup_suffix(s: &str) -> &str {
    if s.ends_with(')') {
        if let Some(open) = s.rfind('(') {
            let digits = &s[open + 1..s.len() - 1];
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                return s[..open].trim_end();
            }
        }
    }
    s
}

/// Outcome of [`choose`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChooseState {
    Ready,
    Unauthorised,
    Missing,
}

/// Result of [`choose`]: state, the serial to use, and the other ready phones as (id, label).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub state: ChooseState,
    pub serial: Option<String>,
    pub others: Vec<(String, String)>,
}

/// Which phone to use. `want` (a phone id) pins the phone. Among one phone's entries USB wins,
/// then `current`, then the first.
pub fn choose(lst: &[Listed], want: Option<&str>, current: Option<&str>) -> Choice {
    let target: Option<String> = match want {
        Some(w) => Some(w.to_string()),
        None => lst
            .iter()
            .find(|l| l.state == "device")
            .or_else(|| lst.first())
            .map(|l| phone_id(&l.serial)),
    };
    let others: Vec<(String, String)> = lst
        .iter()
        .filter(|l| l.state == "device" && Some(phone_id(&l.serial)) != target)
        .map(|l| {
            (
                phone_id(&l.serial),
                l.model.clone().unwrap_or_else(|| phone_id(&l.serial)),
            )
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let Some(target) = target else {
        return Choice {
            state: ChooseState::Missing,
            serial: None,
            others,
        };
    };
    let mine: Vec<&Listed> = lst
        .iter()
        .filter(|l| phone_id(&l.serial) == target)
        .collect();
    let mut ready: Vec<&Listed> = mine
        .iter()
        .copied()
        .filter(|l| l.state == "device")
        .collect();
    ready.sort_by_key(|l| (!l.usb, Some(l.serial.as_str()) != current));
    if let Some(r) = ready.first() {
        return Choice {
            state: ChooseState::Ready,
            serial: Some(r.serial.clone()),
            others,
        };
    }
    match mine.iter().find(|l| l.state == "unauthorized") {
        Some(u) => Choice {
            state: ChooseState::Unauthorised,
            serial: Some(u.serial.clone()),
            others,
        },
        None => Choice {
            state: ChooseState::Missing,
            serial: None,
            others,
        },
    }
}

/// One mDNS connect service: the phone id and its announced `ip:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdnsRecord {
    pub phone_id: String,
    pub addr: String,
}

/// Parse `adb mdns services` into the connect services (`_adb-tls-connect`).
pub fn parse_mdns(out: &str) -> Vec<MdnsRecord> {
    let mut res = Vec::new();
    for line in out.lines() {
        let cols: Vec<String> = if line.contains('\t') {
            line.split('\t').map(str::to_string).collect()
        } else {
            match split_space_mdns(line) {
                Some(c) => c.to_vec(),
                None => Vec::new(),
            }
        };
        if cols.len() >= 3 && cols[1].trim().starts_with("_adb-tls-connect") {
            res.push(MdnsRecord {
                phone_id: phone_id(cols[0].trim()),
                addr: cols[2].trim().to_string(),
            });
        }
    }
    res
}

/// `^(.*?)\s+(_adb-\S+)\s+(\S+)\s*$`: the last two tokens are service type and address, the rest is
/// the (possibly space-containing) name.
fn split_space_mdns(line: &str) -> Option<[String; 3]> {
    let t = line.trim_end();
    let (head, addr) = t.rsplit_once(char::is_whitespace)?;
    let head = head.trim_end();
    let (name, svc) = head.rsplit_once(char::is_whitespace)?;
    if !svc.starts_with("_adb-") || addr.is_empty() {
        return None;
    }
    Some([name.to_string(), svc.to_string(), addr.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICES_L: &str = "List of devices attached\n\
adb-DEVICE0000EXAMPLE-abc123 (2)._adb-tls-connect._tcp device product:peridot model:POCO_F6 device:peridot transport_id:3\n\
192.0.2.78:37123    device product:peridot model:POCO_F6 device:peridot transport_id:4\n\
DEVICE0000EXAMPLE       device usb:1-2 product:peridot model:POCO_F6 device:peridot transport_id:1\n\
PROTECTED0000EXAMPLE            unauthorized usb:1-3 transport_id:2\n\
emulator-5554          offline\n\
\n";

    #[test]
    fn devices_l_with_spaces_in_mdns_serial() {
        let l = parse_devices(DEVICES_L);
        assert_eq!(l.len(), 5);
        assert_eq!(
            l[0].serial,
            "adb-DEVICE0000EXAMPLE-abc123 (2)._adb-tls-connect._tcp"
        );
        assert_eq!(l[0].state, "device");
        assert_eq!(l[0].model.as_deref(), Some("POCO F6"));
        assert_eq!(l[0].tid.as_deref(), Some("3"));
        assert!(l[0].network());
        assert!(!l[0].usb);
        assert_eq!(l[1].serial, "192.0.2.78:37123");
        assert!(!l[1].usb);
        assert!(l[2].usb);
        assert_eq!(l[2].tid.as_deref(), Some("1"));
        assert_eq!(l[3].state, "unauthorized");
        assert_eq!(l[4].state, "offline");
        assert!(l[4].usb); // not a network serial
    }

    #[test]
    fn devices_tab_format_and_noise() {
        let out = "* daemon not running; starting now at tcp:5037\n* daemon started successfully\nList of devices attached\nABC12345\tdevice\n198.51.100.5:5555\toffline\n";
        let l = parse_devices(out);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].serial, "ABC12345");
        assert!(l[0].usb);
        assert_eq!(l[1].state, "offline");
    }

    #[test]
    fn devices_crlf_and_no_permissions() {
        let out =
            "List of devices attached\r\nXYZ  no permissions (user in plugdev group) usb:1\r\n";
        let l = parse_devices(out);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].state, "no permissions");
        assert!(l[0].usb);
    }

    #[test]
    fn phone_ids() {
        assert_eq!(
            phone_id("adb-DEVICE0000EXAMPLE-abc123._adb-tls-connect._tcp"),
            "DEVICE0000EXAMPLE"
        );
        assert_eq!(
            phone_id("adb-DEVICE0000EXAMPLE-abc123 (2)._adb-tls-connect._tcp"),
            "DEVICE0000EXAMPLE"
        );
        assert_eq!(
            phone_id("adb-DEVICE0000EXAMPLE-abc123 (2)"),
            "DEVICE0000EXAMPLE"
        );
        assert_eq!(phone_id("adb-AB-CD-xyz._adb-tls-connect._tcp"), "AB-CD");
        assert_eq!(phone_id("adb-NODASH._adb-tls-connect._tcp"), "NODASH");
        assert_eq!(phone_id("192.0.2.78:37123"), "192.0.2.78:37123");
        assert_eq!(phone_id("DEVICE0000EXAMPLE"), "DEVICE0000EXAMPLE");
    }

    #[test]
    fn mdns_tab_and_space_forms() {
        let out = "List of discovered mdns services\n\
adb-DEVICE0000EXAMPLE-abc123\t_adb-tls-connect._tcp\t192.0.2.78:37123\n\
adb-DEVICE0000EXAMPLE-abc123\t_adb-tls-pairing._tcp\t192.0.2.78:40001\n\
adb-DEVICE0000EXAMPLE-abc123 (2)   _adb-tls-connect._tcp   192.0.2.246:37123\n\
garbage line\n";
        let r = parse_mdns(out);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            MdnsRecord {
                phone_id: "DEVICE0000EXAMPLE".into(),
                addr: "192.0.2.78:37123".into()
            }
        );
        assert_eq!(r[1].addr, "192.0.2.246:37123");
        assert_eq!(r[1].phone_id, "DEVICE0000EXAMPLE");
    }

    #[test]
    fn mdns_name_with_spaces_tab_separated() {
        let out = "adb-DEVICE0000EXAMPLE-abc123 (2)\t_adb-tls-connect._tcp\t198.51.100.2:5555\n";
        let r = parse_mdns(out);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].phone_id, "DEVICE0000EXAMPLE");
    }

    #[test]
    fn choose_prefers_usb_then_current() {
        let l = parse_devices(DEVICES_L);
        let c = choose(&l, Some("DEVICE0000EXAMPLE"), None);
        assert_eq!(c.state, ChooseState::Ready);
        assert_eq!(c.serial.as_deref(), Some("DEVICE0000EXAMPLE"));
        // no usb entry: current wins among network entries
        let net: Vec<Listed> = l.iter().filter(|x| x.network()).cloned().collect();
        let c = choose(
            &net,
            Some("DEVICE0000EXAMPLE"),
            Some("adb-DEVICE0000EXAMPLE-abc123 (2)._adb-tls-connect._tcp"),
        );
        assert_eq!(
            c.serial.as_deref(),
            Some("adb-DEVICE0000EXAMPLE-abc123 (2)._adb-tls-connect._tcp")
        );
    }

    #[test]
    fn choose_unauthorised_missing_and_others() {
        let l = parse_devices(DEVICES_L);
        let c = choose(&l, Some("PROTECTED0000EXAMPLE"), None);
        assert_eq!(c.state, ChooseState::Unauthorised);
        let c = choose(&l, Some("NOSUCH"), None);
        assert_eq!(c.state, ChooseState::Missing);
        assert!(c.serial.is_none());
        assert!(c
            .others
            .iter()
            .any(|(id, label)| id == "DEVICE0000EXAMPLE" && label == "POCO F6"));
        assert_eq!(choose(&[], None, None).state, ChooseState::Missing);
    }
}
