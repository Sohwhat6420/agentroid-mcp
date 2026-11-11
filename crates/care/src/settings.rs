//! Settings leases (phone-guard `leases.py` apply/restore and `restorer.py`
//! `restore_stale`), with the plan's additions: scopes, the R2 refusal of `permanent`, the
//! `read_only` refusal of every scope, re-assert, `changes_in_force` and one journal entry per
//! change and per restore.
//!
//! * **apply**: refusals first (nothing is read or written on a refused request), then the current
//!   value is read, the record is written to `leases.json` **before** the phone is touched (a crash
//!   cannot leak a change), then `settings put`, then one `change` entry.
//! * **restore** is compare-and-swap: the original goes back only if the phone still holds the
//!   applied value; otherwise the user changed it and it is left alone (`left`). An `ABSENT`
//!   original is restored with `settings delete`. The result is read back and must match byte for
//!   byte; only then does the record leave `leases.json`. One `restore` entry each.
//! * **reassert**: a leased key that drifted while its job is alive is put back to the applied value.
//! * **on_reconnect**: after a crash or reboot, every lease on that phone whose owner process is
//!   dead, or whose `until` has passed, is restored; this daemon's live leases are re-asserted.

use std::sync::Arc;

use agentroid_adb_link::runner::Runner;
use agentroid_core::gate::{effective_mode, DeviceMode};
use serde::{Deserialize, Serialize};
use serde_json::Map;

use crate::clock::{parse_rfc3339, rfc3339_utc, CareClock};
use crate::leases::{LeaseEntry, LeaseStore, Scope, BATTERY_NS, THERMAL_NS};
use crate::phone::{check_target, AdbPhone, ABSENT};
use crate::pid::PidProbe;
use crate::sink::{CareEvent, CareEventKind, CareSink};
use crate::thresholds::THRESHOLDS;
use crate::CareError;

/// Who holds a lease (the `owner` object of `schemas/job-view.json` / `schemas/leases.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub pid: u32,
    pub job_id: String,
    pub session: String,
}

impl Owner {
    /// Owned by this process.
    pub fn here(job_id: impl Into<String>, session: impl Into<String>) -> Self {
        Owner {
            pid: std::process::id(),
            job_id: job_id.into(),
            session: session.into(),
        }
    }
}

/// The scope a caller asks for. `Until` carries its expiry (unix ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeReq {
    Session,
    Job,
    Until(u64),
    Permanent,
}

impl ScopeReq {
    pub fn scope(&self) -> Scope {
        match self {
            ScopeReq::Session => Scope::Session,
            ScopeReq::Job => Scope::Job,
            ScopeReq::Until(_) => Scope::Until,
            ScopeReq::Permanent => Scope::Permanent,
        }
    }
}

/// One row of `changes_in_force` (exactly the item shape of `schemas/job-view.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeInForce {
    pub key: String,
    pub ns: String,
    pub before: String,
    pub applied: String,
    pub owner: Owner,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    pub ts: String,
}

impl ChangeInForce {
    pub fn of(e: &LeaseEntry) -> Self {
        ChangeInForce {
            key: e.key.clone(),
            ns: e.namespace.clone(),
            before: e.before.clone(),
            applied: e.applied.clone(),
            owner: Owner {
                pid: e.owner_pid,
                job_id: e.job_id.clone(),
                session: e.session.clone().unwrap_or_default(),
            },
            scope: e.scope(),
            expires: e.expires.clone(),
            ts: e.ts.clone(),
        }
    }
}

/// How one lease ended (or did not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreStatus {
    /// The original value is back (verified).
    Restored,
    /// The key was absent before; `settings delete` removed it (verified).
    Deleted,
    /// The user changed it since; compare-and-swap left it alone.
    Left,
    /// A fake-heat override was reset (verified).
    Reset,
    /// The phone did not answer, or the result could not be verified; kept for later.
    Unreachable,
    /// The phone is `read_only`; kept.
    Refused,
    /// A malformed record; kept and reported.
    Error(String),
}

