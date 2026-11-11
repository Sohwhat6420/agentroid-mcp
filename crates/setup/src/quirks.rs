//! Quirks v0: `quirks.toml`, keyed by OEM/skin and SDK range. `doctor` and `setup` read it.

use crate::tomlite::{self, Value};

/// The file shipped with the crate, compiled in. `Quirks::load` reads another one if given.
pub const BUILTIN: &str = include_str!("../quirks.toml");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Every op must read `allow`; an op that is not listed reads as `absent_allows`.
    AppopsAllow {
        ops: Vec<String>,
        absent_allows: bool,
    },
    /// The settings key must exist and list the helper package.
    SettingsContainsPkg { key: String, absent_fix: String },
    /// Nothing to read: a one-off human step.
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quirk {
    pub id: String,
    pub title: String,
    pub maker: String,
    pub skin: String,
    pub sdk_min: u32,
    pub sdk_max: u32,
    pub check: Check,
    pub human_fix: String,
    pub measured: String,
}

impl Quirk {
    /// True when this quirk applies to a phone with this maker, skin and SDK.
    pub fn applies(&self, maker: &str, skin: &str, sdk: u32) -> bool {
        self.maker.eq_ignore_ascii_case(maker.trim())
            && self.skin.eq_ignore_ascii_case(skin.trim())
            && (self.sdk_min..=self.sdk_max).contains(&sdk)
    }
}

/// Another UiAutomation owner (V49).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub name: String,
    pub packages: Vec<String>,
    pub processes: Vec<String>,
    pub stop_host: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quirks {
    pub quirks: Vec<Quirk>,
    pub owners: Vec<Owner>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuirksError(pub String);

impl std::fmt::Display for QuirksError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "quirks: {}", self.0)
    }
}

impl std::error::Error for QuirksError {}

fn s(v: &Value, k: &str, ctx: &str) -> Result<String, QuirksError> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| QuirksError(format!("{ctx}: missing string {k}")))
}

fn list(v: &Value, k: &str, ctx: &str) -> Result<Vec<String>, QuirksError> {
    v.get(k)
        .and_then(Value::str_list)
        .ok_or_else(|| QuirksError(format!("{ctx}: missing string list {k}")))
}

fn sdk(v: &Value, k: &str, ctx: &str) -> Result<u32, QuirksError> {
    v.get(k)
        .and_then(Value::as_int)
        .and_then(|i| u32::try_from(i).ok())
        .ok_or_else(|| QuirksError(format!("{ctx}: missing integer {k}")))
}

