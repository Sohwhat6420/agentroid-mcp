//! task 2.16: the capability probe. Runs the read-only probe list through a `Runner`
//! (5 s per command), parses the answers into a [`Profile`] (`schemas/profile.json`) and writes
//! `profiles/<serial>.json`.
//!
//! * A `read_only` device runs only `probes/readonly-s24.txt`, the subset reviewed and
//!   approved as safe. For the protected phone the adb guard enforces the same list a second time.
//! * A command that fails, is refused or times out leaves its field at the safe value; the tier is
//!   computed from the fields afterwards, so it drops by itself. An unknown phone, or one that
//!   could not be probed, gets T3 (safe slow mode).
//! * The protected phone's profile, and every `read_only` device's, goes to `profiles/local/` (git-ignored).
//! * No network. `oem_notes` stays empty.
//!   TODO(2.16 follow-up): the dontkillmyapp cache step. A separate, manual step fetches
//!   `/api/v2/<vendor>.json` (CC-BY-4.0, credited in `profiles/README.md`) once, commits the cache
//!   under `profiles/`, and seeds `oem_notes` from it. The probe itself never fetches.
//! * The tier ladder is an ASSUMPTION (the full tier spec was not available): see
//!   [`tier_of`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agentroid_adb_link::guard::S24_SERIAL;
use agentroid_adb_link::runner::Runner;
use agentroid_core::gate::DeviceMode;
use serde::{Deserialize, Serialize};

/// Per-command timeout.
pub const PROBE_TIMEOUT_MS: u64 = 5_000;
/// The full read-only list, compiled in.
pub const FULL_LIST: &str = include_str!("../../../probes/readonly.txt");
/// The subset approved for the protected phone (and so for every `read_only` device), compiled in.
pub const S24_LIST: &str = include_str!("../../../probes/readonly-s24.txt");
const SKINS: &str = include_str!("../../../probes/skins.txt");
const SETTINGS_KEYS: &str = include_str!("../../../probes/settings-keys.txt");

/// The T0 to T3 ladder. T0 is the most capable, T3 the safest and slowest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    T0,
    T1,
    T2,
    T3,
}

impl Tier {
    /// Higher is more capable: T0 = 3 … T3 = 0.
    pub fn capability(self) -> u8 {
        match self {
            Tier::T0 => 3,
            Tier::T1 => 2,
            Tier::T2 => 1,
            Tier::T3 => 0,
        }
    }
}

/// `overlay_box` of the schema. The probe does not measure it; it only round-trips.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayBox {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    pub rotation: u32,
}

/// One device profile: serialises to `schemas/profile.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub serial: String,
    pub maker: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_box: Option<OverlayBox>,
    pub sdk: u32,
    pub skin: String,
    pub skin_version: String,
    pub touchscreen_node: Option<String>,
    pub settings_keys: Vec<String>,
    pub refresh_modes: Vec<u32>,
    pub virtual_display: bool,
    pub ime_switchable: bool,
    pub fgs_types: Vec<String>,
    pub appfunctions: bool,
    pub wifi_adb: bool,
    pub tier: Tier,
    #[serde(default)]
    pub oem_notes: Vec<String>,
    #[serde(default)]
    pub probed: bool,
}

/// The serial a profile carries when `ro.serialno` could not be read. Never written to disk.
pub const UNKNOWN_SERIAL: &str = "unknown";

impl Profile {
    /// Every field at its safe value: what a phone with no probe gets.
    pub fn safe() -> Self {
        Profile {
            serial: UNKNOWN_SERIAL.into(),
            maker: String::new(),
            model: String::new(),
            overlay_box: None,
            sdk: 29,
            skin: String::new(),
            skin_version: String::new(),
            touchscreen_node: None,
            settings_keys: vec![],
            refresh_modes: vec![],
            virtual_display: false,
            ime_switchable: false,
            fgs_types: vec![],
            appfunctions: false,
            wifi_adb: false,
            tier: Tier::T3,
            oem_notes: vec![],
            probed: false,
        }
    }
}

/// A probe or write problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeError(pub String);

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "probe: {}", self.0)
    }
}

impl std::error::Error for ProbeError {}

/// Characters a probe line may use: letters, digits, `.`, `_`, `-` and single spaces. Anything a
/// shell would treat specially is refused, so every line is one exact argv.
fn plain_line(l: &str) -> bool {
    !l.is_empty()
        && l == l.trim()
        && !l.contains("  ")
        && l.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
}

/// Parse a probe list file: `#` comments and blank lines are skipped, every other line must be plain.
pub fn command_list(text: &str) -> Result<Vec<String>, ProbeError> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let l = raw;
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        if !plain_line(l) {
            return Err(ProbeError(format!(
                "probe line {l:?} is not one plain exact command"
            )));
        }
        out.push(l.to_string());
    }
    Ok(out)
}

