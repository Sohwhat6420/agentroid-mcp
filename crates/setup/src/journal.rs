//! The undo journal and `--undo`.
//!
//! Layout under the config dir: `undo/<run>/journal.json` and `undo/<run>/backups/<sha256>` (the bytes
//! of every file before setup changed it, content-addressed). Each entry is written to the journal
//! (atomically) BEFORE the change it describes, so a crash mid-setup still leaves an undoable record.
//!
//! `--undo` reverts the latest run, newest entry first: files byte for byte from the backups, files
//! setup created deleted, directories it created removed, and every device write reverted through the
//! guarded runner (a setting back to its old value, or deleted if it was absent; Doze whitelist entry
//! removed; app-op back to its old mode or `default`; permission revoked). Then it reads the settings
//! back and removes the run's own folder, so the config dir is byte-identical to before the run.
//! A file someone changed since setup is a conflict: undo stops before changing anything.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use agentroid_adb_link::runner::Runner;
use agentroid_core::gate::is_s24;
use serde::{Deserialize, Serialize};

use crate::device::{self, shell_argv};
use crate::sha256;

pub const UNDO_DIR: &str = "undo";
pub const JOURNAL: &str = "journal.json";
pub const BACKUPS: &str = "backups";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    DirCreated {
        path: String,
    },
    FileCreated {
        path: String,
        sha256: String,
    },
    FileModified {
        path: String,
        before_sha256: String,
        sha256: String,
    },
    Setting {
        ns: String,
        key: String,
        before: Option<String>,
        after: String,
    },
    Whitelist {
        pkg: String,
        was_present: bool,
    },
    AppOp {
        pkg: String,
        op: String,
        before: Option<String>,
        after: String,
    },
    Grant {
        pkg: String,
        perm: String,
        was_granted: bool,
    },
}

impl Entry {
    pub fn is_device(&self) -> bool {
        matches!(
            self,
            Entry::Setting { .. }
                | Entry::Whitelist { .. }
                | Entry::AppOp { .. }
                | Entry::Grant { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    pub run: String,
    /// The adb target used; `--undo` may be given another one for the same hardware serial.
    pub target: String,
    pub serial: String,
    pub created_config_dir: bool,
    pub created_undo_root: bool,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalError(pub String);

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "undo: {}", self.0)
    }
}

impl std::error::Error for JournalError {}

fn io<E: std::fmt::Display>(p: &Path) -> impl FnOnce(E) -> JournalError + '_ {
    move |e| JournalError(format!("{}: {e}", p.display()))
}

/// A relative path with `/` separators, no `..`, no root: the only kind the journal holds.
pub fn safe_rel(rel: &str) -> Result<PathBuf, JournalError> {
    let p = Path::new(rel);
    if rel.is_empty()
        || rel.contains('\\')
        || !p.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(JournalError(format!("unsafe journal path {rel:?}")));
    }
    Ok(p.to_path_buf())
}

/// Write `bytes` to `path` through a temporary file and a rename, so a reader never sees half a file.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), JournalError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.agentroid-tmp"));
    {
        use std::io::Write;
        let mut f = fs::File::create(&tmp).map_err(io(&tmp))?;
        f.write_all(bytes).map_err(io(&tmp))?;
        f.sync_all().map_err(io(&tmp))?;
    }
    fs::rename(&tmp, path).map_err(io(path))
}

/// The config hash: SHA-256 over every directory and file under `root` (relative path and the file's
/// own SHA-256), sorted. A missing root has its own hash, different from an empty one.
pub fn tree_hash(root: &Path) -> String {
    if !root.exists() {
        return sha256::hex(b"agentroid-config: absent");
    }
    let mut lines: Vec<String> = Vec::new();
    fn walk(root: &Path, dir: &Path, lines: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(dir) else {
            lines.push(format!("unreadable {}", dir.display()));
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            if p.is_dir() {
                lines.push(format!("D {rel}"));
                walk(root, &p, lines);
            } else {
                let bytes = fs::read(&p).unwrap_or_default();
                lines.push(format!("F {rel} {}", sha256::hex(&bytes)));
            }
        }
    }
    walk(root, root, &mut lines);
    lines.sort();
    sha256::hex(lines.join("\n").as_bytes())
}

