//! The pop-up rulebook (`popups.yaml`) and the dismissal policy (U3, the
//! documented dismissal rule).
//!
//! * A rule is the triple **package, window, button** (`schemas/popup.json`), optionally scoped by
//!   `oem` and `sdk_min` / `sdk_max`. All three must match on the same screen, in the same package.
//! * Only a button that leaves the phone's state unchanged is dismissed automatically
//!   ([`STATE_PRESERVING`]); a `back` rule is state-preserving by definition. Anything else pauses.
//! * A permission grant (`Allow` and its variants, [`PERMISSION_GRANTS`]) is a state change and
//!   **always pauses**, even if a rule says `tap` (the schema fixture `valid-tap-by-title.json` is
//!   such a rule: schema-valid, but this policy downgrades it to a pause).
//! * The same window text and button in a *different* package is a lookalike and pauses.
//! * Any other overlay from a package that is not the flow's app, System UI, a launcher or a
//!   package the job allows is **unknown**: `needs_attention`, the job pauses, keeps its lease and
//!   reports only a crop rectangle of the overlay (never an image).
//! * The per-job switch ([`Context::auto_dismiss`] = false) turns every automatic dismissal into a
//!   pause.
//!
//! The classifier is pure: it reads a tree and returns a [`Verdict`]. The engine performs the tap
//! through core's gate (so `read_only` refuses it) and journals it.

use crate::runner::flow::is_integer;
use agentroid_core::tree::parse;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// Buttons that dismiss without changing anything (plan: `Got it`, `Not now`, `Later`; the rest are
/// assumed). Compared trimmed and case-insensitively.
pub const STATE_PRESERVING: [&str; 8] = [
    "Got it",
    "Not now",
    "Later",
    "Wait",
    "Done",
    "No thanks",
    "Dismiss",
    "Remind me later",
];
/// Permission grants: always a pause.
pub const PERMISSION_GRANTS: [&str; 7] = [
    "Allow",
    "While using the app",
    "Only this time",
    "Allow all",
    "Allow all the time",
    "Always allow",
    "Allow only while using the app",
];
/// Home screens. The app leaving for one of these is a relaunch case, not an overlay.
pub const LAUNCHERS: [&str; 6] = [
    "com.miui.home",
    "com.mi.android.globallauncher",
    "com.android.launcher3",
    "com.google.android.apps.nexuslauncher",
    "com.sec.android.app.launcher",
    "com.huawei.android.launcher",
];
pub const SYSTEM_UI: &str = "com.android.systemui";

/// The seeded rulebook, `scaffold/popups.yaml`.
pub const SEEDED_YAML: &str = include_str!("../../../../popups.yaml");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    Title(String),
    Activity(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Tap,
    Back,
    Pause,
}

/// One rule (`schemas/popup.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub package: String,
    pub window: Window,
    pub button: String,
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk_max: Option<u32>,
}

impl Rule {
    /// Does the rule apply to this phone? A scoped rule never applies when the phone's OEM or SDK is
    /// unknown (so the overlay falls through to a pause, the safe side).
    pub fn applies(&self, oem: Option<&str>, sdk: Option<u32>) -> bool {
        if let Some(want) = &self.oem {
            if !oem.is_some_and(|o| o.eq_ignore_ascii_case(want)) {
                return false;
            }
        }
        if self.sdk_min.is_some() || self.sdk_max.is_some() {
            let Some(s) = sdk else { return false };
            if self.sdk_min.is_some_and(|m| s < m) || self.sdk_max.is_some_and(|m| s > m) {
                return false;
            }
        }
        true
    }
    pub fn describe(&self) -> String {
        let w = match &self.window {
            Window::Title(t) => format!("title {t:?}"),
            Window::Activity(a) => format!("activity {a:?}"),
        };
        format!("{} / {w} / {:?}", self.package, self.button)
    }
}