/// A package name or an op as written in the file: plain characters only, since they end up in a
/// shell line (`cmd appops set <pkg> <op> allow`).
fn plain(x: &str) -> bool {
    !x.is_empty()
        && x.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Quirks {
    /// The compiled-in `quirks.toml`.
    pub fn builtin() -> Self {
        Self::parse(BUILTIN).expect("the shipped quirks.toml parses (a test checks it)")
    }

    pub fn load(path: &std::path::Path) -> Result<Self, QuirksError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| QuirksError(format!("{}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, QuirksError> {
        let doc = tomlite::parse(text).map_err(|e| QuirksError(e.to_string()))?;
        if doc.get("schema").and_then(Value::as_int) != Some(1) {
            return Err(QuirksError("schema = 1 expected".into()));
        }
        let mut quirks = Vec::new();
        for q in doc
            .get("quirk")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let id = s(q, "id", "quirk")?;
            let ctx = format!("quirk {id}");
            let check = match s(q, "check", &ctx)?.as_str() {
                "appops_allow" => {
                    let ops = list(q, "ops", &ctx)?;
                    if ops.is_empty() || !ops.iter().all(|o| plain(o)) {
                        return Err(QuirksError(format!(
                            "{ctx}: ops must be plain and non-empty"
                        )));
                    }
                    let absent_allows = match s(q, "absent_means", &ctx)?.as_str() {
                        "allow" => true,
                        "deny" => false,
                        other => return Err(QuirksError(format!("{ctx}: absent_means {other:?}"))),
                    };
                    Check::AppopsAllow { ops, absent_allows }
                }
                "settings_contains_pkg" => {
                    let key = s(q, "key", &ctx)?;
                    if !plain(&key) {
                        return Err(QuirksError(format!("{ctx}: key must be plain")));
                    }
                    Check::SettingsContainsPkg {
                        key,
                        absent_fix: s(q, "absent_fix", &ctx)?,
                    }
                }
                "note" => Check::Note,
                other => return Err(QuirksError(format!("{ctx}: unknown check {other:?}"))),
            };
            let (sdk_min, sdk_max) = (sdk(q, "sdk_min", &ctx)?, sdk(q, "sdk_max", &ctx)?);
            if sdk_min > sdk_max {
                return Err(QuirksError(format!("{ctx}: sdk_min > sdk_max")));
            }
            quirks.push(Quirk {
                title: s(q, "title", &ctx)?,
                maker: s(q, "maker", &ctx)?,
                skin: s(q, "skin", &ctx)?,
                sdk_min,
                sdk_max,
                check,
                human_fix: s(q, "human_fix", &ctx)?,
                measured: s(q, "measured", &ctx)?,
                id,
            });
        }
        let mut owners = Vec::new();
        for o in doc
            .get("owner")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            let name = s(o, "name", "owner")?;
            let ctx = format!("owner {name}");
            let packages = list(o, "packages", &ctx)?;
            if packages.is_empty() || !packages.iter().all(|p| plain(p)) {
                return Err(QuirksError(format!(
                    "{ctx}: packages must be plain and non-empty"
                )));
            }
            owners.push(Owner {
                processes: list(o, "processes", &ctx)?,
                stop_host: s(o, "stop_host", &ctx)?,
                packages,
                name,
            });
        }
        Ok(Quirks { quirks, owners })
    }

    /// The quirks that apply to this phone, in file order.
    pub fn for_device(&self, maker: &str, skin: &str, sdk: u32) -> Vec<&Quirk> {
        self.quirks
            .iter()
            .filter(|q| q.applies(maker, skin, sdk))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_file_parses_and_holds_the_hyperos_items() {
        let q = Quirks::builtin();
        let ids: Vec<&str> = q.quirks.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "hyperos_autostart",
                "hyperos_no_restrictions",
                "hyperos_usb_debugging_security"
            ]
        );
        let auto = &q.quirks[0];
        assert_eq!(
            auto.check,
            Check::AppopsAllow {
                ops: ["10008", "10007", "10021", "10023", "10053"]
                    .map(String::from)
                    .to_vec(),
                absent_allows: false
            }
        );
        assert!(
            matches!(&q.quirks[1].check, Check::SettingsContainsPkg { key, .. } if key == "MILLET_NO_RESTRICT_APP")
        );
        let owners: Vec<&str> = q.owners.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(owners, ["uiautomator2", "Appium", "mobilecli"]);
    }

    #[test]
    fn keyed_by_oem_skin_and_sdk_range() {
        let q = Quirks::builtin();
        assert_eq!(q.for_device("Xiaomi", "HyperOS", 35).len(), 3);
        assert_eq!(
            q.for_device("xiaomi", "hyperos", 34).len(),
            3,
            "case-insensitive"
        );
        assert!(
            q.for_device("Xiaomi", "HyperOS", 33).is_empty(),
            "below the range"
        );
        assert!(
            q.for_device("Xiaomi", "HyperOS", 37).is_empty(),
            "above the range"
        );
        assert!(q.for_device("Samsung", "One UI", 35).is_empty());
        assert!(
            q.for_device("Xiaomi", "", 35).is_empty(),
            "MIUI or an unknown skin is not HyperOS"
        );
    }

    #[test]
    fn a_bad_file_is_refused() {
        let base = BUILTIN.to_string();
        for (from, to) in [
            ("schema = 1", "schema = 2"),
            ("check = \"appops_allow\"", "check = \"guess\""),
            ("absent_means = \"deny\"", "absent_means = \"maybe\""),
            ("\"10008\", ", "\"10008; reboot\", "),
            (
                "sdk_min = 34\nsdk_max = 36\ncheck = \"appops_allow\"",
                "sdk_min = 37\nsdk_max = 36\ncheck = \"appops_allow\"",
            ),
        ] {
            assert!(base.contains(from), "{from}");
            assert!(Quirks::parse(&base.replacen(from, to, 1)).is_err(), "{to}");
        }
    }
}