/// The commands a device in `mode` may be probed with. `read_only` gets the approved subset only.
pub fn commands_for(mode: DeviceMode) -> Vec<String> {
    let text = match mode {
        DeviceMode::ReadOnly => S24_LIST,
        DeviceMode::Full | DeviceMode::Helper => FULL_LIST,
    };
    command_list(text).expect("compiled-in probe list is plain (a test checks it)")
}

struct SkinRow {
    maker: &'static str,
    skin: &'static str,
    key: &'static str,
}

fn skin_rows() -> Vec<SkinRow> {
    SKINS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut p = l.split('|').map(str::trim);
            Some(SkinRow {
                maker: p.next()?,
                skin: p.next()?,
                key: p.next()?,
            })
        })
        .collect()
}

/// Run each command once; keep the stdout of the ones that exited 0. Failures, refusals and
/// timeouts are simply absent.
fn run_all(runner: &dyn Runner, target: &str, cmds: &[String]) -> HashMap<String, String> {
    let mut got = HashMap::new();
    for cmd in cmds {
        let argv = vec![
            "-s".to_string(),
            target.to_string(),
            "shell".to_string(),
            cmd.clone(),
        ];
        if let Ok(o) = runner.run(&argv, PROBE_TIMEOUT_MS) {
            if o.ok() {
                got.insert(cmd.clone(), o.stdout);
            }
        }
    }
    got
}

fn prop(got: &HashMap<String, String>, key: &str) -> Option<String> {
    let v = got.get(&format!("getprop {key}"))?.trim().to_string();
    (!v.is_empty()).then_some(v)
}

fn plain_serial(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_event_node(p: &str) -> bool {
    p.strip_prefix("/dev/input/event")
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// The touchscreen is the device that reports multitouch X and is flagged `INPUT_PROP_DIRECT`
/// (a touchpad is `INPUT_PROP_POINTER` and does not count).
fn touchscreen_node(getevent: &str) -> Option<String> {
    for block in getevent.split("add device ").skip(1) {
        let Some(first) = block.lines().next() else {
            continue;
        };
        let Some((_, path)) = first.split_once(':') else {
            continue;
        };
        let path = path.trim();
        if block.contains("ABS_MT_POSITION_X")
            && block.contains("INPUT_PROP_DIRECT")
            && is_event_node(path)
        {
            return Some(path.to_string());
        }
    }
    None
}

/// Every `fps=<number>` in `dumpsys display`, rounded, de-duplicated, ascending.
fn refresh_modes(display: &str) -> Vec<u32> {
    let mut modes: Vec<u32> = display
        .split("fps=")
        .skip(1)
        .filter_map(|rest| {
            let num: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            num.parse::<f64>().ok()
        })
        .map(|f| f.round() as u32)
        .filter(|f| *f >= 1)
        .collect();
    modes.sort_unstable();
    modes.dedup();
    modes
}

/// Names (never values) of the settings keys listed in `probes/settings-keys.txt` that are present.
fn settings_keys_present(got: &HashMap<String, String>) -> Vec<String> {
    let wanted: Vec<&str> = SETTINGS_KEYS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let mut keys: Vec<String> = ["system", "secure", "global"]
        .iter()
        .filter_map(|ns| got.get(&format!("settings list {ns}")))
        .flat_map(|out| out.lines())
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim()))
        .filter(|k| wanted.contains(k))
        .map(str::to_string)
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// ASSUMPTION: no command in the list prints foreground-service types, so they come from the API
/// level (types that exist at that level and need no extra declaration to *name*).
fn fgs_types(sdk: u32) -> Vec<String> {
    let mut t = vec!["connectedDevice", "mediaProjection"];
    if sdk >= 30 {
        t.extend(["camera", "microphone"]);
    }
    t.into_iter().map(String::from).collect()
}

fn has_appfunctions(features: &str) -> bool {
    features
        .lines()
        .filter_map(|l| l.trim().strip_prefix("feature:"))
        .any(|f| {
            f.to_ascii_lowercase()
                .replace('_', "")
                .contains("appfunction")
        })
}

/// The ladder (ASSUMPTION: the narrowest reading; the full tier spec was not available):
/// * T3 safe slow mode: not probed (no SDK read), maker not in `probes/skins.txt` (unknown phone),
///   or no touchscreen node (it cannot be driven by direct input);
/// * T2: probed, known maker, has a touchscreen, but lacks one of virtual display (API 30+),
///   a switchable IME, or Wi-Fi adb;
/// * T1: all three present;
/// * T0: T1 plus AppFunctions (API 36 and the feature in `pm list features`).
pub fn tier_of(p: &Profile, known_maker: bool) -> Tier {
    if !p.probed || !known_maker {
        return Tier::T3;
    }
    if p.touchscreen_node.is_none() {
        return Tier::T3;
    }
    if !(p.virtual_display && p.ime_switchable && p.wifi_adb) {
        return Tier::T2;
    }
    if p.appfunctions {
        Tier::T0
    } else {
        Tier::T1
    }
}

/// Parse the answers (command to stdout; absent means the command failed) into a profile.
pub fn parse_profile(got: &HashMap<String, String>) -> Profile {
    let mut p = Profile::safe();
    if let Some(s) = prop(got, "ro.serialno").filter(|s| plain_serial(s)) {
        p.serial = s;
    }
    let sdk = prop(got, "ro.build.version.sdk")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|s| *s >= 29);
    let rows = skin_rows();
    let mut maker = prop(got, "ro.product.manufacturer");
    p.model = prop(got, "ro.product.model").unwrap_or_default();
    for row in &rows {
        let Some(v) = prop(got, row.key) else {
            continue;
        };
        if maker
            .as_deref()
            .is_none_or(|m| m.eq_ignore_ascii_case(row.maker))
        {
            if maker.is_none() {
                maker = Some(row.maker.to_string());
            }
            p.skin = row.skin.to_string();
            p.skin_version = v;
            break;
        }
    }
    p.maker = maker.unwrap_or_default();
    let known_maker = rows.iter().any(|r| r.maker.eq_ignore_ascii_case(&p.maker));
    let Some(sdk) = sdk else {
        // No API level: the phone was not really probed. Everything else stays safe.
        p.tier = Tier::T3;
        return p;
    };
    p.sdk = sdk;
    p.probed = true;
    p.touchscreen_node = got.get("getevent -pl").and_then(|o| touchscreen_node(o));
    p.settings_keys = settings_keys_present(got);
    p.refresh_modes = got
        .get("dumpsys display")
        .map(|o| refresh_modes(o))
        .unwrap_or_default();
    p.virtual_display = sdk >= 30;
    p.ime_switchable = got.get("ime list -s").is_some_and(|o| {
        o.lines()
            .map(str::trim)
            .filter(|l| l.contains('/') && !l.contains(' '))
            .count()
            >= 2
    });
    p.fgs_types = fgs_types(sdk);
    p.appfunctions = sdk >= 36
        && got
            .get("pm list features")
            .is_some_and(|o| has_appfunctions(o));
    p.wifi_adb = sdk >= 30 && p.settings_keys.iter().any(|k| k == "adb_wifi_enabled");
    p.tier = tier_of(&p, known_maker);
    p
}

