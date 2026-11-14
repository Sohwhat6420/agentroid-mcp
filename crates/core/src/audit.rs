//! The hash-chained audit log (D3, 10 Nov 2026), its rotation and anchor (D3b), and
//! `agentroid audit verify`.
//!
//! One JSON object per line. Every acting call the gate lets through (`allowed`) and every refusal
//! (`blocked`, `asked`) is appended BEFORE anything reaches the phone; if the line cannot be written,
//! the call is refused (no unlogged action). Reads that pass are not logged (volume).
//!
//! Each record carries `prev` (the previous record's `hash`; 64 zeros for the first) and
//! `hash = SHA-256(prev || 0x1f || canonical fields)`. The canonical form is built from the field
//! values, not from the JSON text, so a verifier does not depend on key order or spacing. Editing a
//! field, deleting, inserting or reordering lines breaks the chain at the first bad line; re-hashing an
//! edited line breaks it at the next one. Cutting lines off the END is caught against the head file
//! (`<log>.head`: `seq hash`, rewritten on every append), against the anchor, or against a head the
//! caller kept elsewhere.
//!
//! **Rotation (D3b).** With a [`Rotation`] policy the active file `audit.jsonl` is closed when it holds
//! `max_records` records or its first record is older than `max_age_ms` (7 days: the flight-recorder
//! retention), and renamed `audit.000001.jsonl`, `audit.000002.jsonl`... The chain does NOT restart:
//! `seq` keeps counting and the first record of the new file carries the previous file's final hash
//! as its `prev`, so [`verify_chain`] spans every part in order and a missing or reordered part is
//! caught at its first line. No part is ever deleted here (retention is a policy decision left open).
//!
//! **Anchor (D3b).** With an [`Anchor`] the head (`seq hash`) is also written to a second location
//! configured by path (a synced folder, a share, another machine's mount): an attacker who rewrites
//! the log and its head file still has to reach the anchor. By default a failed anchor write refuses
//! the call like a failed log write (`required`); `best_effort` keeps going and counts the failures.
//!
//! Privacy: typed text is never logged (only its length); labels are truncated to 60 characters.

use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Format marker, in every record.
pub const VERSION: &str = "agentroid-audit/1";
/// Seven days, the flight-recorder window.
pub const SEVEN_DAYS_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// What the caller supplies for one record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// Hardware serial (or adb serial when the hardware one is unknown).
    pub device: String,
    pub tool: String,
    /// `allowed`, `blocked` or `asked`.
    pub decision: String,
    /// The rule that decided: `gate`, `screen_clear`, `escape_button`, `screen:payment`, `s24`,
    /// `approved:ntfy`, `shell:factory_reset`, `file:system_path`...
    pub rule: String,
    pub reason: String,
    /// What the call would touch (label or launch target; never typed text).
    pub target: String,
    /// Screen-wide findings (`payment,destructive`) and the foreground packages.
    pub screen: String,
}

/// A stored record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub v: String,
    pub seq: u64,
    pub ts_ms: u64,
    pub entry: Entry,
    pub prev: String,
    pub hash: String,
}

fn canonical(seq: u64, ts_ms: u64, e: &Entry) -> String {
    // unit separator between fields; a field holding U+001F is escaped first so fields cannot shift
    let esc = |s: &str| s.replace('\\', "\\\\").replace('\u{1f}', "\\u001f");
    [
        VERSION.to_string(),
        seq.to_string(),
        ts_ms.to_string(),
        esc(&e.device),
        esc(&e.tool),
        esc(&e.decision),
        esc(&e.rule),
        esc(&e.reason),
        esc(&e.target),
        esc(&e.screen),
    ]
    .join("\u{1f}")
}

/// `SHA-256(prev || 0x1f || canonical)`, lower-case hex.
pub fn chain_hash(prev: &str, seq: u64, ts_ms: u64, e: &Entry) -> String {
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update([0x1f]);
    h.update(canonical(seq, ts_ms, e).as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn trunc(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

impl Record {
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "v": self.v,
            "seq": self.seq,
            "ts_ms": self.ts_ms,
            "device": self.entry.device,
            "tool": self.entry.tool,
            "decision": self.entry.decision,
            "rule": self.entry.rule,
            "reason": self.entry.reason,
            "target": self.entry.target,
            "screen": self.entry.screen,
            "prev": self.prev,
            "hash": self.hash,
        })
        .to_string()
    }

    pub fn from_json(line: &str) -> Result<Record, String> {
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
        let o = v.as_object().ok_or("not a JSON object")?;
        let s = |k: &str| -> Result<String, String> {
            o.get(k)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("missing string field {k:?}"))
        };
        let n = |k: &str| -> Result<u64, String> {
            o.get(k)
                .and_then(|x| x.as_u64())
                .ok_or_else(|| format!("missing number field {k:?}"))
        };
        Ok(Record {
            v: s("v")?,
            seq: n("seq")?,
            ts_ms: n("ts_ms")?,
            entry: Entry {
                device: s("device")?,
                tool: s("tool")?,
                decision: s("decision")?,
                rule: s("rule")?,
                reason: s("reason")?,
                target: s("target")?,
                screen: s("screen")?,
            },
            prev: s("prev")?,
            hash: s("hash")?,
        })
    }
}

/// Where and why verification failed. `line` is 1-based; `file` names the part when a chain of
/// several files was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broken {
    pub line: usize,
    pub why: String,
    pub file: Option<PathBuf>,
}

impl Broken {
    fn at(line: usize, why: impl Into<String>) -> Broken {
        Broken {
            line,
            why: why.into(),
            file: None,
        }
    }
    fn in_file(mut self, f: &Path) -> Broken {
        if self.file.is_none() {
            self.file = Some(f.to_path_buf());
        }
        self
    }
}

impl fmt::Display for Broken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(p) => write!(f, "{} line {}: {}", p.display(), self.line, self.why),
            None => write!(f, "line {}: {}", self.line, self.why),
        }
    }
}

/// The error of an append whose record IS in the log, the chain moved on from it, but whose head
/// file or anchor could not be written ("record written, anchor not"). A caller that queues
/// records to write again (the protected lease's closes) must not write this one again: tell it
/// from every other append error with [`record_written`] (review round 4e, L4).
#[derive(Debug)]
pub struct RecordWritten(pub String);

impl fmt::Display for RecordWritten {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "record written, {}", self.0)
    }
}

impl std::error::Error for RecordWritten {}

/// Is `e` an append error whose record is in the log ([`RecordWritten`])?
pub fn record_written(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|x| x.is::<RecordWritten>())
}

/// A log (or a chain of rotated logs) that verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub records: u64,
    /// Last `seq` and `hash` (`GENESIS` for an empty log).
    pub head_seq: u64,
    pub head_hash: String,
    pub allowed: u64,
    pub blocked: u64,
    pub asked: u64,
    /// `ts_ms` of the first and last records of the text checked (None when empty).
    pub first_ts_ms: Option<u64>,
    pub last_ts_ms: Option<u64>,
    /// How many files the chain spanned (1 for a single log).
    pub files: usize,
    /// The head repairs made at open ([`REPAIRED_HEAD`] records): `seq N: reason`, oldest first.
    pub repairs: Vec<String>,
}

/// The rule of the record an open appends when it brings the head file or the anchor up to the
/// log's last record ([`AuditLog::open_with`]). An unkeyed chain cannot tell a crash's leftover
/// record from one forged, correctly chained append; this record is the evidence that a record
/// was adopted, and `audit verify` lists every one (review round 4f, item 2).
pub const REPAIRED_HEAD: &str = "audit:repaired_head";

