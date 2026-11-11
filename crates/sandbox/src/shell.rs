//! `agentroid.shell(cmd)` classification ("Logged shell tier", against the 2.4
//! read-only diagnostics list, line 262).
//!
//! * **R0** only when the whole argv matches one 2.4 pattern exactly (never a prefix match).
//! * Everything else is **R2** ("R2 by default", field notes Q2 (b)) and refused: no approval
//!   channel exists in Phase 2, so there is no exception and no chat route.
//! * Named R2: `dumpsys battery set|reset|unplug`, `dumpsys deviceidle force-idle|unforce|step`.
//! * `pm clear` is not on the list, so refused (R2 or higher; the R4 row is 3.3's).
//! * A shell metacharacter, quote or control character anywhere refuses the command before it is
//!   split, so nothing reaches the phone's `sh`.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RiskClass {
    R0,
    /// "R2 or higher"; Phase 2 has no finer classifier (3.3).
    R2,
}

impl RiskClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            RiskClass::R0 => "R0",
            RiskClass::R2 => "R2",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellVerdict {
    /// R0: run this argv (the daemon quotes every element).
    Allowed {
        argv: Vec<String>,
    },
    Refused {
        class: RiskClass,
        reason: String,
    },
}

impl ShellVerdict {
    pub fn class(&self) -> RiskClass {
        match self {
            ShellVerdict::Allowed { .. } => RiskClass::R0,
            ShellVerdict::Refused { class, .. } => *class,
        }
    }
}

/// 2.4: `;` `&` `|` `$` `` ` `` `(` `)` `<` `>` `\` `'` `"`, newline, `*?[]`. Control characters
/// (newline, CR, tab, NUL...) and non-ASCII are refused separately.
pub const METACHARS: [char; 16] = [
    ';', '&', '|', '$', '`', '(', ')', '<', '>', '\\', '\'', '"', '*', '?', '[', ']',
];
/// 2.4: `dumpsys <service>` with no further arguments, for these services only.
pub const DUMPSYS_SERVICES: [&str; 9] = [
    "battery",
    "thermalservice",
    "display",
    "deviceidle",
    "input",
    "power",
    "window",
    "activity",
    "package",
];
pub const SETTINGS_NAMESPACES: [&str; 3] = ["system", "secure", "global"];
pub const PM_LIST_WHAT: [&str; 3] = ["packages", "features", "permissions"];
pub const PM_LIST_FLAGS: [&str; 6] = ["-f", "-d", "-e", "-s", "-3", "-U"];
/// The named R2 forms (2.4 and the shell tier).
pub const NAMED_R2: [[&str; 3]; 6] = [
    ["dumpsys", "battery", "set"],
    ["dumpsys", "battery", "reset"],
    ["dumpsys", "battery", "unplug"],
    ["dumpsys", "deviceidle", "force-idle"],
    ["dumpsys", "deviceidle", "unforce"],
    ["dumpsys", "deviceidle", "step"],
];
/// Words that make a command R2 or higher wherever they appear (2.4 / the shell tier).
pub const R2_WORDS: [&str; 10] = [
    "set",
    "reset",
    "put",
    "delete",
    "force",
    "unforce",
    "enable",
    "disable",
    "step",
    "whitelist",
];
const MAX_CMD: usize = 1024;

