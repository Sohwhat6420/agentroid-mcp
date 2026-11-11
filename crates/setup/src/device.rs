//! Talking to a phone through a `Runner` (the guarded one in production): the reads doctor sends,
//! parsers for their output, and the few writes `setup` may make, each with its exact shell line.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use agentroid_adb_link::runner::Runner;

/// The helper's applicationId (android/app/build.gradle.kts).
pub const HELPER_PKG: &str = "dev.agentroid.helper";
/// The helper's accessibility service, as `enabled_accessibility_services` spells a component.
pub const HELPER_SERVICE: &str =
    "dev.agentroid.helper/dev.agentroid.helper.a11y.HelperAccessibilityService";
/// The short spelling Android may also store.
pub const HELPER_SERVICE_SHORT: &str = "dev.agentroid.helper/.a11y.HelperAccessibilityService";
/// The permission the keeper needs (full mode only).
pub const WSS: &str = "android.permission.WRITE_SECURE_SETTINGS";
/// Per-command timeout, the same as the probe's.
pub const READ_TIMEOUT_MS: u64 = 5_000;

/// doctor's read list, compiled in.
pub const DOCTOR_READS: &str = include_str!("../doctor-reads.txt");
/// The probe's full list, compiled in (doctor reuses its read-only commands).
pub const PROBE_FULL: &str = include_str!("../../../probes/readonly.txt");

/// Words that make a shell line a write. A read line holding any of them is refused by a test.
pub const WRITE_WORDS: [&str; 16] = [
    "put",
    "delete",
    "set",
    "grant",
    "revoke",
    "whitelist",
    "install",
    "uninstall",
    "am",
    "input",
    "reboot",
    "rm",
    "reset",
    "clear",
    "disable",
    "enable",
];

/// The argv for one shell line on `target`.
pub fn shell_argv(target: &str, line: &str) -> Vec<String> {
    vec![
        "-s".to_string(),
        target.to_string(),
        "shell".to_string(),
        line.to_string(),
    ]
}

/// Run each line once; keep the stdout of the ones that exited 0. Failures, refusals and timeouts
/// are simply absent (the caller reports the check as not done).
pub fn run_reads(runner: &dyn Runner, target: &str, lines: &[String]) -> HashMap<String, String> {
    let mut got = HashMap::new();
    for l in lines {
        if let Ok(o) = runner.run(&shell_argv(target, l), READ_TIMEOUT_MS) {
            if o.ok() {
                got.insert(l.clone(), o.stdout);
            }
        }
    }
    got
}

/// Quote a value for the phone's shell as one word: `'...'`, with `'` written as `'\''`.
pub fn sh_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\\''"))
}

/// `settings list <ns>` to a map. Values are kept byte for byte (only the line break is dropped).
pub fn settings_map(out: &str) -> BTreeMap<String, String> {
    out.lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| !k.trim().is_empty() && !k.contains(' '))
        .map(|(k, v)| (k.to_string(), v.trim_end_matches('\r').to_string()))
        .collect()
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// The user Doze whitelist from `dumpsys deviceidle`: the packages under `Whitelist user apps:`
/// and `Whitelist (except idle) user apps:`.
pub fn whitelist_user(out: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let mut header: Option<usize> = None;
    for l in out.lines() {
        let t = l.trim();
        if t.starts_with("Whitelist") && t.ends_with("apps:") {
            header = t.contains("user apps").then_some(indent(l));
            continue;
        }
        if let Some(h) = header {
            if t.is_empty() || indent(l) <= h {
                header = None;
                continue;
            }
            if t.contains('.') && !t.contains(' ') {
                set.insert(t.to_string());
            }
        }
    }
    set
}

/// `cmd appops get <pkg>` to `op name -> mode` (`RUN_ANY_IN_BACKGROUND: allow; time=...` gives allow).
pub fn appops_map(out: &str) -> BTreeMap<String, String> {
    out.lines()
        .filter_map(|l| l.trim().split_once(": "))
        .filter(|(k, _)| !k.is_empty() && !k.contains(' '))
        .map(|(k, v)| {
            (
                k.to_string(),
                v.split(';').next().unwrap_or("").trim().to_string(),
            )
        })
        .collect()
}

/// The mode of one op, by name or by number. A numeric HyperOS op may print as `MIUIOP(10008)` or as
/// the bare number (ASSUMPTION, checked on the POCO: see SETUP-REPORT.md).
pub fn op_mode<'a>(ops: &'a BTreeMap<String, String>, op: &str) -> Option<&'a str> {
    ops.get(op)
        .or_else(|| ops.get(&format!("MIUIOP({op})")))
        .map(String::as_str)
}

