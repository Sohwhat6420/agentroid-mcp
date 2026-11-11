//! The hand-written safe list (`runner/safe-classes.json`, round 38) and the per-step safety decision.
//!
//! Java twin: `android/runner/src/main/java/dev/agentroid/helper/runner/SafeClasses.java`. Both sides embed the SAME
//! file (Rust through `include_str!`, the app as a Java resource), and both derive the "safe kind" of a step by the
//! same rules, so the phone re-checks every step itself instead of trusting the hub's label.
//!
//! **Safe kind** (what the list names; derived from the step, never taken from it):
//! - `read`, `assert`, `wait`, `scroll`, `back`: the step kind itself;
//! - `tap`: `play_store_update` when the package is `com.android.vending`, the phone is owned and the selector is
//!   exactly `{"text": "Update"}`; otherwise `tap_unique` when the selector is `id`, `text` or `desc` (the runner also
//!   insists on exactly one match on screen at run time); otherwise plain `tap` (coordinates or `near_text`: not on
//!   the list);
//! - `settings_set`: `settings_toggle_leased` when it carries a `lease`, its scope is `job` or `session` (so the lease
//!   restores it), its value is `"0"` or `"1"` and its namespace is system, secure or global; otherwise plain
//!   `settings_set` (not on the list);
//! - every other kind (`type`, `set_value`, `route`, `push`, ...): the kind itself, which the list does not name.
//!
//! **List tokens.** A class lists `step_kinds` and `packages` (the schema `schemas/safe-classes.json` allows nothing
//! else). A token `kind` is allowed in any of the class's packages (`"*"` means any package); a token
//! `kind@package` is allowed only in that one package, whatever the class's package list says. That is how the Play
//! Store "Update" button is R1 without making every Play Store tap R1.
//!
//! **Class and decision.** A step's class is the lowest class whose list allows its (safe kind, package), else R2
//! (not on the safe list). The effective class is the higher of that and the hub's own label (`risk`), so a label can
//! raise a step but never lower it. The decision has the shape of Gemini Computer Use's `safety_decision`:
//! effective R0/R1 gives `allowed`, R2 gives `require_confirmation`, R3 and above give `blocked`. In Phase 2 there is
//! no approval channel (3.4/3.5), so [`Phase::Two`] turns `require_confirmation` into `blocked`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// The bundled list, byte-identical to the one the phone carries.
pub const SAFE_CLASSES_JSON: &str = include_str!("../../../runner/safe-classes.json");

pub const PLAY_STORE: &str = "com.android.vending";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Risk {
    R0,
    R1,
    R2,
    R3,
    R4,
    R5,
}

impl Risk {
    pub fn as_str(self) -> &'static str {
        ["R0", "R1", "R2", "R3", "R4", "R5"][self as usize]
    }
}

impl fmt::Display for Risk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Gemini Computer Use's `safety_decision.decision` values, as the brief names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allowed,
    RequireConfirmation,
    Blocked,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allowed => "allowed",
            Decision::RequireConfirmation => "require_confirmation",
            Decision::Blocked => "blocked",
        }
    }
}

/// Which approval channel exists. Phase 2 has none, so a confirmation cannot be asked for and is a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Two,
}

impl Phase {
    pub fn apply(self, d: Decision) -> Decision {
        match (self, d) {
            (Phase::Two, Decision::RequireConfirmation) => Decision::Blocked,
            (_, d) => d,
        }
    }
}