/// The latest run folder under `<root>/undo/`, if any. Run names are zero-padded numbers.
pub fn latest_run(root: &Path) -> Option<String> {
    let rd = fs::read_dir(root.join(UNDO_DIR)).ok()?;
    rd.flatten()
        .filter(|e| e.path().join(JOURNAL).is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .max()
}

/// A setup run in progress: every write goes through here and is journalled first.
pub struct Txn {
    root: PathBuf,
    run_dir: PathBuf,
    pub journal: Journal,
}

impl Txn {
    /// Start a run: creates the config dir if needed, `undo/` and `undo/<run>/backups/`.
    pub fn begin(root: &Path, target: &str, serial: &str) -> Result<Txn, JournalError> {
        let created_config_dir = !root.exists();
        fs::create_dir_all(root).map_err(io(root))?;
        let undo = root.join(UNDO_DIR);
        let created_undo_root = !undo.exists();
        fs::create_dir_all(&undo).map_err(io(&undo))?;
        let next = latest_run(root)
            .and_then(|r| r.parse::<u32>().ok())
            .map_or(1, |n| n + 1);
        let run = format!("{next:04}");
        let run_dir = undo.join(&run);
        if run_dir.exists() {
            return Err(JournalError(format!(
                "{} already exists",
                run_dir.display()
            )));
        }
        fs::create_dir_all(run_dir.join(BACKUPS)).map_err(io(&run_dir))?;
        let txn = Txn {
            root: root.to_path_buf(),
            run_dir,
            journal: Journal {
                version: 1,
                run,
                target: target.to_string(),
                serial: serial.to_string(),
                created_config_dir,
                created_undo_root,
                entries: vec![],
            },
        };
        txn.persist()?;
        Ok(txn)
    }

    fn persist(&self) -> Result<(), JournalError> {
        let json =
            serde_json::to_string_pretty(&self.journal).map_err(|e| JournalError(e.to_string()))?;
        atomic_write(&self.run_dir.join(JOURNAL), (json + "\n").as_bytes())
    }

    fn record(&mut self, e: Entry) -> Result<(), JournalError> {
        self.journal.entries.push(e);
        self.persist()
    }

    /// Create `rel` and any missing parents, journalling each directory created.
    pub fn create_dirs(&mut self, rel: &str) -> Result<(), JournalError> {
        let rel_path = safe_rel(rel)?;
        let mut acc = PathBuf::new();
        for c in rel_path.components() {
            acc.push(c);
            let abs = self.root.join(&acc);
            if !abs.exists() {
                let r = acc.to_string_lossy().replace('\\', "/");
                self.record(Entry::DirCreated { path: r })?;
                fs::create_dir(&abs).map_err(io(&abs))?;
            }
        }
        Ok(())
    }

    /// Write a file (relative path), backing up its old bytes first. A file whose bytes would not
    /// change is left alone and not journalled.
    pub fn write_file(&mut self, rel: &str, bytes: &[u8]) -> Result<bool, JournalError> {
        let rel_path = safe_rel(rel)?;
        if let Some(parent) = rel_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            self.create_dirs(&parent.to_string_lossy().replace('\\', "/"))?;
        }
        let abs = self.root.join(&rel_path);
        let sha = sha256::hex(bytes);
        match fs::read(&abs) {
            Ok(old) if old == bytes => return Ok(false),
            Ok(old) => {
                let before = sha256::hex(&old);
                let backup = self.run_dir.join(BACKUPS).join(&before);
                atomic_write(&backup, &old)?;
                self.record(Entry::FileModified {
                    path: rel.to_string(),
                    before_sha256: before,
                    sha256: sha,
                })?;
            }
            Err(_) => self.record(Entry::FileCreated {
                path: rel.to_string(),
                sha256: sha,
            })?,
        }
        atomic_write(&abs, bytes)?;
        Ok(true)
    }

    /// Journal a device write (with the state before it), then send it.
    pub fn device(
        &mut self,
        runner: &dyn Runner,
        target: &str,
        entry: Entry,
        line: &str,
    ) -> Result<(), JournalError> {
        debug_assert!(entry.is_device());
        self.record(entry)?;
        let out = runner
            .run(&shell_argv(target, line), device::READ_TIMEOUT_MS)
            .map_err(|e| JournalError(format!("{line}: {e}")))?;
        if !out.ok() {
            return Err(JournalError(format!(
                "{line}: exit {:?} {}",
                out.code,
                out.stderr.trim()
            )));
        }
        Ok(())
    }

    pub fn run(&self) -> &str {
        &self.journal.run
    }
}

