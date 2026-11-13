//! The file guard (D3b, 10 Nov 2026): `push` and `pull` through the same deny-by-default rules and
//! audit log as every tap.
//!
//! * A phone path must be absolute, with no `..` or `.` segment, no control character and no shell
//!   metacharacter. A `.` is refused rather than dropped: the guard judges the path as written.
//! * **Push** is allowed only into shared storage (`/sdcard`, `/storage/emulated/0`,
//!   `/storage/self/primary`) and `/data/local/tmp`. Anything else (`/system`, `/data/data`,
//!   `/data/system`, `/vendor`, `/dev`, `/proc`...) is blocked: writing there is a wipe or a
//!   persistence trick. Pushing executable code (`.apk`, `.dex`, `.so`, `.sh`, `.jar`) anywhere
//!   needs a human yes (it is harmless until installed or run, but it is the first half of an
//!   install).
//! * **Pull** is allowed only from the same places. Pulling from an app's private data, the system
//!   stores, Wi-Fi or keystore directories is blocked: that is where tokens, sessions and OTP
//!   databases live (OWASP LLM02). Pulling another app's `Android/data` directory needs a human yes.
//!
//! The decision's `target` is the phone path (never file contents).

use crate::screenguard::{Decision, Kind, Verdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Push,
    Pull,
}

impl FileOp {
    pub fn as_str(self) -> &'static str {
        match self {
            FileOp::Push => "push",
            FileOp::Pull => "pull",
        }
    }
}

/// Where files may go to and come from.
const SHARED_ROOTS: [&str; 4] = [
    "/sdcard/",
    "/storage/emulated/0/",
    "/storage/self/primary/",
    "/data/local/tmp/",
];
/// Executable or installable code: pushing it needs a human yes.
const CODE_EXT: [&str; 7] = [".apk", ".apks", ".xapk", ".dex", ".so", ".sh", ".jar"];
/// Metacharacters a path may never carry (the devtools quote paths, but the guard does not rely on it).
const META: [char; 14] = [
    ';', '&', '|', '$', '`', '(', ')', '<', '>', '\\', '\'', '"', '*', '?',
];

fn normalise(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    let mut prev_slash = false;
    for c in p.chars() {
        if c == '/' {
            if !prev_slash {
                out.push(c);
            }
            prev_slash = true;
        } else {
            out.push(c);
            prev_slash = false;
        }
    }
    out
}

fn syntax(p: &str) -> Result<String, String> {
    if p.is_empty() || !p.starts_with('/') {
        return Err("phone path must be absolute".into());
    }
    if p.len() > 1024 {
        return Err("phone path longer than 1024 bytes".into());
    }
    if p.chars().any(char::is_control) {
        return Err("phone path holds a control character".into());
    }
    if let Some(c) = p.chars().find(|c| META.contains(c)) {
        return Err(format!("phone path holds the shell metacharacter {c:?}"));
    }
    if p.split('/').any(|s| s == "..") {
        return Err("phone path holds a `..` segment".into());
    }
    // `/sdcard/./Android/data/<pkg>` is another app's folder by a second spelling. Refused, not
    // rewritten: the guard judges the path the phone will open, so it never repairs one.
    if p.split('/').any(|s| s == ".") {
        return Err("phone path holds a `.` segment".into());
    }
    // with `.` and `..` gone, collapsing repeated slashes leaves only real names (and perhaps a
    // trailing slash); a bare `/` or `//` collapses to `/`, which is outside shared storage
    Ok(normalise(p))
}

fn under_shared(p: &str) -> bool {
    SHARED_ROOTS.iter().any(|r| p.starts_with(r))
}

/// `Android/data/<pkg>/...`, `Android/obb/<pkg>/...` or `Android/media/<pkg>/...` under shared
/// storage: another app's files. Letter case is ignored, as [`under_shared`] ignores it: Android's
/// shared storage is case-insensitive, so `/sdcard/android/DATA/<pkg>` is the same directory, and a
/// path the phone would open must not be judged as a different one.
fn app_private_external(p: &str) -> Option<String> {
    let lower = p.to_ascii_lowercase();
    for root in SHARED_ROOTS {
        if let Some(rest) = lower.strip_prefix(root) {
            let mut it = rest.split('/');
            if it.next() == Some("android") {
                if let (Some(kind), Some(pkg)) = (it.next(), it.next()) {
                    if matches!(kind, "data" | "obb" | "media") && !pkg.is_empty() {
                        // the package as the caller wrote it (same byte offsets: ASCII lowering keeps lengths)
                        let at = root.len() + "android/".len() + kind.len() + 1;
                        return Some(p[at..at + pkg.len()].to_string());
                    }
                }
            }
        }
    }
    None
}

