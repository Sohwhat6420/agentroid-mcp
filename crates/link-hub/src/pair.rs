//! `agentroid-link link pair <serial>`: binds a phone's link key to its `ro.serialno` and gives
//! the phone the hub's public key and tailnet address. Everything goes through the guarded adb
//! runner, and only these commands are sent (LINK-REPORT
//! "what the hub half must implement" item 5):
//!
//! 1. `-s <serial> shell getprop ro.serialno` (the identity probe the guarded runner also makes);
//! 2. `-s <serial> shell dumpsys activity service dev.agentroid.helper/.link.LinkService`: the
//!    four DumpText lines, public key only;
//! 3. `-s <serial> shell am broadcast -a dev.agentroid.helper.PAIR -n dev.agentroid.helper/.link.ConfigReceiver
//!    --es hub_key <hub SPKI> --es hub <100.x.y.z:port> --es serial <ro.serialno> [--ez rekey true]`;
//! 4. with `--rekey`, the dump again (the phone made a new key).
//!
//! The read-only S24 is refused before any of it, and again by the guard.

use agentroid_adb_link::guard::{link_dump_argv, link_pair_argv, S24_SERIAL};
use agentroid_adb_link::runner::{RunError, Runner};

use crate::bind::check_phone_hub_address;
use crate::crypto::{b64, is_key_id, key_id, parse_spki, unb64};
use crate::proto::is_serial;
use crate::store::{Binding, StateDir};

const ADB_TIMEOUT_MS: u64 = 20_000;

#[derive(Debug)]
pub enum PairError {
    /// The target is the read-only S24: it is never paired.
    ReadOnlyS24,
    BadArgument(String),
    /// adb or the guard said no.
    Adb(RunError),
    /// The phone's answer was not what the helper prints.
    Phone(String),
    Store(String),
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairError::ReadOnlyS24 => write!(
                f,
                "refused: the protected phone is read-only and is never paired (full devices only)"
            ),
            PairError::BadArgument(m) => write!(f, "{m}"),
            PairError::Adb(e) => write!(f, "adb: {e}"),
            PairError::Phone(m) => write!(f, "{m}"),
            PairError::Store(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for PairError {}

impl From<RunError> for PairError {
    fn from(e: RunError) -> Self {
        PairError::Adb(e)
    }
}

/// The four lines LinkService.dump() prints (`DumpText.java`), parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDump {
    pub spki: Vec<u8>,
    pub key_id: String,
}

/// Parses `dumpsys activity service ...LinkService` output. dumpsys wraps the dump in a
/// `SERVICE ...` header and indents it; the four `key=value` lines are found by content.
pub fn parse_dump(out: &str) -> Result<LinkDump, PairError> {
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    if !lines.contains(&"agentroid-link v1") {
        return Err(PairError::Phone(
            "the helper's LinkService did not answer the dump (is the full or helper app installed and its link started?)".into(),
        ));
    }
    let values =
        |k: &str| -> Vec<&str> { lines.iter().filter_map(|l| l.strip_prefix(k)).collect() };
    if !values("error=").is_empty() {
        return Err(PairError::Phone(
            "the phone's link key cannot be read (error=key_unavailable); run again with --rekey"
                .into(),
        ));
    }
    if values("alg=") != ["SHA256withECDSA"] {
        return Err(PairError::Phone(
            "the dump does not name alg=SHA256withECDSA exactly once".into(),
        ));
    }
    let (keys, ids) = (values("public_key="), values("key_id="));
    if keys.len() != 1 || ids.len() != 1 {
        return Err(PairError::Phone(
            "the dump must hold exactly one public_key and one key_id".into(),
        ));
    }
    let spki = unb64(keys[0])
        .ok_or_else(|| PairError::Phone("public_key is not canonical base64".into()))?;
    parse_spki(&spki).map_err(|e| PairError::Phone(format!("public_key: {e}")))?;
    if !is_key_id(ids[0]) || key_id(&spki) != ids[0] {
        return Err(PairError::Phone(
            "key_id is not the SHA-256 of public_key".into(),
        ));
    }
    Ok(LinkDump {
        key_id: ids[0].to_string(),
        spki,
    })
}

/// Parses `am broadcast` output: `Broadcast completed: result=-1, data="paired key_id=<hex>"`.
pub fn parse_pair_reply(out: &str) -> Result<String, PairError> {
    let line = out
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("Broadcast completed:"))
        .ok_or_else(|| PairError::Phone("am broadcast did not complete".into()))?;
    let data = line
        .split_once("data=\"")
        .map(|(_, d)| d.trim_end_matches('"'))
        .unwrap_or("");
    if !line.contains("result=-1") {
        let why = if data.is_empty() {
            "no receiver answered (is the helper app installed?)".to_string()
        } else {
            data.to_string()
        };
        return Err(PairError::Phone(format!(
            "the phone refused the pairing: {why}"
        )));
    }
    match data.strip_prefix("paired key_id=") {
        Some(id) if is_key_id(id) => Ok(id.to_string()),
        _ => Err(PairError::Phone(format!(
            "unexpected pairing reply {data:?}"
        ))),
    }
}

/// What a successful pairing did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub binding: Binding,
    /// The adb argv sent, in order (identity probes made by the guarded runner not included).
    pub commands: Vec<Vec<String>>,
}