/// Every rule of `schemas/popup.json`, plus `sdk_min <= sdk_max` (the `popup_sdk_order` check of
/// `test_schemas.py`). Empty means valid.
pub fn validate_rule(v: &Value) -> Vec<String> {
    let mut errs = Vec::new();
    let Some(o) = v.as_object() else {
        return vec!["rule must be an object".into()];
    };
    for req in ["package", "window", "button", "action"] {
        if !o.contains_key(req) {
            errs.push(format!("missing {req}"));
        }
    }
    for (k, val) in o {
        let ok = match k.as_str() {
            "package" | "oem" => val.as_str().is_some_and(|s| !s.is_empty()),
            "window" => val.as_object().is_some_and(|w| {
                w.len() == 1
                    && w.iter()
                        .all(|(wk, wv)| (wk == "title" || wk == "activity") && wv.is_string())
            }),
            "button" | "note" => val.is_string(),
            "action" => matches!(val.as_str(), Some("tap" | "back" | "pause")),
            "sdk_min" | "sdk_max" => is_integer(val) && val.as_f64().is_some_and(|f| f >= 29.0),
            other => {
                errs.push(format!("unknown key {other:?}"));
                true
            }
        };
        if !ok {
            errs.push(format!("{k}: invalid value {val}"));
        }
    }
    if let (Some(a), Some(b)) = (
        o.get("sdk_min").and_then(Value::as_f64),
        o.get("sdk_max").and_then(Value::as_f64),
    ) {
        if a > b {
            errs.push("sdk_min is above sdk_max".into());
        }
    }
    errs
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rulebook {
    pub rules: Vec<Rule>,
}

impl Rulebook {
    pub fn empty() -> Self {
        Rulebook::default()
    }
    /// A flat YAML list of rules; every rule is validated, and one bad rule refuses the whole file.
    pub fn from_yaml(text: &str) -> Result<Rulebook, String> {
        let y: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
        let v = crate::runner::flow::yaml_to_json(y)?;
        let Some(items) = v.as_array() else {
            return Err("popups.yaml must be a list of rules".into());
        };
        let mut rules = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let errs = validate_rule(item);
            if !errs.is_empty() {
                return Err(format!("rule {i}: {}", errs.join("; ")));
            }
            rules.push(serde_json::from_value(item.clone()).map_err(|e| format!("rule {i}: {e}"))?);
        }
        Ok(Rulebook { rules })
    }
    /// The seeded rulebook (`scaffold/popups.yaml`, compiled in).
    pub fn seeded() -> Rulebook {
        Rulebook::from_yaml(SEEDED_YAML).expect("scaffold/popups.yaml is valid (tested)")
    }
}

/// A crop rectangle in device pixels: all that is reported about an overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crop {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl Crop {
    pub fn describe(&self) -> String {
        format!("[{},{}][{},{}]", self.l, self.t, self.r, self.b)
    }
}

/// Why a run paused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum PauseReason {
    /// An overlay no rule knows.
    NeedsAttention { package: String },
    /// A permission grant: a state change.
    PermissionAllow { package: String, button: String },
    /// A dismissable rule, but the job switched automatic dismissal off.
    AutoDismissOff { package: String, rule: String },
    /// The rule itself says pause.
    RulePause { package: String, rule: String },
    /// A rule's window text and button in another package.
    Lookalike { package: String, mimics: String },
    /// A `tap` rule whose button is not on the state-preserving list.
    NotStatePreserving { package: String, button: String },
    /// The dismissal itself was refused or failed (for example by the `read_only` gate).
    DismissFailed { package: String, why: String },
    /// A `checkpoint` step waiting for a person.
    AwaitingHuman { step: String },
    /// The tree could not be read or parsed, so no overlay can be ruled out (fail closed; slice-a
    /// review follow-up 1). `why` is the parser's or the transport's message.
    UnreadableScreen { why: String },
    /// After a restart, a step whose intent was journalled but whose outcome was not could not be
    /// re-verified (no post-condition, or it does not hold). Nothing was re-sent (task 2.10b).
    UnverifiedIntent { step: String, why: String },
    /// The lease holder asked for a pause (`job.pause`).
    ByUser,
    /// A person touched the phone (task 2.15 Detector 1, or the job view's "Take over"): the
    /// human profile is back and the job waits for a hand-back.
    TakenOver,
}