/// `{decision, explanation}` like Gemini's `safety_decision`, plus what it was computed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SafetyDecision {
    pub step_id: String,
    pub safe_kind: String,
    pub package: String,
    /// Class from the list alone.
    pub listed: Risk,
    /// The higher of `listed` and the hub's label.
    pub effective: Risk,
    /// Before the phase rule.
    pub decision: Decision,
    /// After the phase rule: the one that counts.
    pub final_decision: Decision,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClassList {
    kinds: Vec<String>,
    packages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeClasses {
    /// R0 first, then R1 (only those two may appear).
    classes: BTreeMap<Risk, ClassList>,
}

impl SafeClasses {
    /// The bundled `runner/safe-classes.json`.
    pub fn bundled() -> SafeClasses {
        SafeClasses::parse(SAFE_CLASSES_JSON).expect("runner/safe-classes.json is valid (tested)")
    }

    /// Parses and checks every rule of `schemas/safe-classes.json` (no JSON Schema crate offline, so the rules are
    /// code): an object with only `classes`; class names only R0/R1, at least one; each class exactly
    /// `step_kinds` and `packages`, arrays of unique strings.
    pub fn parse(text: &str) -> Result<SafeClasses, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let top = v.as_object().ok_or("top level must be an object")?;
        if top.keys().any(|k| k != "classes") {
            return Err("only `classes` is allowed at the top level".into());
        }
        let cl = top
            .get("classes")
            .and_then(Value::as_object)
            .ok_or("`classes` must be an object")?;
        if cl.is_empty() {
            return Err("`classes` needs at least one class".into());
        }
        let mut classes = BTreeMap::new();
        for (name, body) in cl {
            let risk = match name.as_str() {
                "R0" => Risk::R0,
                "R1" => Risk::R1,
                other => {
                    return Err(format!(
                        "class {other} is not allowed: the list holds R0 and R1 only"
                    ))
                }
            };
            let o = body
                .as_object()
                .ok_or_else(|| format!("{name} must be an object"))?;
            if o.len() != 2 || !o.contains_key("step_kinds") || !o.contains_key("packages") {
                return Err(format!("{name} must have exactly step_kinds and packages"));
            }
            let strs = |k: &str| -> Result<Vec<String>, String> {
                let a = o[k]
                    .as_array()
                    .ok_or_else(|| format!("{name}.{k} must be an array"))?;
                let mut out: Vec<String> = Vec::new();
                for x in a {
                    let s = x
                        .as_str()
                        .ok_or_else(|| format!("{name}.{k} items must be strings"))?;
                    if out.iter().any(|y| y == s) {
                        return Err(format!("{name}.{k} repeats {s:?}"));
                    }
                    out.push(s.to_string());
                }
                Ok(out)
            };
            classes.insert(
                risk,
                ClassList {
                    kinds: strs("step_kinds")?,
                    packages: strs("packages")?,
                },
            );
        }
        Ok(SafeClasses { classes })
    }

    fn allows(list: &ClassList, safe_kind: &str, package: &str) -> bool {
        let scoped = format!("{safe_kind}@{package}");
        if list.kinds.contains(&scoped) {
            return true;
        }
        list.kinds.iter().any(|k| k == safe_kind)
            && list.packages.iter().any(|p| p == "*" || p == package)
    }

    /// The class the list gives this (safe kind, package): R0, R1, or R2 when the list does not name it.
    pub fn listed_class(&self, safe_kind: &str, package: &str) -> Risk {
        for (risk, list) in &self.classes {
            if SafeClasses::allows(list, safe_kind, package) {
                return *risk;
            }
        }
        Risk::R2
    }

    /// The full decision for one step.
    pub fn decide(&self, step: &StepView<'_>, owned: bool, phase: Phase) -> SafetyDecision {
        let safe_kind = derive_safe_kind(step, owned);
        let listed = self.listed_class(&safe_kind, step.package);
        let effective = listed.max(step.label);
        let decision = match effective {
            Risk::R0 | Risk::R1 => Decision::Allowed,
            Risk::R2 => Decision::RequireConfirmation,
            _ => Decision::Blocked,
        };
        let final_decision = phase.apply(decision);
        let explanation = match decision {
            Decision::Allowed => format!("{safe_kind} in {} is {effective} on the safe list", step.package),
            Decision::RequireConfirmation if listed == Risk::R2 => format!("{safe_kind} in {} is not on the safe list (R2): needs the device owner's approval, and Phase 2 has no approval channel", step.package),
            Decision::RequireConfirmation => format!("the hub labelled it {}: needs the device owner's approval, and Phase 2 has no approval channel", step.label),
            Decision::Blocked => format!("{effective} is never run by the on-phone runner"),
        };
        SafetyDecision {
            step_id: step.id.to_string(),
            safe_kind,
            package: step.package.to_string(),
            listed,
            effective,
            decision,
            final_decision,
            explanation,
        }
    }
}

/// The fields of a step that classification reads.
#[derive(Debug, Clone, Copy)]
pub struct StepView<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub package: &'a str,
    pub select: Option<&'a Value>,
    pub label: Risk,
    pub value: Option<&'a Value>,
    pub namespace: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub lease: Option<&'a str>,
}

/// See the module docs. Java twin: `SafeClasses.deriveSafeKind`.
pub fn derive_safe_kind(step: &StepView<'_>, owned: bool) -> String {
    match step.kind {
        "read" | "assert" | "wait" | "scroll" | "back" => step.kind.to_string(),
        "tap" => {
            let Some(sel) = step
                .select
                .and_then(Value::as_object)
                .filter(|m| m.len() == 1)
            else {
                return "tap".into();
            };
            let (key, val) = sel.iter().next().expect("len 1");
            if step.package == PLAY_STORE
                && owned
                && key == "text"
                && val.as_str() == Some("Update")
            {
                return "play_store_update".into();
            }
            if matches!(key.as_str(), "id" | "text" | "desc")
                && val.as_str().is_some_and(|s| !s.is_empty())
            {
                "tap_unique".into()
            } else {
                "tap".into()
            }
        }
        "settings_set" => {
            let leased = step.lease.is_some_and(|l| !l.is_empty());
            let reversible = matches!(step.scope, Some("job" | "session"));
            let toggle = matches!(step.value.and_then(Value::as_str), Some("0" | "1"));
            let ns = matches!(step.namespace, Some("system" | "secure" | "global"));
            if leased && reversible && toggle && ns {
                "settings_toggle_leased".into()
            } else {
                "settings_set".into()
            }
        }
        other => other.to_string(),
    }
}
