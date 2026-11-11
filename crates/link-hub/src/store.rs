//! `hub-state/` under a configurable directory. Every whole-file write is atomic (a temp file in
//! the same directory, `sync_all`, then rename over the old file):
//!
//! * `devices.json`: the bindings (hardware serial, phone public key SPKI, key id), written by
//!   `agentroid-link link pair` and read by the hub;
//! * `epochs.json`: the per-device hub epoch, bumped (under a lock file) once per hub process and
//!   persisted before it is used, so it only ever grows across restarts;
//! * `responses/<serial>.jsonl`: the request-id response cache (append-only, compacted atomically),
//!   so a request id answered before a hub restart is answered from here afterwards, never re-run.
//!   Each device keeps at most [`RESPONSE_CACHE_LIMIT`] answers, in memory and on disk (the file is
//!   compacted before it passes [`RESPONSE_FILE_MAX_LINES`] lines), and every line carries its
//!   `ts_ms`; [`StateDir::prune_responses`] drops answers older than 7 days at hub start.
//!
//! No private key is ever stored here (the hub key lives in Credential Manager).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::crypto::{b64, key_id, parse_spki, unb64};
use crate::proto::is_serial;

/// Entries kept in the response cache per device (the phone keeps 4096 too). The count cap: an
/// older answer is dropped (memory and file) when a newer one would exceed it.
pub const RESPONSE_CACHE_LIMIT: usize = 4096;
/// A device's cache file is compacted before it holds more lines than this (the cap plus 1/8
/// slack, so appends stay cheap and the file never grows past ~1.125 x the cap).
pub const RESPONSE_FILE_MAX_LINES: usize = RESPONSE_CACHE_LIMIT + RESPONSE_CACHE_LIMIT / 8;
/// Answers older than this are pruned at hub start.
pub const RESPONSE_MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// What [`StateDir::prune_responses`] removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub files_scanned: usize,
    pub entries_dropped: usize,
    pub files_removed: usize,
}

/// What [`ResponseCache::prune`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CachePrune {
    /// Dropped for being older than the age limit.
    pub by_age: usize,
    /// Dropped, oldest first, to get under the count limit.
    pub by_count: usize,
    /// Old enough to go, but protected as still running.
    pub kept_running: usize,
}

impl CachePrune {
    pub fn dropped(&self) -> usize {
        self.by_age + self.by_count
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Slack lines allowed over `limit` before compaction (`limit / 8`, at least 1).
fn slack(limit: usize) -> usize {
    (limit / 8).max(1)
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Corrupt(String),
    Invalid(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "hub-state I/O error: {e}"),
            StoreError::Corrupt(m) => write!(f, "hub-state file is damaged: {m}"),
            StoreError::Invalid(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// Writes `bytes` to `path` atomically: readers see the old file or the new one, never a mix.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no parent directory"))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{name}.tmp-{}-{nanos}", std::process::id()));
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        // std::fs::rename replaces an existing file on Windows (MoveFileExW REPLACE_EXISTING)
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// A paired phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// `ro.serialno`, read over adb at pairing.
    pub serial: String,
    /// The phone's public key, X.509 SPKI DER.
    pub spki: Vec<u8>,
    /// Lower-case hex SHA-256 of `spki`.
    pub key_id: String,
    pub paired_at_ms: u64,
    pub revoked: bool,
}

impl Binding {
    fn to_json(&self) -> Value {
        json!({"serial": self.serial, "public_key": b64(&self.spki), "key_id": self.key_id, "paired_at_ms": self.paired_at_ms, "revoked": self.revoked})
    }

    fn from_json(v: &Value) -> Result<Self, StoreError> {
        let bad = |m: &str| StoreError::Corrupt(format!("devices.json: {m}"));
        let serial = v["serial"]
            .as_str()
            .filter(|s| is_serial(s))
            .ok_or_else(|| bad("serial"))?
            .to_string();
        let spki = v["public_key"]
            .as_str()
            .and_then(unb64)
            .ok_or_else(|| bad("public_key"))?;
        parse_spki(&spki).map_err(|e| bad(&e.0))?;
        let kid = key_id(&spki);
        if v["key_id"].as_str() != Some(kid.as_str()) {
            return Err(bad("key_id does not match public_key"));
        }
        Ok(Self {
            serial,
            spki,
            key_id: kid,
            paired_at_ms: v["paired_at_ms"].as_u64().unwrap_or(0),
            revoked: v["revoked"] == true,
        })
    }
}

/// Per-file names for a serial: `[A-Za-z0-9_-]` kept, anything else (`:` is not allowed in a
/// Windows file name, `.` could form `..`) written as `~hh`.
pub fn file_stem(serial: &str) -> String {
    serial
        .bytes()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                (c as char).to_string()
            } else {
                format!("~{c:02x}")
            }
        })
        .collect()
}

