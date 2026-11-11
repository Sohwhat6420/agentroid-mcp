//! `setup`: a fresh device profile and config for one phone, in at most 6 prompts.
//!
//! 1. Pick the device (prompt only when several are connected and none was named).
//! 2. Refuse the protected phone: by spelling before anything is sent, then by hardware serial after the one
//!    approved read `getprop ro.serialno`. Nothing else is ever sent to it.
//! 3. Mode and name (prompts unless given).
//! 4. Probe (task 2.16's probe, read-only) and doctor (read-only).
//! 5. Plan: `devices.toml` entry, `profiles/<serial>.json`, and the adb fixes doctor named.
//! 6. `--dry-run` prints the plan and stops: no file, no journal, no device write.
//! 7. Confirm (prompt unless `--yes`), then apply, every write journalled first (`journal.rs`).
//! 8. One prompt for the human-only steps (HyperOS "No restrictions"), unless `--yes`.
//! 9. Read back with doctor; if the accessibility service did not stay on (Android 13+), one prompt
//!    for "Allow restricted settings", then one retry.

use std::path::{Path, PathBuf};

use agentroid_adb_link::parse::parse_devices;
use agentroid_adb_link::runner::Runner;
use agentroid_core::gate::{is_s24, DeviceMode};
use bridged::probe::{self, Profile};

use crate::device::{self, Change};
use crate::doctor::{self, Finding, Report, ACCESSIBILITY_OFF};
use crate::journal::{self, Entry, Txn};
use crate::quirks::Quirks;
use crate::tomlite;

/// The prompt cap.
pub const MAX_PROMPTS: usize = 6;
pub const DEVICES_TOML: &str = "devices.toml";
pub const PROFILES_DIR: &str = "profiles";

/// One question to the human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: &'static str,
    pub text: String,
    pub default: String,
}

/// Where answers come from: the terminal in the binary, a script in tests.
pub trait Prompter {
    fn ask(&mut self, q: &Question) -> String;
}

/// Counts prompts and refuses the 7th.
pub(crate) struct Budget<'a> {
    pub(crate) inner: &'a mut dyn Prompter,
    pub(crate) asked: usize,
}

impl Budget<'_> {
    pub(crate) fn ask(
        &mut self,
        id: &'static str,
        text: String,
        default: &str,
    ) -> Result<String, SetupError> {
        if self.asked >= MAX_PROMPTS {
            return Err(SetupError::TooManyPrompts(id));
        }
        self.asked += 1;
        let a = self.inner.ask(&Question {
            id,
            text,
            default: default.to_string(),
        });
        let a = a.trim().to_string();
        Ok(if a.is_empty() { default.to_string() } else { a })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupError {
    /// The protected phone (or another refusal): nothing was written.
    Refused(String),
    TooManyPrompts(&'static str),
    NoDevice(String),
    Doctor(String),
    Journal(String),
    Config(String),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::Refused(m) => write!(f, "setup refused: {m}"),
            SetupError::TooManyPrompts(id) => write!(
                f,
                "setup would need a 7th prompt ({id}); the cap is {MAX_PROMPTS}"
            ),
            SetupError::NoDevice(m) => write!(f, "setup: {m}"),
            SetupError::Doctor(m) => write!(f, "setup: {m}"),
            SetupError::Journal(m) => write!(f, "setup: {m}"),
            SetupError::Config(m) => write!(f, "setup: {m}"),
        }
    }
}

impl std::error::Error for SetupError {}