/// `getprop` / `settings get` keys: 2.4's `^[A-Za-z0-9_.:-]+$`, narrowed to start with an
/// alphanumeric so a key can never be read as an option.
fn is_key(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// A Java package name: dot-separated segments, each starting with a letter.
fn is_package(s: &str) -> bool {
    s.len() <= 255
        && s.contains('.')
        && s.split('.').all(|seg| {
            seg.starts_with(|c: char| c.is_ascii_alphabetic())
                && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// An app-op name (`CAMERA`, `android:camera`, `RUN_IN_BACKGROUND`).
fn is_op(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':'))
}

/// Exact match against the 2.4 read-only list.
pub fn is_allowlisted(argv: &[&str]) -> bool {
    match argv {
        ["dumpsys", svc] => DUMPSYS_SERVICES.contains(svc),
        ["getprop", key] => is_key(key),
        ["settings", "get", ns, key] => SETTINGS_NAMESPACES.contains(ns) && is_key(key),
        ["settings", "list", ns] => SETTINGS_NAMESPACES.contains(ns),
        ["pm", "list", what, flags @ ..] => {
            PM_LIST_WHAT.contains(what)
                && flags.iter().all(|f| PM_LIST_FLAGS.contains(f))
                && flags
                    .iter()
                    .enumerate()
                    .all(|(i, f)| !flags[..i].contains(f))
        }
        ["cmd", "appops", "get", pkg] => is_package(pkg),
        ["cmd", "appops", "get", pkg, op] => is_package(pkg) && is_op(op),
        ["wm", "size"] | ["wm", "density"] | ["ime", "list", "-s"] | ["getevent", "-pl"] => true,
        _ => false,
    }
}

fn refused(reason: impl Into<String>) -> ShellVerdict {
    ShellVerdict::Refused {
        class: RiskClass::R2,
        reason: reason.into(),
    }
}

/// Classify one `agentroid.shell` command line (what would follow `adb shell`).
pub fn classify(cmd: &str) -> ShellVerdict {
    if cmd.len() > MAX_CMD {
        return refused(format!("command longer than {MAX_CMD} bytes"));
    }
    if let Some(c) = cmd.chars().find(|c| METACHARS.contains(c)) {
        return refused(format!(
            "shell metacharacter {c:?} refused before anything is sent"
        ));
    }
    if let Some(c) = cmd.chars().find(|c| c.is_control() || !c.is_ascii()) {
        return refused(format!(
            "control or non-ASCII character {c:?} refused before anything is sent"
        ));
    }
    let argv: Vec<&str> = cmd.split(' ').filter(|a| !a.is_empty()).collect();
    if argv.is_empty() {
        return refused("empty command");
    }
    if is_allowlisted(&argv) {
        return ShellVerdict::Allowed {
            argv: argv.iter().map(|s| s.to_string()).collect(),
        };
    }
    if let Some(named) = NAMED_R2
        .iter()
        .find(|n| argv.len() >= 3 && argv[..3] == n[..])
    {
        return refused(format!(
            "`{}` is named R2: it changes device state",
            named.join(" ")
        ));
    }
    if argv.len() >= 2 && argv[0] == "pm" && argv[1] == "clear" {
        return refused(
            "`pm clear` wipes app data and is not on the 2.4 read-only list (R2 or higher)",
        );
    }
    if let Some(w) = argv
        .iter()
        .find(|a| R2_WORDS.contains(a) || **a == "+" || **a == "-")
    {
        return refused(format!("`{w}` makes a command R2 or higher"));
    }
    if argv[0] == "dumpsys" && argv.len() > 2 {
        return refused("an argument after the dumpsys service name is R2 or higher");
    }
    refused("not an exact match for the 2.4 read-only list, so R2 by default")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r0(cmd: &str) -> bool {
        matches!(classify(cmd), ShellVerdict::Allowed { .. })
    }

    #[test]
    fn the_2_4_list_is_r0() {
        for cmd in [
            "dumpsys battery",
            "dumpsys thermalservice",
            "dumpsys display",
            "dumpsys deviceidle",
            "dumpsys input",
            "dumpsys power",
            "dumpsys window",
            "dumpsys activity",
            "dumpsys package",
            "getprop ro.serialno",
            "getprop ro.mi.os.version.name",
            "settings get system screen_brightness",
            "settings get global adb_enabled",
            "settings list secure",
            "pm list packages",
            "pm list packages -3 -f",
            "pm list features",
            "pm list permissions -d",
            "cmd appops get com.android.settings",
            "cmd appops get com.android.settings CAMERA",
            "wm size",
            "wm density",
            "ime list -s",
            "getevent -pl",
            "  getprop   ro.serialno  ",
        ] {
            assert!(r0(cmd), "{cmd}");
        }
    }

    #[test]
    fn named_r2_and_pm_clear_are_refused_as_r2() {
        for cmd in [
            "dumpsys battery set level 5",
            "dumpsys battery set temp 450",
            "dumpsys battery reset",
            "dumpsys battery unplug",
            "dumpsys deviceidle force-idle",
            "dumpsys deviceidle unforce",
            "dumpsys deviceidle step",
            "pm clear com.x",
            "pm clear com.android.settings",
        ] {
            let v = classify(cmd);
            assert_eq!(v.class(), RiskClass::R2, "{cmd}");
            assert!(matches!(v, ShellVerdict::Refused { .. }), "{cmd}");
        }
        assert!(
            matches!(classify("dumpsys battery unplug"), ShellVerdict::Refused { reason, .. } if reason.contains("named R2"))
        );
        assert!(
            matches!(classify("pm clear com.x"), ShellVerdict::Refused { reason, .. } if reason.contains("pm clear"))
        );
    }

    #[test]
    fn near_misses_are_not_r0() {
        for cmd in [
            "dumpsys",
            "dumpsys battery -a",
            "dumpsys battery --checkin",
            "dumpsys meminfo",
            "dumpsys deviceidle whitelist +com.x",
            "dumpsys deviceidle whitelist -com.x",
            "getprop",
            "getprop a b",
            "getprop -T",
            "settings get",
            "settings get system",
            "settings get other a",
            "settings put system a 1",
            "settings delete system a",
            "settings list",
            "settings list system extra",
            "pm list",
            "pm list packages -f -f",
            "pm list packages com.x",
            "pm list users",
            "pm enable com.x",
            "pm disable com.x",
            "pm disable-user com.x",
            "cmd appops set com.x CAMERA allow",
            "cmd appops get",
            "cmd appops get notapackage",
            "cmd appops get com.x OP extra",
            "wm size 720x1280",
            "wm density 240",
            "wm size reset",
            "ime list",
            "ime set com.x/.Ime",
            "getevent",
            "getevent -l",
            "rm -rf /sdcard",
            "reboot",
            "input tap 1 1",
            "am start -n com.x/.A",
            "adb shell getprop ro.serialno",
            "sh -c id",
            "",
            "   ",
        ] {
            assert_eq!(classify(cmd).class(), RiskClass::R2, "{cmd:?}");
        }
    }

    #[test]
    fn metacharacters_and_control_characters_are_refused() {
        for cmd in [
            "pm list packages; pm clear com.x",
            "getprop x && input tap 1 1",
            "getprop $(input tap 1 1)",
            "getprop `id`",
            "settings get system a|settings put system a 1",
            "getprop ro.serialno\ninput tap 1 1",
            "getprop ro.serialno\rreboot",
            "getprop ro.serialno\tx",
            "getprop 'ro.serialno'",
            "getprop \"ro.serialno\"",
            "getprop ro.*",
            "getprop ro.serial?o",
            "getprop [ro]",
            "dumpsys battery > /sdcard/x",
            "dumpsys battery < /x",
            "getprop a\\b",
            "getprop ro.serialnö",
            "getprop ro.serialno\0",
        ] {
            let v = classify(cmd);
            assert_eq!(v.class(), RiskClass::R2, "{cmd:?}");
            assert!(
                matches!(&v, ShellVerdict::Refused { reason, .. } if reason.contains("refused before anything is sent")),
                "{cmd:?}: {v:?}"
            );
        }
    }

    #[test]
    fn allowed_argv_is_the_split_command() {
        assert_eq!(
            classify(" pm  list packages -3 "),
            ShellVerdict::Allowed {
                argv: vec!["pm".into(), "list".into(), "packages".into(), "-3".into()]
            }
        );
    }
}
