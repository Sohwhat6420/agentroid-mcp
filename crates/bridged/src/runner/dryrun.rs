//! Offline dry run: a flow against saved trees, with no phone and no
//! transport. For each step (each `for_each` row expanded, variables substituted) it reports which
//! selector would match, in which saved tree, and how (core's resolution disclosure: unique,
//! disambiguated or ambiguous). It also flags what the engine would refuse: R2+ in Phase 2, and on a
//! `read_only` device every acting step. Pop-up verdicts are reported per saved tree.

use crate::runner::flow::{Flow, Kind, Risk, Select, Step};
use crate::runner::popups::{self, Context, Rulebook, Screen, Verdict};
use agentroid_core::gate::DeviceMode;
use agentroid_core::selector::{resolve, Chosen, Status};
use agentroid_core::tree::parse;
use serde_json::{Map, Value};

/// A saved uiautomator dump.
#[derive(Debug, Clone, Copy)]
pub struct SavedTree<'a> {
    pub name: &'a str,
    pub xml: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict2 {
    /// Matched in `tree`. `status` is `unique`, `disambiguated (...)` or `point`.
    Match {
        tree: String,
        matched_by: &'static str,
        status: String,
        element: Option<usize>,
        label: String,
        describe: String,
    },
    /// Several candidates and none strictly better, in every tree that had any.
    Ambiguous { tree: String, describe: String },
    /// No tree had it. `tried` lists the reason per tree.
    NoMatch { tried: Vec<String> },
    /// The step names no selector (a `wait` on time, `scroll`, `back`, ...).
    NoSelector,
    /// The engine would refuse the step before looking.
    Refused(String),
    /// Variable substitution failed.
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DryRow {
    pub step_id: String,
    pub kind: &'static str,
    /// Which selector field: `select`, `selectors[i]`, `until`, `auto_success`, `post.select`.
    pub field: String,
    /// The selector as written (after substitution), for example `text="Bluetooth"`.
    pub selector: String,
    /// `for_each` row binding, for example `row 1 of fe`.
    pub row: Option<String>,
    pub verdict: Verdict2,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DryReport {
    pub flow: String,
    pub rows: Vec<DryRow>,
    /// (tree name, pop-up verdict) for every saved tree.
    pub overlays: Vec<(String, String)>,
}

impl DryReport {
    pub fn render(&self) -> String {
        let mut out = vec![format!(
            "dry run of {} against {} saved tree(s)",
            self.flow,
            self.overlays.len()
        )];
        for r in &self.rows {
            let row = r
                .row
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            let what = match &r.verdict {
                Verdict2::Match { tree, describe, .. } => format!("MATCH in {tree}: {describe}"),
                Verdict2::Ambiguous { tree, describe } => {
                    format!("AMBIGUOUS in {tree}: {describe}")
                }
                Verdict2::NoMatch { tried } => format!("NO MATCH ({})", tried.join("; ")),
                Verdict2::NoSelector => "no selector".into(),
                Verdict2::Refused(w) => format!("REFUSED: {w}"),
                Verdict2::Error(e) => format!("ERROR: {e}"),
            };
            let sel = if r.selector.is_empty() {
                String::new()
            } else {
                format!(" {}={}", r.field, r.selector)
            };
            out.push(format!("{} ({}){row}{sel}: {what}", r.step_id, r.kind));
        }
        for (t, v) in &self.overlays {
            out.push(format!("pop-ups in {t}: {v}"));
        }
        out.join("\n")
    }
    pub fn matched(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.verdict, Verdict2::Match { .. }))
            .count()
    }
}

