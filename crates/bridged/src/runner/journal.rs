//! The run journal on disk (task 2.10 slice b: "journal with group-commit
//! fsync").
//!
//! One directory per job under a configurable jobs directory:
//!
//! ```text
//! <jobs_dir>/<run_id>/journal.ndjson     one schemas/journal.json record per line
//! <jobs_dir>/<run_id>/progress.ndjson    one schemas/progress.json event per line (2.11a's path)
//! <jobs_dir>/<run_id>/checkpoint.json    the latest engine checkpoint (pauses, checkpoint steps)
//! ```
//!
//! **Progress (2.11a).** Each progress event is appended to `progress.ndjson` as one whole line the
//! moment the engine emits it, so a tail or Monitor sees it within the plan's 2 s (the file is
//! opened with Rust's default sharing, so readers may open it while the job runs). Its fsync is
//! grouped with the next commit.
//!
//! **Group commit.** Journal records are buffered in memory and reach the file only in
//! [`JournalWriter::commit`], which writes the buffer whole and then `sync_data`s it. A commit
//! happens when a `sent` record arrives (so `planned` + `sent` share one fsync), when the batch is
//! full, when the oldest buffered line is older than the delay, on [`JournalWriter::commit`] and on
//! drop. The engine sends nothing to the phone until `journal(sent)` returned `Ok`, so **an acting
//! step's intent is durable before it is sent**. The bytes known to be fsynced are published through
//! [`JournalWriter::durable_handle`]: a simulated crash keeps exactly that prefix of the file.
//!
//! Each record is at most [`MAX_RECORD_BYTES`] (the plan's "at most 1 KB per step"); a longer intent
//! is shortened, never the cursor. Sequence numbers are the writer's own and continue across
//! resumes, so one file per job always counts 0, 1, 2, ...

use crate::runner::resume::{load_journal, LoadError};
use crate::runner::sink::{
    Checkpoint, CheckpointStore, JournalRecord, JournalStatus, ProgressEvent, RunSink,
};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub const JOURNAL_FILE: &str = "journal.ndjson";
pub const PROGRESS_FILE: &str = "progress.ndjson";
pub const CHECKPOINT_FILE: &str = "checkpoint.json";
/// "at most 1 KB per step".
pub const MAX_RECORD_BYTES: usize = 1024;

/// A run id becomes a directory name: 1 to 64 of `A-Z a-z 0-9 _ - .`, not starting with a dot.
pub fn valid_run_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// `<jobs_dir>/<run_id>`, refusing ids that are not plain names.
pub fn run_dir(jobs_dir: &Path, run_id: &str) -> Result<PathBuf, String> {
    if !valid_run_id(run_id) {
        return Err(format!(
            "{run_id:?} is not a valid run id (1 to 64 of A-Z a-z 0-9 _ - .)"
        ));
    }
    Ok(jobs_dir.join(run_id))
}

/// When a batch is committed besides on every `sent` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupCommit {
    pub max_batch: usize,
    pub max_delay_ms: u64,
}

impl Default for GroupCommit {
    fn default() -> Self {
        GroupCommit {
            max_batch: 64,
            max_delay_ms: 200,
        }
    }
}

/// Commit timings, for the plan's "journal p99 at most 5 ms".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitStats {
    pub commits: u64,
    pub lines: u64,
    /// Progress events appended (each reaches the file when emitted; 2.11a).
    pub progress_lines: u64,
    /// Microseconds per commit (write plus fsync of both files), most recent last, at most 10,000.
    pub micros: Vec<u64>,
}

impl CommitStats {
    /// The `q` quantile (0..=1) of commit time in microseconds.
    pub fn quantile_us(&self, q: f64) -> u64 {
        if self.micros.is_empty() {
            return 0;
        }
        let mut v = self.micros.clone();
        v.sort_unstable();
        let i = ((v.len() as f64 - 1.0) * q.clamp(0.0, 1.0)).round() as usize;
        v[i.min(v.len() - 1)]
    }
}