impl Verified {
    fn empty(start_seq: u64, start_prev: &str) -> Verified {
        Verified {
            records: 0,
            head_seq: start_seq,
            head_hash: start_prev.to_string(),
            allowed: 0,
            blocked: 0,
            asked: 0,
            first_ts_ms: None,
            last_ts_ms: None,
            files: 1,
            repairs: Vec::new(),
        }
    }
}

/// Verify a whole log's text that starts the chain (first record: seq 1, prev GENESIS); `head` is
/// the expected last `(seq, hash)` if one was kept.
pub fn verify_str(text: &str, head: Option<(u64, &str)>) -> Result<Verified, Broken> {
    verify_str_from(text, head, 0, GENESIS)
}

/// Verify a log's text whose first record must be `start_seq + 1` chained from `start_prev` (a
/// rotated part continues the previous part's end).
pub fn verify_str_from(
    text: &str,
    head: Option<(u64, &str)>,
    start_seq: u64,
    start_prev: &str,
) -> Result<Verified, Broken> {
    let mut prev = start_prev.to_string();
    let mut out = Verified::empty(start_seq, start_prev);
    let lines: Vec<&str> = text.split('\n').collect();
    for (expect_seq, (i, raw)) in (start_seq + 1..).zip(lines.iter().enumerate()) {
        let line = i + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if raw.is_empty() {
            if i + 1 == lines.len() {
                break; // the final newline
            }
            return Err(Broken::at(line, "empty line inside the log"));
        }
        let r = Record::from_json(raw).map_err(|why| Broken::at(line, why))?;
        if r.v != VERSION {
            return Err(Broken::at(line, format!("unknown format {:?}", r.v)));
        }
        if r.seq != expect_seq {
            return Err(Broken::at(
                line,
                format!(
                    "seq {} where {} was expected (a line was deleted, inserted or moved{})",
                    r.seq,
                    expect_seq,
                    if i == 0 && start_seq > 0 {
                        ", or a rotated part is missing"
                    } else {
                        ""
                    }
                ),
            ));
        }
        if r.prev != prev {
            return Err(Broken::at(
                line,
                if i == 0 && start_seq > 0 {
                    "prev does not match the previous part's final hash (a part was edited, replaced or is missing)"
                } else {
                    "prev does not match the previous record's hash (the previous line was edited, or lines were removed or reordered)"
                },
            ));
        }
        let want = chain_hash(&r.prev, r.seq, r.ts_ms, &r.entry);
        if r.hash != want {
            return Err(Broken::at(
                line,
                "hash does not match the record's contents (this line was edited)",
            ));
        }
        match r.entry.decision.as_str() {
            "allowed" => out.allowed += 1,
            "blocked" => out.blocked += 1,
            "asked" => out.asked += 1,
            d => return Err(Broken::at(line, format!("unknown decision {d:?}"))),
        }
        if r.entry.rule == REPAIRED_HEAD {
            out.repairs
                .push(format!("seq {}: {}", r.seq, r.entry.reason));
        }
        prev = r.hash.clone();
        out.records += 1;
        out.head_seq = r.seq;
        out.head_hash = r.hash;
        out.first_ts_ms.get_or_insert(r.ts_ms);
        out.last_ts_ms = Some(r.ts_ms);
    }
    if let Some((seq, hash)) = head {
        if seq != out.head_seq || hash != out.head_hash {
            return Err(Broken::at(
                lines.len(),
                format!(
                    "the log ends at seq {} but the head says seq {seq} ({}): records were cut off the end, or the head is stale",
                    out.head_seq,
                    trunc(hash, 12)
                ),
            ));
        }
    }
    Ok(out)
}

/// The head file next to a log: `<log>.head`.
pub fn head_path(log: &Path) -> PathBuf {
    let mut p = log.as_os_str().to_owned();
    p.push(".head");
    PathBuf::from(p)
}

/// Read a head file (`seq hash`).
pub fn read_head(path: &Path) -> io::Result<Option<(u64, String)>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let mut it = s.split_whitespace();
            let seq = it.next().and_then(|x| x.parse().ok());
            let hash = it.next().map(str::to_string);
            match (seq, hash) {
                (Some(s), Some(h)) => Ok(Some((s, h))),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "head file is not `seq hash`",
                )),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The rotated parts of `log`, oldest first: `<stem>.NNNNNN.jsonl` beside it, where `<stem>` is the
/// log's file stem. Returns `(part number, path)`.
pub fn rotated_parts(log: &Path) -> Vec<(u32, PathBuf)> {
    let Some(stem) = log.file_stem().and_then(|s| s.to_str()) else {
        return vec![];
    };
    let ext = log
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jsonl")
        .to_string();
    let dir = match log.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let prefix = format!("{stem}.");
    let suffix = format!(".{ext}");
    let mut parts: Vec<(u32, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let mid = name.strip_prefix(&prefix)?.strip_suffix(&suffix)?;
            if mid.len() == 6 && mid.chars().all(|c| c.is_ascii_digit()) {
                Some((mid.parse().ok()?, e.path()))
            } else {
                None
            }
        })
        .collect();
    parts.sort_unstable();
    parts
}

/// A chain of rotated parts plus the active log, verified end to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub all: Verified,
    /// The active (last) file alone.
    pub active: Verified,
    pub parts: Vec<PathBuf>,
}

/// Verify every rotated part of `log` in order, then `log` itself, each chained from the previous
/// one's end; then the end against the head file (when one exists) and `anchor` (when given).
pub fn verify_chain(log: &Path, anchor: Option<&Path>) -> Result<Chain, Broken> {
    let head = read_head(&head_path(log))
        .map_err(|e| Broken::at(0, format!("head file: {e}")).in_file(log))?;
    let anchor_head = match anchor {
        Some(a) => Some(
            read_head(a)
                .map_err(|e| Broken::at(0, format!("anchor: {e}")).in_file(a))?
                .ok_or_else(|| Broken::at(0, "anchor file is missing").in_file(a))?,
        ),
        None => None,
    };
    verify_chain_against(log, head, anchor_head)
}

/// [`verify_chain`] against the given head and anchor heads (`None`: not checked).
fn verify_chain_against(
    log: &Path,
    head: Option<(u64, String)>,
    anchor_head: Option<(u64, String)>,
) -> Result<Chain, Broken> {
    let parts = rotated_parts(log);
    for (expect_part, (n, p)) in (1u32..).zip(parts.iter()) {
        if *n != expect_part {
            return Err(Broken::at(
                0,
                format!("rotated part {expect_part:06} is missing (found part {n:06} next)"),
            )
            .in_file(p));
        }
    }
    let mut seq = 0u64;
    let mut prev = GENESIS.to_string();
    let mut all = Verified::empty(0, GENESIS);
    all.files = 0;
    let mut active = Verified::empty(0, GENESIS);
    let files: Vec<&Path> = parts
        .iter()
        .map(|(_, p)| p.as_path())
        .chain(std::iter::once(log))
        .collect();
    let last = files.len() - 1;
    for (i, f) in files.iter().enumerate() {
        let text = std::fs::read_to_string(f)
            .map_err(|e| Broken::at(0, format!("cannot read {}: {e}", f.display())))?;
        let this_head = if i == last {
            head.as_ref().map(|(s, h)| (*s, h.as_str()))
        } else {
            None
        };
        let v = verify_str_from(&text, this_head, seq, &prev).map_err(|b| b.in_file(f))?;
        seq = v.head_seq;
        prev = v.head_hash.clone();
        all.records += v.records;
        all.allowed += v.allowed;
        all.blocked += v.blocked;
        all.asked += v.asked;
        all.files += 1;
        all.repairs.extend(v.repairs.iter().cloned());
        if all.first_ts_ms.is_none() {
            all.first_ts_ms = v.first_ts_ms;
        }
        if v.last_ts_ms.is_some() {
            all.last_ts_ms = v.last_ts_ms;
        }
        all.head_seq = v.head_seq;
        all.head_hash = v.head_hash.clone();
        if i == last {
            active = v;
        }
    }
    if let Some((aseq, ahash)) = anchor_head {
        if aseq != all.head_seq || ahash != all.head_hash {
            return Err(Broken::at(
                0,
                format!(
                    "the chain ends at seq {} ({}) but the anchor says seq {aseq} ({}): the log or its head file was rewritten, or the anchor is stale",
                    all.head_seq,
                    trunc(&all.head_hash, 12),
                    trunc(&ahash, 12)
                ),
            )
            .in_file(log));
        }
    }
    Ok(Chain {
        all,
        active,
        parts: parts.into_iter().map(|(_, p)| p).collect(),
    })
}

