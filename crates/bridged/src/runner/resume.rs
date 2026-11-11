//! `resume(run_id)` from the journal (task 2.10 slice b)` with
//! intent and post-condition").
//!
//! 1. [`load_journal`] reads `journal.ndjson` back. Every line must be a `schemas/journal.json`
//!    record of one run with `seq` counting 0, 1, 2, ... A bad line with nothing good after it is a
//!    **torn tail** (a crash mid-write): the file is truncated to the end of its last good record and
//!    fsynced. A bad line *followed by* a good one is a **corrupt middle**: the resume is refused and
//!    the file is left exactly as it was.
//! 2. [`plan`] finds the last durable position: the last record that carries the engine's cursor
//!    (`post._at`), compared with the saved checkpoint (pauses and checkpoint steps); the later
//!    position wins. For the step there:
//!    * intent only (`planned` / `sent`, or a `failed` send) gives [`ResumeMode::Reverify`]: the
//!      engine checks the step's post-condition (or the `verify` / `require` step after it) and
//!      pauses with `needs_attention` if it cannot. **It is never re-sent blindly.**
//!    * `done` with no post-condition gives [`ResumeMode::Skip`]; `done` with a post-condition is
//!      re-verified too (the post-condition was not yet confirmed when the record was written).
//!    * a pop-up or relaunch record (the step's own calls had not started), an `interrupted` record
//!      (a person was asked to look) or a newer checkpoint gives [`ResumeMode::Rerun`].

use crate::runner::engine::{Resume, ResumeMode};
use crate::runner::sink::{Checkpoint, JournalRecord, JournalStatus};
use serde::Deserialize;
use serde_json::Value;
use std::fmt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// No journal file.
    Missing,
    /// A bad record with good records after it. Refused; the file is kept untouched.
    CorruptMiddle {
        line: usize,
        why: String,
    },
    Io(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Missing => write!(f, "no journal"),
            LoadError::CorruptMiddle { line, why } => write!(f, "journal line {line} is corrupt and good records follow it ({why}); resume refused and the file kept as it is"),
            LoadError::Io(e) => write!(f, "journal not readable: {e}"),
        }
    }
}

/// What [`load_journal`] read.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub records: Vec<JournalRecord>,
    /// Bytes cut off a torn tail (0 when the file was clean).
    pub truncated_bytes: u64,
    /// The file's length after loading (the end of the last good record).
    pub good_len: u64,
}

/// One line as a record. `index` is the `seq` it must carry; `None` checks the shape only (used
/// after a bad line, to tell a corrupt middle from a torn tail even when a record went missing).
fn check_line(
    line: &[u8],
    index: Option<usize>,
    run_id: Option<&str>,
) -> Result<JournalRecord, String> {
    if line.is_empty() {
        return Err("empty line".into());
    }
    let v: Value = serde_json::from_slice(line).map_err(|e| format!("not JSON: {e}"))?;
    let o = v.as_object().ok_or("not an object")?;
    for k in [
        "v", "run_id", "seq", "step_id", "intent", "post", "status", "ts_phone", "ts_hub", "epoch",
        "display",
    ] {
        if !o.contains_key(k) {
            return Err(format!("missing {k}"));
        }
    }
    if o.len() != 11 {
        return Err("unexpected keys".into());
    }
    let rec: JournalRecord =
        serde_json::from_value(v).map_err(|e| format!("not a journal record: {e}"))?;
    if rec.v != 1 {
        return Err(format!("v is {}, expected 1", rec.v));
    }
    if let Some(index) = index {
        if rec.seq != index as u64 {
            return Err(format!("seq {} where {index} was expected", rec.seq));
        }
    }
    if rec.run_id.is_empty()
        || rec.step_id.is_empty()
        || rec.post.get("kind").and_then(Value::as_str).is_none()
    {
        return Err("empty run_id or step_id, or post without kind".into());
    }
    if run_id.is_some_and(|r| r != rec.run_id) {
        return Err(format!(
            "run_id {} in a journal of {}",
            rec.run_id,
            run_id.unwrap_or_default()
        ));
    }
    Ok(rec)
}

