//! Migration between owned phones ("selectors only; app-pack version range; a
//! matching capability from `profiles/<serial>.json`; `migratable: false` for measurement jobs").
//!
//! [`choose_target`] filters, then ranks. A candidate is refused when it is: the source phone; not
//! owned; `read_only` (configured, or the protected phone by any of its names: core's `effective_mode`);
//! `helper` (care's leases need adb shell); not available (busy, or its guard is resting or
//! stopped); without a probed profile whose serial matches; outside the app-pack version range;
//! or missing a needed capability. The survivors are ranked by tier (T0 best), then the cooler
//! battery, then serial (deterministic).

use std::path::Path;

use agentroid_core::gate::{effective_mode, DeviceMode};
use serde::Deserialize;

use crate::CareError;

/// The fields of `profiles/<serial>.json` (`schemas/profile.json`, written by bridged `probe.rs`)
/// that routing reads. Unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProfileCaps {
    pub serial: String,
    #[serde(default)]
    pub maker: String,
    #[serde(default)]
    pub model: String,
    pub sdk: u32,
    #[serde(default)]
    pub skin: String,
    #[serde(default)]
    pub settings_keys: Vec<String>,
    #[serde(default)]
    pub refresh_modes: Vec<u32>,
    #[serde(default)]
    pub virtual_display: bool,
    #[serde(default)]
    pub ime_switchable: bool,
    #[serde(default)]
    pub fgs_types: Vec<String>,
    #[serde(default)]
    pub appfunctions: bool,
    #[serde(default)]
    pub wifi_adb: bool,
    pub tier: String,
    #[serde(default)]
    pub probed: bool,
}

impl ProfileCaps {
    /// T0 = 3 … T3 = 0 (bridged `Tier::capability`); unknown = 0.
    pub fn tier_rank(&self) -> u8 {
        match self.tier.as_str() {
            "T0" => 3,
            "T1" => 2,
            "T2" => 1,
            _ => 0,
        }
    }
}

