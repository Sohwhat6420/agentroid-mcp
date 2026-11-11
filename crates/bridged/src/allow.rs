//! The device allowlist as the daemon builds it at start-up.
//!
//! Nothing is allowed by default. A device is driven only when its hardware serial is named on the
//! command line (`--allow-serial <serial>`, repeatable), in the allow file (one serial per line,
//! `#` starts a comment; default `<state dir>/allow.txt`, or `--allow-file <path>`), or when
//! `--allow-emulators` admits every `emulator-<port>`. `Core::allowed` matches an entry, ignoring
//! case, against the device's hardware serial, its adb serial, or the `adb-<serial>-...` name adb
//! gives a wireless device; it is never a bare substring.
//!
//! An entry is a hardware serial: 6 to 64 letters and digits. Addresses (`192.0.2.9:5555`) are
//! refused because they name a place, not a phone. The file is UTF-8 or UTF-16 (what Windows
//! PowerShell writes), with or without a byte-order mark. A missing default file means an empty
//! list; a missing explicit `--allow-file`, an unreadable file or a bad entry stops the daemon.

use std::path::Path;

pub const FILE_NAME: &str = "allow.txt";
pub const EMULATOR_PREFIX: &str = "emulator-";
pub const MIN_LEN: usize = 6;
pub const MAX_LEN: usize = 64;

/// One entry, or why it is refused.
pub fn check_entry(entry: &str) -> Result<(), String> {
    if entry.len() < MIN_LEN || entry.len() > MAX_LEN {
        return Err(format!("allowlist entry {entry:?} must be {MIN_LEN} to {MAX_LEN} characters: a hardware serial, not a fragment"));
    }
    if !entry.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("allowlist entry {entry:?} must be a hardware serial (letters and digits only, as `adb shell getprop ro.serialno` prints it); use --allow-emulators for emulators"));
    }
    if agentroid_core::gate::is_s24(entry) {
        return Err(format!(
            "allowlist entry {entry:?} names the protected phone, which can never be driven"
        ));
    }
    Ok(())
}

/// File bytes as text: UTF-8 or UTF-16 (little or big endian), a leading byte-order mark dropped.
pub fn decode_file(bytes: &[u8]) -> Result<String, String> {
    let text = if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        let le = bytes[0] == 0xFF;
        if !bytes.len().is_multiple_of(2) {
            return Err("the file looks like UTF-16 but has an odd number of bytes".into());
        }
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16(&units).map_err(|_| "the file is not valid UTF-16".to_string())?
    } else {
        String::from_utf8(bytes.to_vec()).map_err(|_| {
            "the file is not valid UTF-8 (save it as UTF-8 or UTF-16 text)".to_string()
        })?
    };
    Ok(text.trim_start_matches('\u{feff}').to_string())
}

/// The entries of an allow file. Blank lines and `#` comments are skipped (also after an entry).
pub fn parse_file(text: &str) -> Result<Vec<String>, String> {
    let mut out = vec![];
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        check_entry(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        out.push(line.to_string());
    }
    Ok(out)
}

/// The allowlist: command-line entries, then the file's, without repeats (case ignored); `emulator-` last when asked for.
pub fn build(
    flags: &[String],
    file_text: Option<&str>,
    allow_emulators: bool,
) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = vec![];
    let mut add = |entry: String| {
        if !out.iter().any(|e| e.eq_ignore_ascii_case(&entry)) {
            out.push(entry);
        }
    };
    for f in flags {
        check_entry(f.trim())?;
        add(f.trim().to_string());
    }
    if let Some(text) = file_text {
        for e in parse_file(text)? {
            add(e);
        }
    }
    if allow_emulators {
        add(EMULATOR_PREFIX.to_string());
    }
    Ok(out)
}