/// Probe the device behind adb target `target` (a `-s` selector value) and parse the answers.
/// The runner is trusted with nothing: it is called only with `-s <target> shell <line>` for lines
/// of the list that `mode` allows.
pub fn probe(runner: &dyn Runner, target: &str, mode: DeviceMode) -> Profile {
    let got = run_all(runner, target, &commands_for(mode));
    parse_profile(&got)
}

/// Where a profile goes: `<dir>/<serial>.json`, or `<dir>/local/<serial>.json` for the protected phone and for
/// every `read_only` device.
pub fn profile_path(
    profiles_dir: &Path,
    serial: &str,
    mode: DeviceMode,
) -> Result<PathBuf, ProbeError> {
    if serial == UNKNOWN_SERIAL || !plain_serial(serial) {
        return Err(ProbeError(format!(
            "no usable serial ({serial:?}): not writing a profile"
        )));
    }
    let local = mode == DeviceMode::ReadOnly || serial.eq_ignore_ascii_case(S24_SERIAL);
    let dir = if local {
        profiles_dir.join("local")
    } else {
        profiles_dir.to_path_buf()
    };
    Ok(dir.join(format!("{serial}.json")))
}

/// Write the profile to its path (creating directories) and return the path.
pub fn write_profile(
    profiles_dir: &Path,
    p: &Profile,
    mode: DeviceMode,
) -> Result<PathBuf, ProbeError> {
    let path = profile_path(profiles_dir, &p.serial, mode)?;
    let json = serde_json::to_string_pretty(p).map_err(|e| ProbeError(e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ProbeError(format!("{}: {e}", parent.display())))?;
    }
    std::fs::write(&path, json + "\n")
        .map_err(|e| ProbeError(format!("{}: {e}", path.display())))?;
    Ok(path)
}

