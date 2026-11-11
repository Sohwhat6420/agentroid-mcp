//! The flight recorder and run retention (task 2.11a: "the last 10 trees and 3
//! frames dumped on stuck, with 7-day local retention pruned at job start").
//!
//! * [`FlightRecorder`] is a ring per job: the last [`TREES`] UI trees the engine observed and the
//!   last [`FRAMES`] screenshots. The engine hands both to its [`crate::runner::sink::RunSink`]
//!   (`tree` / `frame`), and the daemon's sink keeps them here. Frames are only taken when the
//!   screen keeps coming back (the cycle detector's last sightings before it trips, see
//!   `RunOpts::flight_frames`), so a routine run takes no screenshots at all.
//! * [`FlightRecorder::dump`] writes the ring to `<run dir>/flight/` when the job goes `stuck`:
//!   `tree-NN.xml` and `frame-N.jpg`, oldest first, plus `index.json`. The dump stays on this
//!   laptop; nothing in it is pushed anywhere.
//! * [`prune_runs`] deletes run directories whose last activity is older than [`RETENTION_MS`]
//!   (7 days) and keeps younger ones. The daemon calls it at every `job.start` with its injectable
//!   clock.

use crate::runner::journal::{valid_run_id, write_atomic, JOURNAL_FILE, PROGRESS_FILE};
use serde_json::json;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// UI trees kept per job.
pub const TREES: usize = 10;
/// Screenshots kept per job.
pub const FRAMES: usize = 3;
/// The dump directory inside a run directory.
pub const FLIGHT_DIR: &str = "flight";
/// Local retention: 7 days (round 33).
pub const RETENTION_MS: u64 = 7 * 24 * 3600 * 1000;
/// Most run directories kept besides the live ones (wiring 2, item 3): each holds a journal and,
/// after a stuck job, a flight dump of frames, so the age rule alone is not a bound.
pub const MAX_RUNS: usize = 200;

#[derive(Debug, Clone, PartialEq)]
struct Entry<T> {
    ts: String,
    step_id: String,
    data: T,
}

/// The ring of recent trees and frames for one job.
#[derive(Debug, Clone, PartialEq)]
pub struct FlightRecorder {
    trees: VecDeque<Entry<String>>,
    frames: VecDeque<Entry<Vec<u8>>>,
    tree_cap: usize,
    frame_cap: usize,
}

impl Default for FlightRecorder {
    fn default() -> Self {
        FlightRecorder::new(TREES, FRAMES)
    }
}

/// What a dump wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dump {
    pub dir: PathBuf,
    pub trees: usize,
    pub frames: usize,
}

fn push_capped<T>(q: &mut VecDeque<T>, cap: usize, v: T) {
    if cap == 0 {
        return;
    }
    while q.len() >= cap {
        q.pop_front();
    }
    q.push_back(v);
}

/// `jpg` for JPEG (core's screenshots), `png` for PNG, else `bin`.
fn image_ext(b: &[u8]) -> &'static str {
    if b.starts_with(&[0xff, 0xd8]) || b.starts_with(&[0xff, 0xff]) {
        "jpg"
    } else if b.starts_with(b"\x89PNG") {
        "png"
    } else {
        "bin"
    }
}

impl FlightRecorder {
    pub fn new(tree_cap: usize, frame_cap: usize) -> FlightRecorder {
        FlightRecorder {
            trees: VecDeque::with_capacity(tree_cap),
            frames: VecDeque::with_capacity(frame_cap),
            tree_cap,
            frame_cap,
        }
    }
    pub fn tree(&mut self, ts: String, step_id: &str, xml: &str) {
        push_capped(
            &mut self.trees,
            self.tree_cap,
            Entry {
                ts,
                step_id: step_id.to_string(),
                data: xml.to_string(),
            },
        );
    }
    pub fn frame(&mut self, ts: String, step_id: &str, image: &[u8]) {
        push_capped(
            &mut self.frames,
            self.frame_cap,
            Entry {
                ts,
                step_id: step_id.to_string(),
                data: image.to_vec(),
            },
        );
    }
    pub fn trees(&self) -> usize {
        self.trees.len()
    }
    pub fn frames(&self) -> usize {
        self.frames.len()
    }
    /// The newest frame's bytes (the job view's `frame.jpg`).
    pub fn latest_frame(&self) -> Option<Vec<u8>> {
        self.frames.back().map(|e| e.data.clone())
    }