/// Decide a `push` to or a `pull` from `phone_path`.
pub fn classify_file(op: FileOp, phone_path: &str) -> Decision {
    let target = format!("{} {}", op.as_str(), {
        let t: String = phone_path.chars().take(110).collect();
        format!("{t:?}")
    });
    let dec = |verdict: Verdict, kind: Kind, rule: &str, reason: String| Decision {
        verdict,
        kind: Some(kind),
        rule: format!("file:{rule}"),
        reason,
        target: target.clone(),
    };
    let p = match syntax(phone_path) {
        Ok(p) => p,
        Err(why) => return dec(Verdict::Blocked, Kind::Security, "path_syntax", why),
    };
    let lower = p.to_ascii_lowercase();
    if !under_shared(&lower) {
        let (kind, why) = match op {
            FileOp::Push => (
                Kind::FactoryReset,
                "push outside shared storage and /data/local/tmp rewrites system or app state",
            ),
            FileOp::Pull => (
                Kind::Credential,
                "pull outside shared storage and /data/local/tmp reads private app data, tokens or system stores",
            ),
        };
        return dec(Verdict::Blocked, kind, "outside_shared", why.to_string());
    }
    if op == FileOp::Push && CODE_EXT.iter().any(|e| lower.ends_with(e)) {
        return dec(
            Verdict::Ask,
            Kind::Install,
            "code",
            "pushing executable or installable code".to_string(),
        );
    }
    if op == FileOp::Pull {
        if let Some(pkg) = app_private_external(&p) {
            return dec(
                Verdict::Ask,
                Kind::Credential,
                "app_external_data",
                format!("pulling from {pkg}'s Android/data or obb directory"),
            );
        }
    }
    Decision::allowed("file_shared_storage", target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(op: FileOp, p: &str) -> Verdict {
        classify_file(op, p).verdict
    }

    #[test]
    fn shared_storage_and_local_tmp_are_allowed_both_ways() {
        for p in [
            "/sdcard/Download/x.bin",
            "/sdcard/Android/data/com.agentroid.testapp/files/my fixture.bin",
            "/storage/emulated/0/DCIM/Camera/IMG_1.jpg",
            "/storage/self/primary/Pictures/a.png",
            "/data/local/tmp/probe.txt",
            "/sdcard//Download///x.bin",
        ] {
            assert_eq!(v(FileOp::Push, p), Verdict::Allowed, "push {p}");
        }
        for p in [
            "/sdcard/Download/x.bin",
            "/storage/emulated/0/DCIM/Camera/IMG_1.jpg",
            "/data/local/tmp/out.txt",
        ] {
            assert_eq!(v(FileOp::Pull, p), Verdict::Allowed, "pull {p}");
        }
    }

    #[test]
    fn system_and_private_paths_are_blocked() {
        for p in [
            "/system/app/x.apk",
            "/system/etc/hosts",
            "/vendor/x",
            "/data/data/com.x/databases/a.db",
            "/data/user/0/com.x/shared_prefs/a.xml",
            "/data/system/users/0/settings_secure.xml",
            "/data/misc/wifi/WifiConfigStore.xml",
            "/data/misc/keystore/user_0/x",
            "/data/local/x",
            "/data/local/tmp",
            "/dev/block/sda",
            "/proc/1/maps",
            "/sdcard/../data/x",
            "/sdcard/x/../../data/x",
            "relative/x",
            "/sdcard/a\nb",
            "/sdcard/a;rm -rf /",
            "/sdcard/$(id)",
            "",
        ] {
            let d = classify_file(FileOp::Push, p);
            assert_eq!(d.verdict, Verdict::Blocked, "push {p:?}: {d:?}");
            let d = classify_file(FileOp::Pull, p);
            assert_eq!(d.verdict, Verdict::Blocked, "pull {p:?}: {d:?}");
        }
        assert_eq!(
            classify_file(FileOp::Pull, "/data/data/com.x/a.db").kind,
            Some(Kind::Credential)
        );
        assert_eq!(
            classify_file(FileOp::Push, "/system/etc/hosts").kind,
            Some(Kind::FactoryReset)
        );
    }

    #[test]
    fn code_pushes_and_other_apps_external_data_pulls_are_asked() {
        for p in [
            "/sdcard/Download/x.apk",
            "/data/local/tmp/frida-server.so",
            "/sdcard/run.sh",
            "/sdcard/X.APK",
        ] {
            let d = classify_file(FileOp::Push, p);
            assert_eq!(d.verdict, Verdict::Ask, "{p}");
            assert_eq!(d.kind, Some(Kind::Install));
        }
        let d = classify_file(FileOp::Pull, "/sdcard/Android/data/com.whatsapp/files/x");
        assert_eq!(d.verdict, Verdict::Ask);
        assert_eq!(d.kind, Some(Kind::Credential));
        assert_eq!(
            v(
                FileOp::Pull,
                "/storage/emulated/0/Android/obb/com.game/main.obb"
            ),
            Verdict::Ask
        );
        // the top-level Android folder itself is not an app's data
        assert_eq!(v(FileOp::Pull, "/sdcard/Android/x.txt"), Verdict::Allowed);
    }

    #[test]
    fn another_apps_external_data_is_asked_in_any_letter_case() {
        // shared storage is case-insensitive on the phone: each of these opens com.whatsapp's files
        for p in [
            "/sdcard/android/data/com.whatsapp/files/x",
            "/sdcard/ANDROID/DATA/com.whatsapp/files/x",
            "/sdcard/Android/Data/com.whatsapp/files/x",
            "/SDCARD/aNdRoId/data/com.whatsapp/files/x",
            "/storage/emulated/0/android/media/com.whatsapp/WhatsApp/Databases/msgstore.db",
            "/storage/self/primary/Android/OBB/com.game/main.obb",
            "/sdcard//android///data/com.whatsapp/files/x",
        ] {
            let d = classify_file(FileOp::Pull, p);
            assert_eq!(d.verdict, Verdict::Ask, "{p}: {d:?}");
            assert_eq!(d.kind, Some(Kind::Credential), "{p}");
            assert_eq!(d.rule, "file:app_external_data", "{p}");
        }
        // the reason names the package as the caller wrote it
        let d = classify_file(FileOp::Pull, "/sdcard/android/data/Com.Example.App/files/x");
        assert_eq!(
            d.reason,
            "pulling from Com.Example.App's Android/data or obb directory"
        );
        // the nearest allowed neighbours stay allowed: not the Android folder, or no package under it
        for p in [
            "/sdcard/Download/android/data/com.whatsapp/x",
            "/sdcard/android/x.txt",
            "/sdcard/androidx/data/com.whatsapp/x",
            "/sdcard/android/datax/com.whatsapp/x",
        ] {
            assert_eq!(v(FileOp::Pull, p), Verdict::Allowed, "{p}");
        }
    }

    #[test]
    fn single_dot_segments_are_refused_not_rewritten() {
        // a `.` segment names the same directory on the phone, so each of these would open another
        // app's files (or a shared folder by a second spelling); the guard refuses rather than guess
        for p in [
            "/sdcard/./Android/data/com.whatsapp/files/x",
            "/sdcard/Android/./data/com.whatsapp/files/x",
            "/storage/emulated/0/./android/media/com.whatsapp/x",
            "/sdcard/Android/data/./com.whatsapp/files/x",
            "/sdcard//.//Android/data/com.whatsapp/files/x",
            "/sdcard/Download/.",
            "/sdcard/Android/data/com.whatsapp/files/.",
            "/./sdcard/Android/data/com.whatsapp/files/x",
            "./sdcard/Download/x",
        ] {
            for op in [FileOp::Push, FileOp::Pull] {
                let d = classify_file(op, p);
                assert_eq!(d.verdict, Verdict::Blocked, "{op:?} {p:?}: {d:?}");
                assert_eq!(d.rule, "file:path_syntax", "{op:?} {p:?}");
            }
        }
        // names that only start or end with a dot are ordinary names
        for p in [
            "/sdcard/.nomedia",
            "/sdcard/Download/x.",
            "/sdcard/Download/..x",
            "/sdcard/.thumbnails/a.jpg",
        ] {
            assert_eq!(v(FileOp::Pull, p), Verdict::Allowed, "{p}");
        }
    }

    #[test]
    fn the_target_is_the_path() {
        assert_eq!(
            classify_file(FileOp::Pull, "/sdcard/a.txt").target,
            "pull \"/sdcard/a.txt\""
        );
    }
}
