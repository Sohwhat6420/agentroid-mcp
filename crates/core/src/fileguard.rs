//! The file guard (D3b, 10 Nov 2026): `push` and `pull` through the same deny-by-default rules and
//! audit log as every tap.
//!
//! * A phone path must be absolute, with no `..` or `.` segment, no control character and no shell
//!   metacharacter. A `.` is refused rather than dropped: the guard judges the path as written. An
//!   invisible character is refused where the guard decides (the storage root, and an `Android`
//!   folder and the one after it, where `data` / `obb` / `media` is matched) and allowed elsewhere.
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

/// Invisible format characters (zero-width spaces and joiners, bidi controls, the byte-order mark,
/// soft hyphen, variation selectors, the Mongolian free variation selectors, tag characters and
/// similar). Case-folding storage ignores Unicode's default-ignorable characters when it matches
/// names, so `/sdcard/Andro\u{200B}id/data/<pkg>` opens another app's `Android/data` folder. One is
/// refused where the guard decides ([`invisible_where_decided`]).
pub fn is_invisible(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x034F | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F
        | 0xFEFF | 0xFFA0 | 0xFFF0..=0xFFFB | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        | 0xE0000..=0xE0FFF)
}

/// The first invisible character in the part of `p` the guard decides on: the shared-storage root
/// and, when the folder after it reads as `Android`, that folder and the next one (where `data` /
/// `obb` / `media` is matched). Other segments are names the guard does not judge, so real names
/// keep their joiners and soft hyphens (a Persian name with a ZWNJ, the family emoji with its
/// ZWJs, `/sdcard/Download/<name>`). A path outside shared storage is checked whole (it is refused
/// anyway).
pub fn invisible_where_decided(p: &str) -> Option<char> {
    let n = normalise(p);
    let lower = n.to_ascii_lowercase();
    let decided: Vec<&str> = match SHARED_ROOTS.iter().find(|r| lower.starts_with(*r)) {
        // ASCII lowering keeps byte lengths, so the caller's own spelling starts at the same offset
        Some(r) => {
            let mut it = n[r.len()..].split('/');
            match it.next() {
                // folds_to drops what storage may ignore, so `Andr\u{200D}oid` still reads as Android
                Some(first) if folds_to(first, "android") => {
                    [Some(first), it.next()].into_iter().flatten().collect()
                }
                _ => Vec::new(),
            }
        }
        None => vec![n.as_str()],
    };
    decided
        .iter()
        .flat_map(|s| s.chars())
        .find(|&c| is_invisible(c))
}

/// A segment as case-folding storage may read it: without its invisible characters.
fn visible(seg: &str) -> String {
    seg.chars().filter(|&c| !is_invisible(c)).collect()
}

/// A `.` or `..` segment, also when it is spelt with invisible characters (`.\u{AD}.`), which
/// case-folding storage may ignore.
pub fn is_dot_segment(seg: &str) -> bool {
    let v = visible(seg);
    v == "." || v == ".."
}

/// The guard's syntax check alone: `Ok` when [`classify_file`] would not refuse `p` as
/// `file:path_syntax`. `pull_new` uses it to pass over a file name the guard would refuse.
pub fn path_syntax_ok(p: &str) -> Result<(), String> {
    syntax(p).map(|_| ())
}

