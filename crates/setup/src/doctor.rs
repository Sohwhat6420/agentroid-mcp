//! `doctor`: reads a phone through the (guarded) runner with read-only commands only, and names
//! each switch that is wrong, with the exact fix. It also flags any other UiAutomation owner (V49).
//!
//! * The first command is always `getprop ro.serialno`. If the target is spelt like the protected phone, or its
//!   hardware serial is the protected phone's, or the device is `read_only`, doctor sends only the approved
//!   subset (`probes/readonly-s24.txt`, via the probe's own list) and checks no switch: every check is
//!   reported as not done. The adb guard enforces the same list a second time.
//! * Otherwise it sends `doctor-reads.txt`. A read that fails leaves its checks "not checked", never
//!   "fine".
//! * doctor never writes. The fixes it prints are exact commands or taps; `setup` applies the adb ones.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use agentroid_adb_link::runner::Runner;
use agentroid_core::gate::{is_s24, DeviceMode};
use bridged::probe::{self, Profile};

use crate::device::{self, Change, HELPER_PKG, WSS};
use crate::quirks::{Check, Quirks};

/// The switch ids doctor knows outside quirks.toml.
pub const HELPER_MISSING: &str = "helper_not_installed";
pub const WIRELESS_DEBUGGING_OFF: &str = "wireless_debugging_off";
pub const ACCESSIBILITY_OFF: &str = "accessibility_service_off";
pub const BATTERY_OPTIMISATION_ON: &str = "battery_optimisation_on";
pub const BACKGROUND_RESTRICTED: &str = "background_restricted";
pub const WSS_MISSING: &str = "write_secure_settings_missing";
/// Prefix of an owner finding: `uiautomation_owner:<name>`.
pub const OWNER_PREFIX: &str = "uiautomation_owner:";
/// Suffix of a settings quirk whose key is missing: `<quirk id>:key_absent`.
pub const KEY_ABSENT_SUFFIX: &str = ":key_absent";

/// One wrong switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub id: String,
    pub title: String,
    /// What was read, in plain words.
    pub detail: String,
    /// The adb writes that fix it (`setup` applies these, journalled). Empty if adb cannot.
    pub changes: Vec<Change>,
    /// The human step, if one is needed (or the fallback when the adb fix does not hold).
    pub human: Option<String>,
}

impl Finding {
    /// The exact fix: one `adb -s <target> shell "<line>"` per change, then the human step.
    pub fn fix_text(&self, target: &str) -> String {
        let mut parts: Vec<String> = self
            .changes
            .iter()
            .map(|c| match c.line() {
                Ok(l) => format!("adb -s {target} shell \"{l}\""),
                Err(e) => format!("(no adb fix: {e})"),
            })
            .collect();
        if let Some(h) = &self.human {
            parts.push(h.clone());
        }
        parts.join("\n    ")
    }
}

/// What doctor read, parsed (setup takes its "before" values from here).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub settings: BTreeMap<String, BTreeMap<String, String>>,
    pub whitelist: Option<BTreeSet<String>>,
    pub appops: Option<BTreeMap<String, String>>,
    pub packages: Option<BTreeSet<String>>,
    pub processes: Option<BTreeSet<String>>,
    pub wss: Option<bool>,
}

