//! Flow files (task 2.10 slice a): YAML in, validated against `schemas/flow.json`, out as typed steps.
//!
//! Loading is three passes:
//! 1. [`yaml_to_json`]: `serde_yaml` 0.9 value to a JSON value. Tags, non-string keys and non-finite
//!    numbers are refused, so the validator sees exactly what a JSON loader would hand it
//!    (`schemas/README.md`: YAML formats are validated as the JSON object a loader produces).
//! 2. [`validate_schema`]: a hand-written check of every rule in `schemas/flow.json`, including the
//!    step `allOf` block. No JSON Schema crate is in the offline cache, so the rules are code; the
//!    test `flow_fixtures_agree_with_the_schema` proves agreement on every fixture.
//! 3. [`check_semantics`]: rules the engine needs that the schema does not state (unique step ids,
//!    a `for_each` body, selector-shaped `until` / `auto_success`, supported `post` kinds). A flow
//!    can pass the schema and still fail here; the agreement test uses pass 2 only.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;

use agentroid_core::selector::Selector;

/// `step.kind`, in the schema's order (the same order as [`Kind`]).
pub const KINDS: [&str; 15] = [
    "tap",
    "type",
    "set_value",
    "wait",
    "assert",
    "read",
    "for_each",
    "checkpoint",
    "route",
    "push",
    "settings_set",
    "verify",
    "require",
    "scroll",
    "back",
];
const TOP_KEYS: [&str; 6] = ["flow", "version", "app", "vars", "steps", "migratable"];
const RISKS: [&str; 6] = ["R0", "R1", "R2", "R3", "R4", "R5"];
const DIRECTIONS: [&str; 4] = ["up", "down", "left", "right"];
const NAMESPACES: [&str; 4] = ["system", "secure", "global", "vendor"];
const SCOPES: [&str; 4] = ["session", "job", "until", "permanent"];
/// `post.kind` values the v0 engine can check (a semantic rule, not a schema rule).
pub const POST_KINDS: [&str; 4] = ["none", "present", "absent", "text_equals"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowError {
    /// Not YAML, or YAML that has no JSON equivalent (tags, non-string keys, NaN).
    Yaml(String),
    /// Breaks a `schemas/flow.json` rule; one line per violation, with a JSON-pointer-like path.
    Schema(Vec<String>),
    /// Valid per the schema, but the engine cannot run it.
    Semantic(Vec<String>),
}

impl fmt::Display for FlowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlowError::Yaml(e) => write!(f, "flow is not loadable YAML: {e}"),
            FlowError::Schema(v) => write!(f, "flow breaks schemas/flow.json: {}", v.join("; ")),
            FlowError::Semantic(v) => write!(f, "flow cannot run: {}", v.join("; ")),
        }
    }
}

impl std::error::Error for FlowError {}

// ---------------------------------------------------------------- YAML -> JSON

/// Convert a YAML value to its JSON equivalent, refusing anything JSON cannot hold.
pub fn yaml_to_json(v: serde_yaml::Value) -> Result<Value, String> {
    use serde_yaml::Value as Y;
    Ok(match v {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(u) = n.as_u64() {
                Value::from(u)
            } else {
                let f = n.as_f64().ok_or("number out of range")?;
                Value::Number(
                    serde_json::Number::from_f64(f)
                        .ok_or_else(|| format!("{f} has no JSON form"))?,
                )
            }
        }
        Y::String(s) => Value::String(s),
        Y::Sequence(items) => Value::Array(
            items
                .into_iter()
                .map(yaml_to_json)
                .collect::<Result<_, _>>()?,
        ),
        Y::Mapping(m) => {
            let mut out = Map::new();
            for (k, v) in m {
                let Y::String(key) = k else {
                    return Err(format!("mapping key {k:?} is not a string"));
                };
                out.insert(key, yaml_to_json(v)?);
            }
            Value::Object(out)
        }
        Y::Tagged(t) => return Err(format!("YAML tag {} is not allowed in a flow", t.tag)),
    })
}

/// Parse YAML text to a JSON value (pass 1 only).
pub fn parse_yaml_value(text: &str) -> Result<Value, FlowError> {
    let y: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| FlowError::Yaml(e.to_string()))?;
    yaml_to_json(y).map_err(FlowError::Yaml)
}