/// Probe, then write.
pub fn probe_and_write(
    runner: &dyn Runner,
    target: &str,
    mode: DeviceMode,
    profiles_dir: &Path,
) -> Result<(Profile, PathBuf), ProbeError> {
    let p = probe(runner, target, mode);
    let path = write_profile(profiles_dir, &p, mode)?;
    Ok((p, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentroid_adb_link::guard::{GuardedRunner, S24_READS};
    use agentroid_adb_link::runner::{RunError, RunOutput};
    use serde_json::Value;
    use std::cell::RefCell;
    use std::collections::HashSet;

    const POCO: &str = include_str!("../fixtures/probe/synthetic-poco-x6-pro.txt");
    const UNKNOWN_MAKER: &str = include_str!("../fixtures/probe/synthetic-unknown-maker.txt");
    const S24: &str = include_str!("../fixtures/probe/synthetic-s24-subset.txt");
    const SCHEMA: &str = include_str!("../../../schemas/profile.json");
    const TARGET: &str = "192.0.2.99:40000";

    /// Fake adb: answers from a fixture (`### <command>` sections); a command with no section
    /// fails; commands in `timeouts` time out. Records every call.
    struct Fake {
        sections: HashMap<String, String>,
        timeouts: HashSet<String>,
        serial: Option<String>,
        calls: RefCell<Vec<(Vec<String>, u64)>>,
    }

    impl Fake {
        fn new(fixture: &str) -> Self {
            Fake {
                sections: parse_fixture(fixture),
                timeouts: HashSet::new(),
                serial: None,
                calls: RefCell::new(vec![]),
            }
        }
        fn seen(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|(a, _)| a.last().cloned().unwrap_or_default())
                .collect()
        }
    }

    fn parse_fixture(text: &str) -> HashMap<String, String> {
        let mut map: HashMap<String, String> = HashMap::new();
        let mut cur: Option<String> = None;
        for line in text.lines() {
            if let Some(cmd) = line.strip_prefix("### ") {
                map.insert(cmd.trim().to_string(), String::new());
                cur = Some(cmd.trim().to_string());
            } else if let Some(c) = &cur {
                let e = map.get_mut(c).unwrap();
                e.push_str(line);
                e.push('\n');
            }
        }
        map
    }

    impl Runner for Fake {
        fn run(&self, a: &[String], t: u64) -> Result<RunOutput, RunError> {
            self.calls.borrow_mut().push((a.to_vec(), t));
            let cmd = a.last().cloned().unwrap_or_default();
            if self.timeouts.contains(&cmd) {
                return Err(RunError::Io(format!("adb timed out after {t} ms")));
            }
            if cmd == "getprop ro.serialno" {
                if let Some(s) = &self.serial {
                    return Ok(RunOutput {
                        code: Some(0),
                        stdout: format!("{s}\n"),
                        stderr: String::new(),
                    });
                }
            }
            match self.sections.get(&cmd) {
                Some(out) => Ok(RunOutput {
                    code: Some(0),
                    stdout: out.clone(),
                    stderr: String::new(),
                }),
                None => Ok(RunOutput {
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "not found".into(),
                }),
            }
        }
    }

    fn expected_s24() -> Vec<String> {
        [
            "getprop ro.serialno",
            "getprop ro.build.version.sdk",
            "getprop ro.build.version.release",
            "getprop ro.product.model",
            "getprop ro.build.version.oneui",
            "wm size",
            "wm density",
            "pm list features",
            "dumpsys battery",
            "dumpsys thermalservice",
            "dumpsys power",
        ]
        .map(String::from)
        .to_vec()
    }

    fn expected_full() -> Vec<String> {
        [
            "getprop ro.serialno",
            "getprop ro.build.version.sdk",
            "getprop ro.build.version.release",
            "getprop ro.product.manufacturer",
            "getprop ro.product.model",
            "getprop ro.build.version.oneui",
            "getprop ro.mi.os.version.name",
            "getevent -pl",
            "settings list system",
            "settings list secure",
            "settings list global",
            "cmd display",
            "dumpsys display",
            "pm list features",
            "dumpsys deviceidle",
            "cmd appops get dev.agentroid.helper",
            "cmd appops get com.android.shell",
            "dumpsys input",
            "wm size",
            "wm density",
            "ime list -s",
            "dumpsys battery",
            "dumpsys thermalservice",
            "dumpsys power",
        ]
        .map(String::from)
        .to_vec()
    }

    // ---- the lists ----

    #[test]
    fn the_lists_are_exact_and_plain() {
        assert_eq!(command_list(FULL_LIST).unwrap(), expected_full());
        assert_eq!(command_list(S24_LIST).unwrap(), expected_s24());
        // the protected phone subset is a subset of the full list, and of S24_READS
        let full = command_list(FULL_LIST).unwrap();
        for c in command_list(S24_LIST).unwrap() {
            assert!(full.contains(&c), "{c}");
            assert!(S24_READS.contains(&format!("shell {c}").as_str()), "{c}");
        }
        // appops packages are fixed lines of the file, plain package names
        for c in full.iter().filter(|c| c.starts_with("cmd appops")) {
            let pkg = c.strip_prefix("cmd appops get ").expect("appops get only");
            assert!(
                !pkg.is_empty()
                    && pkg
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.'),
                "{c}"
            );
        }
        // the vendor table's version keys are probed
        for row in skin_rows() {
            assert!(
                full.contains(&format!("getprop {}", row.key)),
                "{}",
                row.key
            );
        }
    }

    #[test]
    fn list_lines_with_shell_metacharacters_are_refused() {
        for bad in [
            "getprop ro.serialno; reboot",
            "wm size && id",
            "dumpsys battery | cat",
            "getprop $X",
            "wm size ",
            " wm size",
            "wm  size",
            "ls `id`",
            "a\\b",
            "cmd 'x'",
        ] {
            assert!(command_list(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            command_list("# c\n\nwm size\n").unwrap(),
            vec!["wm size".to_string()]
        );
    }

    // ---- fixtures ----

    #[test]
    fn every_fixture_parses_and_yields_schema_valid_json() {
        for (name, text) in [("poco", POCO), ("unknown", UNKNOWN_MAKER), ("s24", S24)] {
            let sections = parse_fixture(text);
            assert!(sections.len() >= 5, "{name}");
            let p = parse_profile(&sections);
            let v = serde_json::to_value(&p).unwrap();
            validate(&serde_json::from_str(SCHEMA).unwrap(), &v)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        // the schema fixtures: valid-* pass and deserialise, invalid-* fail my validator
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/fixtures/profile");
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let mut n = 0;
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if name.starts_with("valid-") {
                validate(&schema, &v).unwrap_or_else(|e| panic!("{name}: {e}"));
                let p: Profile =
                    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
                // round-trips; `oem_notes` is optional in the schema and always written (empty) by us
                let mut back = serde_json::to_value(&p).unwrap();
                if v.get("oem_notes").is_none() {
                    back.as_object_mut().unwrap().remove("oem_notes");
                }
                assert_eq!(back, v, "{name} round-trips");
            } else {
                assert!(validate(&schema, &v).is_err(), "{name} should be invalid");
            }
            n += 1;
        }
        assert_eq!(n, 6);
    }

    // ---- the profile ----

    #[test]
    fn poco_fixture_gives_t1_and_the_expected_fields() {
        let fake = Fake::new(POCO);
        let p = probe(&fake, TARGET, DeviceMode::Full);
        assert_eq!(p.tier, Tier::T1);
        assert_eq!(p.serial, "PLACEHOLDER-POCO-SERIAL");
        assert_eq!(
            (p.maker.as_str(), p.model.as_str()),
            ("Xiaomi", "POCO X6 Pro")
        );
        assert_eq!(
            (p.skin.as_str(), p.skin_version.as_str()),
            ("HyperOS", "OS2.0")
        );
        assert_eq!(p.sdk, 35);
        assert_eq!(
            p.touchscreen_node.as_deref(),
            Some("/dev/input/event5"),
            "not the touchpad decoy"
        );
        assert_eq!(
            p.settings_keys,
            [
                "accessibility_enabled",
                "adb_wifi_enabled",
                "min_refresh_rate",
                "peak_refresh_rate",
                "screen_brightness",
                "stay_on_while_plugged_in"
            ]
        );
        assert_eq!(p.refresh_modes, vec![60, 120]);
        assert!(p.virtual_display && p.ime_switchable && p.wifi_adb && p.probed);
        assert!(!p.appfunctions);
        assert_eq!(
            p.fgs_types,
            ["connectedDevice", "mediaProjection", "camera", "microphone"]
        );
        assert!(p.oem_notes.is_empty());
        // exactly the full list, in order, each as one `-s <target> shell <line>` with 5 s
        assert_eq!(fake.seen(), expected_full());
        for (a, t) in fake.calls.borrow().iter() {
            assert_eq!(&a[..3], ["-s", TARGET, "shell"]);
            assert_eq!(a.len(), 4);
            assert_eq!(*t, 5_000);
        }
    }

    #[test]
    fn a_fake_unknown_device_gets_t3() {
        // nothing answers at all
        let silent = Fake::new("");
        let p = probe(&silent, TARGET, DeviceMode::Full);
        assert_eq!(p.tier, Tier::T3);
        assert!(!p.probed);
        assert_eq!(p, Profile::safe());
        // a capable phone from an unlisted maker is still T3 ...
        let p = probe(&Fake::new(UNKNOWN_MAKER), TARGET, DeviceMode::Full);
        assert_eq!(p.tier, Tier::T3);
        assert!(
            p.probed
                && p.touchscreen_node.is_some()
                && p.virtual_display
                && p.ime_switchable
                && p.wifi_adb
        );
        // ... and the maker is the only thing holding it there
        let listed = UNKNOWN_MAKER.replace("Acme Handsets", "Xiaomi");
        assert_eq!(
            probe(&Fake::new(&listed), TARGET, DeviceMode::Full).tier,
            Tier::T1
        );
    }

    #[test]
    fn a_timed_out_command_leaves_the_safe_value_and_a_lower_tier() {
        let base = probe(&Fake::new(POCO), TARGET, DeviceMode::Full);
        let mut fake = Fake::new(POCO);
        fake.timeouts.insert("ime list -s".into());
        let p = probe(&fake, TARGET, DeviceMode::Full);
        assert!(!p.ime_switchable, "safe value");
        assert_eq!(p.tier, Tier::T2);
        assert!(p.tier.capability() < base.tier.capability());
        // every command was still tried, with the 5 s timeout
        assert_eq!(fake.seen(), expected_full());
        // no touchscreen answer: T3
        let mut fake = Fake::new(POCO);
        fake.timeouts.insert("getevent -pl".into());
        let p = probe(&fake, TARGET, DeviceMode::Full);
        assert_eq!(p.touchscreen_node, None);
        assert_eq!(p.tier, Tier::T3);
        // no API level: everything is the safe value
        let mut fake = Fake::new(POCO);
        fake.timeouts.insert("getprop ro.build.version.sdk".into());
        let p = probe(&fake, TARGET, DeviceMode::Full);
        assert_eq!(
            (
                p.tier,
                p.probed,
                p.sdk,
                p.virtual_display,
                p.touchscreen_node.clone()
            ),
            (Tier::T3, false, 29, false, None)
        );
        // a failed settings read: the keys are absent, Wi-Fi adb off, tier drops
        let mut fake = Fake::new(POCO);
        fake.timeouts.insert("settings list global".into());
        let p = probe(&fake, TARGET, DeviceMode::Full);
        assert!(!p.wifi_adb && p.tier == Tier::T2);
    }

    #[test]
    fn appfunctions_need_api_36_and_the_feature() {
        let mut f = parse_fixture(POCO);
        f.insert(
            "pm list features".into(),
            "feature:android.software.app_functions\n".into(),
        );
        assert!(!parse_profile(&f).appfunctions, "API 35");
        f.insert("getprop ro.build.version.sdk".into(), "36\n".into());
        let p = parse_profile(&f);
        assert!(p.appfunctions);
        assert_eq!(p.tier, Tier::T0);
        f.insert(
            "pm list features".into(),
            "feature:android.hardware.touchscreen\n".into(),
        );
        assert!(!parse_profile(&f).appfunctions, "no feature");
    }

    // ---- read_only ----

    #[test]
    fn a_read_only_device_is_probed_with_only_the_s24_subset() {
        let fake = Fake::new(POCO); // a phone that could answer far more
        let p = probe(&fake, TARGET, DeviceMode::ReadOnly);
        assert_eq!(fake.seen(), expected_s24());
        for (a, t) in fake.calls.borrow().iter() {
            assert_eq!(&a[..3], ["-s", TARGET, "shell"]);
            assert_eq!(*t, 5_000);
        }
        // nothing from the full list's extra commands, so those fields are safe values
        assert_eq!(p.touchscreen_node, None);
        assert!(
            p.settings_keys.is_empty()
                && p.refresh_modes.is_empty()
                && !p.ime_switchable
                && !p.wifi_adb
        );
        assert_eq!(p.tier, Tier::T3);
        // helper and full run the whole list
        for mode in [DeviceMode::Full, DeviceMode::Helper] {
            let f = Fake::new(POCO);
            probe(&f, TARGET, mode);
            assert_eq!(f.seen(), expected_full(), "{mode:?}");
        }
    }

    #[test]
    fn the_guard_enforces_the_subset_again_for_the_s24() {
        let mut inner = Fake::new(S24);
        inner.serial = Some(S24_SERIAL.to_string());
        let guarded = GuardedRunner::new(inner);
        let p = probe(&guarded, TARGET, DeviceMode::ReadOnly);
        assert!(p.probed, "the 10 approved commands got through the guard");
        assert_eq!(p.serial, S24_SERIAL);
        assert_eq!(
            (p.maker.as_str(), p.skin.as_str(), p.skin_version.as_str()),
            ("Samsung", "One UI", "80000")
        );
        assert_eq!(p.model, "SM-SYNTHETIC");
        assert!(p.appfunctions && p.virtual_display && p.touchscreen_node.is_none());
        assert_eq!(p.tier, Tier::T3);
        // the full list through the same guard: every command outside the subset is refused
        let subset = expected_s24();
        for c in command_list(FULL_LIST).unwrap() {
            let argv = ["-s", TARGET, "shell", c.as_str()]
                .map(String::from)
                .to_vec();
            let r = guarded.run(&argv, 1000);
            if subset.contains(&c) {
                assert!(r.is_ok(), "{c}");
            } else {
                assert!(matches!(r, Err(RunError::Refused(_))), "{c}: {r:?}");
            }
        }
    }

    #[test]
    fn the_s24_profile_path_is_under_profiles_local() {
        let dir = Path::new("profiles");
        for mode in [DeviceMode::Full, DeviceMode::Helper, DeviceMode::ReadOnly] {
            let p = profile_path(dir, S24_SERIAL, mode).unwrap();
            assert_eq!(
                p,
                Path::new("profiles")
                    .join("local")
                    .join(format!("{S24_SERIAL}.json")),
                "{mode:?}"
            );
            assert!(p.starts_with(Path::new("profiles").join("local")));
        }
        // lower-case spelling too
        assert!(
            profile_path(dir, &S24_SERIAL.to_lowercase(), DeviceMode::Full)
                .unwrap()
                .starts_with("profiles/local")
        );
        // an ordinary phone is not local; any read_only device is
        assert_eq!(
            profile_path(dir, "ABC123", DeviceMode::Full).unwrap(),
            Path::new("profiles").join("ABC123.json")
        );
        assert!(profile_path(dir, "ABC123", DeviceMode::ReadOnly)
            .unwrap()
            .starts_with("profiles/local"));
        // no usable serial, no file; no path tricks
        for bad in [UNKNOWN_SERIAL, "", "../x", "a/b", "a\\b", "x y"] {
            assert!(profile_path(dir, bad, DeviceMode::Full).is_err(), "{bad:?}");
        }
        // and the ignore rule is in .gitignore
        let ignore = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.gitignore"),
        )
        .unwrap();
        assert!(ignore.lines().any(|l| l.trim() == "profiles/local/"));
    }

    #[test]
    fn profiles_are_written_to_disk_and_validate() {
        let dir = std::env::temp_dir().join(format!("agentroid-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (p, path) = probe_and_write(&Fake::new(POCO), TARGET, DeviceMode::Full, &dir).unwrap();
        assert_eq!(path, dir.join("PLACEHOLDER-POCO-SERIAL.json"));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        validate(&serde_json::from_str(SCHEMA).unwrap(), &v).unwrap();
        assert_eq!(serde_json::from_value::<Profile>(v).unwrap(), p);
        let (_, local) =
            probe_and_write(&Fake::new(S24), TARGET, DeviceMode::ReadOnly, &dir).unwrap();
        assert_eq!(local, dir.join("local").join("PLACEHOLDER-S24-SERIAL.json"));
        // a phone that never answered writes nothing
        assert!(probe_and_write(&Fake::new(""), TARGET, DeviceMode::Full, &dir).is_err());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "the POCO file and local/"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- identifiers ----

    fn alnum10(s: &str) -> bool {
        s.len() >= 10 && s.chars().all(|c| c.is_ascii_alphanumeric())
    }

    fn has_mac(text: &str) -> bool {
        let b = text.as_bytes();
        b.windows(17).any(|w| {
            let sep = w[2];
            (sep == b':' || sep == b'-')
                && w.iter().enumerate().all(|(i, c)| {
                    if i % 3 == 2 {
                        *c == sep
                    } else {
                        c.is_ascii_hexdigit()
                    }
                })
        })
    }

    /// The identifier patterns a fixture must never contain.
    fn identifier_problem(text: &str) -> Option<String> {
        if text.to_ascii_lowercase().contains("android_id") {
            return Some("android_id".into());
        }
        if has_mac(text) {
            return Some("MAC address".into());
        }
        let lines: Vec<&str> = text.lines().collect();
        for (i, l) in lines.iter().enumerate() {
            if !l.contains("ro.serialno") {
                continue;
            }
            // `[ro.serialno]: [VALUE]`, `ro.serialno=VALUE`, or the value on the next line
            let tail = l
                .rsplit([':', '='])
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(|c| c == '[' || c == ']')
                .trim();
            let next = lines.get(i + 1).map(|n| n.trim()).unwrap_or("");
            if alnum10(tail) || alnum10(next) {
                return Some(format!("serial after {l:?}"));
            }
        }
        None
    }

    #[test]
    fn the_identifier_detector_detects() {
        assert!(identifier_problem("[ro.serialno]: [ABCDE12345XY]").is_some());
        assert!(identifier_problem("### getprop ro.serialno\nABCDE12345XY\n").is_some());
        assert!(identifier_problem("aa:bb:cc:dd:ee:ff").is_some());
        assert!(identifier_problem("AA-BB-CC-DD-EE-FF").is_some());
        assert!(identifier_problem("android_id=0123").is_some());
        assert!(identifier_problem("### getprop ro.serialno\nPLACEHOLDER-S24-SERIAL\n").is_none());
    }

    #[test]
    fn no_fixture_contains_an_identifier() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut checked = 0;
        for d in ["fixtures/probe", "../../schemas/fixtures/profile"] {
            for e in std::fs::read_dir(root.join(d)).unwrap() {
                let path = e.unwrap().path();
                let text = std::fs::read_to_string(&path).unwrap();
                assert_eq!(identifier_problem(&text), None, "{}", path.display());
                checked += 1;
            }
        }
        assert_eq!(checked, 3 + 6);
        // and the protected phone's real serial appears in no fixture
        for t in [POCO, UNKNOWN_MAKER, S24] {
            assert!(!t.to_ascii_uppercase().contains(S24_SERIAL));
        }
    }

    // ---- a small JSON Schema checker for exactly the keywords schemas/profile.json uses ----

    fn type_ok(t: &str, v: &Value) -> bool {
        match t {
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "boolean" => v.is_boolean(),
            "object" => v.is_object(),
            "array" => v.is_array(),
            "null" => v.is_null(),
            _ => panic!("validator does not know type {t}"),
        }
    }

    fn validate(schema: &Value, v: &Value) -> Result<(), String> {
        let s = schema.as_object().ok_or("schema is not an object")?;
        if let Some(t) = s.get("type") {
            let ok = match t {
                Value::String(t) => type_ok(t, v),
                Value::Array(ts) => ts.iter().any(|t| type_ok(t.as_str().unwrap(), v)),
                _ => false,
            };
            if !ok {
                return Err(format!("{v} is not of type {t}"));
            }
        }
        if let Some(Value::Array(e)) = s.get("enum") {
            if !e.contains(v) {
                return Err(format!("{v} not in enum {e:?}"));
            }
        }
        if let Some(c) = s.get("const") {
            if c != v {
                return Err(format!("{v} is not const {c}"));
            }
        }
        if let (Some(m), Some(n)) = (s.get("minimum").and_then(Value::as_f64), v.as_f64()) {
            if n < m {
                return Err(format!("{n} < minimum {m}"));
            }
        }
        if let (Some(m), Some(st)) = (s.get("minLength").and_then(Value::as_u64), v.as_str()) {
            if (st.chars().count() as u64) < m {
                return Err(format!("{st:?} shorter than {m}"));
            }
        }
        if let (Some(p), Some(st)) = (s.get("pattern").and_then(Value::as_str), v.as_str()) {
            // the one pattern in the profile schema: ^/dev/input/event\d+$
            assert_eq!(
                p, r"^/dev/input/event\d+$",
                "validator only knows the touchscreen pattern"
            );
            if !is_event_node(st) {
                return Err(format!("{st:?} does not match {p}"));
            }
        }
        if let (Some(items), Some(arr)) = (s.get("items"), v.as_array()) {
            for (i, x) in arr.iter().enumerate() {
                validate(items, x).map_err(|e| format!("[{i}]: {e}"))?;
            }
        }
        if s.get("uniqueItems") == Some(&Value::Bool(true)) {
            if let Some(arr) = v.as_array() {
                for (i, a) in arr.iter().enumerate() {
                    if arr[..i].contains(a) {
                        return Err(format!("duplicate item {a}"));
                    }
                }
            }
        }
        if let Some(obj) = v.as_object() {
            if let Some(Value::Array(req)) = s.get("required") {
                for r in req {
                    if !obj.contains_key(r.as_str().unwrap()) {
                        return Err(format!("missing required {r}"));
                    }
                }
            }
            let props = s.get("properties").and_then(Value::as_object);
            for (k, x) in obj {
                match props.and_then(|p| p.get(k)) {
                    Some(sub) => validate(sub, x).map_err(|e| format!("{k}: {e}"))?,
                    None if s.get("additionalProperties") == Some(&Value::Bool(false)) => {
                        return Err(format!("unexpected property {k}"));
                    }
                    None => {}
                }
            }
        }
        if let (Some(cond), Some(then)) = (s.get("if"), s.get("then")) {
            if validate(cond, v).is_ok() {
                validate(then, v).map_err(|e| format!("then: {e}"))?;
            }
        }
        Ok(())
    }
}