/// Everything the daemon does to get its list: flags, the file (`explicit` is `--allow-file`, else
/// `<state_dir>/allow.txt` if it exists) and `--allow-emulators`. Any problem is an error.
pub fn load(
    flags: &[String],
    explicit: Option<&Path>,
    state_dir: &Path,
    allow_emulators: bool,
) -> Result<Vec<String>, String> {
    let path = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state_dir.join(FILE_NAME));
    let text = match std::fs::read(&path) {
        Ok(bytes) => Some(decode_file(&bytes).map_err(|e| format!("{}: {e}", path.display()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => None,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    build(flags, text.as_deref(), allow_emulators).map_err(|e| {
        if text.is_some() {
            format!("{}: {e}", path.display())
        } else {
            e
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agentroid-allow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn nothing_is_allowed_by_default() {
        assert_eq!(build(&[], None, false).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn flags_come_before_the_file_without_repeats_and_emulators_last() {
        let got = build(
            &s(&["ABCDEF123", "ZZZZZZ99"]),
            Some("# my phones\nzzzzzz99\nQQQQQQ11  # spare\n\n"),
            true,
        )
        .unwrap();
        assert_eq!(
            got,
            s(&["ABCDEF123", "ZZZZZZ99", "QQQQQQ11", "emulator-"]),
            "a repeat in another case is the same entry"
        );
    }

    #[test]
    fn an_entry_that_could_match_a_class_of_devices_is_refused() {
        for bad in [
            "5",
            "abc",
            "12345",
            "has space1",
            "semi;colon1",
            "emulator-",
            "ABCDEF-",
            "_adb-tls-connect._tcp",
            "192.168.1.",
            "192.0.2.99:5555",
            "--allow-emulators",
            "-ABCDEFG",
            &"x".repeat(65),
        ] {
            assert!(check_entry(bad).is_err(), "{bad:?} must be refused");
            assert!(build(&s(&[bad]), None, false).is_err());
        }
        assert!(check_entry("R3CN90ABCDE").is_ok());
    }

    #[test]
    fn the_protected_phone_cannot_be_named() {
        let err = check_entry("PROTECTED0000EXAMPLE").unwrap_err();
        assert!(err.contains("protected"), "{err}");
        assert!(check_entry("protected0000example").is_err());
    }

    #[test]
    fn a_bad_line_in_the_file_names_its_line_and_fails_the_whole_list() {
        let err = parse_file("GOODSERIAL1\nx\n").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        assert!(build(&[], Some("GOODSERIAL1\nx\n"), false).is_err());
    }

    #[test]
    fn comments_inside_a_line_and_whitespace_are_ignored() {
        assert_eq!(
            parse_file("  GOODSERIAL1   # note\r\n#only a comment\r\n").unwrap(),
            s(&["GOODSERIAL1"])
        );
    }

    #[test]
    fn utf8_with_bom_and_utf16_files_are_read() {
        assert_eq!(
            decode_file(b"\xEF\xBB\xBFGOODSERIAL1\n").unwrap(),
            "GOODSERIAL1\n"
        );
        let le: Vec<u8> = [0xFFu8, 0xFE]
            .into_iter()
            .chain("GOODSERIAL1\r\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_file(&le).unwrap(), "GOODSERIAL1\r\n");
        let be: Vec<u8> = [0xFEu8, 0xFF]
            .into_iter()
            .chain("GOODSERIAL1\n".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        assert_eq!(decode_file(&be).unwrap(), "GOODSERIAL1\n");
        assert!(decode_file(&[0xFF, 0xFE, 0x41]).is_err());
        assert!(decode_file(&[0x80, 0x81]).unwrap_err().contains("UTF-8"));
    }

    #[test]
    fn load_missing_default_file_is_empty_but_a_missing_explicit_file_stops_the_daemon() {
        let d = tmp("missing");
        assert_eq!(load(&[], None, &d, false).unwrap(), Vec::<String>::new());
        let err = load(&[], Some(&d.join("nope.txt")), &d, false).unwrap_err();
        assert!(err.contains("nope.txt"), "{err}");
    }

    #[test]
    fn load_reads_the_default_file_and_an_invalid_one_stops_the_daemon() {
        let d = tmp("file");
        std::fs::write(d.join(FILE_NAME), "GOODSERIAL1\n").unwrap();
        assert_eq!(
            load(&s(&["OTHERSER22"]), None, &d, true).unwrap(),
            s(&["OTHERSER22", "GOODSERIAL1", "emulator-"])
        );
        std::fs::write(d.join(FILE_NAME), "GOODSERIAL1\n192.0.2.9:5555\n").unwrap();
        let err = load(&[], None, &d, false).unwrap_err();
        assert!(err.contains("line 2") && err.contains(FILE_NAME), "{err}");
    }
}