/// "Never panel-off while working": values that could turn the panel off are refused.
pub fn panel_off_refusal(ns: &str, key: &str, value: &str) -> Option<String> {
    match (ns, key) {
        ("system", "screen_brightness") => match value.trim().parse::<i64>() {
            Ok(v) if v >= 1 => None,
            Ok(_) => Some(format!("screen_brightness {value} can turn the panel off; the minimum is {}", THRESHOLDS.dim_minimum)),
            Err(_) => Some(format!("screen_brightness {value:?} is not a number")),
        },
        ("system", "screen_off_timeout") => match value.trim().parse::<i64>() {
            Ok(v) if v >= THRESHOLDS.min_screen_timeout_ms as i64 => None,
            _ => Some(format!(
                "screen_off_timeout {value} would let the panel go off while working (minimum {} ms)",
                THRESHOLDS.min_screen_timeout_ms
            )),
        },
        _ => None,
    }
}

/// Refresh-rate keys some skins add, applied only when the phone's profile lists them.
/// Namespaces are assumptions (`secure miui_refresh_rate` is the usual name); verify per skin.
pub const VENDOR_REFRESH_KEYS: [(&str, &str); 2] = [
    ("secure", "miui_refresh_rate"),
    ("secure", "user_refresh_rate"),
];

/// The working lease bundle: dim to minimum, 60 Hz (plus vendor refresh keys the profile lists), a
/// screen timeout that keeps the panel on. `profile_keys` is `settings_keys` of `profiles/<serial>.json`.
pub fn agent_bundle(profile_keys: &[String]) -> Vec<(&'static str, &'static str, String)> {
    let t = &THRESHOLDS;
    let mut b = vec![
        ("system", "screen_brightness_mode", "0".to_string()),
        ("system", "screen_brightness", t.dim_minimum.to_string()),
        ("system", "peak_refresh_rate", t.refresh_hz.to_string()),
        ("system", "min_refresh_rate", t.refresh_hz.to_string()),
        (
            "system",
            "screen_off_timeout",
            t.working_screen_timeout_ms.to_string(),
        ),
    ];
    for (ns, key) in VENDOR_REFRESH_KEYS {
        if profile_keys.iter().any(|k| k == key) {
            b.push((ns, key, t.refresh_hz.to_string()));
        }
    }
    b
}

/// What `on_reconnect` did.
#[derive(Debug, Clone, Default)]
pub struct ReconnectReport {
    pub restored: Vec<(LeaseEntry, RestoreStatus)>,
    pub reasserted: Vec<String>,
}

/// The lease manager: one per daemon, over the shared store.
pub struct LeaseManager<S: CareSink> {
    pub store: LeaseStore,
    pub sink: S,
    clock: Arc<dyn CareClock>,
    pids: Arc<dyn PidProbe>,
    /// This daemon's pid: only its own live leases are re-asserted.
    pub pid: u32,
    /// Journal failures after a restore (the restore itself stood).
    pub journal_errors: Vec<String>,
}

impl<S: CareSink> LeaseManager<S> {
    pub fn new(
        store: LeaseStore,
        sink: S,
        clock: Arc<dyn CareClock>,
        pids: Arc<dyn PidProbe>,
    ) -> Self {
        LeaseManager {
            store,
            sink,
            clock,
            pids,
            pid: std::process::id(),
            journal_errors: vec![],
        }
    }

    fn ts(&self) -> String {
        rfc3339_utc(self.clock.unix_ms())
    }

    fn event(&self, kind: CareEventKind, ro: &str, ns: &str, key: &str) -> CareEvent {
        CareEvent {
            kind,
            ro_serial: ro.to_string(),
            ns: ns.to_string(),
            key: key.to_string(),
            before: None,
            applied: None,
            owner: None,
            scope: None,
            outcome: String::new(),
            class: None,
            reason: None,
            ts: self.ts(),
        }
    }

    fn event_of(&self, kind: CareEventKind, e: &LeaseEntry, outcome: &str) -> CareEvent {
        let mut ev = self.event(kind, e.ident(), &e.namespace, &e.key);
        ev.before = Some(e.before.clone());
        ev.applied = Some(e.applied.clone());
        ev.owner = Some(ChangeInForce::of(e).owner);
        ev.scope = Some(e.scope().as_str().to_string());
        ev.outcome = outcome.to_string();
        ev
    }

    fn refuse(
        &mut self,
        ro: &str,
        ns: &str,
        key: &str,
        owner: &Owner,
        scope: Scope,
        err: CareError,
    ) -> CareError {
        let mut ev = self.event(CareEventKind::Refused, ro, ns, key);
        ev.owner = Some(owner.clone());
        ev.scope = Some(scope.as_str().to_string());
        ev.outcome = "refused".into();
        ev.class = err.class().map(str::to_string);
        ev.reason = Some(err.to_string());
        if let Err(j) = self.sink.record(&ev) {
            self.journal_errors.push(j);
        }
        err
    }