/// `pm list packages` to package names.
pub fn packages(out: &str) -> BTreeSet<String> {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix("package:"))
        .map(|p| p.trim().to_string())
        .collect()
}

/// `ps -A -o NAME` to process names (the header line dropped).
pub fn processes(out: &str) -> BTreeSet<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "NAME")
        .map(str::to_string)
        .collect()
}

/// Whether `dumpsys package <helper>` shows WRITE_SECURE_SETTINGS granted.
pub fn wss_granted(out: &str) -> bool {
    out.lines()
        .any(|l| l.trim().starts_with(&format!("{WSS}: granted=true")))
}

/// True when `list` (a `:`-separated component list) holds the helper's service in either spelling.
pub fn has_helper_service(list: &str) -> bool {
    list.split(':').any(|c| {
        c.eq_ignore_ascii_case(HELPER_SERVICE) || c.eq_ignore_ascii_case(HELPER_SERVICE_SHORT)
    })
}

/// Append the helper's service to an existing component list, never replacing what is there.
pub fn append_service(existing: Option<&str>) -> String {
    match existing
        .map(str::trim)
        .filter(|e| !e.is_empty() && *e != "null")
    {
        Some(e) => format!("{e}:{HELPER_SERVICE}"),
        None => HELPER_SERVICE.to_string(),
    }
}

/// True when a package list held in a settings value (`,`, `:`, `;` or space separated) lists `pkg`.
pub fn value_lists_pkg(value: &str, pkg: &str) -> bool {
    value
        .split(|c: char| c == ',' || c == ':' || c == ';' || c.is_whitespace())
        .any(|p| p == pkg)
}

/// One write `setup` may make on a phone. Every one has an exact undo (see `journal.rs`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    /// `settings put <ns> <key> <value>`
    Setting {
        ns: String,
        key: String,
        value: String,
    },
    /// `dumpsys deviceidle whitelist +<pkg>`
    WhitelistAdd { pkg: String },
    /// `cmd appops set <pkg> <op> <mode>`
    AppOp {
        pkg: String,
        op: String,
        mode: String,
    },
    /// `pm grant <pkg> <perm>`
    Grant { pkg: String, perm: String },
}