fn plain_serial(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Read `profiles/<serial>.json`. The serial must be plain (no path tricks) and match the file's.
/// `read_only` profiles live under `profiles/local/` and are never read here.
pub fn load_profile(profiles_dir: &Path, serial: &str) -> Result<ProfileCaps, CareError> {
    if !plain_serial(serial) {
        return Err(CareError::Invalid(format!(
            "serial {serial:?} is not plain"
        )));
    }
    let path = profiles_dir.join(format!("{serial}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| CareError::Invalid(format!("cannot read {}: {e}", path.display())))?;
    let p: ProfileCaps = serde_json::from_str(&text)
        .map_err(|e| CareError::Invalid(format!("{}: {e}", path.display())))?;
    if p.serial != serial {
        return Err(CareError::Invalid(format!(
            "{} names serial {:?}",
            path.display(),
            p.serial
        )));
    }
    Ok(p)
}

/// One phone the job could move to.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// adb token.
    pub serial: String,
    /// `ro.serialno`, when known.
    pub ro_serial: Option<String>,
    pub mode: DeviceMode,
    pub owned: bool,
    /// Not busy with another job, and its heat guard is working.
    pub available: bool,
    pub profile: Option<ProfileCaps>,
    /// Installed versionName of the job's app package, if installed.
    pub app_version: Option<String>,
    /// Battery temperature, deci-°C, for ranking.
    pub battery_dc: Option<i32>,
}

/// What the job needs from a target.
#[derive(Debug, Clone)]
pub struct MigrationNeed {
    pub job_id: String,
    /// The phone it runs on now.
    pub source: String,
    /// `migratable: false` (measurement jobs) never move.
    pub migratable: bool,
    pub package: String,
    /// `flow.json` `app.version_range`; see [`version_in_range`].
    pub version_range: String,
    /// Capabilities; see [`has_capability`].
    pub needs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateError {
    NotMigratable,
    /// No phone qualified; why each was refused.
    NoTarget(Vec<(String, String)>),
    Invalid(String),
}

fn parse_version(v: &str) -> Option<Vec<u64>> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    if v.is_empty() {
        return None;
    }
    v.split('.')
        .map(|seg| {
            let d: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
            d.parse::<u64>().ok()
        })
        .collect()
}

fn cmp_versions(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let n = a.len().max(b.len());
    (0..n)
        .map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// Does `version` satisfy `range`? Grammar (ASSUMED: the plan leaves `version_range` free):
/// `*` or empty = any; otherwise comparators separated by commas or spaces, all of which must
/// hold: `>=X`, `>X`, `<=X`, `<X`, `=X`, `X` (exact), `X.*` / `X.x` (prefix). Versions are dotted
/// numbers (a leading `v` and per-segment suffixes such as `-beta` are ignored).
pub fn version_in_range(version: &str, range: &str) -> Result<bool, String> {
    let r = range.trim();
    if r.is_empty() || r == "*" {
        return Ok(true);
    }
    let v = parse_version(version)
        .ok_or_else(|| format!("version {version:?} is not dotted numbers"))?;
    for tok in r.split([',', ' ']).map(str::trim).filter(|t| !t.is_empty()) {
        let (op, rest) = [">=", "<=", ">", "<", "="]
            .iter()
            .find_map(|op| tok.strip_prefix(op).map(|r| (*op, r)))
            .unwrap_or(("", tok));
        if op.is_empty() || op == "=" {
            if let Some(prefix) = rest.strip_suffix(".*").or_else(|| rest.strip_suffix(".x")) {
                let p = parse_version(prefix).ok_or_else(|| format!("bad range term {tok:?}"))?;
                if v.len() < p.len() || v[..p.len()] != p[..] {
                    return Ok(false);
                }
                continue;
            }
        }
        let b = parse_version(rest).ok_or_else(|| format!("bad range term {tok:?}"))?;
        let o = cmp_versions(&v, &b);
        let ok = match op {
            ">=" => o.is_ge(),
            "<=" => o.is_le(),
            ">" => o.is_gt(),
            "<" => o.is_lt(),
            _ => o.is_eq(),
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Capability vocabulary (ASSUMED; each maps onto one `profile.json` field): `virtual_display`,
/// `ime_switchable`, `appfunctions`, `wifi_adb`, `refresh:<hz>`, `settings_key:<k>`,
/// `fgs:<type>`, `sdk>=<n>`, `tier<=T<n>` (T0 best). An unknown word is an error (fail closed).
pub fn has_capability(p: &ProfileCaps, cap: &str) -> Result<bool, String> {
    let bad = || format!("unknown capability {cap:?}");
    Ok(match cap {
        "virtual_display" => p.virtual_display,
        "ime_switchable" => p.ime_switchable,
        "appfunctions" => p.appfunctions,
        "wifi_adb" => p.wifi_adb,
        _ => {
            if let Some(hz) = cap.strip_prefix("refresh:") {
                p.refresh_modes
                    .contains(&hz.parse::<u32>().map_err(|_| bad())?)
            } else if let Some(k) = cap.strip_prefix("settings_key:") {
                p.settings_keys.iter().any(|x| x == k)
            } else if let Some(t) = cap.strip_prefix("fgs:") {
                p.fgs_types.iter().any(|x| x == t)
            } else if let Some(n) = cap.strip_prefix("sdk>=") {
                p.sdk >= n.parse::<u32>().map_err(|_| bad())?
            } else if let Some(t) = cap.strip_prefix("tier<=") {
                let want = match t {
                    "T0" => 3,
                    "T1" => 2,
                    "T2" => 1,
                    "T3" => 0,
                    _ => return Err(bad()),
                };
                p.tier_rank() >= want
            } else {
                return Err(bad());
            }
        }
    })
}

/// Why `c` cannot take the job, or `None` if it can.
pub fn refusal(need: &MigrationNeed, c: &Candidate) -> Result<Option<String>, MigrateError> {
    let mut ids: Vec<&str> = vec![c.serial.as_str()];
    if let Some(r) = &c.ro_serial {
        ids.push(r);
    }
    if let Some(p) = &c.profile {
        ids.push(&p.serial);
    }
    if effective_mode(&ids, c.mode) == DeviceMode::ReadOnly {
        return Ok(Some("read_only: never a migration target".into()));
    }
    if c.mode != DeviceMode::Full {
        return Ok(Some(format!(
            "{} mode: care's leases need adb shell",
            c.mode.as_str()
        )));
    }
    if c.serial == need.source || c.ro_serial.as_deref() == Some(need.source.as_str()) {
        return Ok(Some("the source phone".into()));
    }
    if !c.owned {
        return Ok(Some("not an owned phone".into()));
    }
    if !c.available {
        return Ok(Some("busy, or its heat guard is not working".into()));
    }
    let Some(p) = &c.profile else {
        return Ok(Some("no profiles/<serial>.json".into()));
    };
    if !p.probed {
        return Ok(Some("profile was never probed".into()));
    }
    if p.serial != c.serial && c.ro_serial.as_deref() != Some(p.serial.as_str()) {
        return Ok(Some(format!("profile is for {}", p.serial)));
    }
    let Some(v) = &c.app_version else {
        return Ok(Some(format!("{} is not installed", need.package)));
    };
    if !version_in_range(v, &need.version_range).map_err(MigrateError::Invalid)? {
        return Ok(Some(format!(
            "{} {v} is outside {}",
            need.package, need.version_range
        )));
    }
    for cap in &need.needs {
        if !has_capability(p, cap).map_err(MigrateError::Invalid)? {
            return Ok(Some(format!("lacks {cap}")));
        }
    }
    Ok(None)
}

/// Pick the target. Never a `read_only` device; never moves a `migratable: false` job.
pub fn choose_target(
    need: &MigrationNeed,
    candidates: &[Candidate],
) -> Result<String, MigrateError> {
    if !need.migratable {
        return Err(MigrateError::NotMigratable);
    }
    let mut ok: Vec<&Candidate> = vec![];
    let mut refused = vec![];
    for c in candidates {
        match refusal(need, c)? {
            None => ok.push(c),
            Some(why) => refused.push((c.serial.clone(), why)),
        }
    }
    ok.sort_by(|a, b| {
        let ta = a.profile.as_ref().map_or(0, ProfileCaps::tier_rank);
        let tb = b.profile.as_ref().map_or(0, ProfileCaps::tier_rank);
        tb.cmp(&ta)
            .then(
                a.battery_dc
                    .unwrap_or(i32::MAX)
                    .cmp(&b.battery_dc.unwrap_or(i32::MAX)),
            )
            .then(a.serial.cmp(&b.serial))
    });
    ok.first()
        .map(|c| c.serial.clone())
        .ok_or(MigrateError::NoTarget(refused))
}
