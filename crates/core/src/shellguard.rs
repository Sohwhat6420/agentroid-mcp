//! The shell guard (D3b, 10 Nov 2026): the screen guard's twin for raw `adb shell` lines, so that
//! `run_script`'s `agentroid.shell` and the devtools reach the same deny-by-default rules and the
//! same audit log as every tap.
//!
//! Order, like the screen guard: blocked families first, then the ones that need a human yes, then
//! the exact read-only allowlist (2.4's list, shared with the sandbox's own classifier), then the
//! default. **The default is `asked`**, never `allowed`: a line that matches nothing is a line the
//! guard does not understand, and only a human may let it through.
//!
//! * **Blocked** (no approval can lower it): factory reset and wipes (`am broadcast ...
//!   MASTER_CLEAR`, `recovery --wipe_data`, `bmgr wipe`, `wipe`, `dd`, `mkfs`, `rm -r` on a root);
//!   security settings (`settings put secure|global ...`, `locksettings`, `dpm`, `svc wifi|data
//!   disable`, `setprop persist.*`, `pm grant` of dangerous permissions, `appops set` of
//!   accessibility or overlay); shell escapes and privilege (`su`, `sh -c`, `busybox`, `run-as`,
//!   `service call`, `sqlite3`); synthetic input (`input tap|text|keyevent|swipe`), because it
//!   bypasses the screen guard; reads of secret stores (`cat /data/data/...`, `content query
//!   content://sms`), because they hand over OTPs and tokens (OWASP LLM02).
//! * **Asked**: uninstall, clear, disable or force-stop an app (`pm uninstall|clear|disable*|hide|
//!   suspend`, `cmd package uninstall`, `am force-stop|kill`); install (`pm install*`); grants
//!   (`pm grant|revoke`, `cmd appops set`); `settings put system|delete`; `am start|startservice|
//!   broadcast` (judged by component, action and data with the screen guard's launch rules);
//!   `svc power`, `reboot`; `content insert|update|delete`; `sms send`, `telecom`; anything unknown.
//! * **Allowed**: the 2.4 read-only list, exactly (`dumpsys <svc>`, `getprop <key>`, `settings get`,
//!   `pm list ...`, `wm size`...).
//!
//! Metacharacters, quotes and control characters are refused before anything is split (the
//! sandbox's rule), so the argv the guard judges is the argv the phone would run.

use crate::gate::Call;
use crate::screenguard::{decide_launch, Decision, Finding, Kind, Verdict};

/// 2.4: `;` `&` `|` `$` `` ` `` `(` `)` `<` `>` `\` `'` `"`, `*?[]`.
pub const METACHARS: [char; 16] = [
    ';', '&', '|', '$', '`', '(', ')', '<', '>', '\\', '\'', '"', '*', '?', '[', ']',
];
const MAX_CMD: usize = 1024;