/// Whether case-folding storage could open path segment `seg` as the ASCII name `word`: the same
/// letters in any case, perhaps with characters it ignores (variation selectors and the like) or
/// letters that fold to ASCII (the Kelvin sign to `k`, the long s to `s`). Any other non-ASCII letter
/// makes it a different name. Errs towards a match: a false match only asks a human.
fn folds_to(seg: &str, word: &str) -> bool {
    if seg.is_ascii() {
        return seg.eq_ignore_ascii_case(word);
    }
    let skeleton: String = seg
        .chars()
        .filter_map(|c| match c {
            '\u{212A}' => Some('k'),
            '\u{017F}' => Some('s'),
            c if c.is_ascii() => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    skeleton == word
}

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
    if let Some(c) = invisible_where_decided(p) {
        return Err(format!(
            "phone path holds the invisible character U+{:04X} where the guard decides (the storage root, or an Android folder and the one after it)",
            c as u32
        ));
    }
    if let Some(c) = p.chars().find(|c| META.contains(c)) {
        return Err(format!("phone path holds the shell metacharacter {c:?}"));
    }
    // compared without invisible characters: `.\u{AD}.` may still be `..` to case-folding storage
    if p.split('/').any(|s| visible(s) == "..") {
        return Err("phone path holds a `..` segment".into());
    }
    // `/sdcard/./Android/data/<pkg>` is another app's folder by a second spelling. Refused, not
    // rewritten: the guard judges the path the phone will open, so it never repairs one.
    if p.split('/').any(|s| visible(s) == ".") {
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
///
/// The `Android` and `data` / `obb` / `media` segments are matched as case-folding storage would
/// match them ([`folds_to`]), so a spelling with an ignored or folding character is still caught.
fn app_private_external(p: &str) -> Option<String> {
    let lower = p.to_ascii_lowercase();
    for root in SHARED_ROOTS {
        if lower.starts_with(root) {
            // ASCII lowering keeps byte lengths, so the caller's own spelling starts at the same offset
            let mut it = p[root.len()..].split('/');
            if it.next().is_some_and(|s| folds_to(s, "android")) {
                if let (Some(kind), Some(pkg)) = (it.next(), it.next()) {
                    let kind_ok = ["data", "obb", "media"].iter().any(|w| folds_to(kind, w));
                    if kind_ok && !pkg.is_empty() {
                        return Some(pkg.to_string());
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
    // without invisible characters, which a file name may now hold: `x.ap\u{AD}k` may open as `x.apk`;
    // and with the long s and the Kelvin sign folded, as case-folding storage folds them (`x.\u{17F}h`)
    let shown: String = visible(&lower)
        .chars()
        .map(|c| match c {
            '\u{017F}' => 's',
            '\u{212A}' => 'k',
            c => c,
        })
        .collect();
    if op == FileOp::Push && CODE_EXT.iter().any(|e| shown.ends_with(e)) {
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
    fn invisible_characters_are_refused_where_the_guard_decides() {
        // each would open com.whatsapp's files on case-folding storage, which ignores them
        for p in [
            "/sdcard/Andro\u{200B}id/data/com.whatsapp/files/x",
            "/sdcard/Andr\u{200D}oid/data/x",
            "/sdcard/Android/da\u{200D}ta/com.whatsapp/files/x",
            "/sdcard/\u{FEFF}Android/data/com.whatsapp/files/x",
            "/sdcard/Android\u{2060}/obb/com.game/main.obb",
            "/sdcard/Android/media\u{00AD}/com.whatsapp/x",
            "/sdcard//Andro\u{200B}id///data/com.whatsapp/x",
            "/storage/emulated/0/andr\u{200C}oid/DATA/com.whatsapp/x",
            "/data/local/tmp/Android/d\u{AD}ata/com.x/y",
            // in the root itself: not shared storage, refused whole
            "/storage/emulated/0\u{200B}/Download/x",
            // a dot segment spelt with an invisible character is still a dot segment
            "/sdcard/Download/.\u{AD}./.\u{AD}./x",
            "/sdcard/Download/\u{200B}./x",
            // also with a variation selector or a Mongolian free variation selector
            "/sdcard/Download/.\u{FE0F}./x",
            "/sdcard/Download/.\u{180B}./x",
            "/sdcard/Download/.\u{180F}/x",
            // a variation selector in the Android folder or the one after it
            "/sdcard/Andr\u{FE0F}oid/data/com.whatsapp/files/x",
            "/sdcard/Android/d\u{FE00}ata/com.whatsapp/files/x",
        ] {
            for op in [FileOp::Pull, FileOp::Push] {
                let d = classify_file(op, p);
                assert_eq!(d.verdict, Verdict::Blocked, "{op:?} {p:?}: {d:?}");
                assert_eq!(d.rule, "file:path_syntax", "{op:?} {p:?}");
            }
        }
        // ordinary non-ASCII names stay allowed
        for p in [
            "/sdcard/Download/caf\u{E9}.jpg",
            "/sdcard/DCIM/\u{732B}.png",
            "/sdcard/Pictures/\u{2764}\u{FE0F}.png",
        ] {
            assert_eq!(v(FileOp::Pull, p), Verdict::Allowed, "{p:?}");
        }
    }

    #[test]
    fn real_names_may_hold_joiners_and_soft_hyphens() {
        // a Persian name with a ZWNJ, the family emoji (ZWJs), a soft hyphen, a tag character
        for p in [
            "/sdcard/Download/\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}.pdf",
            "/sdcard/Download/\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}.jpg",
            "/sdcard/Download/re\u{AD}port.pdf",
            "/sdcard/Download/a\u{E0041}b.txt",
            "/storage/emulated/0/DCIM/Camera/\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}.jpg",
            "/sdcard/\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}.pdf",
        ] {
            for op in [FileOp::Pull, FileOp::Push] {
                let d = classify_file(op, p);
                assert_eq!(d.verdict, Verdict::Allowed, "{op:?} {p:?}: {d:?}");
            }
            assert!(path_syntax_ok(p).is_ok(), "{p:?}");
        }
        // under another app's folder the name does not change the answer: still asked
        let d = classify_file(
            FileOp::Pull,
            "/sdcard/Android/data/com.whatsapp/files/x\u{202E}gpj.exe",
        );
        assert_eq!(d.verdict, Verdict::Ask, "{d:?}");
        // and the audit target shows the character escaped, not as a reversal
        assert!(d.target.contains("\\u{202e}"), "{}", d.target);
        // a code extension with an ignored character in it is still code
        let d = classify_file(FileOp::Push, "/sdcard/Download/x.ap\u{AD}k");
        assert_eq!((d.verdict, d.kind), (Verdict::Ask, Some(Kind::Install)));
        // as is one with a variation selector, or with a letter that folds to ASCII
        for p in [
            "/sdcard/Download/x.ap\u{FE0F}k",
            "/sdcard/Download/x.ap\u{180B}k",
            "/sdcard/Download/x.\u{17F}h",
            "/sdcard/Download/x.ap\u{212A}",
        ] {
            let d = classify_file(FileOp::Push, p);
            assert_eq!(
                (d.verdict, d.kind),
                (Verdict::Ask, Some(Kind::Install)),
                "{p:?}: {d:?}"
            );
        }
    }

    #[test]
    fn letters_that_storage_folds_or_ignores_still_name_android_data() {
        // a variation selector is now refused outright (see the invisible-character test); another
        // non-ASCII mark the guard cannot place errs towards a match: still asked
        for p in [
            "/sdcard/Andro\u{301}id/data/com.whatsapp/files/x",
            "/sdcard/Android/da\u{301}ta/com.whatsapp/files/x",
            "/sdcard/Android/ob\u{301}b/com.game/main.obb",
            "/storage/emulated/0/Android/media\u{301}/com.whatsapp/x",
        ] {
            let d = classify_file(FileOp::Pull, p);
            assert_eq!(d.verdict, Verdict::Ask, "{p:?}: {d:?}");
            assert_eq!(d.rule, "file:app_external_data", "{p:?}");
        }
        assert!(folds_to("\u{17F}dcard", "sdcard") && folds_to("\u{212A}", "k"));
        // a different letter is a different folder: allowed, as before
        for p in [
            "/sdcard/Andr\u{F6}id/data/com.whatsapp/x",
            "/sdcard/\u{410}ndroid/data/com.whatsapp/x",
        ] {
            assert_eq!(v(FileOp::Pull, p), Verdict::Allowed, "{p:?}");
        }
        // the reason still names the package as written
        let d = classify_file(FileOp::Pull, "/sdcard/Andro\u{301}id/data/Com.X/f");
        assert_eq!(
            d.reason,
            "pulling from Com.X's Android/data or obb directory"
        );
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
