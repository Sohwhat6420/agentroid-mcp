//! Task 2.11a, engine and disk side: progress reaches `progress.ndjson` as it happens, the flight
//! recorder keeps the last 10 trees and 3 frames and dumps them on `stuck`, and run directories
//! older than 7 days are pruned. Fake phone and fake clock; no adb, no device.

use super::engine::{run, Driver, Hooks, RunOpts, RunReport, Stop, CYCLE_THRESHOLD};
use super::flight::{
    last_activity_ms, prune_runs, Dump, FlightRecorder, FLIGHT_DIR, FRAMES, RETENTION_MS, TREES,
};
use super::flow::Flow;
use super::journal::{GroupCommit, JournalWriter, PROGRESS_FILE};
use super::popups::Rulebook;
use super::sink::{
    EventKind, JournalRecord, MemoryCheckpoints, MemorySink, ProgressEvent, RunSink,
};
use super::tests::{app_screen, battery_sheet, flow_of, opts, Phone, POWER};
use agentroid_core::fake::FakeClock;
use agentroid_core::gate::Call;
use agentroid_core::settle::{Settle, SettleOpts};
use agentroid_core::transport::{Core, CoreError, Outcome};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("agentroid-2.11a-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn asserts(n: usize) -> Flow {
    let steps: Vec<Value> = (0..n)
        .map(|i| json!({"id": format!("a{i}"), "kind": "assert", "select": {"text": "Bluetooth"}}))
        .collect();
    flow_of(Value::Array(steps))
}

// ---------------------------------------------------------------- progress NDJSON within 2 s

/// Core, except that a `wait` really sleeps (a slow step after the one being watched).
struct SlowWait<'a> {
    core: &'a mut Core<Phone, FakeClock>,
    real: Duration,
    wait_done: Arc<Mutex<Option<Instant>>>,
}

impl Driver for SlowWait<'_> {
    fn now_ms(&self) -> u64 {
        Driver::now_ms(&*self.core)
    }
    fn settle(&mut self, serial: Option<&str>, opts: SettleOpts) -> Result<Settle, CoreError> {
        Driver::settle(self.core, serial, opts)
    }
    fn admit(&mut self, serial: Option<&str>, call: &Call) -> Result<(), CoreError> {
        Driver::admit(self.core, serial, call)
    }
    fn call(&mut self, serial: Option<&str>, call: &Call) -> Result<Outcome, CoreError> {
        if matches!(call, Call::Wait { .. }) {
            std::thread::sleep(self.real);
            *self.wait_done.lock().unwrap() = Some(Instant::now());
        }
        Driver::call(self.core, serial, call)
    }
}

/// (kind, step id, when it was handed to the writer), per event in seq order.
type Emitted = Arc<Mutex<Vec<(EventKind, Option<String>, Instant)>>>;

/// The journal writer, plus the moment each progress event was handed to it.
struct Stamped {
    w: JournalWriter,
    emitted: Emitted,
}

impl RunSink for Stamped {
    fn progress(&mut self, ev: &ProgressEvent) {
        self.emitted
            .lock()
            .unwrap()
            .push((ev.kind, ev.step_id.clone(), Instant::now()));
        self.w.progress(ev);
    }
    fn journal(&mut self, rec: &JournalRecord) -> Result<(), String> {
        self.w.journal(rec)
    }
}

