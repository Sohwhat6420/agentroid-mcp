//! `doctor` `diagnose`: the explicit read-only allowlist.
//!
//! * Every allowed form is a **complete argv pattern**, never a prefix. `adb shell` hands the
//!   joined string to the phone's `sh`, so an argument with a shell metacharacter or a quote is
//!   refused before anything is built ([`refuse_metachar`]), and the daemon quotes every element
//!   itself when it builds the remote line ([`shell_line`], [`quote`]).
//! * `dumpsys <service>` takes no further argument; `wm size` and `wm density` take none.
//!   `dumpsys battery set|reset|unplug` and `dumpsys deviceidle force-idle|unforce|step` are named
//!   R2; anything else that is not on the list is R2 by default in Phase 2 and refused.
//! * The protected phone (deny-by-default) answers only the 11 commands of `S24_READS` (`getprop ro.serialno`
//!   and the 2.16 probe subset), each matched as an exact string.
//! * Pure functions here; the daemon arm is in `devtools.rs`.

use agentroid_adb_link::guard::S24_READS;

/// Services `dumpsys <service>` may name (no further argument).
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
/// Flags `pm list packages|features|permissions` may carry.
pub const PM_FLAGS: [&str; 6] = ["-f", "-d", "-e", "-s", "-3", "-U"];
/// Words that change state; any of them in an argv puts it in class R2.
pub const STATE_WORDS: [&str; 10] = [
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
/// Named R2 forms (`dumpsys <service> <word>`).
const NAMED_R2: [(&str, &str); 6] = [
    ("battery", "set"),
    ("battery", "reset"),
    ("battery", "unplug"),
    ("deviceidle", "force-idle"),
    ("deviceidle", "unforce"),
    ("deviceidle", "step"),
];
/// Longest argument accepted (a sanity cap; the plan gives none).
const MAX_ARG: usize = 256;
/// Most arguments in one command.
const MAX_ARGS: usize = 12;

/// Why a command was refused, and in which class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    /// `metachar`, `R2` or `not_on_list` (not on the list: treated as R2 by default in Phase 2).
    pub class: &'static str,
    pub message: String,
}

fn denied(class: &'static str, message: impl Into<String>) -> Denied {
    Denied {
        class,
        message: message.into(),
    }
}

/// A shell metacharacter, a quote, a newline or another control character (the plan's list:
/// `; & | $ ` ( ) < > \ ' "`, newline, `* ? [ ]`).
pub fn is_meta(c: char) -> bool {
    matches!(
        c,
        ';' | '&'
            | '|'
            | '$'
            | '`'
            | '('
            | ')'
            | '<'
            | '>'
            | '\\'
            | '\''
            | '"'
            | '*'
            | '?'
            | '['
            | ']'
    ) || c.is_control()
}

/// Refuse a command line that carries a metacharacter, before it is even split.
pub fn refuse_metachar(raw: &str) -> Result<(), Denied> {
    match raw.chars().find(|c| is_meta(*c)) {
        Some(c) => Err(denied("metachar", format!("diagnose refused: the command contains {c:?}, a shell metacharacter, quote or control character; nothing was sent"))),
        None => Ok(()),
    }
}