fn plain_word(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Change {
    /// The exact shell line. Names are checked plain; a value is quoted as one word and may not hold
    /// a line break or NUL.
    pub fn line(&self) -> Result<String, String> {
        match self {
            Change::Setting { ns, key, value } => {
                if !["global", "secure", "system"].contains(&ns.as_str()) || !plain_word(key) {
                    return Err(format!("bad setting {ns} {key}"));
                }
                if value.contains(['\n', '\r', '\0']) {
                    return Err(format!("setting {key}: value holds a line break"));
                }
                Ok(format!("settings put {ns} {key} {}", sh_quote(value)))
            }
            Change::WhitelistAdd { pkg } if plain_word(pkg) => {
                Ok(format!("dumpsys deviceidle whitelist +{pkg}"))
            }
            Change::AppOp { pkg, op, mode }
                if plain_word(pkg) && plain_word(op) && plain_word(mode) =>
            {
                Ok(format!("cmd appops set {pkg} {op} {mode}"))
            }
            Change::Grant { pkg, perm } if plain_word(pkg) && plain_word(perm) => {
                Ok(format!("pm grant {pkg} {perm}"))
            }
            other => Err(format!("bad change {other:?}")),
        }
    }

    /// What a human reads in the plan.
    pub fn describe(&self) -> String {
        match self {
            Change::Setting { ns, key, value } => format!("set {ns} setting {key} to {value:?}"),
            Change::WhitelistAdd { pkg } => format!("add {pkg} to the Doze whitelist"),
            Change::AppOp { pkg, op, mode } => format!("set app-op {op} of {pkg} to {mode}"),
            Change::Grant { pkg, perm } => format!("grant {perm} to {pkg}"),
        }
    }
}

/// The exact commands that undo a device write, from the state before it.
pub fn undo_line(entry: &crate::journal::Entry) -> Option<Result<String, String>> {
    use crate::journal::Entry;
    Some(match entry {
        Entry::Setting {
            ns, key, before, ..
        } => match before {
            Some(v) => Change::Setting {
                ns: ns.clone(),
                key: key.clone(),
                value: v.clone(),
            }
            .line(),
            None if plain_word(key) && ["global", "secure", "system"].contains(&ns.as_str()) => {
                Ok(format!("settings delete {ns} {key}"))
            }
            None => Err(format!("bad setting {ns} {key}")),
        },
        Entry::Whitelist { pkg, was_present } => {
            if *was_present {
                return None;
            }
            if !plain_word(pkg) {
                return Some(Err(format!("bad package {pkg}")));
            }
            Ok(format!("dumpsys deviceidle whitelist -{pkg}"))
        }
        Entry::AppOp {
            pkg, op, before, ..
        } => {
            let mode = before.clone().unwrap_or_else(|| "default".to_string());
            Change::AppOp {
                pkg: pkg.clone(),
                op: op.clone(),
                mode,
            }
            .line()
        }
        Entry::Grant {
            pkg,
            perm,
            was_granted,
        } => {
            if *was_granted {
                return None;
            }
            if !(plain_word(pkg) && plain_word(perm)) {
                return Some(Err(format!("bad grant {pkg} {perm}")));
            }
            Ok(format!("pm revoke {pkg} {perm}"))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsers_read_the_real_formats() {
        let s =
            settings_map("adb_wifi_enabled=1\nenabled_accessibility_services=a/b:c/d\nweird=x=y\n");
        assert_eq!(s["adb_wifi_enabled"], "1");
        assert_eq!(s["weird"], "x=y");
        let idle = "  Whitelist (except idle) system apps:\n    com.android.providers.downloads\n  Whitelist user apps:\n    com.tailscale.ipn\n    dev.agentroid.helper\n  Whitelist (except idle) all app ids:\n    1000\n";
        assert_eq!(
            whitelist_user(idle).into_iter().collect::<Vec<_>>(),
            ["com.tailscale.ipn", "dev.agentroid.helper"]
        );
        let ops = appops_map("RUN_ANY_IN_BACKGROUND: ignore\nSTART_FOREGROUND: allow; time=+3m ago\nMIUIOP(10008): deny\n");
        assert_eq!(op_mode(&ops, "RUN_ANY_IN_BACKGROUND"), Some("ignore"));
        assert_eq!(op_mode(&ops, "START_FOREGROUND"), Some("allow"));
        assert_eq!(op_mode(&ops, "10008"), Some("deny"));
        assert_eq!(op_mode(&ops, "10007"), None);
        assert!(packages("package:a.b\npackage:c.d\n").contains("c.d"));
        assert!(processes("NAME\ninit\ncom.x\n").contains("com.x"));
        assert!(wss_granted("    install permissions:\n      android.permission.WRITE_SECURE_SETTINGS: granted=true\n"));
        assert!(!wss_granted(
            "      android.permission.WRITE_SECURE_SETTINGS: granted=false\n"
        ));
    }

    #[test]
    fn the_service_is_appended_never_replaced() {
        assert_eq!(
            append_service(Some("x.y/.Artemis")),
            format!("x.y/.Artemis:{HELPER_SERVICE}")
        );
        assert_eq!(append_service(None), HELPER_SERVICE);
        assert_eq!(append_service(Some("null")), HELPER_SERVICE);
        assert!(has_helper_service(&format!(
            "x.y/.A:{HELPER_SERVICE_SHORT}"
        )));
        assert!(!has_helper_service("x.y/.A"));
    }

    #[test]
    fn change_lines_are_exact_and_quoted() {
        let c = Change::Setting {
            ns: "secure".into(),
            key: "k".into(),
            value: "a'b c".into(),
        };
        assert_eq!(c.line().unwrap(), "settings put secure k 'a'\\''b c'");
        assert!(Change::Setting {
            ns: "secure".into(),
            key: "k;reboot".into(),
            value: "1".into()
        }
        .line()
        .is_err());
        assert!(Change::Setting {
            ns: "other".into(),
            key: "k".into(),
            value: "1".into()
        }
        .line()
        .is_err());
        assert!(Change::Setting {
            ns: "global".into(),
            key: "k".into(),
            value: "a\nb".into()
        }
        .line()
        .is_err());
        assert!(Change::AppOp {
            pkg: "p".into(),
            op: "10008 ; reboot".into(),
            mode: "allow".into()
        }
        .line()
        .is_err());
        assert_eq!(
            Change::WhitelistAdd {
                pkg: HELPER_PKG.into()
            }
            .line()
            .unwrap(),
            "dumpsys deviceidle whitelist +dev.agentroid.helper"
        );
    }
}