#[test]
fn a_progress_event_reaches_the_ndjson_within_2_s_of_its_step_while_the_job_runs() {
    let dir = tmp("ndjson");
    let flow = flow_of(json!([
        {"id": "a", "kind": "assert", "select": {"text": "Bluetooth"}},
        {"id": "w", "kind": "wait", "value": 1000},
        {"id": "b", "kind": "assert", "select": {"text": "Display"}}
    ]));
    let path = dir.join("run-1").join(PROGRESS_FILE);
    // a separate reader, like a tail or Claude Code's Monitor: it opens the file by path while the
    // job holds it open for appending, and notes when each seq first appears
    let stop = Arc::new(AtomicBool::new(false));
    let seen: Arc<Mutex<HashMap<u64, Instant>>> = Arc::default();
    let reader = {
        let (stop, seen, path) = (stop.clone(), seen.clone(), path.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let now = Instant::now();
                    for line in text.lines() {
                        let v: Value =
                            serde_json::from_str(line).expect("the reader never sees half a line");
                        seen.lock()
                            .unwrap()
                            .entry(v["seq"].as_u64().unwrap())
                            .or_insert(now);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let mut core = Core::new(Phone::new(&app_screen()), FakeClock::new());
    let wait_done: Arc<Mutex<Option<Instant>>> = Arc::default();
    let emitted: Emitted = Arc::default();
    // a long group-commit delay, so only the per-event append can make the 2 s
    let w = JournalWriter::open(
        &dir,
        "run-1",
        GroupCommit {
            max_batch: 1000,
            max_delay_ms: 60_000,
        },
    )
    .unwrap();
    let mut sink = Stamped {
        w,
        emitted: emitted.clone(),
    };
    let mut cps = MemoryCheckpoints::default();
    let mut driver = SlowWait {
        core: &mut core,
        real: Duration::from_millis(2_600),
        wait_done: wait_done.clone(),
    };
    let rep = run(
        &mut driver,
        &flow,
        &Rulebook::seeded(),
        opts(),
        Hooks {
            sink: &mut sink,
            checkpoints: &mut cps,
            lease: None,
            control: None,
        },
        None,
    );
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::SeqCst);
    reader.join().unwrap();
    drop(sink);

    let emitted = emitted.lock().unwrap().clone();
    let seen = seen.lock().unwrap().clone();
    assert!(emitted.len() >= 4, "{emitted:?}");
    for (seq, (kind, step, at)) in emitted.iter().enumerate() {
        let got = seen
            .get(&(seq as u64))
            .unwrap_or_else(|| panic!("event {seq} ({kind:?} {step:?}) never reached the file"));
        let lag = got.saturating_duration_since(*at);
        assert!(
            lag < Duration::from_secs(2),
            "event {seq} ({kind:?} {step:?}) reached the file {lag:?} after its step"
        );
    }
    // the step event of `a` was readable while the job was still busy in `w`
    let a_seq = emitted
        .iter()
        .position(|(k, s, _)| *k == EventKind::Step && s.as_deref() == Some("a"))
        .unwrap();
    let wait_end = wait_done.lock().unwrap().expect("the wait ran");
    assert!(
        seen[&(a_seq as u64)] < wait_end,
        "the step event of a appeared only after the next step finished"
    );
}

// ---------------------------------------------------------------- the flight recorder

/// A sink that keeps a flight recorder and dumps it on the stuck event, like the daemon's Tee.
struct Recorder {
    inner: MemorySink,
    fr: FlightRecorder,
    dir: PathBuf,
    dumped: Option<Result<Dump, String>>,
}

impl RunSink for Recorder {
    fn progress(&mut self, ev: &ProgressEvent) {
        self.inner.progress(ev);
        if ev.kind == EventKind::Stuck {
            self.dumped = Some(self.fr.dump(&self.dir, &ev.run_id, &ev.ts));
        }
    }
    fn journal(&mut self, rec: &JournalRecord) -> Result<(), String> {
        self.inner.journal(rec)
    }
    fn tree(&mut self, step_id: &str, xml: &str) {
        self.fr
            .tree("2026-10-20T00:00:00.000Z".into(), step_id, xml);
    }
    fn frame(&mut self, step_id: &str, image: &[u8]) {
        self.fr
            .frame("2026-10-20T00:00:00.000Z".into(), step_id, image);
    }
}

fn flight_opts() -> RunOpts {
    let mut o = opts();
    o.flight_frames = FRAMES as u32;
    o
}

fn record(core: &mut Core<Phone, FakeClock>, flow: &Flow, dir: &Path) -> (RunReport, Recorder) {
    let mut rec = Recorder {
        inner: MemorySink::default(),
        fr: FlightRecorder::default(),
        dir: dir.to_path_buf(),
        dumped: None,
    };
    let mut cps = MemoryCheckpoints::default();
    let rep = run(
        core,
        flow,
        &Rulebook::seeded(),
        flight_opts(),
        Hooks {
            sink: &mut rec,
            checkpoints: &mut cps,
            lease: None,
            control: None,
        },
        None,
    );
    (rep, rec)
}

fn screenshots(core: &Core<Phone, FakeClock>) -> usize {
    core.transport
        .dev
        .calls
        .iter()
        .filter(|c| **c == "screenshot")
        .count()
}

#[test]
fn the_ring_holds_the_last_10_trees_and_3_frames() {
    let mut fr = FlightRecorder::default();
    for i in 0..25 {
        fr.tree(format!("t{i}"), &format!("s{i}"), &format!("<tree {i}/>"));
        fr.frame(format!("t{i}"), &format!("s{i}"), &[0xff, 0xd8, i as u8]);
    }
    assert_eq!((fr.trees(), fr.frames()), (TREES, FRAMES));
    assert_eq!((TREES, FRAMES), (10, 3));
    let dir = tmp("ring");
    let d = fr.dump(&dir, "run-1", "now").unwrap();
    assert_eq!((d.trees, d.frames), (10, 3));
    // oldest first: trees 15..24, frames 22..24
    assert_eq!(
        std::fs::read_to_string(dir.join(FLIGHT_DIR).join("tree-00.xml")).unwrap(),
        "<tree 15/>"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(FLIGHT_DIR).join("tree-09.xml")).unwrap(),
        "<tree 24/>"
    );
    assert_eq!(
        std::fs::read(dir.join(FLIGHT_DIR).join("frame-0.jpg")).unwrap(),
        vec![0xff, 0xd8, 22]
    );
    // a second dump replaces the first, it never accumulates
    let mut small = FlightRecorder::default();
    small.tree("t".into(), "s", "<only/>");
    small.dump(&dir, "run-1", "later").unwrap();
    let names: Vec<String> = std::fs::read_dir(dir.join(FLIGHT_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}"); // tree-00.xml + index.json
}

#[test]
fn a_stuck_run_dumps_10_trees_and_3_frames_and_a_routine_run_takes_no_screenshot() {
    // routine: 12 steps, no repeats past the flight threshold, so not one screenshot
    let mut core = Core::new(Phone::new(&app_screen()), FakeClock::new());
    let dir = tmp("routine");
    let (rep, rec) = record(&mut core, &asserts(12), &dir);
    assert_eq!(rep.stop, Stop::Done, "{}", rep.stop.describe());
    assert_eq!(
        screenshots(&core),
        0,
        "a routine run takes no flight frames"
    );
    assert_eq!(rec.fr.trees(), TREES);
    assert!(
        rec.dumped.is_none() && !dir.join(FLIGHT_DIR).exists(),
        "no dump without stuck"
    );

    // stuck: after 8 steps the battery sheet comes and comes back after every Got it
    let mut phone = Phone::new(&app_screen());
    phone.later = Some((16, battery_sheet(POWER)));
    let mut core = Core::new(phone, FakeClock::new());
    let dir = tmp("stuck");
    let (rep, rec) = record(&mut core, &asserts(12), &dir);
    assert!(
        matches!(rep.stop, Stop::Stuck { .. }),
        "{}",
        rep.stop.describe()
    );
    assert_eq!(
        screenshots(&core),
        FRAMES,
        "frames only for the last sightings before the trip"
    );
    let d = rec.dumped.expect("dumped on stuck").unwrap();
    assert_eq!((d.trees, d.frames), (10, 3));
    let flight = dir.join(FLIGHT_DIR);
    let count = |pre: &str| {
        std::fs::read_dir(&flight)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(pre)
            })
            .count()
    };
    assert_eq!(count("tree-"), 10);
    assert_eq!(count("frame-"), 3);
    let index: Value =
        serde_json::from_str(&std::fs::read_to_string(flight.join("index.json")).unwrap()).unwrap();
    assert_eq!(index["trees"].as_array().unwrap().len(), 10);
    assert_eq!(index["frames"].as_array().unwrap().len(), 3);
    assert_eq!(index["reason"], "stuck");
    // the newest trees are the stuck screen, the oldest ones the steps before it
    assert!(std::fs::read_to_string(flight.join("tree-09.xml"))
        .unwrap()
        .contains("Battery saver"));
    assert!(!std::fs::read_to_string(flight.join("tree-00.xml"))
        .unwrap()
        .contains("Battery saver"));
    assert_eq!(CYCLE_THRESHOLD, 5);
}