/// Read a journal back, truncating a torn tail and refusing a corrupt middle (see the module docs).
pub fn load_journal(path: &Path) -> Result<Loaded, LoadError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(LoadError::Missing),
        Err(e) => return Err(LoadError::Io(format!("{}: {e}", path.display()))),
    };
    // (start, end exclusive of '\n', terminated by '\n')
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            lines.push((start, i, true));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push((start, bytes.len(), false));
    }
    let mut records: Vec<JournalRecord> = Vec::new();
    let mut first_bad: Option<(usize, usize, String)> = None; // (line index, byte offset, why)
    for (idx, (s, e, terminated)) in lines.iter().enumerate() {
        let expect = if first_bad.is_none() {
            Some(records.len())
        } else {
            None
        };
        let parsed = if *terminated {
            check_line(
                &bytes[*s..*e],
                expect,
                records.first().map(|r| r.run_id.as_str()),
            )
        } else {
            Err("no newline: torn write".into())
        };
        match (parsed, &first_bad) {
            (Ok(rec), None) => records.push(rec),
            (Ok(_), Some((bad, _, why))) => {
                return Err(LoadError::CorruptMiddle {
                    line: bad + 1,
                    why: why.clone(),
                })
            }
            (Err(why), None) => first_bad = Some((idx, *s, why)),
            (Err(_), Some(_)) => {}
        }
    }
    let mut truncated_bytes = 0;
    let mut good_len = bytes.len() as u64;
    if let Some((_, offset, _)) = first_bad {
        good_len = offset as u64;
        truncated_bytes = bytes.len() as u64 - good_len;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| LoadError::Io(format!("{}: {e}", path.display())))?;
        f.set_len(good_len)
            .and_then(|()| f.sync_all())
            .map_err(|e| LoadError::Io(format!("truncating {}: {e}", path.display())))?;
    }
    Ok(Loaded {
        records,
        truncated_bytes,
        good_len,
    })
}

/// The engine's cursor in `post._at`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Cursor {
    pub path: Vec<usize>,
    #[serde(default)]
    pub executed: u32,
    #[serde(default)]
    pub done: u32,
    #[serde(default)]
    pub retries: u32,
    #[serde(default)]
    pub retried_steps: u32,
    #[serde(default)]
    pub relaunches: u32,
    #[serde(default)]
    pub elapsed_ms: u64,
    #[serde(default)]
    pub act: bool,
}

pub fn cursor_of(rec: &JournalRecord) -> Option<Cursor> {
    rec.post
        .get("_at")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .filter(|c: &Cursor| !c.path.is_empty())
}

/// Where a resume starts.
#[derive(Debug, Clone, PartialEq)]
pub enum Start {
    /// Nothing durable yet: run from the first step.
    Fresh,
    From(Resume),
}

/// An `interrupted` record written when a person took the phone (`post.interrupted_by: "human"`).
fn interrupted_by_human(rec: &JournalRecord) -> bool {
    rec.post.get("interrupted_by").and_then(Value::as_str) == Some("human")
}

fn has_post(rec: &JournalRecord) -> bool {
    rec.post
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|k| k != "none")
}

/// Decide where to continue (see the module docs). `human_confirmed` is passed through to the engine
/// (it lets a `checkpoint` step that waits for a person pass).
pub fn plan(
    run_id: &str,
    records: &[JournalRecord],
    checkpoint: Option<&Checkpoint>,
    human_confirmed: bool,
) -> Start {
    let last = records
        .iter()
        .rev()
        .find_map(|r| cursor_of(r).map(|c| (r, c)));
    let reads = checkpoint.map(|c| c.reads.clone()).unwrap_or_default();
    match (last, checkpoint) {
        (None, None) => Start::Fresh,
        (None, Some(cp)) => Start::From(Resume {
            from: cp.clone(),
            human_confirmed,
            mode: ResumeMode::Rerun,
        }),
        (Some((_, cur)), Some(cp)) if cp.path > cur.path => Start::From(Resume {
            from: cp.clone(),
            human_confirmed,
            mode: ResumeMode::Rerun,
        }),
        (Some((rec, cur)), _) => {
            let mode = if !cur.act {
                ResumeMode::Rerun
            } else {
                match rec.status {
                    JournalStatus::Planned | JournalStatus::Sent | JournalStatus::Failed => {
                        ResumeMode::Reverify
                    }
                    JournalStatus::Done if has_post(rec) => ResumeMode::Reverify,
                    JournalStatus::Done => ResumeMode::Skip,
                    // a person took the phone mid-step (2.15): some of its calls may have landed,
                    // so its post-condition is checked, never blindly re-sent
                    JournalStatus::Interrupted if interrupted_by_human(rec) => ResumeMode::Reverify,
                    JournalStatus::Interrupted => ResumeMode::Rerun,
                }
            };
            let from = Checkpoint {
                run_id: run_id.to_string(),
                step_id: rec.step_id.clone(),
                path: cur.path.clone(),
                reason: format!("journal seq {} ({:?})", rec.seq, rec.status).to_lowercase(),
                executed: cur.executed,
                relaunches: cur.relaunches,
                elapsed_ms: cur.elapsed_ms,
                reads,
                done: cur.done,
                retries: cur.retries,
                retried_steps: cur.retried_steps,
            };
            Start::From(Resume {
                from,
                human_confirmed,
                mode,
            })
        }
    }
}