const DUMPSYS_SERVICES: [&str; 9] = [
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
const SETTINGS_NAMESPACES: [&str; 3] = ["system", "secure", "global"];
const PM_LIST_WHAT: [&str; 3] = ["packages", "features", "permissions"];
const PM_LIST_FLAGS: [&str; 6] = ["-f", "-d", "-e", "-s", "-3", "-U"];

/// Permissions whose grant weakens the phone or hands over secrets.
const DANGEROUS_PERMISSIONS: [&str; 10] = [
    "android.permission.BIND_ACCESSIBILITY_SERVICE",
    "android.permission.BIND_DEVICE_ADMIN",
    "android.permission.SYSTEM_ALERT_WINDOW",
    "android.permission.WRITE_SECURE_SETTINGS",
    "android.permission.REQUEST_INSTALL_PACKAGES",
    "android.permission.INSTALL_PACKAGES",
    "android.permission.READ_SMS",
    "android.permission.RECEIVE_SMS",
    "android.permission.BIND_NOTIFICATION_LISTENER_SERVICE",
    "android.permission.PACKAGE_USAGE_STATS",
];
/// App ops whose `set ... allow` weakens the phone.
const DANGEROUS_OPS: [&str; 6] = [
    "SYSTEM_ALERT_WINDOW",
    "ACCESS_NOTIFICATIONS",
    "BIND_ACCESSIBILITY_SERVICE",
    "REQUEST_INSTALL_PACKAGES",
    "GET_USAGE_STATS",
    "WRITE_SETTINGS",
];
/// Content URIs that hold OTPs, tokens or private data (LLM02).
const SECRET_CONTENT: [&str; 7] = [
    "content://sms",
    "content://mms",
    "content://call_log",
    "content://contacts",
    "content://com.android.contacts",
    "content://telephony",
    "content://settings/secure",
];
/// Paths whose contents are secrets or whose change is a wipe.
const SECRET_PATHS: [&str; 7] = [
    "/data/data/",
    "/data/user/",
    "/data/user_de/",
    "/data/system/",
    "/data/misc/",
    "/data/system_ce/",
    "/data/system_de/",
];
const ROOTS: [&str; 12] = [
    "/",
    "/data",
    "/data/",
    "/system",
    "/system/",
    "/sdcard",
    "/sdcard/",
    "/storage",
    "/storage/",
    "/storage/emulated/0",
    "/storage/emulated/0/",
    "/vendor",
];

fn is_key(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

fn is_package(s: &str) -> bool {
    s.len() <= 255
        && s.contains('.')
        && s.split('.').all(|seg| {
            seg.starts_with(|c: char| c.is_ascii_alphabetic())
                && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

fn is_op(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':'))
}

/// Exact match against the 2.4 read-only list (the same list as the sandbox's classifier).
pub fn is_read_only(argv: &[&str]) -> bool {
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

fn finding(kind: Kind, rule: &'static str, evidence: String) -> Finding {
    Finding {
        kind,
        rule,
        evidence,
    }
}

fn shown(argv: &[&str]) -> String {
    let s = argv.join(" ");
    let t: String = s.chars().take(120).collect();
    format!("shell {t:?}")
}

/// The option value after `flag` in an `am` argv (`-n`, `-a`, `-d`, `--es`...).
fn opt<'a>(argv: &[&'a str], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|a| *a == flag)
        .and_then(|i| argv.get(i + 1))
        .copied()
}

/// What an `am start|startservice|broadcast|start-activity` line would reach, judged with the
/// screen guard's launch rules (component, action, data).
fn am_findings(argv: &[&str]) -> Vec<Finding> {
    let mut fs = Vec::new();
    let joined = argv.join(" ").to_ascii_uppercase();
    if joined.contains("MASTER_CLEAR") || joined.contains("FACTORY_RESET") {
        fs.push(finding(
            Kind::FactoryReset,
            "shell_am",
            "MASTER_CLEAR / FACTORY_RESET intent wipes the phone".into(),
        ));
    }
    let component = opt(argv, "-n").unwrap_or("");
    let action = opt(argv, "-a").map(str::to_string);
    let data = opt(argv, "-d").map(str::to_string);
    let call = Call::StartActivity {
        component: component.to_string(),
        action,
        data,
    };
    if let Some(d) = decide_launch(&call) {
        if let Some(k) = d.kind {
            fs.push(finding(k, "shell_am", d.reason));
        }
    }
    fs
}

/// Blocked families: nothing can lower these.
fn blocked(argv: &[&str]) -> Option<Finding> {
    let a0 = argv[0];
    let lower: Vec<String> = argv.iter().map(|a| a.to_ascii_lowercase()).collect();
    let l: Vec<&str> = lower.iter().map(String::as_str).collect();
    let f = |k: Kind, why: &str| Some(finding(k, "shell", why.to_string()));
    // factory reset and wipes
    if matches!(a0, "wipe" | "recovery" | "dd" | "fastboot" | "flash_image")
        || (a0 == "bmgr" && l.get(1) == Some(&"wipe"))
        || a0.starts_with("mkfs")
        || a0.starts_with("make_ext4fs")
    {
        return f(Kind::FactoryReset, "wipes or reflashes storage");
    }
    if a0 == "rm"
        && l[1..]
            .iter()
            .any(|a| a.starts_with("-r") || a.starts_with("-fr"))
    {
        if l[1..].iter().any(|a| ROOTS.contains(a)) {
            return f(Kind::FactoryReset, "recursive delete of a storage root");
        }
        if l[1..]
            .iter()
            .any(|a| a.starts_with("/data/") || a.starts_with("/system"))
        {
            return f(
                Kind::FactoryReset,
                "recursive delete under /data or /system",
            );
        }
    }
    if a0 == "reboot"
        && l.get(1).is_some_and(|m| {
            matches!(
                *m,
                "recovery" | "bootloader" | "fastboot" | "edl" | "download"
            )
        })
    {
        return f(
            Kind::FactoryReset,
            "reboots into recovery or the bootloader",
        );
    }
    if a0 == "am"
        && l.iter().any(|a| {
            a.contains("master_clear") || a.contains("masterclear") || a.contains("factory_reset")
        })
    {
        return f(Kind::FactoryReset, "MASTER_CLEAR intent wipes the phone");
    }
    // security settings and grants
    if a0 == "settings"
        && l.get(1) == Some(&"put")
        && matches!(l.get(2), Some(&"secure") | Some(&"global"))
    {
        return f(
            Kind::Security,
            "settings put secure|global changes a security setting",
        );
    }
    if matches!(
        a0,
        "locksettings" | "dpm" | "device_policy" | "keystore" | "vdc"
    ) {
        return f(
            Kind::Security,
            "changes the lock, device policy or keystore",
        );
    }
    if a0 == "svc"
        && matches!(
            l.get(1),
            Some(&"wifi") | Some(&"data") | Some(&"bluetooth") | Some(&"nfc") | Some(&"usb")
        )
        && l.get(2)
            .is_some_and(|m| matches!(*m, "disable" | "setFunctions" | "setfunctions"))
    {
        return f(
            Kind::Security,
            "turns off the link the laptop drives the phone over",
        );
    }
    if matches!(a0, "setprop" | "resetprop") {
        return f(Kind::Security, "sets a system property");
    }
    if a0 == "pm"
        && matches!(l.get(1), Some(&"grant"))
        && argv
            .get(3)
            .is_some_and(|p| DANGEROUS_PERMISSIONS.contains(p))
    {
        return f(
            Kind::Security,
            "grants a permission that weakens the phone or reads SMS",
        );
    }
    if (a0 == "appops" && l.get(1) == Some(&"set"))
        || (a0 == "cmd" && l.get(1) == Some(&"appops") && l.get(2) == Some(&"set"))
    {
        let op = argv
            .iter()
            .find(|a| DANGEROUS_OPS.iter().any(|d| a.eq_ignore_ascii_case(d)));
        if op.is_some() {
            return f(Kind::Security, "sets an app op that weakens the phone");
        }
    }
    if a0 == "pm"
        && l.get(1)
            .is_some_and(|m| m.starts_with("set-") || *m == "trim-caches")
        && l.iter()
            .any(|a| a.contains("install") || a.contains("verif"))
    {
        return f(Kind::Security, "changes install verification");
    }
    // shell escapes and privilege
    if matches!(
        a0,
        "su" | "sh"
            | "bash"
            | "mksh"
            | "busybox"
            | "toybox"
            | "run-as"
            | "exec"
            | "eval"
            | "nohup"
            | "env"
            | "xargs"
    ) {
        return f(Kind::Security, "a shell escape or privilege change");
    }
    if a0 == "service" && l.get(1) == Some(&"call") {
        return f(
            Kind::Security,
            "service call reaches system services directly (ISms, IPhone...)",
        );
    }
    if matches!(a0, "sqlite3" | "toybox") {
        return f(Kind::Security, "opens databases directly");
    }
    // synthetic input bypasses the screen guard
    if matches!(a0, "input" | "sendevent" | "monkey") {
        return f(Kind::Security, "synthetic input bypasses the screen guard");
    }
    if a0 == "uiautomator" && l.get(1) == Some(&"runtest") {
        return f(Kind::Security, "runs a test APK");
    }
    // secret stores (LLM02)
    if matches!(
        a0,
        "cat"
            | "head"
            | "tail"
            | "cp"
            | "ls"
            | "find"
            | "grep"
            | "strings"
            | "base64"
            | "xxd"
            | "od"
            | "tar"
            | "cut"
            | "awk"
            | "sed"
            | "more"
            | "less"
    ) && l[1..]
        .iter()
        .any(|a| SECRET_PATHS.iter().any(|s| a.starts_with(s)))
    {
        return f(
            Kind::Credential,
            "reads or copies an app's private data (tokens, sessions, OTP stores)",
        );
    }
    if a0 == "content"
        && l.get(1) == Some(&"query")
        && l.iter()
            .any(|a| SECRET_CONTENT.iter().any(|s| a.starts_with(s)))
    {
        return f(
            Kind::Credential,
            "reads SMS, call log, contacts or secure settings",
        );
    }
    if a0 == "logcat" && l.iter().any(|a| a.contains("sms") || a.contains("otp")) {
        return f(Kind::Credential, "filters the log for SMS or OTP text");
    }
    None
}

/// Families that need a human yes.
fn asked(argv: &[&str]) -> Option<Finding> {
    let a0 = argv[0];
    let lower: Vec<String> = argv.iter().map(|a| a.to_ascii_lowercase()).collect();
    let l: Vec<&str> = lower.iter().map(String::as_str).collect();
    let f = |k: Kind, why: &str| Some(finding(k, "shell", why.to_string()));
    let sub = l.get(1).copied().unwrap_or("");
    let cmd_sub = if a0 == "cmd" {
        (
            l.get(1).copied().unwrap_or(""),
            l.get(2).copied().unwrap_or(""),
        )
    } else {
        ("", "")
    };
    if a0 == "pm" || cmd_sub.0 == "package" {
        let verb = if a0 == "pm" { sub } else { cmd_sub.1 };
        match verb {
            "uninstall" | "clear" | "disable" | "disable-user" | "disable-until-used" | "hide"
            | "suspend" | "unsuspend" | "enable" | "unhide" | "reset-permissions"
            | "set-home-activity" | "set-installer" | "remove-user" | "create-user"
            | "trim-caches" => {
                return f(
                    Kind::Destructive,
                    "removes, clears, disables or changes an app or user",
                )
            }
            "install" | "install-create" | "install-write" | "install-commit"
            | "install-existing" | "install-abandon" => return f(Kind::Install, "installs an app"),
            "grant" | "revoke" | "set-permission-flags" | "clear-permission-flags" => {
                return f(Kind::Permission, "changes an app's permissions")
            }
            _ => {}
        }
    }
    if a0 == "am" {
        match sub {
            "force-stop" | "kill" | "kill-all" | "crash" | "clear-debug-app" | "set-debug-app"
            | "clear-watch-heap" => return f(Kind::Destructive, "stops or kills an app"),
            "start"
            | "start-activity"
            | "startservice"
            | "start-service"
            | "start-foreground-service"
            | "startforegroundservice"
            | "broadcast"
            | "send-trim-memory" => {
                let fs = am_findings(argv);
                return Some(
                    fs.into_iter()
                        .max_by_key(|f| f.kind.verdict())
                        .unwrap_or_else(|| {
                            finding(
                                Kind::Permission,
                                "shell_am",
                                "starts a component outside the screen guard".into(),
                            )
                        }),
                );
            }
            _ => {}
        }
    }
    if a0 == "settings" && (sub == "put" || sub == "delete" || sub == "reset") {
        return f(Kind::Destructive, "changes a system setting");
    }
    if a0 == "appops" || cmd_sub.0 == "appops" {
        let verb = if a0 == "appops" { sub } else { cmd_sub.1 };
        if matches!(verb, "set" | "reset" | "write-settings") {
            return f(Kind::Permission, "changes an app op");
        }
    }
    if a0 == "svc" || a0 == "reboot" || a0 == "stop" || a0 == "start" {
        return f(Kind::Destructive, "power, radio or service control");
    }
    if a0 == "content" && matches!(sub, "insert" | "update" | "delete" | "call") {
        return f(Kind::Destructive, "writes a content provider");
    }
    if matches!(a0, "sms" | "telecom") || cmd_sub.0 == "telecom" || cmd_sub.0 == "phone" {
        return f(
            if a0 == "sms" { Kind::Send } else { Kind::Call },
            "sends a message or places a call",
        );
    }
    if a0 == "rm" || a0 == "rmdir" || a0 == "mv" || a0 == "truncate" || a0 == "shred" {
        return f(Kind::Destructive, "deletes or moves files");
    }
    if matches!(
        a0,
        "cmd" | "ime" | "wm" | "dumpsys" | "notification" | "bmgr" | "logcat"
    ) {
        return f(
            Kind::Permission,
            "a privileged command that is not on the read-only list",
        );
    }
    None
}

/// Classify one `adb shell` line. The `target` of the decision is the line itself, truncated (it is
/// a command, never typed text).
pub fn classify_shell(line: &str) -> Decision {
    let blocked_dec = |why: String| Decision {
        verdict: Verdict::Blocked,
        kind: Some(Kind::Security),
        rule: "shell_syntax".into(),
        reason: why,
        target: format!("shell ({} chars)", line.chars().count()),
    };
    if line.len() > MAX_CMD {
        return blocked_dec(format!("command longer than {MAX_CMD} bytes"));
    }
    if let Some(c) = line.chars().find(|c| METACHARS.contains(c)) {
        return blocked_dec(format!(
            "shell metacharacter {c:?} refused before anything is sent"
        ));
    }
    if let Some(c) = line.chars().find(|c| c.is_control() || !c.is_ascii()) {
        return blocked_dec(format!(
            "control or non-ASCII character {c:?} refused before anything is sent"
        ));
    }
    let argv: Vec<&str> = line.split(' ').filter(|a| !a.is_empty()).collect();
    if argv.is_empty() {
        return blocked_dec("empty command".into());
    }
    let target = shown(&argv);
    let from = |f: Finding| Decision {
        verdict: f.kind.verdict(),
        kind: Some(f.kind),
        rule: format!("shell:{}", f.kind.as_str()),
        reason: format!("{} ({})", f.kind.as_str(), f.evidence),
        target: target.clone(),
    };
    if let Some(f) = blocked(&argv) {
        return from(f);
    }
    if is_read_only(&argv) {
        return Decision::allowed("shell_read_only", target);
    }
    if let Some(f) = asked(&argv) {
        let mut d = from(f);
        // a family that only needs a human yes never lowers below Ask (am_findings may say Blocked)
        d.verdict = d.verdict.max(Verdict::Ask);
        return d;
    }
    Decision {
        verdict: Verdict::Ask,
        kind: Some(Kind::Permission),
        rule: "shell:unknown".into(),
        reason: "not on the read-only list and not a known family: needs a human yes".into(),
        target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(line: &str) -> Verdict {
        classify_shell(line).verdict
    }
    fn k(line: &str) -> Option<Kind> {
        classify_shell(line).kind
    }

    #[test]
    fn the_read_only_list_is_allowed_and_nothing_near_it() {
        for cmd in [
            "dumpsys battery",
            "getprop ro.serialno",
            "settings get system screen_brightness",
            "settings list secure",
            "pm list packages -3 -f",
            "cmd appops get com.android.settings CAMERA",
            "wm size",
            "getevent -pl",
            "  getprop   ro.serialno  ",
        ] {
            assert_eq!(v(cmd), Verdict::Allowed, "{cmd}");
        }
        for cmd in [
            "dumpsys",
            "dumpsys meminfo",
            "getprop",
            "settings get other a",
            "pm list packages -f -f",
            "wm size 720x1280",
            "ime set com.x/.Ime",
        ] {
            assert_ne!(v(cmd), Verdict::Allowed, "{cmd}");
        }
    }

    #[test]
    fn wipes_and_factory_resets_are_blocked() {
        for cmd in [
            "am broadcast -a android.intent.action.MASTER_CLEAR",
            "am broadcast -a android.intent.action.MASTER_CLEAR -n android/com.android.server.MasterClearReceiver",
            "am start -n com.android.settings/.MasterClear",
            "recovery --wipe_data",
            "wipe data",
            "bmgr wipe com.x",
            "rm -rf /sdcard",
            "rm -rf /data/",
            "rm -r /storage/emulated/0",
            "rm -rf /data/data/com.x",
            "dd if=/dev/zero of=/dev/block/sda",
            "mkfs.ext4 /dev/block/sda1",
            "reboot recovery",
            "reboot bootloader",
        ] {
            assert_eq!(v(cmd), Verdict::Blocked, "{cmd}");
            assert_eq!(k(cmd), Some(Kind::FactoryReset), "{cmd}");
        }
    }

    #[test]
    fn security_settings_grants_and_escapes_are_blocked() {
        for cmd in [
            "settings put secure enabled_accessibility_services com.x/.Svc",
            "settings put secure install_non_market_apps 1",
            "settings put global adb_enabled 0",
            "settings put global package_verifier_enable 0",
            "locksettings clear --old 1234",
            "dpm set-device-owner com.x/.Admin",
            "svc wifi disable",
            "svc data disable",
            "setprop persist.sys.usb.config none",
            "pm grant com.x android.permission.WRITE_SECURE_SETTINGS",
            "pm grant com.x android.permission.READ_SMS",
            "appops set com.x SYSTEM_ALERT_WINDOW allow",
            "cmd appops set com.x ACCESS_NOTIFICATIONS allow",
            "su -c id",
            "sh -c id",
            "run-as com.x cat databases/x.db",
            "service call isms 5 s16 com.android.mms s16 +6500000000 s16 null s16 hi",
            "sqlite3 /data/data/com.x/databases/x.db",
            "input tap 600 2400",
            "input text hello",
            "input keyevent 66",
            "sendevent /dev/input/event2 1 330 1",
            "monkey -p com.x 100",
        ] {
            assert_eq!(v(cmd), Verdict::Blocked, "{cmd}");
            assert_eq!(k(cmd), Some(Kind::Security), "{cmd}");
        }
    }

    #[test]
    fn secret_reads_are_blocked_as_credential() {
        for cmd in [
            "cat /data/data/com.whatsapp/shared_prefs/x.xml",
            "cp /data/user/0/com.x/databases/a.db /sdcard/a.db",
            "content query --uri content://sms/inbox",
            "content query --uri content://com.android.contacts/contacts",
            "ls /data/misc/wifi",
            "logcat -d -s SmsReceiver",
        ] {
            assert_eq!(v(cmd), Verdict::Blocked, "{cmd}");
            assert_eq!(k(cmd), Some(Kind::Credential), "{cmd}");
        }
    }

    #[test]
    fn app_changes_installs_grants_and_unknown_lines_are_asked() {
        let cases: [(&str, Kind); 14] = [
            ("pm uninstall com.x", Kind::Destructive),
            ("pm uninstall -k --user 0 com.x", Kind::Destructive),
            ("cmd package uninstall com.x", Kind::Destructive),
            ("pm clear com.x", Kind::Destructive),
            ("pm disable-user --user 0 com.x", Kind::Destructive),
            ("am force-stop com.x", Kind::Destructive),
            ("pm install -r /sdcard/x.apk", Kind::Install),
            ("pm grant com.x android.permission.CAMERA", Kind::Permission),
            ("cmd appops set com.x CAMERA allow", Kind::Permission),
            (
                "settings put system screen_brightness 10",
                Kind::Destructive,
            ),
            ("settings delete system a", Kind::Destructive),
            ("svc power stayon true", Kind::Destructive),
            (
                "content delete --uri content://media/external/images/media/5",
                Kind::Destructive,
            ),
            ("some_unknown_binary --flag", Kind::Permission),
        ];
        for (cmd, kind) in cases {
            let d = classify_shell(cmd);
            assert_eq!(d.verdict, Verdict::Ask, "{cmd}: {d:?}");
            assert_eq!(d.kind, Some(kind), "{cmd}");
        }
    }

    #[test]
    fn am_start_is_judged_by_the_launch_rules() {
        // a plain activity: asked (it leaves the screen guard's sight)
        assert_eq!(
            v("am start -n com.android.settings/.Settings"),
            Verdict::Ask
        );
        // the launch rules say blocked for a banking app or a device-admin action: stays blocked
        assert_eq!(
            v("am start -n com.dbs.sg.dbsmbanking/.Main"),
            Verdict::Blocked
        );
        assert_eq!(
            k("am start -n com.dbs.sg.dbsmbanking/.Main"),
            Some(Kind::Banking)
        );
        assert_eq!(
            v("am start -a android.app.action.ADD_DEVICE_ADMIN -n com.x/.Admin"),
            Verdict::Blocked
        );
        assert_eq!(
            k("am start -a android.intent.action.SENDTO -d smsto:+6500000000"),
            Some(Kind::Send)
        );
        assert_eq!(
            v("am start -a android.intent.action.VIEW -d tel:*2767*3855%23"),
            Verdict::Blocked
        );
    }

    #[test]
    fn metacharacters_and_control_characters_are_blocked_before_splitting() {
        for cmd in [
            "pm list packages; pm clear com.x",
            "getprop x && input tap 1 1",
            "getprop $(input tap 1 1)",
            "getprop ro.serialno\ninput tap 1 1",
            "getprop 'ro.serialno'",
            "dumpsys battery > /sdcard/x",
            "getprop ro.serialnö",
            "",
            "   ",
        ] {
            let d = classify_shell(cmd);
            assert_eq!(d.verdict, Verdict::Blocked, "{cmd:?}");
            assert_eq!(d.rule, "shell_syntax", "{cmd:?}");
        }
    }

    #[test]
    fn the_target_is_the_command_not_typed_text() {
        let d = classify_shell("pm uninstall com.x");
        assert_eq!(d.target, "shell \"pm uninstall com.x\"");
        assert_eq!(d.rule, "shell:destructive");
    }
}
