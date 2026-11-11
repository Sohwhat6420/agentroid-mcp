//! One journal entry per change and per restore (changes-in-force rows, Phase 2
//! version: "one entry per change and per restore in the 2.10 run journal"), through [`CareSink`].
//!
//! This crate cannot name bridged's `RunSink`/`JournalRecord` (bridged will depend on care), so a
//! [`CareEvent`] carries everything a `schemas/journal.json` record needs: [`CareEvent::step_id`],
//! [`CareEvent::intent`], [`CareEvent::post`] (an object with `kind`) and [`CareEvent::status`]
//! (`done` or `failed`). bridged fills in `v`, `run_id`, `seq`, `ts_*`, `epoch` and `display`.

use serde::Serialize;
use serde_json::{json, Value};

use crate::settings::Owner;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CareEventKind {
    /// A lease applied a setting.
    Change,
    /// A leased setting had drifted and was put back to the applied value.
    Reassert,
    /// A lease ended: restored, deleted (absent key) or left (the user changed it; CAS).
    Restore,
    /// A lease request refused before the phone was touched (read_only, R2 permanent, panel-off ...).
    Refused,
    /// A fake-heat override was set (thermalservice or battery temperature).
    OverrideSet,
    /// A fake-heat override was reset, verified.
    OverrideReset,
}

impl CareEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CareEventKind::Change => "change",
            CareEventKind::Reassert => "reassert",
            CareEventKind::Restore => "restore",
            CareEventKind::Refused => "refused",
            CareEventKind::OverrideSet => "override_set",
            CareEventKind::OverrideReset => "override_reset",
        }
    }
}

/// One entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CareEvent {
    pub kind: CareEventKind,
    pub ro_serial: String,
    pub ns: String,
    pub key: String,
    pub before: Option<String>,
    pub applied: Option<String>,
    pub owner: Option<Owner>,
    pub scope: Option<String>,
    /// `applied`, `reasserted`, `restored`, `deleted`, `left`, `reset`, `refused`, `set`.
    pub outcome: String,
    /// The refusal class (`R2`, `read_only`) when refused.
    pub class: Option<String>,
    pub reason: Option<String>,
    pub ts: String,
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

impl CareEvent {
    /// `care-<kind>-<ns>-<key>`, for `journal.json` `step_id`.
    pub fn step_id(&self) -> String {
        format!("care-{}-{}-{}", self.kind.as_str(), self.ns, self.key)
    }

    /// One short line, values clipped so a record stays well under 1 KB.
    pub fn intent(&self) -> String {
        let v = |o: &Option<String>| o.as_deref().map_or("-".to_string(), |s| clip(s, 64));
        let owner = self
            .owner
            .as_ref()
            .map_or(String::new(), |o| format!(" (job {})", clip(&o.job_id, 64)));
        match self.kind {
            CareEventKind::Refused => format!(
                "care refused {}/{} {}: {}",
                self.ns,
                self.key,
                self.class.as_deref().unwrap_or(""),
                clip(self.reason.as_deref().unwrap_or(""), 200)
            ),
            _ => format!(
                "care {} {}/{} {} -> {}{owner}: {}",
                self.kind.as_str(),
                self.ns,
                self.key,
                v(&self.before),
                v(&self.applied),
                self.outcome
            ),
        }
    }

    /// `journal.json` `post`: an object with `kind` plus the change itself.
    pub fn post(&self) -> Value {
        json!({
            "kind": format!("care.{}", self.kind.as_str()),
            "ro_serial": self.ro_serial,
            "ns": self.ns,
            "key": self.key,
            "before": self.before.as_deref().map(|s| clip(s, 128)),
            "applied": self.applied.as_deref().map(|s| clip(s, 128)),
            "owner": self.owner,
            "scope": self.scope,
            "outcome": self.outcome,
            "class": self.class,
        })
    }

    /// `journal.json` `status`.
    pub fn status(&self) -> &'static str {
        if self.kind == CareEventKind::Refused {
            "failed"
        } else {
            "done"
        }
    }
}

/// Where care's journal entries go (bridged adapts its run journal to this).
pub trait CareSink {
    fn record(&mut self, ev: &CareEvent) -> Result<(), String>;
}

/// Keeps entries in memory (tests).
#[derive(Debug, Default, Clone)]
pub struct MemoryCareSink {
    pub events: Vec<CareEvent>,
}

impl MemoryCareSink {
    pub fn count(&self, kind: CareEventKind) -> usize {
        self.events.iter().filter(|e| e.kind == kind).count()
    }
}

impl CareSink for MemoryCareSink {
    fn record(&mut self, ev: &CareEvent) -> Result<(), String> {
        self.events.push(ev.clone());
        Ok(())
    }
}