/// Verify a log file, against its head file when one exists, spanning its rotated parts.
pub fn verify_file(path: &Path) -> Result<Verified, Broken> {
    verify_chain(path, None).map(|c| c.all)
}

/// An append writes the line (synced), renames the head file, then writes the anchor. A crash
/// between the sync and the head's rename leaves exactly one whole, chained record past the head
/// file and the anchor; a crash between the head's rename and the anchor's write leaves the
/// anchor alone one record behind (review round 4f, item 3). Verified without them, a log whose
/// last record, in the active file, is one of these is that crash: its chain is returned, with
/// what was behind, so the caller brings both up to it and records the repair. Any other mismatch
/// is `None`: refused as before (review round 4e, L5). The anchor is never accepted ahead of the
/// head: this write order cannot leave it there (review round 4f, item 4).
fn one_past_head(log: &Path, anchor: Option<&Path>) -> Option<(Chain, &'static str)> {
    let (hseq, hhash) = read_head(&head_path(log)).ok()??;
    let c = verify_chain_against(log, None, None).ok()?;
    let text = std::fs::read_to_string(log).ok()?;
    let last = Record::from_json(text.lines().rev().find(|l| !l.is_empty())?).ok()?;
    if last.seq != c.all.head_seq {
        return None;
    }
    // (seq, hash) is the record just before the last one
    let behind = |seq: u64, hash: &str| seq.checked_add(1) == Some(last.seq) && hash == last.prev;
    let head_behind = behind(hseq, &hhash);
    let what = match anchor {
        None if head_behind => "the head file",
        None => return None,
        Some(a) => {
            let (aseq, ahash) = read_head(a).ok()??;
            let head_level = hseq == last.seq && hhash == last.hash;
            let repairable = behind(aseq, &ahash) && (head_behind || head_level);
            if !repairable {
                return None;
            }
            if head_behind {
                "the head file and the anchor"
            } else {
                "the anchor"
            }
        }
    };
    Some((c, what))
}

/// A head rename can fail for a moment on Windows while another process (a scanner, the indexer,
/// a verifier reading it) has the file open: access denied (os error 5) or a sharing violation
/// (os error 32). It is tried again after each of these waits, then the write fails closed
/// (review round 4e, L6).
const RENAME_BACKOFF_MS: [u64; 3] = [20, 50, 100];

fn transient_rename_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
        || (cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32)))
}

/// `rename(from, to)`, tried again after each of [`RENAME_BACKOFF_MS`] while it fails with a
/// transient error.
fn rename_retrying(
    from: &Path,
    to: &Path,
    rename: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    sleep: &mut dyn FnMut(u64),
) -> io::Result<()> {
    let mut r = rename(from, to);
    for ms in RENAME_BACKOFF_MS {
        let transient = matches!(&r, Err(e) if transient_rename_error(e));
        if !transient {
            return r;
        }
        sleep(ms);
        r = rename(from, to);
    }
    r
}

/// When the active file is closed and a new one started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    /// Close the active file once it holds this many records.
    pub max_records: u64,
    /// Close the active file once its first record is this old.
    pub max_age_ms: u64,
}

impl Default for Rotation {
    fn default() -> Self {
        Rotation {
            max_records: 10_000,
            max_age_ms: SEVEN_DAYS_MS,
        }
    }
}

/// The second place the head is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub path: PathBuf,
    /// `true`: a failed anchor write refuses the call (deny by default). `false`: best effort, the
    /// failure is counted in [`AuditLog::anchor_failures`].
    pub required: bool,
}

/// How a log is opened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub rotation: Option<Rotation>,
    pub anchor: Option<Anchor>,
}

/// An open log that appends.
pub struct AuditLog {
    path: PathBuf,
    file: File,
    seq: u64,
    last: String,
    now: Box<dyn Fn() -> u64 + Send>,
    options: Options,
    /// Records in the active file and its first record's `ts_ms`.
    part_records: u64,
    part_first_ts: Option<u64>,
    /// How many rotations this log has performed (tests).
    rotations: u32,
    anchor_failures: u32,
    /// A part of a record may be in the log and could not be taken out: every later append is
    /// refused, with why (review round 4e, L3).
    poisoned: Option<String>,
    #[cfg(test)]
    faults: Faults,
}

/// Failures a unit test injects.
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
struct Faults {
    /// The line is written and flushed, then the sync fails.
    sync: bool,
    /// Taking a failed append's line out again fails.
    undo: bool,
    /// The anchor write after a record fails.
    anchor: bool,
}

#[cfg(test)]
thread_local! {
    /// Faults a unit test injects into the appends an open makes itself (the repair record).
    static OPEN_FAULTS: std::cell::Cell<Faults> = std::cell::Cell::new(Faults::default());
}