/// `<base>/hub-state`.
#[derive(Debug, Clone)]
pub struct StateDir {
    root: PathBuf,
}

/// A cross-process lock file (create_new), removed on drop. A lock older than 30 s is a crashed
/// holder and is broken.
struct FileLock(PathBuf);

impl FileLock {
    fn take(path: PathBuf) -> io::Result<Self> {
        for _ in 0..400 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(30));
                    if stale {
                        let _ = fs::remove_file(&path);
                    } else {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other(format!(
            "{} is held by another process",
            path.display()
        )))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl StateDir {
    /// Opens (creating) `<base>/hub-state`.
    pub fn open(base: &Path) -> Result<Self, StoreError> {
        let root = base.join("hub-state");
        fs::create_dir_all(root.join("responses"))?;
        Ok(Self { root })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn devices_path(&self) -> PathBuf {
        self.root.join("devices.json")
    }

    fn epochs_path(&self) -> PathBuf {
        self.root.join("epochs.json")
    }

    fn read_json(path: &Path) -> Result<Option<Value>, StoreError> {
        match fs::read(path) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| StoreError::Corrupt(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Every binding (empty when nothing was paired yet).
    pub fn load_devices(&self) -> Result<Vec<Binding>, StoreError> {
        let Some(v) = Self::read_json(&self.devices_path())? else {
            return Ok(Vec::new());
        };
        if v["version"] != 1 {
            return Err(StoreError::Corrupt(
                "devices.json: version must be 1".into(),
            ));
        }
        v["devices"]
            .as_array()
            .ok_or_else(|| StoreError::Corrupt("devices.json: devices must be a list".into()))?
            .iter()
            .map(Binding::from_json)
            .collect()
    }

    fn save_devices(&self, list: &[Binding]) -> Result<(), StoreError> {
        let v =
            json!({"version": 1, "devices": list.iter().map(Binding::to_json).collect::<Vec<_>>()});
        let mut text =
            serde_json::to_string_pretty(&v).map_err(|e| StoreError::Invalid(e.to_string()))?;
        text.push('\n');
        write_atomic(&self.devices_path(), text.as_bytes())?;
        Ok(())
    }

    /// Binds `spki` to `serial`, replacing any older binding of that serial or of that key (one
    /// key belongs to one phone). Written atomically under the devices lock.
    pub fn bind_device(
        &self,
        serial: &str,
        spki: &[u8],
        now_ms: u64,
    ) -> Result<Binding, StoreError> {
        if !is_serial(serial) {
            return Err(StoreError::Invalid(format!(
                "serial {serial:?} must be 1..64 of [A-Za-z0-9._:-]"
            )));
        }
        parse_spki(spki).map_err(|e| StoreError::Invalid(e.0))?;
        let _lock = FileLock::take(self.root.join("devices.lock"))?;
        let b = Binding {
            serial: serial.to_string(),
            spki: spki.to_vec(),
            key_id: key_id(spki),
            paired_at_ms: now_ms,
            revoked: false,
        };
        let mut list = self.load_devices()?;
        list.retain(|x| x.serial != b.serial && x.key_id != b.key_id);
        list.push(b.clone());
        self.save_devices(&list)?;
        Ok(b)
    }

    /// Marks a binding revoked: the phone then gets `needs_repair key_revoked` until paired again.
    pub fn revoke(&self, serial: &str) -> Result<bool, StoreError> {
        let _lock = FileLock::take(self.root.join("devices.lock"))?;
        let mut list = self.load_devices()?;
        let mut found = false;
        for b in list.iter_mut().filter(|b| b.serial == serial) {
            b.revoked = true;
            found = true;
        }
        if found {
            self.save_devices(&list)?;
        }
        Ok(found)
    }

    fn load_epochs(&self) -> Result<BTreeMap<String, u64>, StoreError> {
        let Some(v) = Self::read_json(&self.epochs_path())? else {
            return Ok(BTreeMap::new());
        };
        let map = v["epochs"]
            .as_object()
            .ok_or_else(|| StoreError::Corrupt("epochs.json: epochs must be an object".into()))?;
        map.iter()
            .map(|(k, n)| {
                n.as_u64()
                    .map(|n| (k.clone(), n))
                    .ok_or_else(|| StoreError::Corrupt(format!("epochs.json: {k}")))
            })
            .collect()
    }

    /// The last epoch persisted for `serial` (0 if none).
    pub fn current_epoch(&self, serial: &str) -> Result<u64, StoreError> {
        Ok(self.load_epochs()?.get(serial).copied().unwrap_or(0))
    }

    /// Allocates the next epoch for `serial`: strictly greater than every epoch any hub process
    /// on this state directory has used for it, persisted (atomically, under a lock file) before
    /// it is returned.
    pub fn next_epoch(&self, serial: &str) -> Result<u64, StoreError> {
        let _lock = FileLock::take(self.root.join("epochs.lock"))?;
        let mut map = self.load_epochs()?;
        let next = map
            .get(serial)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| StoreError::Invalid("epoch overflow".into()))?;
        map.insert(serial.to_string(), next);
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({"version": 1, "epochs": map})).unwrap_or_default()
        );
        write_atomic(&self.epochs_path(), text.as_bytes())?;
        Ok(next)
    }

    /// Every persisted epoch, by serial.
    pub fn epochs(&self) -> Result<BTreeMap<String, u64>, StoreError> {
        self.load_epochs()
    }

    /// Records that `serial` now runs at `epoch` (an epoch handed in by the daemon's registry, the
    /// one epoch source in bridged). Only ever raises the stored value, under the epochs lock.
    pub fn raise_epoch(&self, serial: &str, epoch: u64) -> Result<u64, StoreError> {
        let _lock = FileLock::take(self.root.join("epochs.lock"))?;
        let mut map = self.load_epochs()?;
        let cur = map.get(serial).copied().unwrap_or(0);
        if epoch <= cur {
            return Ok(cur);
        }
        map.insert(serial.to_string(), epoch);
        let text = format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({"version": 1, "epochs": map})).unwrap_or_default()
        );
        write_atomic(&self.epochs_path(), text.as_bytes())?;
        Ok(epoch)
    }

    pub fn response_cache(&self, serial: &str) -> Result<ResponseCache, StoreError> {
        ResponseCache::open(
            self.root
                .join("responses")
                .join(format!("{}.jsonl", file_stem(serial))),
            RESPONSE_CACHE_LIMIT,
        )
    }

    /// Drops every cached answer older than `max_age_ms` (by its `ts_ms`; a line written before
    /// `ts_ms` existed takes the file's modification time) and keeps at most
    /// [`RESPONSE_CACHE_LIMIT`] per device (newest kept). A file left empty is deleted; temp files
    /// of a crashed compaction are removed. Run once at hub start, before any cache is opened.
    pub fn prune_responses(&self, now_ms: u64, max_age_ms: u64) -> Result<PruneReport, StoreError> {
        let dir = self.root.join("responses");
        let mut rep = PruneReport::default();
        let cutoff = now_ms.saturating_sub(max_age_ms);
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && name.contains(".tmp-") {
                let _ = fs::remove_file(&path);
                continue;
            }
            if !name.ends_with(".jsonl") || !entry.file_type()?.is_file() {
                continue;
            }
            rep.files_scanned += 1;
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(now_ms);
            let text = fs::read_to_string(&path)?;
            let mut kept: Vec<&str> = Vec::new();
            let mut total = 0usize;
            for line in text.lines() {
                total += 1;
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if v["id"].as_str().is_none() || v["req"].as_str().is_none() {
                    continue;
                }
                let ts = v["ts_ms"].as_u64().unwrap_or(mtime);
                if ts >= cutoff {
                    kept.push(line);
                }
            }
            if kept.len() > RESPONSE_CACHE_LIMIT {
                kept.drain(..kept.len() - RESPONSE_CACHE_LIMIT);
            }
            rep.entries_dropped += total - kept.len();
            if kept.is_empty() {
                fs::remove_file(&path)?;
                rep.files_removed += 1;
            } else if kept.len() != total {
                let mut out = String::with_capacity(text.len());
                for l in kept {
                    out.push_str(l);
                    out.push('\n');
                }
                write_atomic(&path, out.as_bytes())?;
            }
        }
        Ok(rep)
    }
}