/// The [`RunSink`] that persists a run: NDJSON with group-commit fsync.
pub struct JournalWriter {
    dir: PathBuf,
    journal: File,
    progress: File,
    jbuf: Vec<u8>,
    /// Progress lines written since the last fsync (they are already in the file).
    pdirty: bool,
    pending: usize,
    oldest: Option<Instant>,
    next_jseq: u64,
    next_pseq: u64,
    /// Journal bytes known to be on disk (written and fsynced).
    durable: Arc<AtomicU64>,
    policy: GroupCommit,
    /// The first commit failure. After it every journal call fails, so nothing more is sent.
    failed: Option<String>,
    pub stats: CommitStats,
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn count_lines(path: &Path) -> u64 {
    std::fs::read(path)
        .map(|b| b.iter().filter(|c| **c == b'\n').count() as u64)
        .unwrap_or(0)
}

impl JournalWriter {
    /// Open (or create) `<jobs_dir>/<run_id>/` for writing. An existing journal is loaded first: a
    /// torn tail is truncated to its last good record, a corrupt middle refuses (the file is kept),
    /// and the sequence numbers continue after the last record.
    pub fn open(
        jobs_dir: &Path,
        run_id: &str,
        policy: GroupCommit,
    ) -> Result<JournalWriter, String> {
        let dir = run_dir(jobs_dir, run_id)?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let jpath = dir.join(JOURNAL_FILE);
        let (next_jseq, good_len) = match load_journal(&jpath) {
            Ok(l) => (l.records.len() as u64, l.good_len),
            Err(LoadError::Missing) => (0, 0),
            Err(e) => return Err(e.to_string()),
        };
        let ppath = dir.join(PROGRESS_FILE);
        let next_pseq = count_lines(&ppath);
        let journal = open_append(&jpath).map_err(|e| format!("{}: {e}", jpath.display()))?;
        let progress = open_append(&ppath).map_err(|e| format!("{}: {e}", ppath.display()))?;
        Ok(JournalWriter {
            dir,
            journal,
            progress,
            jbuf: Vec::new(),
            pdirty: false,
            pending: 0,
            oldest: None,
            next_jseq,
            next_pseq,
            durable: Arc::new(AtomicU64::new(good_len)),
            policy,
            failed: None,
            stats: CommitStats::default(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn journal_path(&self) -> PathBuf {
        self.dir.join(JOURNAL_FILE)
    }
    pub fn progress_path(&self) -> PathBuf {
        self.dir.join(PROGRESS_FILE)
    }
    /// Records written to this journal so far (durable or still buffered).
    pub fn next_seq(&self) -> u64 {
        self.next_jseq
    }
    /// Shared count of journal bytes that are on disk. A crash loses everything after it.
    pub fn durable_handle(&self) -> Arc<AtomicU64> {
        self.durable.clone()
    }

    /// Write the journal buffer whole, then fsync it; fsync the progress lines already appended.
    /// On failure the writer stays failed.
    pub fn commit(&mut self) -> Result<(), String> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        if self.jbuf.is_empty() && !self.pdirty {
            return Ok(());
        }
        let t = Instant::now();
        let res = (|| -> std::io::Result<()> {
            if !self.jbuf.is_empty() {
                self.journal.write_all(&self.jbuf)?;
                self.journal.sync_data()?;
            }
            if self.pdirty {
                self.progress.sync_data()?;
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                self.durable
                    .fetch_add(self.jbuf.len() as u64, Ordering::SeqCst);
                self.stats.commits += 1;
                self.stats.lines += self.pending as u64;
                if self.stats.micros.len() >= 10_000 {
                    self.stats.micros.remove(0);
                }
                self.stats
                    .micros
                    .push(u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX));
                self.jbuf.clear();
                self.pdirty = false;
                self.pending = 0;
                self.oldest = None;
                Ok(())
            }
            Err(e) => {
                let msg = format!("journal commit failed in {}: {e}", self.dir.display());
                self.failed = Some(msg.clone());
                Err(msg)
            }
        }
    }

    fn buffered(&mut self) {
        self.pending += 1;
        if self.oldest.is_none() {
            self.oldest = Some(Instant::now());
        }
    }

    fn due(&self) -> bool {
        self.pending >= self.policy.max_batch.max(1)
            || self
                .oldest
                .is_some_and(|t| t.elapsed().as_millis() >= u128::from(self.policy.max_delay_ms))
    }
}

/// Serialize `rec`, shortening its intent until the line fits [`MAX_RECORD_BYTES`].
fn encode_record(rec: &JournalRecord) -> Result<String, String> {
    let mut r = rec.clone();
    loop {
        let line = serde_json::to_string(&r).map_err(|e| e.to_string())?;
        if line.len() <= MAX_RECORD_BYTES {
            return Ok(line);
        }
        let over = line.len() - MAX_RECORD_BYTES;
        let keep = r.intent.chars().count().saturating_sub(over.max(8) + 1);
        if keep == 0 {
            return Err(format!("journal record for {} is {} bytes even with an empty intent; the limit is {MAX_RECORD_BYTES}", r.step_id, line.len()));
        }
        r.intent = r.intent.chars().take(keep).collect::<String>() + "…";
    }
}

impl RunSink for JournalWriter {
    /// 2.11a: each event reaches `progress.ndjson` **as it happens** (one whole-line append, so a
    /// tail or Monitor reading the file never sees half a line), well inside the plan's "within 2 s
    /// of its step". Only the fsync is grouped: it rides on the next commit. Journal records keep
    /// their group commit unchanged.
    fn progress(&mut self, ev: &ProgressEvent) {
        if self.failed.is_some() {
            return;
        }
        let mut e = ev.clone();
        e.seq = self.next_pseq;
        let Ok(line) = serde_json::to_string(&e) else {
            return;
        };
        self.next_pseq += 1;
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        if let Err(err) = self.progress.write_all(&bytes) {
            // sticky, like a failed commit: the next journal call fails, so nothing more is sent
            self.failed = Some(format!(
                "progress write failed in {}: {err}",
                self.dir.display()
            ));
            return;
        }
        self.pdirty = true;
        self.stats.progress_lines += 1;
        if self.due() {
            let _ = self.commit(); // a failure is sticky and stops the next journal call
        }
    }

    fn journal(&mut self, rec: &JournalRecord) -> Result<(), String> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        let mut r = rec.clone();
        r.seq = self.next_jseq;
        let line = encode_record(&r)?;
        self.next_jseq += 1;
        self.jbuf.extend_from_slice(line.as_bytes());
        self.jbuf.push(b'\n');
        self.buffered();
        // `sent` is the last record before the phone is touched: it and everything before it
        // (its `planned`) must be on disk before this returns.
        if r.status == JournalStatus::Sent || self.due() {
            self.commit()?;
        }
        Ok(())
    }
}

impl Drop for JournalWriter {
    fn drop(&mut self) {
        let _ = self.commit();
    }
}

/// [`CheckpointStore`] over `<run dir>/checkpoint.json`: temp file, `sync_all`, rename.
pub struct FileCheckpoints {
    pub path: PathBuf,
}

impl FileCheckpoints {
    pub fn new(run_dir: &Path) -> FileCheckpoints {
        FileCheckpoints {
            path: run_dir.join(CHECKPOINT_FILE),
        }
    }
}

/// Atomic write: temp file in the same directory, `sync_all`, rename over the target.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let res = (|| -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    res.map_err(|e| format!("{}: {e}", path.display()))
}

impl CheckpointStore for FileCheckpoints {
    fn save(&mut self, cp: &Checkpoint) -> Result<(), String> {
        let text = serde_json::to_string(cp).map_err(|e| e.to_string())?;
        write_atomic(&self.path, format!("{text}\n").as_bytes())
    }
}

/// The saved checkpoint, if any.
pub fn read_checkpoint(run_dir: &Path) -> Result<Option<Checkpoint>, String> {
    let path = run_dir.join(CHECKPOINT_FILE);
    match std::fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Every complete progress event (a torn last line is skipped; progress is not the journal).
pub fn read_progress(run_dir: &Path) -> Vec<ProgressEvent> {
    let Ok(bytes) = std::fs::read(run_dir.join(PROGRESS_FILE)) else {
        return vec![];
    };
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect()
}