impl PauseReason {
    /// `needs_attention` for anything overlay-related, as the plan names the state; the variant
    /// name is in [`PauseReason::detail`].
    pub fn state(&self) -> &'static str {
        match self {
            PauseReason::AwaitingHuman { .. } => "awaiting_human",
            PauseReason::ByUser => "paused",
            PauseReason::TakenOver => "taken_over",
            _ => "needs_attention",
        }
    }
    pub fn detail(&self) -> &'static str {
        match self {
            PauseReason::NeedsAttention { .. } => "unknown_overlay",
            PauseReason::PermissionAllow { .. } => "permission_allow",
            PauseReason::AutoDismissOff { .. } => "auto_dismiss_off",
            PauseReason::RulePause { .. } => "rule_pause",
            PauseReason::Lookalike { .. } => "lookalike",
            PauseReason::NotStatePreserving { .. } => "not_state_preserving",
            PauseReason::DismissFailed { .. } => "dismiss_failed",
            PauseReason::AwaitingHuman { .. } => "awaiting_human",
            PauseReason::UnreadableScreen { .. } => "unreadable_screen",
            PauseReason::UnverifiedIntent { .. } => "unverified_intent",
            PauseReason::ByUser => "paused_by_user",
            PauseReason::TakenOver => "taken_over",
        }
    }
}

/// How to dismiss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Press {
    /// Tap this device point (the button's centre).
    Tap {
        x: i32,
        y: i32,
    },
    Back,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dismissal {
    pub rule: String,
    pub package: String,
    pub button: String,
    pub press: Press,
    pub crop: Crop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No overlay.
    Clear,
    Dismiss(Dismissal),
    Pause {
        reason: PauseReason,
        crop: Option<Crop>,
    },
}

/// What the classifier knows about the job and the phone.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The flow's `app.package`.
    pub app: &'a str,
    /// Other packages this job expects on screen (not overlays).
    pub extra_packages: &'a [String],
    pub oem: Option<&'a str>,
    pub sdk: Option<u32>,
    /// The per-job switch. `false`: every dismissable overlay pauses instead.
    pub auto_dismiss: bool,
}

/// The screen as read: the tree, plus the foreground activity when the transport knows it (the v0
/// transport does not, so `activity` rules never match in the engine yet and those overlays pause).
#[derive(Debug, Clone, Copy)]
pub struct Screen<'a> {
    pub xml: &'a str,
    pub activity: Option<&'a str>,
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

pub fn is_permission_grant(button: &str) -> bool {
    PERMISSION_GRANTS.iter().any(|g| same(g, button))
}

pub fn is_state_preserving(button: &str) -> bool {
    !button.trim().is_empty()
        && STATE_PRESERVING.iter().any(|g| same(g, button))
        && !is_permission_grant(button)
}

struct View {
    /// (package, text, desc, bounds) for every node with a package and bounds.
    nodes: Vec<(String, String, String, Option<Crop>)>,
}

impl View {
    fn read(xml: &str) -> Result<View, String> {
        let tree = parse(xml, false).map_err(|e| e.to_string())?;
        let nodes = tree
            .nodes
            .iter()
            .filter(|n| !n.package.is_empty())
            .map(|n| {
                (
                    n.package.to_string(),
                    n.text.to_string(),
                    n.desc.to_string(),
                    n.bounds.filter(|b| b.r > b.l && b.b > b.t).map(|b| Crop {
                        l: b.l,
                        t: b.t,
                        r: b.r,
                        b: b.b,
                    }),
                )
            })
            .collect();
        Ok(View { nodes })
    }
    fn packages(&self) -> BTreeSet<&str> {
        self.nodes.iter().map(|n| n.0.as_str()).collect()
    }
    fn has_title(&self, pkg: &str, title: &str) -> bool {
        !title.trim().is_empty()
            && self
                .nodes
                .iter()
                .any(|n| n.0 == pkg && (n.1.contains(title) || n.2.contains(title)))
    }
    /// A node labelled `button` in `pkg`, with a usable rectangle.
    fn button(&self, pkg: &str, button: &str) -> Option<Crop> {
        self.nodes
            .iter()
            .find(|n| n.0 == pkg && (same(&n.1, button) || same(&n.2, button)) && n.3.is_some())
            .and_then(|n| n.3)
    }
    fn crop_of(&self, pkg: &str) -> Option<Crop> {
        self.nodes
            .iter()
            .filter(|n| n.0 == pkg)
            .filter_map(|n| n.3)
            .reduce(|a, b| Crop {
                l: a.l.min(b.l),
                t: a.t.min(b.t),
                r: a.r.max(b.r),
                b: a.b.max(b.b),
            })
    }
}

fn window_matches(view: &View, screen: &Screen<'_>, rule: &Rule) -> bool {
    match &rule.window {
        Window::Title(t) => view.has_title(&rule.package, t),
        Window::Activity(a) => screen
            .activity
            .is_some_and(|act| act == a || act.ends_with(a.as_str())),
    }
}