// ---------------------------------------------------------------- retention

const DAY: u64 = 24 * 3600 * 1000;

fn run_dir_aged(root: &Path, name: &str, now: u64, age_ms: u64) -> PathBuf {
    let d = root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    for f in ["job.json", "progress.ndjson"] {
        let p = d.join(f);
        std::fs::write(&p, "{}\n").unwrap();
        let t = UNIX_EPOCH + Duration::from_millis(now - age_ms);
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }
    d
}

/// The same run directory, ended: a receipt is written when a job ends, with the files' time.
fn ended(dir: &Path, now: u64, age_ms: u64) -> PathBuf {
    let p = dir.join("receipt.json");
    std::fs::write(&p, "{}\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_millis(now - age_ms))
        .unwrap();
    dir.to_path_buf()
}

#[test]
fn an_8_day_old_run_is_pruned_and_a_6_day_old_one_is_kept() {
    let root = tmp("prune");
    // the injected clock: a fixed "now", nowhere near the real one
    let now = 1_900_000_000_000u64;
    let old = run_dir_aged(&root, "job-old", now, 8 * DAY);
    let young = run_dir_aged(&root, "job-young", now, 6 * DAY);
    let live = run_dir_aged(&root, "job-live", now, 30 * DAY);
    let edge = run_dir_aged(&root, "job-edge", now, 7 * DAY);
    let stray = root.join("not-a-run");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::write(stray.join("notes.txt"), "x").unwrap();
    std::fs::File::options()
        .write(true)
        .open(stray.join("notes.txt"))
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_millis(now - 30 * DAY))
        .unwrap();
    assert_eq!(last_activity_ms(&old), Some(now - 8 * DAY));

    let rep = prune_runs(&root, now, RETENTION_MS, &|id| id == "job-live");
    assert_eq!(rep.pruned, vec!["job-old".to_string()]);
    assert!(!old.exists(), "8 days old: pruned");
    assert!(young.exists(), "6 days old: kept");
    assert!(live.exists(), "a live job is kept whatever its age");
    assert!(edge.exists(), "exactly 7 days is not older than 7 days");
    assert!(
        stray.exists(),
        "a directory that is not a run is never touched"
    );
    assert!(rep.errors.is_empty(), "{:?}", rep.errors);
    // the same tree a day later: the 7-day-old one goes too, the 6-day-old one is now 7 and stays
    let rep = prune_runs(&root, now + DAY, RETENTION_MS, &|id| id == "job-live");
    assert_eq!(rep.pruned, vec!["job-edge".to_string()]);
    assert!(young.exists());
}

