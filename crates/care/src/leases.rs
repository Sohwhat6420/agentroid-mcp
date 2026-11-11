//! `leases.json`, the one store shared with claude-phone's phone-guard ("one
//! schema"). Port of phone-guard `leases.py` `LeaseStore`, file-compatible both ways:
//!
//! * a JSON array of `{key, namespace, before, applied, owner_pid, job_id, ts, serial, ro_serial}`,
//!   written in that field order with phone-guard's `json.dump(indent=1)` layout;
//! * `before` is the original value or the literal `"ABSENT"`;
//! * pseudo-namespaces `battery_override` and `thermal_override` record fake-heat overrides;
//! * identity of a lease is `(ro_serial or serial, namespace, key)`; a second apply keeps the
//!   **original** `before`;
//! * writes are atomic: temp file, fsync, rename (retried while Windows has the file open), under
//!   the same `leases.json.lock` file phone-guard uses (O_EXCL create; stale after 15 s);
//! * a record leaves the file only after its restore verifiably worked.
//!
//! Additive, optional keys this crate writes and phone-guard ignores: `scope` (left out for `job`,
//! the default, so a job lease is byte-for-byte phone-guard's shape), `expires` (only for
//! `until`), `session` (only when known). Unknown keys from either tool are kept.
//! Records in the `schemas/leases.json` shape (`ns`, `owner{pid,job_id,session}`) are read too.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::CareError;

/// Pseudo-namespace: restore means `dumpsys battery reset`, verified.
pub const BATTERY_NS: &str = "battery_override";
/// Pseudo-namespace: restore means `cmd thermalservice reset`, verified (key = sample name, applied = type).
pub const THERMAL_NS: &str = "thermal_override";

/// Lease scopes. `permanent` is R2 and refused in Phase 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Session,
    Job,
    Until,
    Permanent,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::Session => "session",
            Scope::Job => "job",
            Scope::Until => "until",
            Scope::Permanent => "permanent",
        }
    }
}

fn is_job(s: &Option<Scope>) -> bool {
    matches!(s, None | Some(Scope::Job))
}

/// One record. Field order is phone-guard's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaseEntry {
    pub key: String,
    pub namespace: String,
    pub before: String,
    pub applied: String,
    #[serde(default)]
    pub owner_pid: u32,
    #[serde(default)]
    pub job_id: String,
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub ro_serial: String,
    #[serde(default, skip_serializing_if = "is_job")]
    pub scope: Option<Scope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Keys neither tool knows today; kept on rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl LeaseEntry {
    /// `ro_serial`, else the adb token it was written over (phone-guard `_ident`).
    pub fn ident(&self) -> &str {
        if self.ro_serial.is_empty() {
            &self.serial
        } else {
            &self.ro_serial
        }
    }
    pub fn same(&self, o: &LeaseEntry) -> bool {
        self.ident() == o.ident() && self.namespace == o.namespace && self.key == o.key
    }
    pub fn scope(&self) -> Scope {
        self.scope.unwrap_or(Scope::Job)
    }
    /// A real settings lease (not a fake-heat override).
    pub fn is_setting(&self) -> bool {
        crate::phone::NAMESPACES.contains(&self.namespace.as_str())
    }
}

/// Accept the `schemas/leases.json` shape too: `ns` → `namespace`, `owner{pid,job_id,session}` → flat.
fn normalise(mut v: Value) -> Value {
    if let Value::Object(m) = &mut v {
        if !m.contains_key("namespace") {
            if let Some(ns) = m.remove("ns") {
                m.insert("namespace".into(), ns);
            }
        }
        if let Some(Value::Object(owner)) = m.get("owner").cloned() {
            m.remove("owner");
            if let Some(p) = owner.get("pid") {
                m.entry("owner_pid").or_insert(p.clone());
            }
            if let Some(j) = owner.get("job_id") {
                m.entry("job_id").or_insert(j.clone());
            }
            if let Some(s) = owner.get("session") {
                m.entry("session").or_insert(s.clone());
            }
        }
    }
    v
}

/// One element of the file: a record we understand, or something we keep untouched.
#[derive(Debug, Clone)]
enum Item {
    Lease(Box<LeaseEntry>),
    Opaque(Value),
}