/// Classify the screen. Order: a tree that does not parse pauses (`unreadable_screen`), then a
/// matching rule (triple, in scope), then lookalikes, then unknown foreign packages, else clear.
pub fn classify(book: &Rulebook, screen: &Screen<'_>, ctx: &Context<'_>) -> Verdict {
    // Fail closed: an overlay the parser cannot read must not slip through as "clear".
    let view = match View::read(screen.xml) {
        Ok(v) => v,
        Err(why) => {
            return Verdict::Pause {
                reason: PauseReason::UnreadableScreen { why },
                crop: None,
            }
        }
    };
    let pkgs = view.packages();
    // 1. the triple
    for rule in book.rules.iter().filter(|r| r.applies(ctx.oem, ctx.sdk)) {
        if !pkgs.contains(rule.package.as_str()) || !window_matches(&view, screen, rule) {
            continue;
        }
        let button_at = if rule.button.trim().is_empty() {
            None
        } else {
            view.button(&rule.package, &rule.button)
        };
        if !rule.button.trim().is_empty() && button_at.is_none() {
            continue;
        }
        let crop = view.crop_of(&rule.package);
        let package = rule.package.clone();
        if is_permission_grant(&rule.button) {
            return Verdict::Pause {
                reason: PauseReason::PermissionAllow {
                    package,
                    button: rule.button.clone(),
                },
                crop,
            };
        }
        let press = match rule.action {
            Action::Pause => {
                return Verdict::Pause {
                    reason: PauseReason::RulePause {
                        package,
                        rule: rule.describe(),
                    },
                    crop,
                }
            }
            Action::Tap => match button_at {
                Some(b) if is_state_preserving(&rule.button) => {
                    let (x, y) = ((b.l + b.r).div_euclid(2), (b.t + b.b).div_euclid(2));
                    Press::Tap { x, y }
                }
                _ => {
                    return Verdict::Pause {
                        reason: PauseReason::NotStatePreserving {
                            package,
                            button: rule.button.clone(),
                        },
                        crop,
                    }
                }
            },
            Action::Back => Press::Back,
        };
        if !ctx.auto_dismiss {
            return Verdict::Pause {
                reason: PauseReason::AutoDismissOff {
                    package,
                    rule: rule.describe(),
                },
                crop,
            };
        }
        let crop = crop.unwrap_or(Crop {
            l: 0,
            t: 0,
            r: 0,
            b: 0,
        });
        return Verdict::Dismiss(Dismissal {
            rule: rule.describe(),
            package,
            button: rule.button.clone(),
            press,
            crop,
        });
    }
    // 2. lookalikes: a rule's title and button, together, in some other package (any scope)
    for rule in &book.rules {
        let Window::Title(t) = &rule.window else {
            continue;
        };
        if rule.button.trim().is_empty() {
            continue;
        }
        for pkg in pkgs.iter().filter(|p| **p != rule.package) {
            if view.has_title(pkg, t) && view.button(pkg, &rule.button).is_some() {
                return Verdict::Pause {
                    reason: PauseReason::Lookalike {
                        package: (*pkg).to_string(),
                        mimics: rule.describe(),
                    },
                    crop: view.crop_of(pkg),
                };
            }
        }
    }
    // 3. unknown overlays
    let neutral = |p: &str| {
        p == ctx.app
            || p == SYSTEM_UI
            || LAUNCHERS.contains(&p)
            || ctx.extra_packages.iter().any(|e| e == p)
    };
    if let Some(pkg) = pkgs.iter().find(|p| !neutral(p)) {
        return Verdict::Pause {
            reason: PauseReason::NeedsAttention {
                package: (*pkg).to_string(),
            },
            crop: view.crop_of(pkg),
        };
    }
    Verdict::Clear
}

/// The app is no longer on screen: only a launcher and/or System UI is. `Some(true)` when only
/// System UI is showing (the lock screen), `Some(false)` for a launcher, `None` when the app (or
/// something else) is there, or the tree carries no packages at all.
pub fn app_gone(xml: &str, app: &str) -> Option<bool> {
    let view = View::read(xml).ok()?;
    let pkgs = view.packages();
    if pkgs.is_empty() || pkgs.contains(app) {
        return None;
    }
    if pkgs.iter().all(|p| *p == SYSTEM_UI) {
        return Some(true);
    }
    if pkgs
        .iter()
        .all(|p| *p == SYSTEM_UI || LAUNCHERS.contains(p))
    {
        return Some(false);
    }
    None
}