/// Split on spaces. The metacharacter refusal has already removed tabs and newlines.
pub fn split(raw: &str) -> Vec<String> {
    raw.split(' ')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// `^[A-Za-z0-9_.:-]+$`, and not starting with `-` (so a key is never read as an option).
fn is_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_ARG
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

fn is_package(s: &str) -> bool {
    let mut cs = s.chars();
    s.len() <= MAX_ARG
        && cs.next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
}

fn is_op(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_ns(s: &str) -> bool {
    matches!(s, "system" | "secure" | "global")
}

/// Is `argv` exactly one of the allowlisted read-only forms?
pub fn matches_allowlist(argv: &[String]) -> bool {
    let a: Vec<&str> = argv.iter().map(String::as_str).collect();
    if a.len() > MAX_ARGS || a.iter().any(|s| s.len() > MAX_ARG) {
        return false;
    }
    match a.as_slice() {
        ["dumpsys", svc] => DUMPSYS_SERVICES.contains(svc),
        ["getprop", key] => is_key(key),
        ["settings", "get", ns, key] => is_ns(ns) && is_key(key),
        ["settings", "list", ns] => is_ns(ns),
        ["pm", "list", kind, flags @ ..] => {
            matches!(*kind, "packages" | "features" | "permissions")
                && flags.len() <= PM_FLAGS.len()
                && flags.iter().all(|f| PM_FLAGS.contains(f))
        }
        ["cmd", "appops", "get", pkg] => is_package(pkg),
        ["cmd", "appops", "get", pkg, op] => is_package(pkg) && is_op(op),
        ["wm", "size"] | ["wm", "density"] => true,
        // bare `cmd display` only prints its usage (probes/readonly.txt lists it); any subcommand is refused
        ["cmd", "display"] => true,
        ["ime", "list", "-s"] => true,
        ["getevent", "-pl"] => true,
        _ => false,
    }
}

/// The class of a command that is not allowed: a named R2 form, R2 by the state words and the
/// "argument after a service" rule, or just not on the list (R2 by default in Phase 2).
fn refusal_class(argv: &[String]) -> Denied {
    let a: Vec<&str> = argv.iter().map(String::as_str).collect();
    let line = a.join(" ");
    if let ["dumpsys", svc, word, ..] = a.as_slice() {
        if NAMED_R2.contains(&(*svc, *word)) {
            return denied("R2", format!("diagnose refused: `{line}` is a named R2 form (it changes device state); only the read-only forms are answered"));
        }
    }
    if a.first() == Some(&"dumpsys") && a.len() > 2 {
        return denied("R2", format!("diagnose refused: `dumpsys <service>` takes no further argument (`{line}` is R2 or higher)"));
    }
    if a.first() == Some(&"wm") && a.len() > 2 {
        return denied("R2", format!("diagnose refused: `{line}` changes the display (R2); only `wm size` and `wm density` with no arguments are answered"));
    }
    let stateful = |w: &str| {
        STATE_WORDS
            .iter()
            .any(|s| w == *s || w.strip_prefix(*s).is_some_and(|r| r.starts_with('-')))
            || w == "unplug"
            || w.starts_with('+')
    };
    if a.iter().any(|w| stateful(w)) {
        return denied(
            "R2",
            format!("diagnose refused: `{line}` contains a state-changing word (R2 or higher)"),
        );
    }
    denied("not_on_list", format!("diagnose refused: `{line}` is not on the read-only allowlist (R2 by default in Phase 2)"))
}

/// Classify a raw `diagnose` string for an ordinary (allowed) device. `Ok` is the argv to run.
pub fn classify(raw: &str) -> Result<Vec<String>, Denied> {
    refuse_metachar(raw)?;
    let argv = split(raw);
    if argv.is_empty() {
        return Err(denied(
            "not_on_list",
            "diagnose refused: the command is empty",
        ));
    }
    if matches_allowlist(&argv) {
        Ok(argv)
    } else {
        Err(refusal_class(&argv))
    }
}

/// The deny-by-default answer for the protected phone: exactly `S24_READS`, matched as whole strings.
pub fn classify_s24(raw: &str) -> Result<Vec<String>, Denied> {
    refuse_metachar(raw)?;
    let argv = split(raw);
    let line = argv.join(" ");
    if !argv.is_empty() && S24_READS.contains(&format!("shell {line}").as_str()) {
        Ok(argv)
    } else {
        Err(denied("s24", format!("diagnose refused: the protected phone is deny-by-default and answers only `getprop ro.serialno` and its approved probes; `{line}` is not one of them")))
    }
}

/// POSIX single-quote one argument: bare when it is made only of `A-Za-z0-9_.:/=+,@%-`, otherwise
/// wrapped in `'...'` with `'` written `'\''`.
pub fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '.' | ':' | '/' | '=' | '+' | ',' | '@' | '%' | '-')
        })
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// The remote command line the daemon builds from an argv.
pub fn shell_line(argv: &[String]) -> String {
    argv.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Vec<String> {
        classify(s).unwrap_or_else(|d| panic!("{s:?} refused: {d:?}"))
    }
    fn no(s: &str) -> Denied {
        classify(s)
            .err()
            .unwrap_or_else(|| panic!("{s:?} was admitted"))
    }

    #[test]
    fn the_read_forms_are_admitted() {
        for s in [
            "dumpsys battery",
            "dumpsys thermalservice",
            "dumpsys display",
            "dumpsys deviceidle",
            "dumpsys input",
            "dumpsys power",
            "dumpsys window",
            "dumpsys activity",
            "dumpsys package",
            "getprop ro.product.model",
            "getprop sys.boot_completed",
            "settings get system screen_off_timeout",
            "settings get secure android_id",
            "settings list global",
            "pm list packages",
            "pm list packages -3 -f",
            "pm list features",
            "pm list permissions -d",
            "cmd appops get com.x.y",
            "cmd appops get com.x.y CAMERA",
            "wm size",
            "wm density",
            "ime list -s",
            "getevent -pl",
            "cmd display",
        ] {
            ok(s);
        }
    }

    #[test]
    fn named_r2_forms_and_trailing_arguments_are_refused_as_r2() {
        for s in [
            "dumpsys battery set level 5",
            "dumpsys battery set",
            "dumpsys battery reset",
            "dumpsys battery unplug",
            "dumpsys deviceidle force-idle",
            "dumpsys deviceidle unforce",
            "dumpsys deviceidle step",
            "dumpsys battery level",
            "dumpsys battery foo",
            "dumpsys deviceidle whitelist +com.x",
            "dumpsys deviceidle whitelist -com.x",
            "settings put system a 1",
            "settings delete global x",
            "cmd appops set com.x CAMERA allow",
            "pm enable com.x",
            "pm disable com.x",
            "pm disable-user com.x",
            "wm size 720x1280",
            "wm density 240",
            "wm size reset",
        ] {
            let d = no(s);
            assert_eq!(d.class, "R2", "{s}: {d:?}");
        }
    }

    #[test]
    fn metacharacters_are_refused_before_anything_else() {
        for s in [
            "pm list packages; pm clear com.x",
            "getprop x && input tap 1 1",
            "getprop $(input tap 1 1)",
            "settings get system a|settings put system a 1",
            "dumpsys battery\ndumpsys power",
            "getprop `id`",
            "getprop a>b",
            "getprop a<b",
            "getprop 'a'",
            "getprop \"a\"",
            "getprop a\\b",
            "getprop a*",
            "getprop a?",
            "getprop a[1]",
            "getprop (a)",
            "dumpsys battery\r",
            "getprop a\tb",
            "getprop a\0",
            "getprop a&b",
        ] {
            assert_eq!(no(s).class, "metachar", "{s:?}");
        }
    }

    #[test]
    fn anything_else_is_not_on_the_list() {
        for s in [
            "",
            "   ",
            "rm -rf /sdcard",
            "ls /sdcard",
            "stat /sdcard",
            "getprop",
            "getprop a b",
            "getprop -h",
            "settings get system",
            "settings get foo bar",
            "settings list",
            "settings list system x",
            "pm list",
            "pm list packages com.x",
            "pm list users",
            "pm clear com.x",
            "dumpsys",
            "dumpsys -l",
            "dumpsys meminfo",
            "dumpsys battery x y",
            "cmd appops get",
            "cmd appops get -h",
            "cmd appops get com.x A B",
            "cmd display get-displays",
            "cmd display help",
            "cmd",
            "ime list",
            "getevent",
            "getevent -pl x",
            "input tap 1 1",
            "am start x",
            "wm",
            "reboot",
        ] {
            let d = no(s);
            assert!(d.class == "not_on_list" || d.class == "R2", "{s:?}: {d:?}");
        }
    }

    #[test]
    fn matching_is_whole_argv_never_a_prefix() {
        assert!(!matches_allowlist(&split("dumpsys battery extra")));
        assert!(!matches_allowlist(&split("getevent -pl extra")));
        assert!(!matches_allowlist(&split("wm size extra")));
        assert!(!matches_allowlist(&split("ime list -s x")));
    }

    #[test]
    fn s24_answers_only_its_ten() {
        for s in [
            "getprop ro.serialno",
            "getprop ro.product.model",
            "wm size",
            "wm density",
            "pm list features",
            "dumpsys battery",
            "dumpsys thermalservice",
            "getprop ro.build.version.sdk",
        ] {
            classify_s24(s).unwrap_or_else(|d| panic!("{s}: {d:?}"));
        }
        for s in [
            "getprop ro.product.manufacturer",
            "getprop ro.serial",
            "dumpsys window",
            // the screen lock's own reader may read the call state (14 Nov 2026); doctor may not
            "dumpsys telephony.registry",
            "settings get system a",
            "pm list packages",
            "ime list -s",
            "getevent -pl",
            "dumpsys battery set level 5",
            "getprop ro.serialno x",
            "",
            "getprop ro.serialno; id",
        ] {
            assert!(classify_s24(s).is_err(), "{s:?}");
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("ro.serialno"), "ro.serialno");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(quote("/sdcard/My Files/x;y"), "'/sdcard/My Files/x;y'");
        assert_eq!(
            shell_line(&ok("pm list packages -3")),
            "pm list packages -3"
        );
    }
}