/// One pairing. `runner` should be a [`agentroid_adb_link::guard::GuardedRunner`]; `selector`
/// is what the user typed (`agentroid link pair <serial>`); `hub` is `<tailnet IPv4>:<port>`.
pub fn pair(
    runner: &dyn Runner,
    store: &StateDir,
    hub_spki: &[u8],
    selector: &str,
    hub: &str,
    rekey: bool,
    now_ms: u64,
) -> Result<Paired, PairError> {
    if selector.is_empty() || selector.chars().any(char::is_whitespace) || selector.starts_with('-')
    {
        return Err(PairError::BadArgument(format!(
            "serial {selector:?} must be one adb serial, as `adb devices` lists it"
        )));
    }
    if selector.to_ascii_uppercase().contains(S24_SERIAL) {
        return Err(PairError::ReadOnlyS24);
    }
    check_phone_hub_address(hub).map_err(|e| PairError::BadArgument(e.0))?;
    parse_spki(hub_spki).map_err(|e| PairError::BadArgument(format!("hub key: {e}")))?;
    let mut commands = Vec::new();

    // 1. the hardware serial the key is bound to
    let probe: Vec<String> = ["-s", selector, "shell", "getprop", "ro.serialno"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let out = runner.run(&probe, ADB_TIMEOUT_MS)?;
    let serial = out.stdout.trim().to_string();
    if !out.ok() || !is_serial(&serial) {
        return Err(PairError::Phone(format!(
            "cannot read ro.serialno of {selector} (got {serial:?})"
        )));
    }
    if serial.eq_ignore_ascii_case(S24_SERIAL) {
        runner.deny(selector);
        return Err(PairError::ReadOnlyS24);
    }
    commands.push(probe);

    // 2. the phone's public key
    let dump_argv = link_dump_argv(selector);
    let dump_out = runner.run(&dump_argv, ADB_TIMEOUT_MS)?;
    commands.push(dump_argv.clone());
    let dump = match parse_dump(&dump_out.stdout) {
        Ok(d) => Some(d),
        Err(_) if rekey => None, // a broken key is what --rekey replaces
        Err(e) => return Err(e),
    };

    // 3. the hub's public key and address to the DUMP-protected receiver
    let pair_argv = link_pair_argv(selector, &b64(hub_spki), hub, &serial, rekey);
    let reply = runner.run(&pair_argv, ADB_TIMEOUT_MS)?;
    commands.push(pair_argv);
    let paired_id = parse_pair_reply(&reply.stdout)?;

    // 4. with --rekey the phone made a new key: read it again
    let dump = if rekey {
        let again = runner.run(&dump_argv, ADB_TIMEOUT_MS)?;
        commands.push(dump_argv);
        parse_dump(&again.stdout)?
    } else {
        dump.expect("present unless rekey")
    };
    if dump.key_id != paired_id {
        return Err(PairError::Phone(format!(
            "the phone answered key_id={paired_id} but its dump shows {}: nothing was stored, run the pairing again",
            dump.key_id
        )));
    }
    let binding = store
        .bind_device(&serial, &dump.spki, now_ms)
        .map_err(|e| PairError::Store(e.to_string()))?;
    Ok(Paired { binding, commands })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAElBzRSBOqvQxwEYCpN/1DuQI3gEUUz9D/G9TMxnS5dzmreLHq35/Jow/QCJafy2YrlGwsTzbViPFrMpn/ruHetQ==";
    const KID: &str = "679ca2e20ea98deb26e7f905175c49077d98b1068e3e1dfa80847cc7c39cd58a";

    #[test]
    fn dump_is_parsed_inside_the_dumpsys_wrapper() {
        let out = format!(
            "SERVICE dev.agentroid.helper/.link.LinkService 9f1c2d pid=4242 user=0\n  Client:\n    agentroid-link v1\n    alg=SHA256withECDSA\n    public_key={KEY}\n    key_id={KID}\n"
        );
        let d = parse_dump(&out).unwrap();
        assert_eq!(d.key_id, KID);
        assert!(parse_dump("No services match: dev.agentroid.helper/.link.LinkService").is_err());
        assert!(parse_dump(&out.replace(KID, &"0".repeat(64))).is_err());
        assert!(
            parse_dump("agentroid-link v1\nalg=SHA256withECDSA\nerror=key_unavailable\n")
                .unwrap_err()
                .to_string()
                .contains("--rekey")
        );
        let twice = format!("{out}    public_key={KEY}\n");
        assert!(parse_dump(&twice).is_err());
    }

    #[test]
    fn pair_reply_is_parsed() {
        let ok = format!("Broadcasting: Intent {{ act=dev.agentroid.helper.PAIR flg=0x400000 cmp=dev.agentroid.helper/.link.ConfigReceiver (has extras) }}\nBroadcast completed: result=-1, data=\"paired key_id={KID}\"\n");
        assert_eq!(parse_pair_reply(&ok).unwrap(), KID);
        let refused = "Broadcast completed: result=0, data=\"refused: hub address refused: x\"";
        assert!(parse_pair_reply(refused)
            .unwrap_err()
            .to_string()
            .contains("refused: hub address"));
        assert!(parse_pair_reply("Broadcast completed: result=0")
            .unwrap_err()
            .to_string()
            .contains("no receiver"));
    }
}