impl fmt::Debug for AuditLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AuditLog({}, seq {})", self.path.display(), self.seq)
    }
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn write_head(path: &Path, seq: u64, hash: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    // written beside it and renamed over it, so a failed write leaves the old head whole
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let r = std::fs::write(&tmp, format!("{seq} {hash}\n")).and_then(|()| {
        rename_retrying(&tmp, path, &mut |a, b| std::fs::rename(a, b), &mut |ms| {
            std::thread::sleep(std::time::Duration::from_millis(ms))
        })
    });
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

impl AuditLog {
    /// Open (or create) a log with no rotation and no anchor. An existing log is verified first
    /// (rotated parts included) and the chain continues from its last record; a broken log is NOT
    /// appended to (the error says where it broke).
    pub fn open(path: &Path) -> io::Result<AuditLog> {
        AuditLog::open_with(path, Options::default())
    }

    /// Open with a rotation policy and/or an anchor.
    pub fn open_with(path: &Path, options: Options) -> io::Result<AuditLog> {
        // an anchor that does not exist yet (first run with one configured) is written on the next
        // append; an anchor that exists must agree with the chain, or the log is not appended to
        let anchor = options
            .anchor
            .as_ref()
            .map(|a| a.path.as_path())
            .filter(|p| p.exists());
        let mut repaired: Option<(u64, String, &'static str)> = None;
        let (seq, last, part_records, part_first_ts) = if path.exists() {
            let c = match verify_chain(path, anchor) {
                Ok(c) => c,
                Err(b) => {
                    let Some((c, what)) = one_past_head(path, anchor) else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "audit log {} is broken at {b}; move it aside before starting a new one",
                                path.display()
                            ),
                        ));
                    };
                    // a crash part-way through an append: the record is adopted, and the repair
                    // record appended below brings the head file and the anchor up past it
                    repaired = Some((c.all.head_seq, c.all.head_hash.clone(), what));
                    c
                }
            };
            (
                c.all.head_seq,
                c.all.head_hash,
                c.active.records,
                c.active.first_ts_ms,
            )
        } else {
            if let Some(dir) = path.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            if !rotated_parts(path).is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "audit log {} is missing but rotated parts exist beside it; move them aside before starting a new one",
                        path.display()
                    ),
                ));
            }
            (0, GENESIS.to_string(), 0, None)
        };
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut log = AuditLog {
            path: path.to_path_buf(),
            file,
            seq,
            last,
            now: Box::new(wall_ms),
            options,
            part_records,
            part_first_ts,
            rotations: 0,
            anchor_failures: 0,
            poisoned: None,
            #[cfg(test)]
            faults: OPEN_FAULTS.with(std::cell::Cell::get),
        };
        if let Some((seq, hash, what)) = repaired {
            // the head file and the anchor go up to the adopted record first, so a repair record
            // that then fails leaves a log the next open finds whole, or (its anchor write
            // failed) with the anchor alone one behind, which it repairs again; never an anchor
            // two behind, refused for good (review round 4g, L1)
            write_head(&head_path(path), seq, &hash)?;
            if let Some(a) = &log.options.anchor {
                match write_head(&a.path, seq, &hash) {
                    Err(e) if a.required => return Err(e),
                    Err(_) => log.anchor_failures += 1,
                    Ok(()) => {}
                }
            }
            // chained like every record, so the adoption is in the log for good; a log whose
            // repair cannot be written is not opened (review round 4f, item 2)
            log.append(&Entry {
                device: String::new(),
                tool: "audit".into(),
                decision: "blocked".into(),
                rule: REPAIRED_HEAD.into(),
                reason: format!(
                    "{what}: one record behind the log at open; record seq {seq} ({hash}) was adopted (an unkeyed chain cannot tell a crash's leftover record from one forged append)"
                ),
                target: String::new(),
                screen: String::new(),
            })?;
        }
        Ok(log)
    }

    /// Write `line` to the log, flushed and synced.
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        #[cfg(test)]
        if self.faults.sync {
            return Err(io::Error::other("injected: the log could not be synced"));
        }
        self.file.sync_data()
    }

    /// Cut the log back to `len` bytes, synced (a failed append's line taken out again).
    fn cut_back(&self, len: u64) -> io::Result<()> {
        #[cfg(test)]
        if self.faults.undo {
            return Err(io::Error::other("injected: the log could not be cut back"));
        }
        OpenOptions::new()
            .write(true)
            .open(&self.path)
            .and_then(|f| f.set_len(len).and_then(|()| f.sync_data()))
    }

    /// Use another clock for `ts_ms` (tests).
    pub fn with_clock(mut self, now: Box<dyn Fn() -> u64 + Send>) -> AuditLog {
        self.now = now;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn head(&self) -> (u64, &str) {
        (self.seq, &self.last)
    }

    pub fn rotations(&self) -> u32 {
        self.rotations
    }

    pub fn anchor_failures(&self) -> u32 {
        self.anchor_failures
    }

    /// Would the next record start a new file?
    fn due_for_rotation(&self, now_ms: u64) -> bool {
        let Some(r) = self.options.rotation else {
            return false;
        };
        if self.part_records == 0 {
            return false;
        }
        self.part_records >= r.max_records
            || self
                .part_first_ts
                .is_some_and(|t0| now_ms.saturating_sub(t0) >= r.max_age_ms)
    }

    /// Close the active file as the next part and start an empty active file. The chain state
    /// (`seq`, `last`) is untouched, so the next record's `prev` is this part's final hash.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_data()?;
        let next = rotated_parts(&self.path).last().map_or(1, |(n, _)| n + 1);
        let stem = self
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("audit");
        let ext = self
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("jsonl");
        let part = self.path.with_file_name(format!("{stem}.{next:06}.{ext}"));
        if part.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("rotation target {} already exists", part.display()),
            ));
        }
        // std opens files with FILE_SHARE_DELETE on Windows, so the open handle does not block the rename
        std::fs::rename(&self.path, &part)?;
        self.file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&self.path)?;
        self.part_records = 0;
        self.part_first_ts = None;
        self.rotations += 1;
        Ok(())
    }

    /// Append one record, flushed and synced, then rewrite the head file and the anchor. Rotates
    /// first when the policy says the active file is full or old.
    pub fn append(&mut self, e: &Entry) -> io::Result<Record> {
        if let Some(why) = &self.poisoned {
            return Err(io::Error::other(format!(
                "audit log {} refuses every append: {why}; move it aside before starting a new one",
                self.path.display()
            )));
        }
        let mut e = e.clone();
        e.target = trunc(&e.target, 120);
        e.reason = trunc(&e.reason, 400);
        let seq = self.seq + 1;
        let ts_ms = (self.now)();
        if self.due_for_rotation(ts_ms) {
            self.rotate()?;
        }
        let hash = chain_hash(&self.last, seq, ts_ms, &e);
        let r = Record {
            v: VERSION.to_string(),
            seq,
            ts_ms,
            entry: e,
            prev: self.last.clone(),
            hash,
        };
        let mut line = r.to_json();
        line.push('\n');
        // A failure up to and including the head write leaves nothing behind: the line is taken
        // out again, so a caller that retries (the protected lease's close queue does) never
        // writes the record twice, and the chain never holds two records with one `seq`. Before
        // this, the line stayed while `seq` did not move, so a retry forked the chain.
        let before = std::fs::metadata(&self.path)?.len();
        // (the error, and whether the whole line was written, flushed and synced)
        let failed = match self.write_line(&line) {
            Err(err) => Some((err, false)),
            Ok(()) => write_head(&head_path(&self.path), r.seq, &r.hash)
                .err()
                .map(|err| (err, true)),
        };
        if let Some((err, whole)) = failed {
            if let Err(undo) = self.cut_back(before) {
                if !whole {
                    // a part of the line may be in the log, and it could not be taken out: no
                    // later record may follow it (review round 4e, L3)
                    let why = format!(
                        "an append failed ({err}) and what it wrote could not be taken out ({undo})"
                    );
                    self.poisoned = Some(why.clone());
                    return Err(io::Error::new(
                        err.kind(),
                        format!("{why}; the log refuses every later append"),
                    ));
                }
                // the whole line is in the log, synced, and could not be taken out: it stands,
                // so the chain moves on from it (the head file is behind it by one record, which
                // the next open brings up to it)
                self.seq = seq;
                self.last = r.hash.clone();
                self.part_records += 1;
                self.part_first_ts.get_or_insert(ts_ms);
                return Err(io::Error::new(
                    err.kind(),
                    RecordWritten(format!(
                        "head not: {err} (the record stays in the log: it could not be taken out: {undo})"
                    )),
                ));
            }
            return Err(err);
        }
        self.seq = seq;
        self.last = r.hash.clone();
        self.part_records += 1;
        self.part_first_ts.get_or_insert(ts_ms);
        if let Some(a) = &self.options.anchor {
            let written = write_head(&a.path, r.seq, &r.hash);
            #[cfg(test)]
            let written = if self.faults.anchor {
                Err(io::Error::other(
                    "injected: the anchor could not be written",
                ))
            } else {
                written
            };
            if let Err(err) = written {
                self.anchor_failures += 1;
                if a.required {
                    return Err(io::Error::new(
                        err.kind(),
                        RecordWritten(format!(
                            "anchor not: anchor {} could not be written: {err}",
                            a.path.display()
                        )),
                    ));
                }
            }
        }
        Ok(r)
    }
}

/// The head repairs a verified chain holds, one per line after the OK line (none: nothing).
fn repairs_text(repairs: &[String]) -> String {
    if repairs.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "\n{} head repair{} at open (each adopted one record the head file or the anchor had not reached):",
        repairs.len(),
        if repairs.len() == 1 { "" } else { "s" }
    );
    for r in repairs {
        out.push_str("\n  ");
        out.push_str(r);
    }
    out
}