/// What `--undo` did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UndoReport {
    pub run: String,
    pub files_restored: usize,
    pub files_removed: usize,
    pub device_reverted: usize,
    pub notes: Vec<String>,
}

/// Undo the latest setup run under `root`. `runner` is needed when the run changed the device;
/// `target` overrides the adb target the run used (same hardware serial required).
pub fn undo(
    root: &Path,
    runner: Option<&dyn Runner>,
    target: Option<&str>,
) -> Result<UndoReport, JournalError> {
    let run = latest_run(root)
        .ok_or_else(|| JournalError(format!("no setup run to undo under {}", root.display())))?;
    let run_dir = root.join(UNDO_DIR).join(&run);
    let jpath = run_dir.join(JOURNAL);
    let text = fs::read_to_string(&jpath).map_err(io(&jpath))?;
    let j: Journal = serde_json::from_str(&text)
        .map_err(|e| JournalError(format!("{}: {e}", jpath.display())))?;
    if j.run != run {
        return Err(JournalError(format!(
            "journal says run {} but sits in {run}",
            j.run
        )));
    }
    let mut rep = UndoReport {
        run: run.clone(),
        ..Default::default()
    };

    // ---- preflight: nothing changes unless every step can be done ----
    for e in &j.entries {
        match e {
            Entry::FileCreated {
                path,
                sha256: after,
            } => {
                let abs = root.join(safe_rel(path)?);
                if let Ok(now) = fs::read(&abs) {
                    if sha256::hex(&now) != *after {
                        return Err(JournalError(format!(
                            "{path} changed since setup; not touching anything"
                        )));
                    }
                }
            }
            Entry::FileModified {
                path,
                before_sha256,
                sha256: after,
            } => {
                let abs = root.join(safe_rel(path)?);
                let now = fs::read(&abs).map_err(io(&abs))?;
                let h = sha256::hex(&now);
                if h != *after && h != *before_sha256 {
                    return Err(JournalError(format!(
                        "{path} changed since setup; not touching anything"
                    )));
                }
                let backup = run_dir.join(BACKUPS).join(before_sha256);
                let b = fs::read(&backup).map_err(io(&backup))?;
                if sha256::hex(&b) != *before_sha256 {
                    return Err(JournalError(format!("backup of {path} is damaged")));
                }
            }
            Entry::DirCreated { path } => {
                safe_rel(path)?;
            }
            _ => {}
        }
    }
    let device_entries: Vec<&Entry> = j.entries.iter().filter(|e| e.is_device()).collect();
    let target = target.unwrap_or(&j.target).to_string();
    if !device_entries.is_empty() {
        let runner = runner
            .ok_or_else(|| JournalError("this run changed the phone: a runner is needed".into()))?;
        if is_s24(&target) || is_s24(&j.serial) {
            return Err(JournalError(
                "the protected phone is never changed, so there is nothing to undo on it".into(),
            ));
        }
        let got = device::run_reads(runner, &target, &["getprop ro.serialno".to_string()]);
        let hw = got
            .get("getprop ro.serialno")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if hw != j.serial || is_s24(&hw) {
            return Err(JournalError(format!(
                "target {target} is serial {hw:?}, the run changed {:?}; not touching anything",
                j.serial
            )));
        }
    }

    // ---- revert, newest first ----
    let mut failures: Vec<String> = Vec::new();
    for e in j.entries.iter().rev() {
        match e {
            Entry::FileCreated { path, .. } => {
                let abs = root.join(safe_rel(path)?);
                if abs.exists() {
                    fs::remove_file(&abs).map_err(io(&abs))?;
                    rep.files_removed += 1;
                }
            }
            Entry::FileModified {
                path,
                before_sha256,
                ..
            } => {
                let abs = root.join(safe_rel(path)?);
                let b = fs::read(run_dir.join(BACKUPS).join(before_sha256)).map_err(io(&abs))?;
                atomic_write(&abs, &b)?;
                rep.files_restored += 1;
            }
            Entry::DirCreated { path } => {
                let abs = root.join(safe_rel(path)?);
                if abs.is_dir() {
                    if let Err(err) = fs::remove_dir(&abs) {
                        rep.notes
                            .push(format!("kept {path}: {err} (something else was put in it)"));
                    }
                }
            }
            dev => {
                let Some(line) = device::undo_line(dev) else {
                    continue;
                };
                let line = line.map_err(JournalError)?;
                let runner = runner.expect("checked in preflight");
                match runner.run(&shell_argv(&target, &line), device::READ_TIMEOUT_MS) {
                    Ok(o) if o.ok() => rep.device_reverted += 1,
                    Ok(o) => {
                        failures.push(format!("{line}: exit {:?} {}", o.code, o.stderr.trim()))
                    }
                    Err(err) => failures.push(format!("{line}: {err}")),
                }
            }
        }
    }

    // ---- read the settings back ----
    if let Some(runner) = runner {
        let mut want: BTreeMap<(String, String), Option<String>> = BTreeMap::new();
        for e in &j.entries {
            if let Entry::Setting {
                ns, key, before, ..
            } = e
            {
                // the oldest entry for a key holds the value from before the run
                want.entry((ns.clone(), key.clone()))
                    .or_insert_with(|| before.clone());
            }
        }
        let namespaces: Vec<String> = {
            let mut v: Vec<String> = want.keys().map(|(ns, _)| ns.clone()).collect();
            v.dedup();
            v
        };
        let lines: Vec<String> = namespaces
            .iter()
            .map(|ns| format!("settings list {ns}"))
            .collect();
        let got = device::run_reads(runner, &target, &lines);
        for ((ns, key), before) in &want {
            match got.get(&format!("settings list {ns}")) {
                None => failures.push(format!("readback: settings list {ns} failed")),
                Some(out) => {
                    let now = device::settings_map(out).get(key).cloned();
                    if now != *before {
                        failures.push(format!(
                            "readback: {ns} {key} is {now:?}, expected {before:?}"
                        ));
                    }
                }
            }
        }
    }
    if !failures.is_empty() {
        // keep the journal: a second --undo retries (file steps are idempotent)
        return Err(JournalError(format!(
            "device undo incomplete, journal kept: {}",
            failures.join("; ")
        )));
    }

    // ---- remove the run's own folder, then what the run created around it ----
    fs::remove_dir_all(&run_dir).map_err(io(&run_dir))?;
    let undo_root = root.join(UNDO_DIR);
    if j.created_undo_root {
        let _ = fs::remove_dir(&undo_root);
    }
    if j.created_config_dir {
        let _ = fs::remove_dir(root);
    }
    Ok(rep)
}