    /// `settings_set(namespace, key, value, scope)` on one phone.
    #[allow(clippy::too_many_arguments)]
    pub fn apply<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
        owner: &Owner,
        ns: &str,
        key: &str,
        value: &str,
        scope: ScopeReq,
    ) -> Result<ChangeInForce, CareError> {
        let sc = scope.scope();
        // 1. read_only refuses every scope, before anything else (even a read).
        if effective_mode(&[phone.serial()], mode) == DeviceMode::ReadOnly {
            let e = CareError::Refused {
                class: Some("read_only"),
                reason: format!(
                    "{} is read_only: every lease scope is refused",
                    phone.serial()
                ),
            };
            return Err(self.refuse(phone.serial(), ns, key, owner, sc, e));
        }
        // 2. permanent is R2 and refused in Phase 2.
        if sc == Scope::Permanent {
            let e = CareError::Refused {
                class: Some("R2"),
                reason: "scope=permanent is R2 (a change meant to stay); Phase 2 has no approval channel, so it is refused".into(),
            };
            return Err(self.refuse(phone.serial(), ns, key, owner, sc, e));
        }
        if mode == DeviceMode::Helper {
            let e = CareError::refused("helper mode has no adb shell; settings leases need it");
            return Err(self.refuse(phone.serial(), ns, key, owner, sc, e));
        }
        let shape = check_target(ns, key).err().or_else(|| {
            if value == ABSENT {
                Some(CareError::Invalid(format!(
                    "{ABSENT:?} is the absent-key sentinel and cannot be applied"
                )))
            } else if value.chars().any(char::is_control) || value.len() > 1024 {
                Some(CareError::Invalid(
                    "a lease value must be at most 1024 bytes with no control characters".into(),
                ))
            } else {
                panel_off_refusal(ns, key, value).map(CareError::refused)
            }
        });
        if let Some(e) = shape {
            return Err(self.refuse(phone.serial(), ns, key, owner, sc, e));
        }
        let expires = match scope {
            ScopeReq::Until(t) if t <= self.clock.unix_ms() => {
                let e = CareError::Invalid("scope=until needs an expiry in the future".into());
                return Err(self.refuse(phone.serial(), ns, key, owner, sc, e));
            }
            ScopeReq::Until(t) => Some(rfc3339_utc(t)),
            _ => None,
        };
        // 3. identity: the protected phone behind an address is read_only too.
        let ro = phone.ro_serial()?;
        if effective_mode(&[phone.serial(), &ro], mode) == DeviceMode::ReadOnly {
            let e = CareError::Refused {
                class: Some("read_only"),
                reason: format!("{ro} is read_only: every lease scope is refused"),
            };
            return Err(self.refuse(&ro, ns, key, owner, sc, e));
        }
        // 4. the original value; one that cannot be restored byte for byte is not leased.
        let cur = phone.settings_get(ns, key)?;
        if let Some(c) = &cur {
            if c == ABSENT || c.chars().any(char::is_control) {
                let e = CareError::refused(format!("the current value of {ns}/{key} cannot be restored byte for byte, so it is not leased"));
                return Err(self.refuse(&ro, ns, key, owner, sc, e));
            }
        }
        let entry = LeaseEntry {
            key: key.to_string(),
            namespace: ns.to_string(),
            before: cur.unwrap_or_else(|| ABSENT.to_string()),
            applied: value.to_string(),
            owner_pid: owner.pid,
            job_id: owner.job_id.clone(),
            ts: self.ts(),
            serial: phone.serial().to_string(),
            ro_serial: ro.clone(),
            scope: (sc != Scope::Job).then_some(sc),
            expires,
            session: (!owner.session.is_empty()).then(|| owner.session.clone()),
            extra: Map::new(),
        };
        // 5. the record first, then the phone.
        let entry = self.store.upsert(entry)?;
        phone.settings_put(ns, key, value)?;
        let ev = self.event_of(CareEventKind::Change, &entry, "applied");
        self.sink.record(&ev).map_err(|j| {
            CareError::Journal(format!(
                "{ns}/{key} was applied and is leased, but the journal refused it: {j}"
            ))
        })?;
        Ok(ChangeInForce::of(&entry))
    }

    /// Apply [`agent_bundle`] as one job's leases. Stops at the first error (what was applied stays leased).
    pub fn apply_agent_bundle<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
        owner: &Owner,
        profile_keys: &[String],
    ) -> Result<Vec<ChangeInForce>, CareError> {
        agent_bundle(profile_keys)
            .into_iter()
            .map(|(ns, key, v)| self.apply(phone, mode, owner, ns, key, &v, ScopeReq::Job))
            .collect()
    }

    /// End one lease. See the module doc for the rules.
    pub fn restore_entry<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
        e: &LeaseEntry,
    ) -> RestoreStatus {
        if effective_mode(&[phone.serial(), e.ident()], mode) == DeviceMode::ReadOnly {
            return RestoreStatus::Refused;
        }
        let status = match e.namespace.as_str() {
            BATTERY_NS => {
                if !phone.battery_reset_verified() {
                    return RestoreStatus::Unreachable;
                }
                RestoreStatus::Reset
            }
            THERMAL_NS => {
                if !(crate::fake_heat::thermal_name_ok(&e.key)
                    && crate::fake_heat::thermal_name_ok(&e.applied))
                {
                    return RestoreStatus::Error(format!(
                        "thermal lease name or type {:?} is not shell-safe",
                        [&e.key, &e.applied]
                    ));
                }
                if !phone.thermal_reset_verified(&e.key, &e.applied) {
                    return RestoreStatus::Unreachable;
                }
                RestoreStatus::Reset
            }
            ns => {
                if let Err(err) = check_target(ns, &e.key) {
                    return RestoreStatus::Error(err.to_string());
                }
                let cur = match phone.settings_get(ns, &e.key) {
                    Ok(c) => c.unwrap_or_else(|| ABSENT.to_string()),
                    Err(_) => return RestoreStatus::Unreachable,
                };
                if cur != e.applied {
                    RestoreStatus::Left
                } else if e.before == ABSENT {
                    if phone.settings_delete(ns, &e.key).is_err()
                        || !matches!(phone.settings_get(ns, &e.key), Ok(None))
                    {
                        return RestoreStatus::Unreachable;
                    }
                    RestoreStatus::Deleted
                } else {
                    if phone.settings_put(ns, &e.key, &e.before).is_err()
                        || phone.settings_get(ns, &e.key).ok().flatten().as_deref()
                            != Some(e.before.as_str())
                    {
                        return RestoreStatus::Unreachable;
                    }
                    RestoreStatus::Restored
                }
            }
        };
        if let Err(err) = self.store.remove(e) {
            return RestoreStatus::Error(format!(
                "restored on the phone but leases.json kept the record: {err}"
            ));
        }
        let (kind, outcome) = match &status {
            RestoreStatus::Restored => (CareEventKind::Restore, "restored"),
            RestoreStatus::Deleted => (CareEventKind::Restore, "deleted"),
            RestoreStatus::Left => (CareEventKind::Restore, "left"),
            _ => (CareEventKind::OverrideReset, "reset"),
        };
        let ev = self.event_of(kind, e, outcome);
        if let Err(j) = self.sink.record(&ev) {
            self.journal_errors.push(j);
        }
        status
    }

    /// Restore every lease on this phone that `pred` selects, each on its own.
    pub fn restore_where<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
        pred: impl Fn(&LeaseEntry) -> bool,
    ) -> Result<Vec<(LeaseEntry, RestoreStatus)>, CareError> {
        let ro = phone.ro_serial()?;
        let mine: Vec<LeaseEntry> = self
            .store
            .entries()?
            .into_iter()
            .filter(|e| e.ident() == ro && pred(e))
            .collect();
        Ok(mine
            .into_iter()
            .map(|e| (e.clone(), self.restore_entry(phone, mode, &e)))
            .collect())
    }

    /// Job end: restore the job's `job`-scoped leases (and any fake-heat overrides it holds).
    pub fn restore_job<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
        job_id: &str,
    ) -> Result<Vec<(LeaseEntry, RestoreStatus)>, CareError> {
        self.restore_where(phone, mode, |e| {
            e.job_id == job_id && e.scope() == Scope::Job
        })
    }

    /// Session end: restore its `session`-scoped leases.
    pub fn restore_session<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
        session: &str,
    ) -> Result<Vec<(LeaseEntry, RestoreStatus)>, CareError> {
        self.restore_where(phone, mode, |e| {
            e.scope() == Scope::Session && e.session.as_deref() == Some(session)
        })
    }

    /// `until` leases whose expiry has passed (an unreadable expiry counts as passed).
    pub fn restore_expired<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
    ) -> Result<Vec<(LeaseEntry, RestoreStatus)>, CareError> {
        let now = self.clock.unix_ms();
        self.restore_where(phone, mode, |e| {
            e.scope() == Scope::Until
                && e.expires
                    .as_deref()
                    .and_then(parse_rfc3339)
                    .is_none_or(|t| t <= now)
        })
    }

    /// Leases whose owner process is dead (phone-guard `restore_stale`): crash, kill, reboot.
    pub fn restore_stale<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
    ) -> Result<Vec<(LeaseEntry, RestoreStatus)>, CareError> {
        let pids = self.pids.clone();
        self.restore_where(phone, mode, |e| !pids.alive(e.owner_pid))
    }

    /// Put drifted settings of this daemon's live leases on this phone back to the applied value
    /// (`job_id`: only that job's). Returns the keys re-asserted.
    pub fn reassert<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
        job_id: Option<&str>,
    ) -> Result<Vec<String>, CareError> {
        let ro = phone.ro_serial()?;
        if effective_mode(&[phone.serial(), &ro], mode) != DeviceMode::Full {
            return Ok(vec![]);
        }
        let mine: Vec<LeaseEntry> = self
            .store
            .entries()?
            .into_iter()
            .filter(|e| {
                e.ident() == ro
                    && e.is_setting()
                    && e.owner_pid == self.pid
                    && job_id.is_none_or(|j| e.job_id == j)
            })
            .collect();
        let mut out = vec![];
        for e in mine {
            let cur = phone
                .settings_get(&e.namespace, &e.key)?
                .unwrap_or_else(|| ABSENT.to_string());
            if cur != e.applied {
                phone.settings_put(&e.namespace, &e.key, &e.applied)?;
                let mut ev = self.event_of(CareEventKind::Reassert, &e, "reasserted");
                ev.reason = Some(format!("found {cur:?}"));
                self.sink.record(&ev).map_err(CareError::Journal)?;
                out.push(format!("{}/{}", e.namespace, e.key));
            }
        }
        Ok(out)
    }

    /// First contact after a reconnect (crash, kill, reboot): restore stale and expired leases on
    /// this phone, re-assert this daemon's live ones.
    pub fn on_reconnect<R: Runner>(
        &mut self,
        phone: &AdbPhone<'_, R>,
        mode: DeviceMode,
    ) -> Result<ReconnectReport, CareError> {
        let mut restored = self.restore_stale(phone, mode)?;
        restored.extend(self.restore_expired(phone, mode)?);
        let reasserted = self.reassert(phone, mode, None)?;
        Ok(ReconnectReport {
            restored,
            reasserted,
        })
    }

    /// Journal a fake-heat override that was just set (used by [`crate::fake_heat`]).
    pub fn note_override(&mut self, e: &LeaseEntry) -> Result<(), CareError> {
        let ev = self.event_of(CareEventKind::OverrideSet, e, "set");
        self.sink.record(&ev).map_err(CareError::Journal)
    }

    /// The pid probe this manager uses (shared with the floor/temperature exclusion markers).
    pub fn pids(&self) -> Arc<dyn PidProbe> {
        self.pids.clone()
    }

    pub fn now_ts(&self) -> String {
        self.ts()
    }

    /// The `changes_in_force` list for one phone (`ro.serialno`): settings leases only.
    pub fn changes_in_force(&self, ro_serial: &str) -> Result<Vec<ChangeInForce>, CareError> {
        Ok(self
            .store
            .entries()?
            .iter()
            .filter(|e| e.is_setting() && e.ident() == ro_serial)
            .map(ChangeInForce::of)
            .collect())
    }

    /// Fake-heat overrides in force on one phone (not part of `changes_in_force`, whose `ns` enum
    /// has no place for them).
    pub fn overrides_in_force(&self, ro_serial: &str) -> Result<Vec<LeaseEntry>, CareError> {
        Ok(self
            .store
            .entries()?
            .into_iter()
            .filter(|e| !e.is_setting() && e.ident() == ro_serial)
            .collect())
    }
}