impl From<journal::JournalError> for SetupError {
    fn from(e: journal::JournalError) -> Self {
        SetupError::Journal(e.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub config_dir: PathBuf,
    /// adb target (`-s` value). None: chosen from `adb devices -l`.
    pub target: Option<String>,
    pub mode: Option<DeviceMode>,
    pub name: Option<String>,
    pub dry_run: bool,
    /// Take every default; human-only steps are listed, not asked.
    pub yes: bool,
}

/// One planned step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    File { rel: String, bytes: Vec<u8> },
    Device(Change),
}

impl Step {
    pub fn describe(&self, root: &Path) -> String {
        match self {
            Step::File { rel, bytes } => {
                let exists = root.join(rel).exists();
                format!(
                    "{} {} ({} bytes)",
                    if exists { "update" } else { "create" },
                    root.join(rel).display(),
                    bytes.len()
                )
            }
            Step::Device(c) => format!(
                "phone: {} ({})",
                c.describe(),
                c.line().unwrap_or_else(|e| e)
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub target: String,
    pub serial: String,
    pub mode: DeviceMode,
    pub plan: Vec<Step>,
    pub applied: bool,
    pub prompts: usize,
    pub run: Option<String>,
    /// What doctor still reports after setup (or before, for a dry run).
    pub remaining: Vec<Finding>,
    pub notes: Vec<String>,
    pub hash_before: String,
    pub hash_after: String,
}

impl Outcome {
    pub fn render(&self, root: &Path) -> String {
        let mut s = format!(
            "setup {} (serial {}, mode {}){}\n",
            self.target,
            self.serial,
            self.mode.as_str(),
            if self.applied { "" } else { ": NOT APPLIED" }
        );
        for st in &self.plan {
            s.push_str(&format!("  - {}\n", st.describe(root)));
        }
        for f in &self.remaining {
            s.push_str(&format!(
                "  still wrong {}: {}\n    fix: {}\n",
                f.id,
                f.title,
                f.fix_text(&self.target)
            ));
        }
        for n in &self.notes {
            s.push_str(&format!("  note: {n}\n"));
        }
        if let Some(r) = &self.run {
            s.push_str(&format!("  undo with: agentroid-setup --undo (run {r})\n"));
        }
        s.push_str(&format!(
            "  config hash before {}\n  config hash after  {}\n  prompts {}\n",
            self.hash_before, self.hash_after, self.prompts
        ));
        s
    }
}

fn pick_target(runner: &dyn Runner, budget: &mut Budget) -> Result<String, SetupError> {
    let out = runner
        .run(&["devices".to_string(), "-l".to_string()], 10_000)
        .map_err(|e| SetupError::NoDevice(format!("adb devices -l: {e}")))?;
    let ready: Vec<_> = parse_devices(&out.stdout)
        .into_iter()
        .filter(|d| d.state == "device")
        .collect();
    // the protected phone is never offered
    let ready: Vec<_> = ready.into_iter().filter(|d| !is_s24(&d.serial)).collect();
    match ready.len() {
        0 => Err(SetupError::NoDevice(
            "no phone is connected (the protected phone is never set up)".into(),
        )),
        1 => Ok(ready[0].serial.clone()),
        _ => {
            let list: Vec<String> = ready
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    format!(
                        "{}) {} {}",
                        i + 1,
                        d.serial,
                        d.model.clone().unwrap_or_default()
                    )
                })
                .collect();
            let a = budget.ask("device", format!("Which phone?\n{}", list.join("\n")), "1")?;
            let i: usize = a
                .parse()
                .map_err(|_| SetupError::NoDevice(format!("not a number: {a}")))?;
            ready
                .get(i.wrapping_sub(1))
                .map(|d| d.serial.clone())
                .ok_or_else(|| SetupError::NoDevice(format!("no phone {i}")))
        }
    }
}

/// The `[devices."<serial>"]` section setup writes.
pub fn device_section(
    serial: &str,
    name: &str,
    mode: DeviceMode,
    p: &Profile,
    profile_rel: &str,
) -> String {
    format!(
        "[devices.{}]\nname = {}\nmode = {}\nmaker = {}\nmodel = {}\nskin = {}\nsdk = {}\nprofile = {}\n",
        tomlite::quote(serial),
        tomlite::quote(name),
        tomlite::quote(mode.as_str()),
        tomlite::quote(&p.maker),
        tomlite::quote(&p.model),
        tomlite::quote(&p.skin),
        p.sdk,
        tomlite::quote(profile_rel),
    )
}

const DEVICES_HEADER: &str = "# Agentroid devices, keyed by hardware serial (ro.serialno), never the adb name.\n# Modes: full, helper, read_only. Written by agentroid-setup; `agentroid-setup --undo` reverts its last run.\n";

/// The new `devices.toml`: the old text with this serial's section replaced (or appended).
pub fn devices_toml(
    existing: Option<&str>,
    serial: &str,
    section: &str,
) -> Result<String, SetupError> {
    let Some(old) = existing else {
        return Ok(format!("{DEVICES_HEADER}\n{section}"));
    };
    tomlite::parse(old).map_err(|e| {
        SetupError::Config(format!(
            "{DEVICES_TOML} does not parse, not editing it: {e}"
        ))
    })?;
    let header = format!("[devices.{}]", tomlite::quote(serial));
    let mut kept: Vec<&str> = Vec::new();
    let mut skipping = false;
    for l in old.lines() {
        let t = l.trim();
        if t == header {
            skipping = true;
            continue;
        }
        if skipping && t.starts_with('[') {
            skipping = false;
        }
        if !skipping {
            kept.push(l);
        }
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(section);
    Ok(out)
}

/// The mode and serial a devices.toml gives a hardware serial, if any.
pub fn configured_mode(config_dir: &Path, serial: &str) -> Option<DeviceMode> {
    let text = std::fs::read_to_string(config_dir.join(DEVICES_TOML)).ok()?;
    let doc = tomlite::parse(&text).ok()?;
    DeviceMode::parse(doc.get("devices")?.get(serial)?.get("mode")?.as_str()?)
}

fn doctor_err(e: doctor::DoctorError) -> SetupError {
    SetupError::Doctor(e.0)
}

/// The device writes for a doctor report, with the "before" each one needs for its undo entry.
fn device_entry(c: &Change, st: &doctor::State) -> Entry {
    match c {
        Change::Setting { ns, key, value } => Entry::Setting {
            ns: ns.clone(),
            key: key.clone(),
            before: st.setting(ns, key).map(str::to_string),
            after: value.clone(),
        },
        Change::WhitelistAdd { pkg } => Entry::Whitelist {
            pkg: pkg.clone(),
            was_present: st.whitelist.as_ref().is_some_and(|w| w.contains(pkg)),
        },
        Change::AppOp { pkg, op, mode } => Entry::AppOp {
            pkg: pkg.clone(),
            op: op.clone(),
            before: st
                .appops
                .as_ref()
                .and_then(|o| device::op_mode(o, op))
                .map(str::to_string),
            after: mode.clone(),
        },
        Change::Grant { pkg, perm } => Entry::Grant {
            pkg: pkg.clone(),
            perm: perm.clone(),
            was_granted: st.wss == Some(true),
        },
    }
}

fn apply_changes(
    txn: &mut Txn,
    runner: &dyn Runner,
    target: &str,
    rep: &Report,
    only: Option<&str>,
) -> Result<(), SetupError> {
    for f in rep
        .findings
        .iter()
        .filter(|f| only.is_none_or(|id| f.id == id))
    {
        for c in &f.changes {
            let line = c.line().map_err(SetupError::Config)?;
            txn.device(runner, target, device_entry(c, &rep.state), &line)?;
        }
    }
    Ok(())
}

/// Run setup. `runner` should be the guarded runner; tests pass a guarded fake.
pub fn setup(
    runner: &dyn Runner,
    quirks: &Quirks,
    prompter: &mut dyn Prompter,
    opts: &Options,
) -> Result<Outcome, SetupError> {
    let root = opts.config_dir.as_path();
    let hash_before = journal::tree_hash(root);
    let mut budget = Budget {
        inner: prompter,
        asked: 0,
    };

    // 1-2: the device, and the protected phone refused before anything else is sent to it
    let target = match &opts.target {
        Some(t) => t.clone(),
        None => pick_target(runner, &mut budget)?,
    };
    if is_s24(&target) {
        return Err(SetupError::Refused(format!(
            "{target} is the protected phone: it is never set up or changed"
        )));
    }
    let got = device::run_reads(runner, &target, &["getprop ro.serialno".to_string()]);
    let serial = got
        .get("getprop ro.serialno")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if is_s24(&serial) {
        return Err(SetupError::Refused(format!(
            "{target} is the protected phone (hardware serial): it is never set up or changed"
        )));
    }
    if serial.is_empty() {
        return Err(SetupError::NoDevice(format!(
            "{target}: cannot read ro.serialno"
        )));
    }

    // 3: mode and name
    let mode = match opts.mode {
        Some(m) => m,
        None if opts.yes => configured_mode(root, &serial).unwrap_or(DeviceMode::Full),
        None => {
            let def = configured_mode(root, &serial)
                .unwrap_or(DeviceMode::Full)
                .as_str();
            let a = budget.ask(
                "mode",
                format!("Mode for {serial}: full, helper or read_only?"),
                def,
            )?;
            DeviceMode::parse(&a)
                .ok_or_else(|| SetupError::Config(format!("unknown mode {a:?}")))?
        }
    };

    // 4: probe and doctor, both read-only
    let profile = probe::probe(runner, &target, mode);
    if profile.serial != serial {
        return Err(SetupError::Refused(format!(
            "serial changed during setup ({serial} then {})",
            profile.serial
        )));
    }
    let name = match &opts.name {
        Some(n) => n.clone(),
        None if opts.yes => profile.model.clone(),
        None => budget.ask(
            "name",
            format!("Name for {} {}?", profile.maker, profile.model),
            &profile.model,
        )?,
    };
    let rep = if mode == DeviceMode::ReadOnly {
        None
    } else {
        Some(doctor::doctor(runner, &target, mode, quirks).map_err(doctor_err)?)
    };

    // 5: the plan
    let profile_abs = probe::profile_path(Path::new(PROFILES_DIR), &serial, mode)
        .map_err(|e| SetupError::Config(e.0))?;
    let profile_rel = profile_abs.to_string_lossy().replace('\\', "/");
    let profile_json = serde_json::to_string_pretty(&profile)
        .map_err(|e| SetupError::Config(e.to_string()))?
        + "\n";
    let existing = std::fs::read_to_string(root.join(DEVICES_TOML)).ok();
    let section = device_section(&serial, &name, mode, &profile, &profile_rel);
    let devices = devices_toml(existing.as_deref(), &serial, &section)?;
    let mut plan = vec![
        Step::File {
            rel: DEVICES_TOML.into(),
            bytes: devices.into_bytes(),
        },
        Step::File {
            rel: profile_rel.clone(),
            bytes: profile_json.into_bytes(),
        },
    ];
    if let Some(r) = &rep {
        for f in &r.findings {
            plan.extend(f.changes.iter().cloned().map(Step::Device));
        }
    }
    let human: Vec<&Finding> = rep
        .iter()
        .flat_map(|r| r.findings.iter())
        .filter(|f| f.changes.is_empty() && !f.id.starts_with(doctor::OWNER_PREFIX))
        .collect();
    let mut notes: Vec<String> = rep.iter().flat_map(|r| r.notes.iter().cloned()).collect();
    if mode == DeviceMode::ReadOnly {
        notes.push("read_only: no phone settings are changed".into());
    }
    let mut outcome = Outcome {
        target: target.clone(),
        serial: serial.clone(),
        mode,
        plan,
        applied: false,
        prompts: budget.asked,
        run: None,
        remaining: rep.as_ref().map(|r| r.findings.clone()).unwrap_or_default(),
        notes,
        hash_before: hash_before.clone(),
        hash_after: hash_before.clone(),
    };

    // 6: dry run stops here, having written nothing
    if opts.dry_run {
        outcome.hash_after = journal::tree_hash(root);
        outcome.prompts = budget.asked;
        return Ok(outcome);
    }

    // 7: confirm, then apply
    if !opts.yes {
        let a = budget.ask(
            "confirm",
            format!("Apply these {} steps? [Y/n]", outcome.plan.len()),
            "y",
        )?;
        if !a.eq_ignore_ascii_case("y") && !a.eq_ignore_ascii_case("yes") {
            outcome.prompts = budget.asked;
            outcome.hash_after = journal::tree_hash(root);
            return Ok(outcome);
        }
    }
    let mut txn = Txn::begin(root, &target, &serial)?;
    for st in &outcome.plan {
        if let Step::File { rel, bytes } = st {
            txn.write_file(rel, bytes)?;
        }
    }
    if let Some(r) = &rep {
        apply_changes(&mut txn, runner, &target, r, None)?;
    }

    // 8: the human-only steps, in one prompt
    if !human.is_empty() {
        let steps: Vec<String> = human
            .iter()
            .map(|f| format!("- {}: {}", f.title, f.human.clone().unwrap_or_default()))
            .collect();
        if opts.yes {
            outcome
                .notes
                .push(format!("left for you on the phone:\n{}", steps.join("\n")));
        } else {
            budget.ask(
                "human_steps",
                format!(
                    "On the phone, please:\n{}\nPress Enter when done (or type skip).",
                    steps.join("\n")
                ),
                "done",
            )?;
        }
    }

    // 9: read back
    let mut after = doctor::doctor(runner, &target, mode, quirks).map_err(doctor_err)?;
    if after.ids().contains(&ACCESSIBILITY_OFF) && after.profile.sdk >= 33 && !opts.yes {
        let a = budget.ask(
            "restricted_settings",
            "The accessibility service did not stay on. On the phone: Settings > Apps > Agentroid helper > \u{22ee} > Allow restricted settings. Press Enter to retry (or type skip).".to_string(),
            "retry",
        )?;
        if !a.eq_ignore_ascii_case("skip") {
            apply_changes(&mut txn, runner, &target, &after, Some(ACCESSIBILITY_OFF))?;
            after = doctor::doctor(runner, &target, mode, quirks).map_err(doctor_err)?;
        }
    }
    outcome.remaining = after.findings;
    outcome.applied = true;
    outcome.run = Some(txn.run().to_string());
    outcome.prompts = budget.asked;
    outcome.hash_after = journal::tree_hash(root);
    Ok(outcome)
}