/// Review defect 6: a run an earlier daemon left running or paused has no receipt.json. This daemon's `protect`
/// does not know it (it is not in memory until someone resumes it), so the count cap must not take it.
#[test]
fn the_run_count_cap_never_takes_a_run_an_earlier_daemon_left_running_or_paused() {
    let root = tmp("prune-count-orphan");
    let now = 1_900_000_000_000u64;
    let hour = 3_600_000u64;
    // the orphan is the OLDEST of the young runs (the cap's first victim); a paused one has a checkpoint too
    let orphan = run_dir_aged(&root, "job-orphan", now, 30 * hour);
    let paused = run_dir_aged(&root, "job-paused", now, 29 * hour);
    std::fs::write(paused.join("checkpoint.json"), "{}\n").unwrap();
    let ended_dirs: Vec<_> = [("job-a", 5), ("job-b", 4), ("job-c", 3), ("job-d", 2)]
        .iter()
        .map(|(n, h)| ended(&run_dir_aged(&root, n, now, h * hour), now, h * hour))
        .collect();
    // cap of 2 ended runs: the two oldest ended runs go; the orphan and the paused run stay
    let rep = super::flight::prune_runs_capped(&root, now, RETENTION_MS, 2, &|_| false);
    assert!(
        orphan.exists(),
        "a run with no receipt is not the count cap's to delete"
    );
    assert!(paused.exists(), "nor is a paused one");
    assert!(
        !ended_dirs[0].exists() && !ended_dirs[1].exists(),
        "the two oldest ENDED runs go instead"
    );
    assert!(ended_dirs[2].exists() && ended_dirs[3].exists());
    assert_eq!(rep.pruned, vec!["job-a".to_string(), "job-b".to_string()]);
    assert!(
        rep.kept.contains(&"job-orphan".to_string())
            && rep.kept.contains(&"job-paused".to_string())
    );
    // a cap of zero still leaves them
    super::flight::prune_runs_capped(&root, now, RETENTION_MS, 0, &|_| false);
    assert!(orphan.exists() && paused.exists());
    assert!(!ended_dirs[2].exists() && !ended_dirs[3].exists());
    // the age rule still applies: eight days with no activity and no receipt is dead
    let dead = run_dir_aged(&root, "job-dead", now, 8 * DAY);
    super::flight::prune_runs_capped(&root, now, RETENTION_MS, 100, &|_| false);
    assert!(
        !dead.exists(),
        "an unfinished run idle for 8 days is pruned by age"
    );
    assert!(orphan.exists());
}