// ---------------------------------------------------------------- schema rules

/// JSON Schema `integer`: a number with no fractional part (so `3.0` counts, as in draft 2020-12).
pub(crate) fn is_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => {
            n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => false,
    }
}

fn str_min1(v: &Value) -> bool {
    v.as_str().is_some_and(|s| s.chars().count() >= 1)
}

fn in_enum(v: &Value, set: &[&str]) -> bool {
    v.as_str().is_some_and(|s| set.contains(&s))
}

/// `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$` with ECMA semantics (`\d` is
/// ASCII, `$` is the true end of the string).
pub fn is_rfc3339ish(s: &str) -> bool {
    let b = s.as_bytes();
    let d = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    let c = |i: usize, ch: u8| b.get(i) == Some(&ch);
    let head = d(0)
        && d(1)
        && d(2)
        && d(3)
        && c(4, b'-')
        && d(5)
        && d(6)
        && c(7, b'-')
        && d(8)
        && d(9)
        && c(10, b'T');
    let time = d(11) && d(12) && c(13, b':') && d(14) && d(15) && c(16, b':') && d(17) && d(18);
    if !(head && time) {
        return false;
    }
    let mut i = 19;
    if c(i, b'.') {
        i += 1;
        let start = i;
        while d(i) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    if c(i, b'Z') {
        return i + 1 == b.len();
    }
    (c(i, b'+') || c(i, b'-'))
        && d(i + 1)
        && d(i + 2)
        && c(i + 3, b':')
        && d(i + 4)
        && d(i + 5)
        && i + 6 == b.len()
}

/// `$defs/select`: exactly one of `id|text|desc|near_text|ref|rel`.
pub fn check_select(v: &Value, at: &str, errs: &mut Vec<String>) {
    let Some(o) = v.as_object() else {
        errs.push(format!("{at}: select must be an object"));
        return;
    };
    if o.len() != 1 {
        errs.push(format!(
            "{at}: select must have exactly one key, has {}",
            o.len()
        ));
    }
    for (k, val) in o {
        match k.as_str() {
            "id" | "text" | "desc" | "near_text" | "ref" => {
                if !val.is_string() {
                    errs.push(format!("{at}/{k}: must be a string"));
                }
            }
            "rel" => check_rel(val, &format!("{at}/rel"), errs),
            other => errs.push(format!("{at}: unknown select key {other:?}")),
        }
    }
}

fn check_rel(v: &Value, at: &str, errs: &mut Vec<String>) {
    let Some(o) = v.as_object() else {
        errs.push(format!("{at}: must be an object"));
        return;
    };
    for k in ["x", "y"] {
        match o.get(k) {
            None => errs.push(format!("{at}: missing {k}")),
            Some(n) => match n.as_f64() {
                Some(f) if (0.0..=1.0).contains(&f) => {}
                Some(f) => errs.push(format!(
                    "{at}/{k}: {f} is not a fraction 0-1 (raw pixels are refused)"
                )),
                None => errs.push(format!("{at}/{k}: must be a number")),
            },
        }
    }
    for (k, val) in o {
        match k.as_str() {
            "x" | "y" => {}
            "device" => match val.as_object() {
                None => errs.push(format!("{at}/device: must be an object")),
                Some(d) => {
                    for dk in ["w", "h"] {
                        match d.get(dk) {
                            None => errs.push(format!("{at}/device: missing {dk}")),
                            Some(n) if is_integer(n) && n.as_f64().is_some_and(|f| f >= 1.0) => {}
                            Some(_) => {
                                errs.push(format!("{at}/device/{dk}: must be an integer >= 1"))
                            }
                        }
                    }
                    for dk in d.keys() {
                        if dk != "w" && dk != "h" {
                            errs.push(format!("{at}/device: unknown key {dk:?}"));
                        }
                    }
                }
            },
            other => errs.push(format!("{at}: unknown key {other:?}")),
        }
    }
}

/// `$defs/step`, including the `allOf` block.
pub fn check_step(v: &Value, at: &str, errs: &mut Vec<String>) {
    let Some(o) = v.as_object() else {
        errs.push(format!("{at}: step must be an object"));
        return;
    };
    for req in ["id", "kind"] {
        if !o.contains_key(req) {
            errs.push(format!("{at}: missing {req}"));
        }
    }
    for (k, val) in o {
        let p = format!("{at}/{k}");
        let ok = match k.as_str() {
            "id" | "tier" | "local" | "phone_path" | "key" => str_min1(val),
            "kind" => in_enum(val, &KINDS),
            "select" => {
                check_select(val, &p, errs);
                true
            }
            "selectors" => match val.as_array() {
                Some(a) if !a.is_empty() => {
                    for (i, s) in a.iter().enumerate() {
                        check_select(s, &format!("{p}/{i}"), errs);
                    }
                    true
                }
                _ => false,
            },
            "value" | "until" | "auto_success" => true,
            "timeout_ms" => is_integer(val) && val.as_f64().is_some_and(|f| f >= 0.0),
            "retry" => is_integer(val) && val.as_f64().is_some_and(|f| (0.0..=3.0).contains(&f)),
            "post" => val
                .as_object()
                .is_some_and(|m| m.get("kind").is_some_and(Value::is_string)),
            "risk" => in_enum(val, &RISKS),
            "atomic" | "awaiting_human" => val.is_boolean(),
            "table" => val.is_array(),
            "direction" => in_enum(val, &DIRECTIONS),
            "namespace" => in_enum(val, &NAMESPACES),
            "scope" => in_enum(val, &SCOPES),
            "expires" => val.as_str().is_some_and(is_rfc3339ish),
            _ => {
                errs.push(format!("{at}: unknown step key {k:?}"));
                true
            }
        };
        if !ok {
            errs.push(format!("{p}: invalid value {val}"));
        }
    }
    // the allOf block, rule by rule
    let kind = o.get("kind").and_then(Value::as_str);
    let need = |keys: &[&str], why: &str, errs: &mut Vec<String>| {
        for k in keys {
            if !o.contains_key(*k) {
                errs.push(format!("{at}: {why} requires {k}"));
            }
        }
    };
    if kind == Some("for_each") {
        need(&["table"], "for_each", errs);
    }
    if kind == Some("push") {
        need(&["local", "phone_path"], "push", errs);
    }
    if kind == Some("settings_set") {
        need(
            &["namespace", "key", "value", "scope"],
            "settings_set",
            errs,
        );
        if o.get("value").is_some_and(|v| !v.is_string()) {
            errs.push(format!("{at}/value: settings_set value must be a string"));
        }
        if o.get("scope").and_then(Value::as_str) == Some("until") {
            need(&["expires"], "settings_set with scope until", errs);
        }
    }
    if matches!(kind, Some("verify" | "require")) {
        need(&["selectors"], "verify/require", errs);
    }
    // `if: {properties: {retry: {minimum: 1}}, required: [retry]}`: `minimum` ignores non-numbers,
    // so any present retry that is not a number below 1 triggers the `then`.
    if let Some(r) = o.get("retry") {
        if !r.as_f64().is_some_and(|f| f < 1.0)
            && !o.get("risk").is_some_and(|k| in_enum(k, &["R0", "R1"]))
        {
            errs.push(format!("{at}: retry >= 1 is only for risk R0 or R1"));
        }
    }
    let scope = o.get("scope").and_then(Value::as_str);
    if scope == Some("permanent")
        && !o
            .get("risk")
            .is_some_and(|k| in_enum(k, &["R2", "R3", "R4", "R5"]))
    {
        errs.push(format!("{at}: scope permanent needs risk R2 or above"));
    }
    if scope != Some("until") && o.contains_key("expires") {
        errs.push(format!("{at}: expires is only allowed with scope until"));
    }
}

/// Every rule of `schemas/flow.json`. Empty means valid.
pub fn validate_schema(v: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let Some(o) = v.as_object() else {
        return vec!["/: flow must be an object".into()];
    };
    for req in ["flow", "version", "app", "steps"] {
        if !o.contains_key(req) {
            errs.push(format!("/: missing {req}"));
        }
    }
    for (k, val) in o {
        match k.as_str() {
            "flow" if !str_min1(val) => errs.push("/flow: must be a non-empty string".into()),
            "version" if !(val.is_string() || is_integer(val)) => {
                errs.push("/version: must be a string or an integer".into())
            }
            "app" => match val.as_object() {
                None => errs.push("/app: must be an object".into()),
                Some(a) => {
                    if !a.get("package").is_some_and(str_min1) {
                        errs.push("/app/package: required non-empty string".into());
                    }
                    if !a.get("version_range").is_some_and(Value::is_string) {
                        errs.push("/app/version_range: required string".into());
                    }
                    for ak in a.keys() {
                        if ak != "package" && ak != "version_range" {
                            errs.push(format!("/app: unknown key {ak:?}"));
                        }
                    }
                }
            },
            "vars" => match val.as_object() {
                None => errs.push("/vars: must be an object".into()),
                Some(m) => {
                    if let Some(s) = m.get("secret") {
                        match s.as_array() {
                            Some(a) if a.iter().all(Value::is_string) => {
                                let set: BTreeSet<&str> =
                                    a.iter().filter_map(Value::as_str).collect();
                                if set.len() != a.len() {
                                    errs.push("/vars/secret: names must be unique".into());
                                }
                            }
                            _ => errs.push("/vars/secret: must be an array of strings".into()),
                        }
                    }
                }
            },
            "steps" => match val.as_array() {
                Some(a) if !a.is_empty() => {
                    for (i, s) in a.iter().enumerate() {
                        check_step(s, &format!("/steps/{i}"), &mut errs);
                    }
                }
                _ => errs.push("/steps: must be a non-empty array".into()),
            },
            "migratable" if !val.is_boolean() => errs.push("/migratable: must be a boolean".into()),
            other if !TOP_KEYS.contains(&other) => errs.push(format!("/: unknown key {other:?}")),
            _ => {}
        }
    }
    errs
}

// ---------------------------------------------------------------- typed flow

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
    pub fn rank(self) -> u8 {
        self as u8
    }
    pub fn as_str(self) -> &'static str {
        RISKS[self as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Tap,
    Type,
    SetValue,
    Wait,
    Assert,
    Read,
    ForEach,
    Checkpoint,
    Route,
    Push,
    SettingsSet,
    Verify,
    Require,
    Scroll,
    Back,
}

impl Kind {
    pub const ALL: [Kind; 15] = [
        Kind::Tap,
        Kind::Type,
        Kind::SetValue,
        Kind::Wait,
        Kind::Assert,
        Kind::Read,
        Kind::ForEach,
        Kind::Checkpoint,
        Kind::Route,
        Kind::Push,
        Kind::SettingsSet,
        Kind::Verify,
        Kind::Require,
        Kind::Scroll,
        Kind::Back,
    ];
    pub fn as_str(self) -> &'static str {
        KINDS[self as usize]
    }
    /// Does a step of this kind change anything on the phone? Exhaustive on purpose: a new kind
    /// cannot be added without deciding this.
    pub fn is_acting(self) -> bool {
        match self {
            Kind::Tap
            | Kind::Type
            | Kind::SetValue
            | Kind::Route
            | Kind::Push
            | Kind::SettingsSet
            | Kind::Scroll
            | Kind::Back => true,
            Kind::Wait
            | Kind::Assert
            | Kind::Read
            | Kind::ForEach
            | Kind::Checkpoint
            | Kind::Verify
            | Kind::Require => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rel {
    pub x: f64,
    pub y: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub w: u32,
    pub h: u32,
}

/// One selector key, as the flow writes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Select {
    Id(String),
    Text(String),
    Desc(String),
    NearText(String),
    /// A stable ref (`r3fa91c`) printed by `ui_tree`/`look`; recorded flows use it (step 8).
    Ref(String),
    Rel(Rel),
}

impl Select {
    /// A selector from a JSON value of the `$defs/select` shape (used for `until`, `auto_success`
    /// and `post.select`, which the schema leaves free).
    pub fn from_value(v: &Value) -> Result<Select, String> {
        let mut errs = Vec::new();
        check_select(v, "select", &mut errs);
        if !errs.is_empty() {
            return Err(errs.join("; "));
        }
        serde_json::from_value(v.clone()).map_err(|e| e.to_string())
    }
    /// The core matcher's selector, after `${var}` substitution.
    pub fn to_core(&self, vars: &Map<String, Value>) -> Result<Selector, String> {
        Ok(match self {
            Select::Id(s) => Selector::id(&subst(s, vars)?),
            Select::Text(s) => Selector::text(&subst(s, vars)?),
            Select::Desc(s) => Selector::desc(&subst(s, vars)?),
            Select::NearText(s) => Selector::near_text(&subst(s, vars)?),
            Select::Ref(s) => Selector::ref_id(&subst(s, vars)?),
            Select::Rel(r) => Selector::rel(r.x, r.y),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct App {
    pub package: String,
    pub version_range: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<Select>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selectors: Option<Vec<Select>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<Risk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atomic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting_human: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_success: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// The `for_each` body, parsed from `value` by [`check_semantics`]. Not a schema field.
    #[serde(skip)]
    pub body: Vec<Step>,
}

impl Step {
    /// The risk the runner assumes: as written, else R0 for reads and R1 for acting kinds (assumed).
    pub fn effective_risk(&self) -> Risk {
        self.risk.unwrap_or(if self.kind.is_acting() {
            Risk::R1
        } else {
            Risk::R0
        })
    }
    /// Attempts allowed: 1 plus `retry` (the schema caps retry at 3 and allows it only on R0/R1).
    pub fn attempts(&self) -> u32 {
        1 + u32::from(self.retry.unwrap_or(0).min(3))
    }
    /// `until` (wait) as a selector.
    pub fn until_select(&self) -> Option<Select> {
        self.until.as_ref().and_then(|v| Select::from_value(v).ok())
    }
    /// `auto_success` (checkpoint) as a selector.
    pub fn auto_success_select(&self) -> Option<Select> {
        self.auto_success
            .as_ref()
            .and_then(|v| Select::from_value(v).ok())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flow {
    pub flow: String,
    pub version: Value,
    pub app: App,
    #[serde(default)]
    pub vars: Map<String, Value>,
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migratable: Option<bool>,
}

impl Flow {
    /// Steps planned: every step once, a `for_each` once plus its body once per table row.
    pub fn planned(&self) -> u32 {
        planned_of(&self.steps)
    }
    /// Names in `vars.secret`.
    pub fn secrets(&self) -> Vec<String> {
        self.vars
            .get("secret")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
    /// Every step, depth first (bodies right after their `for_each`).
    pub fn all_steps(&self) -> Vec<&Step> {
        fn walk<'a>(s: &'a [Step], out: &mut Vec<&'a Step>) {
            for st in s {
                out.push(st);
                walk(&st.body, out);
            }
        }
        let mut v = Vec::new();
        walk(&self.steps, &mut v);
        v
    }
}

fn planned_of(steps: &[Step]) -> u32 {
    steps
        .iter()
        .map(|s| {
            if s.kind == Kind::ForEach {
                let rows = u32::try_from(s.table.as_ref().map_or(0, Vec::len)).unwrap_or(u32::MAX);
                1u32.saturating_add(rows.saturating_mul(planned_of(&s.body)))
            } else {
                1
            }
        })
        .fold(0u32, u32::saturating_add)
}

/// Integral floats (`3.0`) to integers, so typed fields deserialize; the schema already accepted them.
fn normalise_numbers(v: &mut Value) {
    match v {
        Value::Number(n) if !(n.is_i64() || n.is_u64()) => {
            if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.abs() < 9.0e15 {
                    *v = Value::from(f as i64);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(normalise_numbers),
        Value::Object(o) => o.values_mut().for_each(normalise_numbers),
        _ => {}
    }
}

/// `${name}` substitution from `vars` (strings, numbers, booleans). An unknown name is an error.
pub fn subst(s: &str, vars: &Map<String, Value>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else {
            return Err(format!("unclosed variable reference in {s:?}"));
        };
        let name = &after[..j];
        match vars.get(name) {
            Some(Value::String(v)) => out.push_str(v),
            Some(Value::Number(n)) => out.push_str(&n.to_string()),
            Some(Value::Bool(b)) => out.push_str(&b.to_string()),
            _ => return Err(format!("unknown variable {name:?}")),
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Engine rules beyond the schema. Fills each `for_each` body. Empty means runnable.
pub fn check_semantics(flow: &mut Flow) -> Vec<String> {
    let mut errs = Vec::new();
    let mut ids = BTreeSet::new();
    fill(&mut flow.steps, "/steps", &mut ids, &mut errs, 0);
    errs
}

fn fill(
    steps: &mut [Step],
    at: &str,
    ids: &mut BTreeSet<String>,
    errs: &mut Vec<String>,
    depth: u32,
) {
    for (i, s) in steps.iter_mut().enumerate() {
        let p = format!("{at}/{i}");
        if !ids.insert(s.id.clone()) {
            errs.push(format!("{p}: duplicate step id {:?}", s.id));
        }
        if let Some(u) = &s.until {
            check_select(u, &format!("{p}/until"), errs);
        }
        if let Some(a) = &s.auto_success {
            check_select(a, &format!("{p}/auto_success"), errs);
        }
        if let Some(post) = &s.post {
            let k = post.get("kind").and_then(Value::as_str).unwrap_or("");
            if !POST_KINDS.contains(&k) {
                errs.push(format!("{p}/post: kind {k:?} is not one of {POST_KINDS:?}"));
            }
            if matches!(k, "present" | "absent") {
                match post.get("select") {
                    Some(v) => check_select(v, &format!("{p}/post/select"), errs),
                    None => errs.push(format!("{p}/post: {k} needs select")),
                }
            }
            if k == "text_equals" && !post.get("text").is_some_and(Value::is_string) {
                errs.push(format!("{p}/post: text_equals needs a string text"));
            }
        }
        match s.kind {
            Kind::Tap | Kind::Assert | Kind::Read if s.select.is_none() => {
                errs.push(format!("{p}: {} needs select", s.kind.as_str()))
            }
            Kind::Type if !s.value.as_ref().is_some_and(Value::is_string) => {
                errs.push(format!("{p}: type needs a string value"))
            }
            Kind::Wait
                if s.until.is_none() && !s.value.as_ref().is_some_and(|v| v.as_u64().is_some()) =>
            {
                errs.push(format!(
                    "{p}: wait needs until (a selector) or value (milliseconds)"
                ))
            }
            Kind::ForEach => {
                if depth >= 2 {
                    errs.push(format!("{p}: for_each nests at most 2 deep"));
                }
                if !s
                    .table
                    .as_ref()
                    .is_some_and(|t| t.iter().all(Value::is_object))
                {
                    errs.push(format!(
                        "{p}: for_each table rows must be objects (their keys become variables)"
                    ));
                }
                match s.value.as_ref().and_then(Value::as_array) {
                    None => errs.push(format!("{p}: for_each needs its body as value: [steps]")),
                    Some(raw) if raw.is_empty() => {
                        errs.push(format!("{p}: for_each body is empty"))
                    }
                    Some(raw) => {
                        let mut body = Vec::new();
                        for (j, b) in raw.iter().enumerate() {
                            let bp = format!("{p}/value/{j}");
                            let mut e = Vec::new();
                            check_step(b, &bp, &mut e);
                            if !e.is_empty() {
                                errs.extend(e);
                                continue;
                            }
                            let mut b = b.clone();
                            normalise_numbers(&mut b);
                            match serde_json::from_value::<Step>(b) {
                                Ok(st) => body.push(st),
                                Err(e) => errs.push(format!("{bp}: {e}")),
                            }
                        }
                        fill(&mut body, &format!("{p}/value"), ids, errs, depth + 1);
                        s.body = body;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Load a flow from a JSON value: schema, typed parse, semantics.
pub fn load_value(v: Value) -> Result<Flow, FlowError> {
    let errs = validate_schema(&v);
    if !errs.is_empty() {
        return Err(FlowError::Schema(errs));
    }
    let mut v = v;
    normalise_numbers(&mut v);
    let mut flow: Flow =
        serde_json::from_value(v).map_err(|e| FlowError::Schema(vec![e.to_string()]))?;
    let sem = check_semantics(&mut flow);
    if !sem.is_empty() {
        return Err(FlowError::Semantic(sem));
    }
    Ok(flow)
}

/// Load a flow from YAML text (`flows/*.yaml`).
pub fn load_yaml(text: &str) -> Result<Flow, FlowError> {
    load_value(parse_yaml_value(text)?)
}