fn to_item(v: Value) -> Item {
    match serde_json::from_value::<LeaseEntry>(normalise(v.clone())) {
        Ok(e) => Item::Lease(Box::new(e)),
        Err(_) => Item::Opaque(v),
    }
}

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize)]
#[serde(untagged)]
enum Mixed<'a> {
    E(&'a LeaseEntry),
    V(&'a Value),
}

/// Lay out JSON as Python's `json.dump(x, f, indent=1)` does (phone-guard's file layout): one
/// space per level, `", "`-free item separators, `": "` after keys, non-ASCII as `\uXXXX`
/// (`ensure_ascii`), no trailing newline. Records keep phone-guard's field order.
fn render(items: &[Item]) -> String {
    let mixed: Vec<Mixed<'_>> = items
        .iter()
        .map(|i| match i {
            Item::Lease(e) => Mixed::E(e),
            Item::Opaque(v) => Mixed::V(v),
        })
        .collect();
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    mixed.serialize(&mut ser).expect("in-memory JSON");
    let text = String::from_utf8(buf).expect("serde_json writes UTF-8");
    // Non-ASCII can only sit inside JSON strings, so escaping every such char is valid JSON.
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for u in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

/// The file text for these records (the layout `write` uses).
pub fn render_entries(entries: &[LeaseEntry]) -> String {
    render(
        &entries
            .iter()
            .cloned()
            .map(|e| Item::Lease(Box::new(e)))
            .collect::<Vec<_>>(),
    )
}

/// The store.
#[derive(Debug, Clone)]
pub struct LeaseStore {
    pub path: PathBuf,
}

/// Holds `leases.json.lock`; removed on drop.
struct FileLock {
    path: PathBuf,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl LeaseStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        LeaseStore { path: path.into() }
    }

    /// `PHONE_GUARD_LEASES` (phone-guard's variable, so both tools find the same file), else `default`.
    pub fn from_env_or(default: impl Into<PathBuf>) -> Self {
        match std::env::var_os("PHONE_GUARD_LEASES") {
            Some(p) if !p.is_empty() => LeaseStore::new(PathBuf::from(p)),
            _ => LeaseStore::new(default),
        }
    }

    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    fn lock_path(&self) -> PathBuf {
        let mut n = self
            .path
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        n.push(".lock");
        self.path.with_file_name(n)
    }

    fn lock(&self) -> Result<FileLock, CareError> {
        fs::create_dir_all(self.dir()).map_err(|e| {
            CareError::Store(format!("cannot create {}: {e}", self.dir().display()))
        })?;
        let lp = self.lock_path();
        let t0 = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&lp) {
                Ok(_) => return Ok(FileLock { path: lp }),
                Err(e)
                    if e.kind() == std::io::ErrorKind::AlreadyExists
                        || e.kind() == std::io::ErrorKind::PermissionDenied =>
                {
                    let stale = fs::metadata(&lp)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|m| SystemTime::now().duration_since(m).ok())
                        .is_some_and(|age| age > Duration::from_secs(15));
                    if stale {
                        let _ = fs::remove_file(&lp); // a dead process's lock
                        continue;
                    }
                    if t0.elapsed() > Duration::from_secs(10) {
                        return Err(CareError::Store("leases.json lock busy".into()));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    return Err(CareError::Store(format!(
                        "cannot lock {}: {e}",
                        lp.display()
                    )))
                }
            }
        }
    }

    /// The raw items. A missing file is empty; an unparseable file is an error (writing over it
    /// would lose leases).
    fn read_items(&self) -> Result<Vec<Item>, CareError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => {
                return Err(CareError::Store(format!(
                    "cannot read {}: {e}",
                    self.path.display()
                )))
            }
        };
        if text.trim().is_empty() {
            return Ok(vec![]);
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Array(a)) => Ok(a.into_iter().map(to_item).collect()),
            Ok(_) => Ok(vec![]), // phone-guard: a non-list reads as empty
            Err(e) => Err(CareError::Store(format!(
                "{} is not JSON ({e}); refusing to overwrite it",
                self.path.display()
            ))),
        }
    }

    fn write_items(&self, items: &[Item]) -> Result<(), CareError> {
        let text = render(items);
        let mut name = self
            .path
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let tmp = self.path.with_file_name(name);
        let res = (|| {
            let mut f = fs::File::create(&tmp)
                .map_err(|e| CareError::Store(format!("cannot create {}: {e}", tmp.display())))?;
            f.write_all(text.as_bytes())
                .and_then(|_| f.sync_all())
                .map_err(|e| CareError::Store(format!("cannot write {}: {e}", tmp.display())))?;
            drop(f);
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut delay = Duration::from_millis(10);
            loop {
                match fs::rename(&tmp, &self.path) {
                    Ok(()) => return Ok(()),
                    // Windows: a reader (restorer, antivirus, indexer) has the file open
                    Err(e)
                        if e.kind() == std::io::ErrorKind::PermissionDenied
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(delay);
                        delay = (delay * 2).min(Duration::from_millis(250));
                    }
                    Err(e) => {
                        return Err(CareError::Store(format!(
                            "cannot replace {}: {e}",
                            self.path.display()
                        )))
                    }
                }
            }
        })();
        let _ = fs::remove_file(&tmp); // gone after a successful rename; cleaned up after a failure
        res
    }

    /// Every record this crate understands.
    pub fn entries(&self) -> Result<Vec<LeaseEntry>, CareError> {
        let _l = self.lock()?;
        Ok(self
            .read_items()?
            .into_iter()
            .filter_map(|i| match i {
                Item::Lease(e) => Some(*e),
                Item::Opaque(_) => None,
            })
            .collect())
    }

    /// Insert or replace (same identity). The original `before` is kept, never a value applied earlier.
    pub fn upsert(&self, mut entry: LeaseEntry) -> Result<LeaseEntry, CareError> {
        let _l = self.lock()?;
        let mut items = self.read_items()?;
        if let Some(old) = items.iter().find_map(|i| match i {
            Item::Lease(e) if e.same(&entry) => Some(e),
            _ => None,
        }) {
            entry.before = old.before.clone();
        }
        items.retain(|i| !matches!(i, Item::Lease(e) if e.same(&entry)));
        items.push(Item::Lease(Box::new(entry.clone())));
        self.write_items(&items)?;
        Ok(entry)
    }

    pub fn remove(&self, entry: &LeaseEntry) -> Result<(), CareError> {
        let _l = self.lock()?;
        let mut items = self.read_items()?;
        items.retain(|i| !matches!(i, Item::Lease(e) if e.same(entry)));
        self.write_items(&items)
    }
}