/// Dry-run `flow` against `trees` for a device in `mode`.
pub fn dry_run(
    flow: &Flow,
    trees: &[SavedTree<'_>],
    mode: DeviceMode,
    rules: &Rulebook,
) -> DryReport {
    let mut rows = Vec::new();
    walk(&flow.steps, &flow.vars, None, trees, mode, &mut rows);
    let ctx = Context {
        app: &flow.app.package,
        extra_packages: &[],
        oem: None,
        sdk: None,
        auto_dismiss: true,
    };
    let overlays = trees
        .iter()
        .map(|t| {
            let v = match popups::classify(
                rules,
                &Screen {
                    xml: t.xml,
                    activity: None,
                },
                &ctx,
            ) {
                Verdict::Clear => "clear".to_string(),
                Verdict::Dismiss(d) => format!("would dismiss {} ({:?})", d.rule, d.button),
                Verdict::Pause { reason, crop } => format!(
                    "would pause: {} ({}){}",
                    reason.state(),
                    reason.detail(),
                    crop.map(|c| format!(" crop {}", c.describe()))
                        .unwrap_or_default()
                ),
            };
            (t.name.to_string(), v)
        })
        .collect();
    DryReport {
        flow: flow.flow.clone(),
        rows,
        overlays,
    }
}

fn walk(
    steps: &[Step],
    vars: &Map<String, Value>,
    row: Option<String>,
    trees: &[SavedTree<'_>],
    mode: DeviceMode,
    out: &mut Vec<DryRow>,
) {
    for s in steps {
        if s.kind == Kind::ForEach {
            let table = s.table.as_deref().unwrap_or(&[]);
            out.push(DryRow {
                step_id: s.id.clone(),
                kind: s.kind.as_str(),
                field: String::new(),
                selector: String::new(),
                row: row.clone(),
                verdict: Verdict2::NoSelector,
            });
            for (i, r) in table.iter().enumerate() {
                let mut v = vars.clone();
                if let Some(o) = r.as_object() {
                    v.extend(o.iter().map(|(k, x)| (k.clone(), x.clone())));
                }
                walk(
                    &s.body,
                    &v,
                    Some(format!("row {} of {}", i + 1, s.id)),
                    trees,
                    mode,
                    out,
                );
            }
            continue;
        }
        let refusal = if s.effective_risk().rank() >= Risk::R2.rank() {
            Some(format!(
                "{} is refused in Phase 2, with no exception",
                s.effective_risk().as_str()
            ))
        } else if s.kind.is_acting() && mode == DeviceMode::ReadOnly {
            Some("the device is read_only and this step acts".to_string())
        } else {
            None
        };
        let mut sels: Vec<(String, Select)> = Vec::new();
        if let Some(x) = &s.select {
            sels.push(("select".into(), x.clone()));
        }
        for (i, x) in s.selectors.iter().flatten().enumerate() {
            sels.push((format!("selectors[{i}]"), x.clone()));
        }
        if let Some(x) = s.until_select() {
            sels.push(("until".into(), x));
        }
        if let Some(x) = s.auto_success_select() {
            sels.push(("auto_success".into(), x));
        }
        if let Some(x) = s
            .post
            .as_ref()
            .and_then(|p| p.get("select"))
            .and_then(|v| Select::from_value(v).ok())
        {
            sels.push(("post.select".into(), x));
        }
        let base = |field: String, selector: String, verdict: Verdict2| DryRow {
            step_id: s.id.clone(),
            kind: s.kind.as_str(),
            field,
            selector,
            row: row.clone(),
            verdict,
        };
        if let Some(why) = refusal {
            out.push(base(String::new(), String::new(), Verdict2::Refused(why)));
            continue;
        }
        if sels.is_empty() {
            out.push(base(String::new(), String::new(), Verdict2::NoSelector));
            continue;
        }
        for (field, sel) in sels {
            let core_sel = match sel.to_core(vars) {
                Ok(c) => c,
                Err(e) => {
                    out.push(base(field, String::new(), Verdict2::Error(e)));
                    continue;
                }
            };
            out.push(base(field, core_sel.to_string(), judge(&core_sel, trees)));
        }
    }
}

fn judge(sel: &agentroid_core::selector::Selector, trees: &[SavedTree<'_>]) -> Verdict2 {
    let mut tried = Vec::new();
    let mut ambiguous = None;
    for t in trees {
        let tree = match parse(t.xml, false) {
            Ok(tr) => tr,
            Err(e) => {
                tried.push(format!("{}: unreadable ({e})", t.name));
                continue;
            }
        };
        match resolve(&tree, sel) {
            Ok(res) if res.status == Status::Ambiguous => {
                if ambiguous.is_none() {
                    ambiguous = Some(Verdict2::Ambiguous {
                        tree: t.name.to_string(),
                        describe: res.describe(),
                    });
                }
                tried.push(format!("{}: ambiguous", t.name));
            }
            Ok(res) => {
                let (element, label) = match &res.chosen {
                    Some(Chosen::Element(a)) => (Some(a.index), a.label.clone()),
                    Some(Chosen::Point { fx, fy }) => (None, format!("({fx:.3},{fy:.3})")),
                    None => (None, String::new()),
                };
                let status = match &res.status {
                    Status::Unique if element.is_none() => "point".to_string(),
                    Status::Unique => "unique".to_string(),
                    Status::Disambiguated(w) => format!("disambiguated ({w})"),
                    Status::Ambiguous => unreachable!("handled above"),
                };
                return Verdict2::Match {
                    tree: t.name.to_string(),
                    matched_by: res.matched_by.as_str(),
                    status,
                    element,
                    label,
                    describe: res.describe(),
                };
            }
            Err(e) => tried.push(format!("{}: {e}", t.name)),
        }
    }
    ambiguous.unwrap_or(Verdict2::NoMatch { tried })
}