impl State {
    pub fn setting(&self, ns: &str, key: &str) -> Option<&str> {
        self.settings.get(ns)?.get(key).map(String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub target: String,
    /// The hardware serial (`ro.serialno`), or the probe's "unknown".
    pub serial: String,
    pub mode: DeviceMode,
    pub profile: Profile,
    pub findings: Vec<Finding>,
    /// Checks that were not done, and why.
    pub unchecked: Vec<String>,
    /// One-off human steps from quirks.toml (`check = "note"`).
    pub notes: Vec<String>,
    /// Every shell line doctor sent, in order.
    pub sent: Vec<String>,
    pub state: State,
}

impl Report {
    pub fn ids(&self) -> Vec<&str> {
        self.findings.iter().map(|f| f.id.as_str()).collect()
    }

    /// The text the binary prints.
    pub fn render(&self) -> String {
        let mut out = format!(
            "doctor: {} {} (serial {}, {} {}, SDK {}, mode {})\n",
            self.profile.maker,
            self.profile.model,
            self.serial,
            if self.profile.skin.is_empty() {
                "skin unknown"
            } else {
                &self.profile.skin
            },
            self.profile.skin_version,
            self.profile.sdk,
            self.mode.as_str()
        );
        if self.findings.is_empty() {
            out.push_str("  nothing wrong found\n");
        }
        for f in &self.findings {
            out.push_str(&format!(
                "  WRONG {}: {} ({})\n    fix: {}\n",
                f.id,
                f.title,
                f.detail,
                f.fix_text(&self.target)
            ));
        }
        for u in &self.unchecked {
            out.push_str(&format!("  not checked: {u}\n"));
        }
        for n in &self.notes {
            out.push_str(&format!("  one-off: {n}\n"));
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorError(pub String);

impl std::fmt::Display for DoctorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "doctor: {}", self.0)
    }
}

impl std::error::Error for DoctorError {}

/// Plain lines of a list file (`#` comments and blank lines skipped), checked by the probe's rule.
pub fn read_list(text: &str) -> Vec<String> {
    probe::command_list(text).expect("compiled-in list is plain (a test checks it)")
}

/// The lines a device in `mode` may be read with.
pub fn reads_for(mode: DeviceMode) -> Vec<String> {
    match mode {
        DeviceMode::ReadOnly => probe::commands_for(DeviceMode::ReadOnly),
        DeviceMode::Full | DeviceMode::Helper => read_list(device::DOCTOR_READS),
    }
}

const SERIAL_READ: &str = "getprop ro.serialno";

/// Run doctor on `target`. `configured` is the mode from devices.toml (or the caller); the protected phone is
/// always `read_only` whatever is configured.
pub fn doctor(
    runner: &dyn Runner,
    target: &str,
    configured: DeviceMode,
    quirks: &Quirks,
) -> Result<Report, DoctorError> {
    let mut sent = vec![SERIAL_READ.to_string()];
    let mut got: HashMap<String, String> = device::run_reads(runner, target, &sent);
    let serial = got
        .get(SERIAL_READ)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let read_only = configured == DeviceMode::ReadOnly || is_s24(target) || is_s24(&serial);
    let mode = if read_only {
        DeviceMode::ReadOnly
    } else {
        configured
    };
    let rest: Vec<String> = reads_for(mode)
        .into_iter()
        .filter(|l| l != SERIAL_READ)
        .collect();
    got.extend(device::run_reads(runner, target, &rest));
    sent.extend(rest);
    let profile = probe::parse_profile(&got);
    let mut report = Report {
        target: target.to_string(),
        serial: profile.serial.clone(),
        mode,
        profile,
        findings: vec![],
        unchecked: vec![],
        notes: vec![],
        sent,
        state: State::default(),
    };
    if read_only {
        report.unchecked.push(
            "every switch: read-only device, so only the approved reads (probes/readonly-s24.txt) were sent".to_string(),
        );
        return Ok(report);
    }
    check(&mut report, &got, quirks);
    Ok(report)
}

fn check(r: &mut Report, got: &HashMap<String, String>, quirks: &Quirks) {
    let st = &mut r.state;
    for ns in ["global", "secure", "system"] {
        if let Some(out) = got.get(&format!("settings list {ns}")) {
            st.settings
                .insert(ns.to_string(), device::settings_map(out));
        }
    }
    st.whitelist = got
        .get("dumpsys deviceidle")
        .map(|o| device::whitelist_user(o));
    st.appops = got
        .get(&format!("cmd appops get {HELPER_PKG}"))
        .map(|o| device::appops_map(o));
    st.packages = got.get("pm list packages").map(|o| device::packages(o));
    st.processes = got.get("ps -A -o NAME").map(|o| device::processes(o));
    st.wss = got
        .get(&format!("dumpsys package {HELPER_PKG}"))
        .map(|o| device::wss_granted(o));
    let st = r.state.clone();
    let sdk = r.profile.sdk;
    let mut f: Vec<Finding> = Vec::new();
    let mut unchecked: Vec<String> = Vec::new();

    // wireless debugging (Android 11+)
    match st.settings.get("global") {
        _ if !r.profile.probed => unchecked.push("wireless debugging: SDK unknown".into()),
        _ if sdk < 30 => {
            unchecked.push("wireless debugging: Android 10 has none (use adb tcpip)".into())
        }
        None => unchecked.push("wireless debugging: settings list global failed".into()),
        Some(g) => {
            let v = g.get("adb_wifi_enabled").map(String::as_str);
            if v != Some("1") {
                f.push(Finding {
                    id: WIRELESS_DEBUGGING_OFF.into(),
                    title: "Wireless debugging off".into(),
                    detail: format!("global adb_wifi_enabled is {}", v.unwrap_or("absent")),
                    changes: vec![Change::Setting {
                        ns: "global".into(),
                        key: "adb_wifi_enabled".into(),
                        value: "1".into(),
                    }],
                    human: Some(
                        "or on the phone: Settings > Developer options > Wireless debugging: on"
                            .into(),
                    ),
                });
            }
        }
    }

    let installed = st.packages.as_ref().map(|p| p.contains(HELPER_PKG));
    match installed {
        None => unchecked.push(
            "helper installed, accessibility, Doze, background, quirks: pm list packages failed"
                .into(),
        ),
        Some(false) => {
            f.push(Finding {
                id: HELPER_MISSING.into(),
                title: "Agentroid helper not installed".into(),
                detail: format!("{HELPER_PKG} is not in pm list packages"),
                changes: vec![],
                human: Some(format!("install the helper (task 2.5): adb -s {} install <agentroid-helper.apk>, then run setup again", r.target)),
            });
            unchecked.push("accessibility, Doze, background, permission and quirk checks: the helper is not installed".into());
        }
        Some(true) => helper_checks(r, &st, quirks, &mut f, &mut unchecked),
    }

    // other UiAutomation owners (V49), any skin
    for o in &quirks.owners {
        let installed: Vec<&String> = o
            .packages
            .iter()
            .filter(|p| st.packages.as_ref().is_some_and(|ps| ps.contains(*p)))
            .collect();
        let running: Vec<&String> = o
            .processes
            .iter()
            .filter(|p| {
                st.processes.as_ref().is_some_and(|ps| {
                    ps.iter()
                        .any(|n| n == *p || n.starts_with(&format!("{p}:")))
                })
            })
            .collect();
        if installed.is_empty() && running.is_empty() {
            continue;
        }
        let mut steps = vec![o.stop_host.clone()];
        for p in &running {
            if o.packages.contains(p) {
                steps.push(format!("adb -s {} shell \"am force-stop {p}\"", r.target));
            }
        }
        for p in &installed {
            steps.push(format!("to remove it: adb -s {} uninstall {p}", r.target));
        }
        let detail = match (installed.is_empty(), running.is_empty()) {
            (_, false) => format!(
                "running now ({}): it holds or will take the phone's one UiAutomation connection",
                running.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ),
            _ => format!(
                "installed ({}): it takes the phone's one UiAutomation connection whenever its laptop tool starts it",
                installed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ),
        };
        f.push(Finding {
            id: format!("{OWNER_PREFIX}{}", o.name),
            title: format!("Another UiAutomation owner: {}", o.name),
            detail,
            changes: vec![],
            human: Some(steps.join("; ")),
        });
    }
    if st.processes.is_none() {
        unchecked.push(
            "running UiAutomation owners: ps -A -o NAME failed (installed ones were still checked)"
                .into(),
        );
    }
    r.findings = f;
    r.unchecked = unchecked;
}

fn helper_checks(
    r: &mut Report,
    st: &State,
    quirks: &Quirks,
    f: &mut Vec<Finding>,
    unchecked: &mut Vec<String>,
) {
    let sdk = r.profile.sdk;
    // accessibility service
    match st.settings.get("secure") {
        None => unchecked.push("accessibility service: settings list secure failed".into()),
        Some(sec) => {
            let list = sec
                .get("enabled_accessibility_services")
                .map(String::as_str);
            let on = sec.get("accessibility_enabled").map(String::as_str);
            let listed = list.is_some_and(device::has_helper_service);
            if !listed || on != Some("1") {
                let mut changes = vec![];
                if !listed {
                    changes.push(Change::Setting {
                        ns: "secure".into(),
                        key: "enabled_accessibility_services".into(),
                        value: device::append_service(list),
                    });
                }
                if on != Some("1") {
                    changes.push(Change::Setting {
                        ns: "secure".into(),
                        key: "accessibility_enabled".into(),
                        value: "1".into(),
                    });
                }
                let human = if sdk >= 33 {
                    "if it does not stay on (Android 13+ restricted settings): Settings > Apps > Agentroid helper > \u{22ee} > Allow restricted settings, then run setup again"
                } else {
                    "or on the phone: Settings > Accessibility > Agentroid helper: on"
                };
                f.push(Finding {
                    id: ACCESSIBILITY_OFF.into(),
                    title: "Accessibility service off".into(),
                    detail: format!(
                        "enabled_accessibility_services {} the helper; accessibility_enabled is {}",
                        if listed { "lists" } else { "does not list" },
                        on.unwrap_or("absent")
                    ),
                    changes,
                    human: Some(human.into()),
                });
            }
        }
    }
    // Doze whitelist (battery optimisation)
    match &st.whitelist {
        None => unchecked.push("battery optimisation: dumpsys deviceidle failed".into()),
        Some(w) if !w.contains(HELPER_PKG) => f.push(Finding {
            id: BATTERY_OPTIMISATION_ON.into(),
            title: "Battery optimisation on".into(),
            detail: format!("{HELPER_PKG} is not in the Doze user whitelist"),
            changes: vec![Change::WhitelistAdd {
                pkg: HELPER_PKG.into(),
            }],
            human: Some(
                "or on the phone: Settings > Apps > Agentroid helper > Battery > Unrestricted"
                    .into(),
            ),
        }),
        Some(_) => {}
    }
    // background running (AOSP default: allowed, so only an explicit other mode is wrong)
    match &st.appops {
        None => unchecked.push(format!(
            "background running and app-op quirks: cmd appops get {HELPER_PKG} failed"
        )),
        Some(ops) => {
            if let Some(m) = device::op_mode(ops, "RUN_ANY_IN_BACKGROUND").filter(|m| *m != "allow")
            {
                f.push(Finding {
                    id: BACKGROUND_RESTRICTED.into(),
                    title: "Background running restricted".into(),
                    detail: format!("RUN_ANY_IN_BACKGROUND is {m}"),
                    changes: vec![Change::AppOp {
                        pkg: HELPER_PKG.into(),
                        op: "RUN_ANY_IN_BACKGROUND".into(),
                        mode: "allow".into(),
                    }],
                    human: None,
                });
            }
        }
    }
    // WRITE_SECURE_SETTINGS: the keeper's permission, full mode only
    if r.mode == DeviceMode::Full {
        match st.wss {
            None => unchecked.push(format!(
                "WRITE_SECURE_SETTINGS: dumpsys package {HELPER_PKG} failed"
            )),
            Some(false) => f.push(Finding {
                id: WSS_MISSING.into(),
                title: "Keeper permission WRITE_SECURE_SETTINGS missing".into(),
                detail: format!("{WSS} is not granted to {HELPER_PKG}"),
                changes: vec![Change::Grant {
                    pkg: HELPER_PKG.into(),
                    perm: WSS.into(),
                }],
                human: None,
            }),
            Some(true) => {}
        }
    }
    // quirks.toml
    let (maker, skin) = (r.profile.maker.clone(), r.profile.skin.clone());
    for q in quirks.for_device(&maker, &skin, sdk) {
        match &q.check {
            Check::Note => r.notes.push(format!("{}: {}", q.title, q.human_fix)),
            Check::AppopsAllow { ops, absent_allows } => {
                let Some(have) = &st.appops else { continue };
                let wrong: Vec<(&String, String)> = ops
                    .iter()
                    .filter_map(|op| {
                        let m = device::op_mode(have, op);
                        let ok = match m {
                            Some(m) => m == "allow",
                            None => *absent_allows,
                        };
                        (!ok).then(|| (op, m.unwrap_or("absent").to_string()))
                    })
                    .collect();
                if !wrong.is_empty() {
                    f.push(Finding {
                        id: q.id.clone(),
                        title: q.title.clone(),
                        detail: wrong
                            .iter()
                            .map(|(o, m)| format!("op {o} is {m}"))
                            .collect::<Vec<_>>()
                            .join(", "),
                        changes: wrong
                            .iter()
                            .map(|(o, _)| Change::AppOp {
                                pkg: HELPER_PKG.into(),
                                op: (*o).clone(),
                                mode: "allow".into(),
                            })
                            .collect(),
                        human: Some(format!("if adb is refused: {}", q.human_fix)),
                    });
                }
            }
            Check::SettingsContainsPkg { key, absent_fix } => {
                if st.settings.len() < 3 {
                    unchecked.push(format!("{}: a settings list read failed", q.title));
                    continue;
                }
                let found = ["global", "secure", "system"]
                    .iter()
                    .find_map(|ns| st.setting(ns, key).map(|v| (*ns, v)));
                match found {
                    None => f.push(Finding {
                        id: format!("{}{KEY_ABSENT_SUFFIX}", q.id),
                        title: format!("{key} missing"),
                        detail: format!("{key} is in none of settings global, secure, system"),
                        changes: vec![],
                        human: Some(absent_fix.clone()),
                    }),
                    Some((ns, v)) if !device::value_lists_pkg(v, HELPER_PKG) => f.push(Finding {
                        id: q.id.clone(),
                        title: q.title.clone(),
                        detail: format!("{ns} {key} does not list {HELPER_PKG}"),
                        changes: vec![],
                        human: Some(q.human_fix.clone()),
                    }),
                    Some(_) => {}
                }
            }
        }
    }
}
