//! The two lease bundles, `profiles/human.json` and `profiles/agent.json` (schema
//! `schemas/lease-bundle.json`). Every setting key and value, vendor keys included, lives only in
//! those files: this module knows the format, never a key.

use std::fmt;
use std::path::Path;

use serde_json::Value;

/// One `settings put <ns> <key> <value>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SettingWrite {
    pub ns: String,
    pub key: String,
    pub value: String,
}

impl SettingWrite {
    /// `ns:key`, the bundle's own spelling.
    pub fn id(&self) -> String {
        format!("{}:{}", self.ns, self.key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleKind {
    Human,
    Agent,
}

/// A parsed, validated lease bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub kind: BundleKind,
    /// Sorted by `ns:key`.
    pub settings: Vec<SettingWrite>,
    /// The helper-owned DND `AutomaticZenRule`: on (true) or off (false).
    pub dnd_zen_rule: Option<bool>,
    /// The agent works on a scrcpy virtual display.
    pub virtual_display: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError(pub String);

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lease bundle: {}", self.0)
    }
}

impl std::error::Error for ProfileError {}

/// The namespaces `schemas/lease-bundle.json` allows (its `propertyNames` pattern).
pub const NAMESPACES: [&str; 4] = ["system", "secure", "global", "vendor"];

fn key_ok(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Split `ns:key`, checking the schema's pattern `^(system|secure|global|vendor):[A-Za-z0-9_.]+$`.
pub fn split_id(id: &str) -> Option<(&str, &str)> {
    let (ns, key) = id.split_once(':')?;
    (NAMESPACES.contains(&ns) && key_ok(key)).then_some((ns, key))
}

/// Check a JSON value against the rules of `schemas/lease-bundle.json`.
pub fn validate(v: &Value) -> Result<(), ProfileError> {
    let err = |m: &str| Err(ProfileError(m.to_string()));
    let Some(o) = v.as_object() else {
        return err("not an object");
    };
    for k in o.keys() {
        if !["bundle", "settings", "dnd_zen_rule", "virtual_display"].contains(&k.as_str()) {
            return Err(ProfileError(format!("unknown field {k}")));
        }
    }
    match o.get("bundle").and_then(Value::as_str) {
        Some("human") | Some("agent") => {}
        _ => return err("bundle must be \"human\" or \"agent\""),
    }
    let Some(s) = o.get("settings").and_then(Value::as_object) else {
        return err("settings must be an object");
    };
    if s.is_empty() {
        return err("settings needs at least one key");
    }
    for (k, val) in s {
        if split_id(k).is_none() {
            return Err(ProfileError(format!(
                "{k} is not <system|secure|global|vendor>:<key>"
            )));
        }
        if !val.is_string() {
            return Err(ProfileError(format!("{k}: values are strings")));
        }
    }
    for b in ["dnd_zen_rule", "virtual_display"] {
        if o.get(b).is_some_and(|x| !x.is_boolean()) {
            return Err(ProfileError(format!("{b} must be a boolean")));
        }
    }
    Ok(())
}

impl Bundle {
    pub fn parse(json: &str) -> Result<Self, ProfileError> {
        let v: Value = serde_json::from_str(json).map_err(|e| ProfileError(e.to_string()))?;
        validate(&v)?;
        let kind = if v["bundle"] == "human" {
            BundleKind::Human
        } else {
            BundleKind::Agent
        };
        let mut settings: Vec<SettingWrite> = v["settings"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(id, val)| {
                let (ns, key) = split_id(id)?;
                Some(SettingWrite {
                    ns: ns.to_string(),
                    key: key.to_string(),
                    value: val.as_str()?.to_string(),
                })
            })
            .collect();
        settings.sort();
        Ok(Bundle {
            kind,
            settings,
            dnd_zen_rule: v["dnd_zen_rule"].as_bool(),
            virtual_display: v["virtual_display"].as_bool(),
        })
    }

    pub fn load(path: &Path) -> Result<Self, ProfileError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ProfileError(format!("{}: {e}", path.display())))?;
        Self::parse(&text)
    }

    /// The writes for one device. `present` is the device's `settings_keys` from
    /// `profiles/<serial>.json` (2.16) when known: a key it does not list (for example a vendor
    /// refresh key on another maker's phone, "if present") is skipped. `None` writes every key.
    pub fn writes(&self, present: Option<&[String]>) -> Vec<SettingWrite> {
        self.settings
            .iter()
            .filter(|w| present.is_none_or(|p| p.iter().any(|k| k == &w.key)))
            .cloned()
            .collect()
    }
}

/// Both bundles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profiles {
    pub human: Bundle,
    pub agent: Bundle,
}

impl Profiles {
    /// Load `<dir>/human.json` and `<dir>/agent.json`, checking each declares the right bundle.
    pub fn load(dir: &Path) -> Result<Self, ProfileError> {
        let human = Bundle::load(&dir.join("human.json"))?;
        let agent = Bundle::load(&dir.join("agent.json"))?;
        if human.kind != BundleKind::Human || agent.kind != BundleKind::Agent {
            return Err(ProfileError(
                "human.json must be bundle human and agent.json bundle agent".into(),
            ));
        }
        Ok(Self { human, agent })
    }
}