/// `agentroid audit verify <log> [--head SEQ:HASH] [--anchor PATH]`: the command's logic, so tests
/// run it in-process too. Spans the log's rotated parts. Returns the text to print and the exit code
/// (0 verified, 1 broken, 2 usage).
pub fn verify_command(args: &[String]) -> (String, i32) {
    let usage = "usage: agentroid audit verify <log.jsonl> [--head SEQ:HASH] [--anchor PATH]\n  checks every record's hash and link across the log and its rotated parts (<stem>.NNNNNN.jsonl),\n  and the end against <log>.head (or --head) and the anchor file when --anchor is given".to_string();
    let mut path: Option<&str> = None;
    let mut head: Option<(u64, String)> = None;
    let mut anchor: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--head" => {
                let Some(v) = it.next() else {
                    return (usage, 2);
                };
                let Some((s, h)) = v.split_once(':') else {
                    return (usage, 2);
                };
                let Ok(s) = s.parse() else { return (usage, 2) };
                head = Some((s, h.to_string()));
            }
            "--anchor" => {
                let Some(v) = it.next() else {
                    return (usage, 2);
                };
                anchor = Some(PathBuf::from(v));
            }
            "-h" | "--help" => return (usage, 0),
            p if path.is_none() => path = Some(p),
            _ => return (usage, 2),
        }
    }
    let Some(p) = path else { return (usage, 2) };
    let p = Path::new(p);
    let res = match &head {
        // an explicit head checks the single file against it (the caller kept the head elsewhere)
        Some((s, h)) => match std::fs::read_to_string(p) {
            Ok(text) => verify_str(&text, Some((*s, h.as_str()))).map(|v| Chain {
                active: v.clone(),
                all: v,
                parts: vec![],
            }),
            Err(e) => Err(Broken::at(0, format!("cannot read {}: {e}", p.display()))),
        },
        None => verify_chain(p, anchor.as_deref()),
    };
    match res {
        Ok(c) => (
            format!(
                "OK {}: {} records ({} allowed, {} blocked, {} asked) across {} file{}{}, head seq {} {}{}",
                p.display(),
                c.all.records,
                c.all.allowed,
                c.all.blocked,
                c.all.asked,
                c.all.files,
                if c.all.files == 1 { "" } else { "s" },
                if c.parts.is_empty() {
                    String::new()
                } else {
                    format!(" ({} rotated)", c.parts.len())
                },
                c.all.head_seq,
                c.all.head_hash,
                if anchor.is_some() {
                    ", anchor matches"
                } else {
                    ""
                }
            ) + &repairs_text(&c.all.repairs),
            0,
        ),
        Err(b) => (format!("BROKEN {}: {b}", p.display()), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(i: u32, d: &str) -> Entry {
        Entry {
            device: "DEVICE0000EXAMPLE".into(),
            tool: "tap".into(),
            decision: d.into(),
            rule: "screen:payment".into(),
            reason: format!("case {i}"),
            target: "\"Pay now\"".into(),
            screen: "payment".into(),
        }
    }

    fn write(dir: &Path, n: u32) -> PathBuf {
        let p = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(head_path(&p));
        let mut log = AuditLog::open(&p).unwrap().with_clock(Box::new(|| 1_000));
        for i in 0..n {
            log.append(&entry(i, if i % 2 == 0 { "blocked" } else { "allowed" }))
                .unwrap();
        }
        p
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentroid-audit-unit-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_clean_log_verifies_and_reopening_continues_the_chain() {
        let d = tmp("clean");
        let p = write(&d, 5);
        let v = verify_file(&p).unwrap();
        assert_eq!((v.records, v.blocked, v.allowed), (5, 3, 2));
        let mut log = AuditLog::open(&p).unwrap();
        log.append(&entry(9, "asked")).unwrap();
        assert_eq!(verify_file(&p).unwrap().records, 6);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_edited_field_breaks_at_that_line_and_a_rehash_breaks_at_the_next() {
        let d = tmp("edit");
        let p = write(&d, 4);
        let text = std::fs::read_to_string(&p).unwrap();
        let edited = text.replacen("\"decision\":\"blocked\"", "\"decision\":\"allowed\"", 1);
        assert_ne!(edited, text);
        assert_eq!(verify_str(&edited, None).unwrap_err().line, 1);
        // the attacker recomputes line 1's hash: line 2's prev no longer matches
        let mut lines: Vec<String> = edited.lines().map(str::to_string).collect();
        let mut r = Record::from_json(&lines[0]).unwrap();
        r.hash = chain_hash(&r.prev, r.seq, r.ts_ms, &r.entry);
        lines[0] = r.to_json();
        let rehashed = lines.join("\n") + "\n";
        assert_eq!(verify_str(&rehashed, None).unwrap_err().line, 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn deleted_reordered_and_truncated_logs_are_caught() {
        let d = tmp("cut");
        let p = write(&d, 4);
        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let deleted = format!("{}\n{}\n{}\n", lines[0], lines[2], lines[3]);
        assert_eq!(verify_str(&deleted, None).unwrap_err().line, 2);
        let swapped = format!("{}\n{}\n{}\n{}\n", lines[0], lines[2], lines[1], lines[3]);
        assert_eq!(verify_str(&swapped, None).unwrap_err().line, 2);
        // the last line cut off: fine without a head, caught with one
        let cut = format!("{}\n{}\n{}\n", lines[0], lines[1], lines[2]);
        assert!(verify_str(&cut, None).is_ok());
        std::fs::write(&p, &cut).unwrap();
        assert!(verify_file(&p).unwrap_err().why.contains("cut off"));
        // and a broken log is never appended to
        assert!(AuditLog::open(&p).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_command_reports_ok_broken_and_usage() {
        let d = tmp("cmd");
        let p = write(&d, 3);
        let ps = p.display().to_string();
        let (out, code) = verify_command(std::slice::from_ref(&ps));
        assert_eq!(code, 0, "{out}");
        assert!(out.starts_with("OK "));
        let text = std::fs::read_to_string(&p)
            .unwrap()
            .replacen("case 1", "case 7", 1);
        std::fs::write(&p, text).unwrap();
        let (out, code) = verify_command(&[ps]);
        assert_eq!(code, 1);
        assert!(out.contains("line 2"), "{out}");
        assert_eq!(verify_command(&[]).1, 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    // ------------------------------------------------------------------ rotation (D3b)

    fn rotating(dir: &Path, max_records: u64, anchor: Option<Anchor>) -> (PathBuf, AuditLog) {
        let p = dir.join("audit.jsonl");
        let log = AuditLog::open_with(
            &p,
            Options {
                rotation: Some(Rotation {
                    max_records,
                    max_age_ms: SEVEN_DAYS_MS,
                }),
                anchor,
            },
        )
        .unwrap();
        (p, log)
    }

    #[test]
    fn rotation_by_count_chains_parts_and_verify_spans_them() {
        let d = tmp("rotate-count");
        let (p, log) = rotating(&d, 3, None);
        let mut log = log.with_clock(Box::new(|| 7_000));
        for i in 0..8 {
            log.append(&entry(i, "allowed")).unwrap();
        }
        // 8 records at 3 per file: parts 1 and 2 hold 3 each, the active file holds 2
        assert_eq!(log.rotations(), 2);
        let parts = rotated_parts(&p);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].1.ends_with("audit.000001.jsonl"));
        assert!(parts[1].1.ends_with("audit.000002.jsonl"));
        let c = verify_chain(&p, None).unwrap();
        assert_eq!(c.all.records, 8);
        assert_eq!(c.all.files, 3);
        assert_eq!(c.all.head_seq, 8);
        assert_eq!(c.active.records, 2);
        // the first record of part 2 carries part 1's final hash as prev, and seq runs on
        let p1 = std::fs::read_to_string(&parts[0].1).unwrap();
        let p2 = std::fs::read_to_string(&parts[1].1).unwrap();
        let last1 = Record::from_json(p1.lines().last().unwrap()).unwrap();
        let first2 = Record::from_json(p2.lines().next().unwrap()).unwrap();
        assert_eq!(first2.prev, last1.hash);
        assert_eq!(first2.seq, last1.seq + 1);
        assert_eq!(first2.seq, 4);
        // reopening continues the chain and the counts of the active file
        drop(log);
        let mut log = AuditLog::open_with(
            &p,
            Options {
                rotation: Some(Rotation {
                    max_records: 3,
                    max_age_ms: SEVEN_DAYS_MS,
                }),
                anchor: None,
            },
        )
        .unwrap()
        // the same fake clock: with the wall clock the 7-day rule would rotate at once
        .with_clock(Box::new(|| 7_000));
        assert_eq!(log.head().0, 8);
        log.append(&entry(8, "blocked")).unwrap(); // active: 3 records
        log.append(&entry(9, "blocked")).unwrap(); // rotates: part 3
        assert_eq!(rotated_parts(&p).len(), 3);
        let c = verify_chain(&p, None).unwrap();
        assert_eq!(
            (c.all.records, c.all.head_seq, c.active.records),
            (10, 10, 1)
        );
        let (out, code) = verify_command(&[p.display().to_string()]);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("across 4 files (3 rotated)"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rotation_by_age() {
        let d = tmp("rotate-age");
        let p = d.join("audit.jsonl");
        let t = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1_000));
        let t2 = t.clone();
        let mut log = AuditLog::open_with(
            &p,
            Options {
                rotation: Some(Rotation {
                    max_records: 1_000,
                    max_age_ms: SEVEN_DAYS_MS,
                }),
                anchor: None,
            },
        )
        .unwrap()
        .with_clock(Box::new(move || {
            t2.load(std::sync::atomic::Ordering::Relaxed)
        }));
        log.append(&entry(0, "allowed")).unwrap();
        t.store(
            1_000 + SEVEN_DAYS_MS - 1,
            std::sync::atomic::Ordering::Relaxed,
        );
        log.append(&entry(1, "allowed")).unwrap();
        assert_eq!(log.rotations(), 0, "still inside seven days");
        t.store(1_000 + SEVEN_DAYS_MS, std::sync::atomic::Ordering::Relaxed);
        log.append(&entry(2, "allowed")).unwrap();
        assert_eq!(log.rotations(), 1);
        assert_eq!(verify_chain(&p, None).unwrap().all.records, 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_edited_or_swapped_part_breaks_the_chain() {
        let d = tmp("rotate-tamper");
        let (p, log) = rotating(&d, 2, None);
        let mut log = log.with_clock(Box::new(|| 7_000));
        for i in 0..6 {
            log.append(&entry(i, "allowed")).unwrap();
        }
        drop(log);
        let parts = rotated_parts(&p);
        assert_eq!(parts.len(), 2);
        // edit a record in part 1: caught in part 1
        let p1 = parts[0].1.clone();
        let orig = std::fs::read_to_string(&p1).unwrap();
        std::fs::write(&p1, orig.replacen("case 0", "case 9", 1)).unwrap();
        let b = verify_chain(&p, None).unwrap_err();
        assert_eq!(b.file.as_deref(), Some(p1.as_path()));
        assert_eq!(b.line, 1);
        std::fs::write(&p1, &orig).unwrap();
        // replace part 1 with a fresh, self-consistent chain of the same length: part 2's first line breaks
        let fake = {
            let fd = tmp("rotate-fake");
            let fp = write(&fd, 2);
            let t = std::fs::read_to_string(&fp).unwrap();
            let _ = std::fs::remove_dir_all(&fd);
            t
        };
        std::fs::write(&p1, &fake).unwrap();
        let b = verify_chain(&p, None).unwrap_err();
        assert_eq!(b.file.as_deref(), Some(parts[1].1.as_path()));
        assert!(b.why.contains("previous part"), "{b}");
        std::fs::write(&p1, &orig).unwrap();
        // remove part 1: the gap is named before anything is read
        std::fs::rename(&p1, d.join("elsewhere.jsonl")).unwrap();
        let b = verify_chain(&p, None).unwrap_err();
        assert!(b.why.contains("part 000001 is missing"), "{b}");
        std::fs::rename(d.join("elsewhere.jsonl"), &p1).unwrap();
        // remove the LAST part (so the active file follows part 1 directly): the active file's first line breaks
        let p2 = parts[1].1.clone();
        std::fs::rename(&p2, d.join("gone.jsonl")).unwrap();
        let b = verify_chain(&p, None).unwrap_err();
        assert_eq!(b.file.as_deref(), Some(p.as_path()));
        assert!(b.why.contains("rotated part is missing"), "{b}");
        std::fs::rename(d.join("gone.jsonl"), &p2).unwrap();
        assert!(verify_chain(&p, None).is_ok());
        // a broken chain is never appended to
        std::fs::write(&p1, orig.replacen("case 0", "case 9", 1)).unwrap();
        assert!(AuditLog::open(&p).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    // ------------------------------------------------------------------ a failed append

    /// Review round 4c: an append whose head file cannot be written reports a failure AND leaves
    /// nothing behind, so a retry writes the record once and the chain never forks (Windows and
    /// Linux alike: a directory in the head file's place refuses the write on both).
    #[test]
    fn a_failed_append_leaves_nothing_so_a_retry_writes_the_record_once() {
        let d = tmp("failed-append");
        let p = d.join("audit.jsonl");
        let mut log = AuditLog::open(&p).unwrap();
        log.append(&entry(0, "allowed")).unwrap();
        let head = head_path(&p);
        std::fs::remove_file(&head).unwrap();
        std::fs::create_dir_all(&head).unwrap();
        for _ in 0..3 {
            let err = log.append(&entry(1, "blocked")).unwrap_err();
            assert!(!record_written(&err), "taken out again: {err}");
        }
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            text.lines().count(),
            1,
            "nothing of the failed appends: {text}"
        );
        assert_eq!(log.head().0, 1);
        std::fs::remove_dir_all(&head).unwrap();
        let r = log.append(&entry(1, "blocked")).unwrap();
        assert_eq!(r.seq, 2);
        let c = verify_chain(&p, None).unwrap();
        assert_eq!(c.all.records, 2);
        assert_eq!(read_head(&head).unwrap().unwrap().0, 2);
        assert!(
            !d.join("audit.jsonl.head.tmp").exists(),
            "no temporary head left"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // ------------------------------------------------------------------ anchor (D3b)

    #[test]
    fn the_anchor_catches_a_rewritten_log_and_head() {
        let d = tmp("anchor");
        let off = tmp("anchor-off-machine").join("agentroid.head");
        let (p, log) = rotating(
            &d,
            1_000,
            Some(Anchor {
                path: off.clone(),
                required: true,
            }),
        );
        let mut log = log.with_clock(Box::new(|| 9_000));
        for i in 0..4 {
            log.append(&entry(i, "blocked")).unwrap();
        }
        drop(log);
        assert_eq!(read_head(&off).unwrap().unwrap().0, 4);
        let (out, code) = verify_command(&[
            p.display().to_string(),
            "--anchor".into(),
            off.display().to_string(),
        ]);
        assert_eq!(code, 0, "{out}");
        assert!(out.ends_with("anchor matches"), "{out}");
        // the attacker rewrites the whole log AND its head file: consistent on the machine
        let text = std::fs::read_to_string(&p).unwrap();
        let three: Vec<&str> = text.lines().take(3).collect();
        std::fs::write(&p, three.join("\n") + "\n").unwrap();
        let last = Record::from_json(three[2]).unwrap();
        write_head(&head_path(&p), last.seq, &last.hash).unwrap();
        assert!(
            verify_chain(&p, None).is_ok(),
            "without the anchor the rewrite passes"
        );
        let b = verify_chain(&p, Some(&off)).unwrap_err();
        assert!(b.why.contains("anchor says seq 4"), "{b}");
        let (out, code) = verify_command(&[
            p.display().to_string(),
            "--anchor".into(),
            off.display().to_string(),
        ]);
        assert_eq!(code, 1, "{out}");
        // and the daemon would refuse to open it with that anchor
        assert!(AuditLog::open_with(
            &p,
            Options {
                rotation: None,
                anchor: Some(Anchor {
                    path: off.clone(),
                    required: true
                })
            }
        )
        .is_err());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(off.parent().unwrap());
    }

    #[test]
    fn a_required_anchor_that_cannot_be_written_refuses_and_best_effort_counts() {
        let d = tmp("anchor-fail");
        let bad = d.join("a-file-not-a-dir");
        std::fs::write(&bad, "x").unwrap();
        let unwritable = bad.join("head"); // parent is a file: every write fails
        let (p, mut log) = rotating(
            &d,
            1_000,
            Some(Anchor {
                path: unwritable.clone(),
                required: true,
            }),
        );
        let err = log.append(&entry(0, "allowed")).unwrap_err();
        assert!(err.to_string().contains("anchor"), "{err}");
        // review round 4e, L4: a kind of its own, so a caller does not write the record again
        assert!(record_written(&err), "{err}");
        assert!(
            err.to_string().contains("record written, anchor not"),
            "{err}"
        );
        // the record itself was written and the head updated: the log stays consistent
        assert_eq!(verify_chain(&p, None).unwrap().all.records, 1);
        drop(log);
        let mut log = AuditLog::open_with(
            &p,
            Options {
                rotation: None,
                anchor: Some(Anchor {
                    path: unwritable,
                    required: false,
                }),
            },
        )
        .unwrap();
        log.append(&entry(1, "allowed")).unwrap();
        assert_eq!(log.anchor_failures(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Review round 4e, L3: a line whose write, flush or sync failed and that cannot be taken out
    /// poisons the log (every later append is refused); a whole, synced line whose head write
    /// failed stays, and the chain moves on from it.
    #[test]
    fn a_part_record_that_cannot_be_taken_out_poisons_the_log() {
        let d = tmp("poison");
        let p = d.join("audit.jsonl");
        let mut log = AuditLog::open(&p).unwrap();
        log.append(&entry(0, "allowed")).unwrap();
        log.faults = Faults {
            sync: true,
            undo: true,
            ..Faults::default()
        };
        let err = log.append(&entry(1, "blocked")).unwrap_err();
        assert!(!record_written(&err), "{err}");
        assert!(
            err.to_string().contains("refuses every later append"),
            "{err}"
        );
        log.faults = Faults::default();
        for _ in 0..2 {
            let err = log.append(&entry(2, "blocked")).unwrap_err();
            assert!(err.to_string().contains("refuses every append"), "{err}");
        }
        assert_eq!(log.head().0, 1, "the chain did not move");
        // the sync fails but the line is taken out: not poisoned, and a retry writes it once
        let d2 = tmp("poison-undone");
        let p2 = d2.join("audit.jsonl");
        let mut log = AuditLog::open(&p2).unwrap();
        log.faults.sync = true;
        log.append(&entry(0, "allowed")).unwrap_err();
        log.faults.sync = false;
        log.append(&entry(0, "allowed")).unwrap();
        assert_eq!(verify_chain(&p2, None).unwrap().all.records, 1);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&d2);
    }

    #[test]
    fn a_whole_record_whose_head_failed_and_cannot_be_taken_out_stays() {
        let d = tmp("stays");
        let p = d.join("audit.jsonl");
        let mut log = AuditLog::open(&p).unwrap();
        log.append(&entry(0, "allowed")).unwrap();
        let head = head_path(&p);
        std::fs::remove_file(&head).unwrap();
        std::fs::create_dir_all(&head).unwrap();
        log.faults.undo = true;
        let err = log.append(&entry(1, "blocked")).unwrap_err();
        assert!(record_written(&err), "{err}");
        assert!(
            err.to_string().contains("record written, head not"),
            "{err}"
        );
        assert_eq!(
            log.head().0,
            2,
            "the chain moves on from the record that stays"
        );
        log.faults.undo = false;
        std::fs::remove_dir_all(&head).unwrap();
        assert_eq!(log.append(&entry(2, "blocked")).unwrap().seq, 3);
        assert_eq!(verify_chain(&p, None).unwrap().all.records, 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Review round 4e, L5, and round 4f, item 2: one whole record past the head (a crash between
    /// the log's sync and the head's rename) is adopted on open, and a chained repair record
    /// brings the head up past it and names it; `audit verify` lists the repair. Two records past
    /// the head are refused, as is a head ahead of the log.
    #[test]
    fn one_record_past_the_head_is_repaired_on_open_and_nothing_else_is() {
        let d = tmp("one-past");
        let p = d.join("audit.jsonl");
        let head = head_path(&p);
        let mut log = AuditLog::open(&p).unwrap();
        for i in 0..3 {
            log.append(&entry(i, "allowed")).unwrap();
        }
        let at3 = std::fs::read_to_string(&head).unwrap();
        let adopted = log.append(&entry(3, "blocked")).unwrap();
        drop(log);
        std::fs::write(&head, &at3).unwrap();
        let mut log = AuditLog::open(&p).expect("one record past the head opens");
        assert_eq!(log.head().0, 5, "record 4 adopted, then the repair record");
        assert_eq!(
            read_head(&head).unwrap().unwrap().0,
            5,
            "the head is repaired"
        );
        let c = verify_chain(&p, None).unwrap();
        assert_eq!(c.all.repairs.len(), 1, "{:?}", c.all.repairs);
        let repair = &c.all.repairs[0];
        assert!(repair.starts_with("seq 5: the head file: "), "{repair}");
        assert!(
            repair.contains(&format!("record seq 4 ({})", adopted.hash)),
            "{repair}"
        );
        let (out, code) = verify_command(&[p.display().to_string()]);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("1 head repair at open"), "{out}");
        assert!(out.contains(&adopted.hash), "{out}");
        assert_eq!(log.append(&entry(4, "allowed")).unwrap().seq, 6);
        let at6 = std::fs::read_to_string(&head).unwrap();
        log.append(&entry(5, "allowed")).unwrap();
        log.append(&entry(6, "allowed")).unwrap();
        drop(log);
        // two records past the head: refused, and the head is left as it was
        std::fs::write(&head, &at6).unwrap();
        assert!(AuditLog::open(&p).is_err(), "two past the head");
        assert_eq!(std::fs::read_to_string(&head).unwrap(), at6);
        // a head ahead of the log (records cut off the end): refused
        std::fs::write(&head, "9 ".to_string() + &"a".repeat(64) + "\n").unwrap();
        assert!(AuditLog::open(&p).is_err(), "a head ahead of the log");
        // one past a head whose hash is not the previous record's: refused
        std::fs::write(&head, "7 ".to_string() + &"b".repeat(64) + "\n").unwrap();
        assert!(
            AuditLog::open(&p).is_err(),
            "a head that is not the previous record"
        );
        // a refused open appends nothing
        assert_eq!(verify_chain_against(&p, None, None).unwrap().all.records, 8);
        // with an anchor one behind too, both are brought up by the repair record
        let d2 = tmp("one-past-anchor");
        let p2 = d2.join("audit.jsonl");
        let anchor = d2.join("anchor.head");
        let opts = || Options {
            rotation: None,
            anchor: Some(Anchor {
                path: anchor.clone(),
                required: true,
            }),
        };
        let mut log = AuditLog::open_with(&p2, opts()).unwrap();
        log.append(&entry(0, "allowed")).unwrap();
        let at1 = std::fs::read_to_string(&anchor).unwrap();
        log.append(&entry(1, "allowed")).unwrap();
        drop(log);
        std::fs::write(head_path(&p2), &at1).unwrap();
        std::fs::write(&anchor, &at1).unwrap();
        AuditLog::open_with(&p2, opts()).expect("head and anchor one behind");
        assert_eq!(read_head(&anchor).unwrap().unwrap().0, 3);
        let c = verify_chain(&p2, Some(&anchor)).unwrap();
        assert!(
            c.all.repairs.len() == 1
                && c.all.repairs[0].starts_with("seq 3: the head file and the anchor: "),
            "{:?}",
            c.all.repairs
        );
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&d2);
    }

    /// Review round 4f, items 3 and 4: a crash between the head's rename and the anchor's write
    /// (the head at the last record, the anchor one behind) is repaired like a crash before the
    /// head's rename, with a repair record. An anchor ahead of the head (this write order cannot
    /// leave one) is refused, as is an anchor two behind.
    #[test]
    fn a_crash_before_the_anchor_write_is_repaired_and_an_anchor_ahead_is_not() {
        let d = tmp("anchor-behind");
        let p = d.join("audit.jsonl");
        let head = head_path(&p);
        let anchor = d.join("anchor.head");
        let opts = || Options {
            rotation: None,
            anchor: Some(Anchor {
                path: anchor.clone(),
                required: true,
            }),
        };
        let mut log = AuditLog::open_with(&p, opts()).unwrap();
        log.append(&entry(0, "allowed")).unwrap();
        let at1 = std::fs::read_to_string(&anchor).unwrap();
        let adopted = log.append(&entry(1, "allowed")).unwrap();
        drop(log);
        // the head at record 2, the anchor still at record 1
        std::fs::write(&anchor, &at1).unwrap();
        let log = AuditLog::open_with(&p, opts()).expect("the anchor one behind the head");
        assert_eq!(log.head().0, 3, "record 2 adopted, then the repair record");
        drop(log);
        assert_eq!(read_head(&anchor).unwrap().unwrap().0, 3);
        let c = verify_chain(&p, Some(&anchor)).unwrap();
        assert_eq!(c.all.repairs.len(), 1, "{:?}", c.all.repairs);
        let repair = &c.all.repairs[0];
        assert!(repair.starts_with("seq 3: the anchor: "), "{repair}");
        assert!(
            repair.contains(&format!("record seq 2 ({})", adopted.hash)),
            "{repair}"
        );
        // the anchor at the last record and the head one behind it: refused, nothing written
        let mut log = AuditLog::open_with(&p, opts()).unwrap();
        let at3 = std::fs::read_to_string(&head).unwrap();
        log.append(&entry(2, "allowed")).unwrap();
        drop(log);
        std::fs::write(&head, &at3).unwrap();
        assert!(
            AuditLog::open_with(&p, opts()).is_err(),
            "an anchor ahead of the head"
        );
        assert_eq!(std::fs::read_to_string(&head).unwrap(), at3);
        // the anchor two behind the head and the log: refused
        let at4 = std::fs::read_to_string(&anchor).unwrap();
        std::fs::write(&head, &at4).unwrap();
        let mut log = AuditLog::open_with(&p, opts()).unwrap();
        assert_eq!(log.append(&entry(3, "allowed")).unwrap().seq, 5);
        drop(log);
        std::fs::write(&anchor, &at3).unwrap();
        assert!(
            AuditLog::open_with(&p, opts()).is_err(),
            "an anchor two behind"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Review round 4g, L1: a repair record that cannot be written leaves a log the next open
    /// can take. Its anchor write fails: the head file and the anchor were brought up to the
    /// adopted record first, so the anchor is one behind the repair record, and the next open
    /// repairs that too. Its line fails: the log is whole at the adopted record, and opens.
    #[test]
    fn a_repair_record_that_fails_leaves_a_log_the_next_open_takes() {
        let d = tmp("repair-fails");
        let p = d.join("audit.jsonl");
        let head = head_path(&p);
        let anchor = d.join("anchor.head");
        let opts = || Options {
            rotation: None,
            anchor: Some(Anchor {
                path: anchor.clone(),
                required: true,
            }),
        };
        let behind = |n: u32| {
            // the head file and the anchor one record behind the log's last record
            let mut log = AuditLog::open_with(&p, opts()).unwrap();
            log.append(&entry(n, "allowed")).unwrap();
            let before = std::fs::read_to_string(&head).unwrap();
            log.append(&entry(n + 1, "allowed")).unwrap();
            drop(log);
            std::fs::write(&head, &before).unwrap();
            std::fs::write(&anchor, &before).unwrap();
        };
        let set = |f: Faults| OPEN_FAULTS.with(|c| c.set(f));
        // 1. the repair record's anchor write fails
        behind(0);
        set(Faults {
            anchor: true,
            ..Faults::default()
        });
        let err = AuditLog::open_with(&p, opts()).unwrap_err();
        set(Faults::default());
        assert!(record_written(&err), "{err}");
        assert_eq!(
            read_head(&anchor).unwrap().unwrap().0,
            2,
            "up to the adopted record"
        );
        let log = AuditLog::open_with(&p, opts()).expect("the anchor one behind is repaired");
        assert_eq!(
            log.head().0,
            4,
            "the first repair record stays, then a second"
        );
        drop(log);
        let c = verify_chain(&p, Some(&anchor)).unwrap();
        assert_eq!(c.all.repairs.len(), 2, "{:?}", c.all.repairs);
        assert!(
            c.all.repairs[1].starts_with("seq 4: the anchor: "),
            "{:?}",
            c.all.repairs
        );
        // 2. the repair record's line fails (and is taken out)
        behind(4);
        set(Faults {
            sync: true,
            ..Faults::default()
        });
        assert!(AuditLog::open_with(&p, opts()).is_err());
        set(Faults::default());
        let log = AuditLog::open_with(&p, opts()).expect("whole at the adopted record");
        assert_eq!(log.head().0, 6);
        drop(log);
        assert!(verify_chain(&p, Some(&anchor)).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Review round 4e, L6: a head rename refused for a moment (access denied, a sharing
    /// violation) is tried again after 20, 50 and 100 ms, then fails closed; any other error is
    /// not retried.
    #[test]
    fn a_head_rename_refused_for_a_moment_is_retried_then_fails_closed() {
        let (a, b) = (Path::new("from"), Path::new("to"));
        let run = |fails: usize, kind: fn() -> io::Error| {
            let mut calls = 0usize;
            let mut slept = vec![];
            let r = rename_retrying(
                a,
                b,
                &mut |_, _| {
                    calls += 1;
                    if calls <= fails {
                        Err(kind())
                    } else {
                        Ok(())
                    }
                },
                &mut |ms| slept.push(ms),
            );
            (r.is_ok(), calls, slept)
        };
        let denied = || io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(run(0, denied), (true, 1, vec![]));
        assert_eq!(run(2, denied), (true, 3, vec![20, 50]));
        assert_eq!(run(9, denied), (false, 4, vec![20, 50, 100]));
        let missing = || io::Error::new(io::ErrorKind::NotFound, "gone");
        assert_eq!(run(9, missing), (false, 1, vec![]));
        if cfg!(windows) {
            let sharing = || io::Error::from_raw_os_error(32);
            assert_eq!(run(1, sharing), (true, 2, vec![20]));
        }
    }
}