/// One answered request.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedResponse {
    /// Hex SHA-256 of the request's `rpc` text: the same id with a different call is refused.
    pub request_hash: String,
    /// The JSON-RPC response object as the phone sent it.
    pub rpc: Value,
    /// The phone's `replayed` flag when it answered.
    pub replayed: bool,
    /// Unix ms when the hub cached it (0 for a line written before `ts_ms` existed).
    pub ts_ms: u64,
}

/// The request-id response cache of one device: in memory (bounded, oldest out first) and an
/// append-only journal that survives a hub restart.
#[derive(Debug)]
pub struct ResponseCache {
    path: PathBuf,
    file: File,
    map: HashMap<String, CachedResponse>,
    order: VecDeque<String>,
    lines: usize,
    limit: usize,
}

impl ResponseCache {
    pub fn open(path: PathBuf, limit: usize) -> Result<Self, StoreError> {
        let mut map = HashMap::new();
        let mut order = VecDeque::new();
        let mut lines = 0;
        match fs::read_to_string(&path) {
            Ok(text) => {
                for line in text.lines() {
                    lines += 1;
                    // a torn last line after a crash is skipped, not fatal
                    let Ok(v) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    let (Some(id), Some(h)) = (v["id"].as_str(), v["req"].as_str()) else {
                        continue;
                    };
                    let entry = CachedResponse {
                        request_hash: h.to_string(),
                        rpc: v["rpc"].clone(),
                        replayed: v["replayed"] == true,
                        ts_ms: v["ts_ms"].as_u64().unwrap_or(0),
                    };
                    if map.insert(id.to_string(), entry).is_none() {
                        order.push_back(id.to_string());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        while order.len() > limit {
            if let Some(old) = order.pop_front() {
                map.remove(&old);
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut c = Self {
            path,
            file,
            map,
            order,
            lines,
            limit,
        };
        if c.lines > c.limit + slack(c.limit) {
            c.compact()?;
        }
        Ok(c)
    }

    /// Lines in the journal file now (live entries plus not yet compacted ones).
    pub fn file_lines(&self) -> usize {
        self.lines
    }

    pub fn get(&self, id: &str) -> Option<&CachedResponse> {
        self.map.get(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn line(id: &str, e: &CachedResponse) -> String {
        format!(
            "{}\n",
            json!({"id": id, "req": e.request_hash, "rpc": e.rpc, "replayed": e.replayed, "ts_ms": e.ts_ms})
        )
    }

    /// Records an answer (the first answer for an id wins; later ones are ignored). An entry with
    /// `ts_ms == 0` is stamped with the system clock.
    pub fn insert(&mut self, id: &str, mut entry: CachedResponse) -> Result<(), StoreError> {
        if self.map.contains_key(id) {
            return Ok(());
        }
        if entry.ts_ms == 0 {
            entry.ts_ms = unix_ms_now();
        }
        // one write call per line, so a crash leaves at most one torn line
        self.file.write_all(Self::line(id, &entry).as_bytes())?;
        self.lines += 1;
        self.map.insert(id.to_string(), entry);
        self.order.push_back(id.to_string());
        while self.order.len() > self.limit {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        if self.lines > self.limit + slack(self.limit) {
            self.compact()?;
        }
        Ok(())
    }

    /// Drops answers while the hub runs (wiring 2, 25 Oct; [`StateDir::prune_responses`] only runs
    /// at hub start): first every answer older than `max_age_ms` (by its `ts_ms`; a legacy entry
    /// with no stamp has no known age and is left to the start-time prune), then, oldest first,
    /// whatever still exceeds `max_count`. `keep(id)` marks an answer that belongs to a request
    /// still in use (a running segment): it is never dropped, by age or by count, even when that
    /// leaves the cache over `max_count`. The journal is compacted when anything went.
    pub fn prune(
        &mut self,
        now_ms: u64,
        max_age_ms: u64,
        max_count: usize,
        keep: &dyn Fn(&str) -> bool,
    ) -> Result<CachePrune, StoreError> {
        let cutoff = now_ms.saturating_sub(max_age_ms);
        let mut rep = CachePrune::default();
        let mut drop_ids: HashSet<String> = HashSet::new();
        for id in &self.order {
            let Some(e) = self.map.get(id) else { continue };
            if e.ts_ms != 0 && e.ts_ms < cutoff {
                if keep(id) {
                    rep.kept_running += 1;
                } else {
                    drop_ids.insert(id.clone());
                }
            }
        }
        rep.by_age = drop_ids.len();
        let mut live = self.order.len() - drop_ids.len();
        if live > max_count {
            for id in &self.order {
                if live <= max_count {
                    break;
                }
                if drop_ids.contains(id) || keep(id) {
                    continue;
                }
                drop_ids.insert(id.clone());
                rep.by_count += 1;
                live -= 1;
            }
        }
        if drop_ids.is_empty() {
            return Ok(rep);
        }
        for id in &drop_ids {
            self.map.remove(id);
        }
        self.order.retain(|id| self.map.contains_key(id));
        self.compact()?;
        Ok(rep)
    }

    /// Rewrites the journal with only the live entries (atomically) and reopens it.
    fn compact(&mut self) -> Result<(), StoreError> {
        let mut text = String::new();
        for id in &self.order {
            if let Some(e) = self.map.get(id) {
                text.push_str(&Self::line(id, e));
            }
        }
        write_atomic(&self.path, text.as_bytes())?;
        self.file = OpenOptions::new().append(true).open(&self.path)?;
        self.lines = self.order.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::HubKey;

    fn tmp(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!(
            "agentroid-link-hub-store-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn bindings_round_trip_and_replace() {
        let base = tmp("bind");
        let s = StateDir::open(&base).unwrap();
        assert!(s.load_devices().unwrap().is_empty());
        let k1 = HubKey::generate().unwrap();
        let k2 = HubKey::generate().unwrap();
        s.bind_device("POCO1", k1.spki(), 5).unwrap();
        s.bind_device("OTHER", k2.spki(), 6).unwrap();
        s.bind_device("POCO1", k2.spki(), 7).unwrap(); // re-pair: the key moves, the old rows go
        let list = s.load_devices().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].serial, "POCO1");
        assert_eq!(list[0].key_id, k2.key_id());
        assert!(s.bind_device("bad serial", k1.spki(), 0).is_err());
        assert!(s.bind_device("X", &k1.spki()[1..], 0).is_err());
        // no temp files or lock files are left behind
        let names: Vec<String> = fs::read_dir(s.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.contains(".tmp-") && !n.ends_with(".lock")),
            "{names:?}"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn epochs_grow_and_persist() {
        let base = tmp("epoch");
        let s = StateDir::open(&base).unwrap();
        assert_eq!(s.current_epoch("A").unwrap(), 0);
        assert_eq!(s.next_epoch("A").unwrap(), 1);
        assert_eq!(s.next_epoch("A").unwrap(), 2);
        assert_eq!(s.next_epoch("B").unwrap(), 1);
        let again = StateDir::open(&base).unwrap();
        assert_eq!(again.current_epoch("A").unwrap(), 2);
        assert_eq!(again.next_epoch("A").unwrap(), 3);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn response_cache_survives_reopen_and_compacts() {
        let base = tmp("cache");
        let path = base.join("c.jsonl");
        {
            let mut c = ResponseCache::open(path.clone(), 3).unwrap();
            for i in 0..10 {
                let e = CachedResponse {
                    request_hash: format!("h{i}"),
                    rpc: json!({"i": i}),
                    replayed: false,
                    ts_ms: 0,
                };
                c.insert(&format!("id{i}"), e).unwrap();
                assert!(
                    c.file_lines() <= 3 + 1,
                    "the file stays within the cap plus slack"
                );
            }
            assert_eq!(c.len(), 3);
            assert!(c.get("id9").is_some() && c.get("id6").is_none());
            assert!(
                c.get("id9").unwrap().ts_ms > 1_700_000_000_000,
                "stamped with the system clock"
            );
        }
        // a torn last line is skipped
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"id\":\"half")
            .unwrap();
        let c = ResponseCache::open(path.clone(), 3).unwrap();
        assert_eq!(c.get("id8").unwrap().rpc, json!({"i": 8}));
        assert!(fs::read_to_string(&path).unwrap().lines().count() <= 7);
        assert_eq!(file_stem("ab:c.d"), "ab~3ac~2ed");
        drop(c);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn the_per_device_cap_holds_on_disk_at_the_real_limit() {
        let base = tmp("cap");
        let s = StateDir::open(&base).unwrap();
        let mut c = s.response_cache("POCO1").unwrap();
        for i in 0..(RESPONSE_CACHE_LIMIT + 700) {
            let e = CachedResponse {
                request_hash: "h".into(),
                rpc: json!({"i": i}),
                replayed: false,
                ts_ms: 1,
            };
            c.insert(&format!("id{i}"), e).unwrap();
            assert!(c.file_lines() <= RESPONSE_FILE_MAX_LINES);
        }
        assert_eq!(c.len(), RESPONSE_CACHE_LIMIT);
        assert!(
            c.get("id0").is_none()
                && c.get(&format!("id{}", RESPONSE_CACHE_LIMIT + 699))
                    .is_some()
        );
        drop(c);
        let path = s.path().join("responses").join("POCO1.jsonl");
        assert!(fs::read_to_string(&path).unwrap().lines().count() <= RESPONSE_FILE_MAX_LINES);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn prune_drops_answers_older_than_7_days_and_keeps_younger_ones() {
        let base = tmp("prune");
        let s = StateDir::open(&base).unwrap();
        let now = 1_800_000_000_000u64;
        let day = 24 * 60 * 60 * 1000u64;
        {
            let mut a = s.response_cache("A").unwrap();
            a.insert(
                "old",
                CachedResponse {
                    request_hash: "h".into(),
                    rpc: json!(1),
                    replayed: false,
                    ts_ms: now - 8 * day,
                },
            )
            .unwrap();
            a.insert(
                "young",
                CachedResponse {
                    request_hash: "h".into(),
                    rpc: json!(2),
                    replayed: false,
                    ts_ms: now - 6 * day,
                },
            )
            .unwrap();
            let mut b = s.response_cache("B").unwrap();
            b.insert(
                "old",
                CachedResponse {
                    request_hash: "h".into(),
                    rpc: json!(3),
                    replayed: false,
                    ts_ms: now - 9 * day,
                },
            )
            .unwrap();
        }
        // a crashed compaction's temp file is swept too
        fs::write(s.path().join("responses").join(".A.jsonl.tmp-1-2"), b"x").unwrap();
        let rep = s.prune_responses(now, RESPONSE_MAX_AGE_MS).unwrap();
        assert_eq!(
            rep,
            PruneReport {
                files_scanned: 2,
                entries_dropped: 2,
                files_removed: 1
            }
        );
        let a = s.response_cache("A").unwrap();
        assert!(a.get("old").is_none() && a.get("young").is_some());
        assert!(
            !s.path().join("responses").join("B.jsonl").exists(),
            "an emptied file is deleted"
        );
        let names: Vec<String> = fs::read_dir(s.path().join("responses"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["A.jsonl".to_string()]);
        // a second prune changes nothing
        drop(a);
        assert_eq!(
            s.prune_responses(now, RESPONSE_MAX_AGE_MS)
                .unwrap()
                .entries_dropped,
            0
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_line_without_ts_ms_is_aged_by_the_file_time() {
        let base = tmp("prune-old-format");
        let s = StateDir::open(&base).unwrap();
        let path = s.path().join("responses").join("C.jsonl");
        fs::write(
            &path,
            b"{\"id\":\"x\",\"req\":\"h\",\"rpc\":1,\"replayed\":false}\n",
        )
        .unwrap();
        let file_ms = unix_ms_now();
        let day = 24 * 60 * 60 * 1000u64;
        // 6 days after the file was written: kept; 8 days after: dropped
        assert_eq!(
            s.prune_responses(file_ms + 6 * day, RESPONSE_MAX_AGE_MS)
                .unwrap()
                .entries_dropped,
            0
        );
        assert_eq!(
            s.prune_responses(file_ms + 8 * day, RESPONSE_MAX_AGE_MS)
                .unwrap()
                .files_removed,
            1
        );
        fs::remove_dir_all(base).unwrap();
    }

    fn put(c: &mut ResponseCache, id: &str, ts_ms: u64) {
        c.insert(
            id,
            CachedResponse {
                request_hash: "h".into(),
                rpc: json!({"id": id}),
                replayed: false,
                ts_ms,
            },
        )
        .unwrap();
    }

    fn ids(c: &ResponseCache) -> Vec<String> {
        c.order.iter().cloned().collect()
    }

    #[test]
    fn a_running_prune_drops_the_too_old_and_the_over_count_and_keeps_fresh_ones() {
        let base = tmp("live-prune");
        let path = base.join("c.jsonl");
        let (day, now) = (24 * 60 * 60 * 1000u64, 1_800_000_000_000u64);
        let mut c = ResponseCache::open(path.clone(), 100).unwrap();
        put(&mut c, "too-old-1", now - 9 * day);
        put(&mut c, "too-old-2", now - 8 * day);
        put(&mut c, "fresh-1", now - 2 * day);
        put(&mut c, "fresh-2", now - day);
        put(&mut c, "fresh-3", now - 1000);
        // age only (the count limit is far away)
        let r = c.prune(now, 7 * day, 100, &|_| false).unwrap();
        assert_eq!((r.by_age, r.by_count, r.kept_running), (2, 0, 0));
        assert_eq!(ids(&c), ["fresh-1", "fresh-2", "fresh-3"]);
        // the journal went with them: a reopen sees the same three
        drop(c);
        let mut c = ResponseCache::open(path.clone(), 100).unwrap();
        assert_eq!(ids(&c), ["fresh-1", "fresh-2", "fresh-3"]);
        // count only: the oldest of the fresh go first
        let r = c.prune(now, 7 * day, 2, &|_| false).unwrap();
        assert_eq!((r.by_age, r.by_count), (0, 1));
        assert_eq!(ids(&c), ["fresh-2", "fresh-3"]);
        // nothing to do: nothing rewritten, and the same result twice
        assert_eq!(c.prune(now, 7 * day, 2, &|_| false).unwrap().dropped(), 0);
        drop(c);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_running_item_is_never_pruned_by_age_or_by_count() {
        let base = tmp("live-prune-keep");
        let (day, now) = (24 * 60 * 60 * 1000u64, 1_800_000_000_000u64);
        let mut c = ResponseCache::open(base.join("c.jsonl"), 100).unwrap();
        put(&mut c, "running-old", now - 30 * day);
        put(&mut c, "done-old", now - 30 * day);
        put(&mut c, "running-fresh", now - 1000);
        put(&mut c, "done-fresh", now - 500);
        let keep = |id: &str| id.starts_with("running");
        let r = c.prune(now, 7 * day, 1, &keep).unwrap();
        // age took done-old and spared running-old; count then took done-fresh (the only
        // unprotected one left) and still could not reach the limit of 1 without the running ones
        assert_eq!((r.by_age, r.by_count, r.kept_running), (1, 1, 1));
        assert_eq!(ids(&c), ["running-old", "running-fresh"]);
        assert!(c.get("running-old").is_some() && c.get("done-old").is_none());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn legacy_answers_with_no_stamp_are_left_to_the_start_time_prune() {
        let base = tmp("live-prune-legacy");
        let path = base.join("c.jsonl");
        fs::write(
            &path,
            b"{\"id\":\"x\",\"req\":\"h\",\"rpc\":1,\"replayed\":false}\n",
        )
        .unwrap();
        let mut c = ResponseCache::open(path, 100).unwrap();
        assert_eq!(
            c.prune(u64::MAX / 2, 1, 100, &|_| false).unwrap().dropped(),
            0,
            "no ts_ms, no known age"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn raise_epoch_only_raises() {
        let base = tmp("raise");
        let s = StateDir::open(&base).unwrap();
        assert_eq!(s.raise_epoch("A", 5).unwrap(), 5);
        assert_eq!(s.raise_epoch("A", 3).unwrap(), 5);
        assert_eq!(s.next_epoch("A").unwrap(), 6);
        assert_eq!(s.epochs().unwrap().get("A"), Some(&6));
        fs::remove_dir_all(base).unwrap();
    }
}
