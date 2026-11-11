//! The end-of-job receipt (task 2.10 slice b; the card's
//! fields).
//!
//! [`build`] returns two objects:
//! * `card`: exactly `schemas/receipt.json` (every required field, nothing else; that schema has
//!   `additionalProperties: false`).
//! * `detail`: what the job view's receipt panel shows that `receipt.json` has no field for: the
//!   outcome, steps retried and not run, wall time split into work, cooling and waiting (the job
//!   view's words, JOBVIEW-DESIGN §1 and §6), the settings changed and whether each was restored,
//!   and the pop-ups dismissed. **Schema gap, raised and not resolved here:** if `receipt.json` is
//!   widened, these move into the card under the same names.
//!
//! Sources: the engine's counters (cumulative across resumes through the checkpoint and journal
//! cursors), the durable journal (pop-up dismissals, refusals) and the progress events (pauses,
//! temperature). v0 reads no phone temperature or battery and runs no model, so those card fields
//! are 0 and `detail.measured` says which numbers are real.

use crate::runner::sink::{EventKind, JournalRecord, JournalStatus, ProgressEvent};
use serde_json::{json, Value};

/// Everything the receipt is built from.
#[derive(Debug, Clone)]
pub struct ReceiptInput<'a> {
    pub run_id: &'a str,
    pub device: &'a str,
    pub flow: &'a str,
    /// `done`, `stopped`, `failed`, `stuck` or `paused` (a receipt asked for before the end).
    pub outcome: &'a str,
    pub planned: u32,
    pub done: u32,
    /// Retry attempts.
    pub retries: u32,
    /// Steps that needed more than one attempt (the job view's "Done, 2 tries" rows).
    pub retried_steps: u32,
    pub wall_ms: u64,
    /// Paused, waiting for a person.
    pub waiting_ms: u64,
    /// Paused for heat (2.13; always 0 in v0).
    pub cooling_ms: u64,
    /// CPU time of the job's own thread on the laptop, when measured.
    pub laptop_cpu_ms: Option<u64>,
    /// The stop reason and anything else that went wrong.
    pub errors: Vec<String>,
    pub records: &'a [JournalRecord],
    pub events: &'a [ProgressEvent],
}

/// The schema card plus the panel detail.
#[derive(Debug, Clone, PartialEq)]
pub struct Receipt {
    pub card: Value,
    pub detail: Value,
}

impl Receipt {
    /// `{"receipt": card, "detail": detail}`, as `job.receipt` returns it and `receipt.json` in the
    /// run directory holds it.
    pub fn to_value(&self) -> Value {
        json!({"receipt": self.card, "detail": self.detail})
    }
}

fn secs(ms: u64) -> f64 {
    (ms as f64 / 100.0).round() / 10.0
}

/// One automatic dismissal from the journal: `popup_dismiss: <rule> via <how>`.
fn dismissal(rec: &JournalRecord) -> Option<Value> {
    let rest = rec.intent.strip_prefix("popup_dismiss: ")?;
    if rec.status != JournalStatus::Done {
        return None;
    }
    let (rule, how) = rest.rsplit_once(" via ").unwrap_or((rest, ""));
    Some(json!({
        "step_id": rec.step_id,
        "package": rec.post.get("package").and_then(Value::as_str).unwrap_or(""),
        "rule": rule,
        "how": how,
    }))
}

/// Every setting a care lease changed during the job (`post.kind` `care.change`), with how its
/// lease ended (`care.restore`: `restored`, `deleted` or `left`; none yet: `in_force`).
fn settings_changed(records: &[JournalRecord]) -> Vec<Value> {
    let kind = |r: &JournalRecord| {
        r.post
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    };
    let field = |r: &JournalRecord, k: &str| {
        r.post
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut out: Vec<Value> = vec![];
    for r in records {
        match kind(r).as_str() {
            "care.change" => out.push(json!({"ns": field(r, "ns"), "key": field(r, "key"), "before": field(r, "before"), "applied": field(r, "applied"), "ended": "in_force"})),
            "care.restore" => {
                let (ns, key) = (field(r, "ns"), field(r, "key"));
                if let Some(o) = out.iter_mut().rev().find(|o| o["ns"] == ns.as_str() && o["key"] == key.as_str() && o["ended"] == "in_force") {
                    o["ended"] = json!(field(r, "outcome"));
                }
            }
            _ => {}
        }
    }
    out
}

pub fn build(inp: &ReceiptInput<'_>) -> Receipt {
    let temps: Vec<f64> = inp
        .events
        .iter()
        .filter_map(|e| e.temp_c)
        .filter(|t| t.is_finite())
        .collect();
    let peak = temps
        .iter()
        .copied()
        .fold(None, |m: Option<f64>, t| Some(m.map_or(t, |m| m.max(t))));
    let pauses: Vec<&ProgressEvent> = inp
        .events
        .iter()
        .filter(|e| e.kind == EventKind::Pause)
        .collect();
    let heat = pauses
        .iter()
        .filter(|e| e.msg.as_deref().is_some_and(|m| m.starts_with("heat")))
        .count();
    let human = pauses.len() - heat;
    let popups: Vec<Value> = inp.records.iter().filter_map(dismissal).collect();
    let work_ms = inp
        .wall_ms
        .saturating_sub(inp.waiting_ms)
        .saturating_sub(inp.cooling_ms);
    let card = json!({
        "run_id": inp.run_id,
        "device": inp.device,
        "flow": inp.flow,
        "steps_planned": inp.planned,
        "steps_done": inp.done,
        // the runner makes no model calls
        "model_calls": 0,
        "tokens_in": 0,
        "tokens_out": 0,
        "pool": {"name": "none", "usd": 0},
        "wall_time_s": secs(inp.wall_ms),
        // null, not 0, for what v0 does not measure (schemas/receipt.json allows null)
        "phone_peak_c": peak,
        "battery_change": Value::Null,
        "laptop_cpu_s": secs(inp.laptop_cpu_ms.unwrap_or(0)),
        "pauses": {"heat": heat, "human": human},
        "retries": inp.retries,
        "errors": inp.errors,
    });
    let detail = json!({
        "outcome": inp.outcome,
        "steps": {
            "planned": inp.planned,
            "done": inp.done,
            "retried": inp.retried_steps,
            "not_run": inp.planned.saturating_sub(inp.done),
        },
        "time_s": {
            "wall": secs(inp.wall_ms),
            "work": secs(work_ms),
            "cooling": secs(inp.cooling_ms),
            "waiting": secs(inp.waiting_ms),
        },
        "phone_peak_c": peak,
        // care's job-scoped settings leases (2.13, wired 20 Oct), from the journal's care records
        "settings_changed": settings_changed(inp.records),
        "popups_dismissed": popups,
        "measured": {
            "phone_peak_c": peak.is_some(),
            "battery_change": false,
            "laptop_cpu_s": inp.laptop_cpu_ms.is_some(),
        },
    });
    Receipt { card, detail }
}