    /// Write the ring to `<run_dir>/flight/` (an earlier dump of the same run is replaced).
    pub fn dump(&self, run_dir: &Path, run_id: &str, dumped_at: &str) -> Result<Dump, String> {
        let dir = run_dir.join(FLIGHT_DIR);
        let io = |what: &Path, e: std::io::Error| format!("{}: {e}", what.display());
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| io(&dir, e))?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| io(&dir, e))?;
        let mut trees = Vec::new();
        for (i, t) in self.trees.iter().enumerate() {
            let name = format!("tree-{i:02}.xml");
            let p = dir.join(&name);
            std::fs::write(&p, t.data.as_bytes()).map_err(|e| io(&p, e))?;
            trees.push(
                json!({"file": name, "ts": t.ts, "step_id": t.step_id, "bytes": t.data.len()}),
            );
        }
        let mut frames = Vec::new();
        for (i, f) in self.frames.iter().enumerate() {
            let name = format!("frame-{i}.{}", image_ext(&f.data));
            let p = dir.join(&name);
            std::fs::write(&p, &f.data).map_err(|e| io(&p, e))?;
            frames.push(
                json!({"file": name, "ts": f.ts, "step_id": f.step_id, "bytes": f.data.len()}),
            );
        }
        let index = json!({"v": 1, "run_id": run_id, "dumped_at": dumped_at, "reason": "stuck", "trees": trees, "frames": frames});
        write_atomic(
            &dir.join("index.json"),
            (index.to_string() + "\n").as_bytes(),
        )?;
        Ok(Dump {
            dir,
            trees: self.trees.len(),
            frames: self.frames.len(),
        })
    }
}

fn mtime_ms(p: &Path) -> Option<u64> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    m.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The newest modification time of the files in a run directory (and in its `flight/`), or the
/// directory's own time when it holds no files.
pub fn last_activity_ms(run_dir: &Path) -> Option<u64> {
    let mut newest: Option<u64> = None;
    for dir in [run_dir.to_path_buf(), run_dir.join(FLIGHT_DIR)] {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                if let Some(t) = mtime_ms(&p) {
                    newest = Some(newest.map_or(t, |n| n.max(t)));
                }
            }
        }
    }
    newest.or_else(|| mtime_ms(run_dir))
}

/// Written when a job ends (not when it pauses): its absence means the run is running or paused.
const RECEIPT_FILE: &str = "receipt.json";

/// A directory that a job or a script run wrote (so nothing else under the jobs dir is touched).
fn is_run_dir(p: &Path) -> bool {
    ["job.json", JOURNAL_FILE, PROGRESS_FILE]
        .iter()
        .any(|f| p.join(f).is_file())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub pruned: Vec<String>,
    pub kept: Vec<String>,
    pub errors: Vec<String>,
}

/// Delete every run directory under `jobs_dir` whose last activity is more than `keep_ms` before
/// `now_unix_ms`; keep the rest. `protect(run_id)` keeps a run regardless (a job that is running
/// or paused in this daemon). Only directories with a valid run id that hold a run's files are
/// considered; anything else under the jobs directory is left alone.
pub fn prune_runs(
    jobs_dir: &Path,
    now_unix_ms: u64,
    keep_ms: u64,
    protect: &dyn Fn(&str) -> bool,
) -> PruneReport {
    prune_runs_capped(jobs_dir, now_unix_ms, keep_ms, usize::MAX, protect)
}

/// [`prune_runs`] by age AND by count: after the too-old runs are gone, if more than `max_runs`
/// unprotected runs are left, the ones with the oldest last activity go until `max_runs` remain.
/// A protected run (running or paused in this daemon) is never deleted and does not count against the cap.
/// Neither does a run that has no `receipt.json`: a run that was running or paused when an earlier daemon went away
/// is resumable (`job.resume`), and only the age rule (7 days with no activity) may remove it, never the count.
pub fn prune_runs_capped(
    jobs_dir: &Path,
    now_unix_ms: u64,
    keep_ms: u64,
    max_runs: usize,
    protect: &dyn Fn(&str) -> bool,
) -> PruneReport {
    let mut rep = PruneReport::default();
    let Ok(rd) = std::fs::read_dir(jobs_dir) else {
        return rep;
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut young: Vec<(u64, String, PathBuf)> = Vec::new();
    for e in entries {
        let p = e.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            continue;
        };
        if !p.is_dir() || !valid_run_id(&name) || !is_run_dir(&p) {
            continue;
        }
        if protect(&name) {
            rep.kept.push(name);
            continue;
        }
        let last = last_activity_ms(&p);
        match last.map(|t| now_unix_ms.saturating_sub(t)) {
            Some(a) if a > keep_ms => match std::fs::remove_dir_all(&p) {
                Ok(()) => rep.pruned.push(name),
                Err(err) => rep.errors.push(format!("{}: {err}", p.display())),
            },
            // no receipt: it never ended (an earlier daemon's running or paused run), so the count cap leaves it alone
            _ if !p.join(RECEIPT_FILE).is_file() => rep.kept.push(name),
            _ => young.push((last.unwrap_or(u64::MAX), name, p)),
        }
    }
    young.sort();
    let over = young.len().saturating_sub(max_runs);
    for (i, (_, name, p)) in young.into_iter().enumerate() {
        if i < over {
            match std::fs::remove_dir_all(&p) {
                Ok(()) => rep.pruned.push(name),
                Err(err) => rep.errors.push(format!("{}: {err}", p.display())),
            }
        } else {
            rep.kept.push(name);
        }
    }
    rep
}