#[test]
fn the_run_count_cap_drops_the_oldest_ended_runs_keeps_fresh_ones_and_never_a_live_one() {
    let root = tmp("prune-count");
    let now = 1_900_000_000_000u64;
    // five young runs, 1..5 hours old, a live one that is the oldest of all, and one too old
    let ages = [
        ("job-a", 5),
        ("job-b", 4),
        ("job-c", 3),
        ("job-d", 2),
        ("job-e", 1),
    ];
    let dirs: Vec<_> = ages
        .iter()
        .map(|(n, h)| {
            ended(
                &run_dir_aged(&root, n, now, h * 3_600_000),
                now,
                h * 3_600_000,
            )
        })
        .collect();
    let live = run_dir_aged(&root, "job-live", now, 6 * 3_600_000);
    let old = ended(&run_dir_aged(&root, "job-old", now, 9 * DAY), now, 9 * DAY);
    let rep = super::flight::prune_runs_capped(&root, now, RETENTION_MS, 3, &|id| id == "job-live");
    assert!(!old.exists(), "too old, by age");
    assert!(
        !dirs[0].exists() && !dirs[1].exists(),
        "the two oldest ended runs go, by count"
    );
    assert!(
        dirs[2].exists() && dirs[3].exists() && dirs[4].exists(),
        "the three newest are kept"
    );
    assert!(
        live.exists(),
        "a live run is never pruned, even though it is the oldest, and does not count"
    );
    assert_eq!(
        rep.pruned,
        vec![
            "job-old".to_string(),
            "job-a".to_string(),
            "job-b".to_string()
        ]
    );
    assert!(rep.errors.is_empty());
    // a cap of zero removes every ended run but still not the live one
    let rep = super::flight::prune_runs_capped(&root, now, RETENTION_MS, 0, &|id| id == "job-live");
    assert_eq!(rep.pruned.len(), 3);
    assert!(live.exists());
}
