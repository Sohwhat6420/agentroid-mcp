//! The exclusion between the battery-floor check and the battery temperature lease (phone-guard
//! S5, V3: "never with the battery-floor check"). Marker files next to `leases.json`, created
//! exclusively, one per holder, named and shaped exactly as phone-guard's
//! (`leases.json.<kind>.<pid>.<nonce>.hold`, `{"pid": N, "serial": "..."}`), so the exclusion
//! holds across both tools and across processes. A marker whose owner is dead is removed.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use crate::leases::LeaseStore;
use crate::pid::PidProbe;
use crate::CareError;

/// The battery-floor check is running.
pub const FLOOR: &str = "floor";
/// A battery temperature lease (`dumpsys battery set temp`) is active.
pub const TEMP_LEASE: &str = "templease";

static NONCE: AtomicU64 = AtomicU64::new(0);

/// A held marker; removed on drop (also on panic unwinding).
#[derive(Debug)]
pub struct Hold {
    path: PathBuf,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn prefix(store: &LeaseStore, kind: &str) -> String {
    format!(
        "{}.{kind}.",
        store
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    )
}

/// Create a marker for `kind` on `serial` (`None`: every phone, spelt `_` as phone-guard does).
pub fn hold(store: &LeaseStore, kind: &str, serial: Option<&str>) -> Result<Hold, CareError> {
    fs::create_dir_all(store.dir()).map_err(|e| CareError::Store(e.to_string()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let nonce =
        nanos.wrapping_add((NONCE.fetch_add(1, Ordering::SeqCst) as u32).wrapping_mul(0x9e37_79b9));
    let name = format!(
        "{}{}.{nonce:08x}.hold",
        prefix(store, kind),
        std::process::id()
    );
    let path = store.dir().join(name);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| CareError::Store(format!("cannot create {}: {e}", path.display())))?;
    let body = json!({"pid": std::process::id(), "serial": serial.unwrap_or("_")});
    f.write_all(body.to_string().as_bytes())
        .map_err(|e| CareError::Store(e.to_string()))?;
    Ok(Hold { path })
}

/// Is a live holder's `kind` marker present for `serial` (or for every phone)?
pub fn held_by_other(
    store: &LeaseStore,
    kind: &str,
    serial: Option<&str>,
    pids: &dyn PidProbe,
) -> bool {
    let pre = prefix(store, kind);
    let Ok(rd) = fs::read_dir(store.dir()) else {
        return false;
    };
    let want = serial.unwrap_or("_");
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !(name.starts_with(&pre) && name.ends_with(".hold")) {
            continue;
        }
        let Ok(text) = fs::read_to_string(ent.path()) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let (Some(pid), Some(ser)) = (
            v.get("pid").and_then(Value::as_u64),
            v.get("serial").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !pids.alive(pid as u32) {
            let _ = fs::remove_file(ent.path());
            continue;
        }
        if ser == "_" || want == "_" || ser == want {
            return true;
        }
    }
    false
}
