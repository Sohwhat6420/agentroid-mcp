//! Shared fakes for the 2.15 tests: a fake clock, fake settings leases with compare-and-swap, a
//! fixed planner, and fixture paths. Each test binary uses part of this.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use agentroid_adb_link::runner::Clock;
use handover::clash::Clash;
use handover::mode::{Caps, LeaseError, ModeMachine, Planner, SettingsLeases};
use handover::profile::{Profiles, SettingWrite};
use handover::vdisplay::Plan;

pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn fixture(rel: &str) -> String {
    let p = crate_dir().join("fixtures").join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

pub fn profiles_dir() -> PathBuf {
    crate_dir().join("../../profiles")
}

pub fn profiles() -> Profiles {
    Profiles::load(&profiles_dir()).expect("profiles/human.json and profiles/agent.json load")
}

/// `profiles/<name>.json`'s settings, read with serde_json directly (independent of `profile.rs`).
pub fn raw_settings(name: &str) -> BTreeMap<String, String> {
    let text = std::fs::read_to_string(profiles_dir().join(format!("{name}.json"))).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["settings"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect()
}

/// Fake hub clock, shared between the machine and the fake leases.
#[derive(Clone, Default)]
pub struct FakeClock(pub Rc<Cell<u64>>);

impl FakeClock {
    pub fn at(ms: u64) -> Self {
        Self(Rc::new(Cell::new(ms)))
    }
    pub fn set(&self, ms: u64) {
        self.0.set(ms)
    }
    pub fn advance(&self, ms: u64) {
        self.0.set(self.0.get() + ms)
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
    fn sleep_ms(&self, ms: u64) {
        self.advance(ms)
    }
}

/// The phone's settings store as seen through care: `ns:key` -> value.
#[derive(Default)]
pub struct Store {
    pub values: BTreeMap<String, String>,
    /// owner's leases: `ns:key` -> (before, applied); `None` before means the key was absent.
    pub leased: BTreeMap<String, (Option<String>, String)>,
    pub dnd_rule: bool,
    pub calls: Vec<String>,
}

/// Fake leases: each call takes `latency_ms` on the fake clock; `fail_next` calls fail first.
#[derive(Clone)]
pub struct FakeLeases {
    pub store: Rc<RefCell<Store>>,
    pub clock: FakeClock,
    pub latency_ms: u64,
    pub fail_next: Rc<Cell<u32>>,
}

impl FakeLeases {
    pub fn new(clock: FakeClock, latency_ms: u64) -> Self {
        let mut s = Store::default();
        // the phone's own values before any job (what compare-and-swap restores)
        for (k, v) in [
            ("system:screen_brightness", "143"),
            ("system:screen_brightness_mode", "1"),
            ("system:peak_refresh_rate", "120"),
            ("system:min_refresh_rate", "60"),
            ("system:screen_off_timeout", "30000"),
            ("global:stay_on_while_plugged_in", "0"),
            ("global:heads_up_notifications_enabled", "1"),
            ("global:wifi_scan_always_enabled", "1"),
            ("global:window_animation_scale", "0.5"),
        ] {
            s.values.insert(k.into(), v.into());
        }
        Self {
            store: Rc::new(RefCell::new(s)),
            clock,
            latency_ms,
            fail_next: Rc::new(Cell::new(0)),
        }
    }

    fn take_time_and_maybe_fail(&self, what: &str) -> Result<(), LeaseError> {
        self.clock.advance(self.latency_ms);
        if self.fail_next.get() > 0 {
            self.fail_next.set(self.fail_next.get() - 1);
            return Err(LeaseError(format!("{what}: settings put failed (fake)")));
        }
        Ok(())
    }
}

impl SettingsLeases for FakeLeases {
    fn lease_agent(
        &mut self,
        owner: &str,
        writes: &[SettingWrite],
        dnd_rule: bool,
    ) -> Result<(), LeaseError> {
        self.take_time_and_maybe_fail("lease_agent")?;
        let mut s = self.store.borrow_mut();
        s.calls.push(format!("lease_agent {owner}"));
        for w in writes {
            let id = w.id();
            let before = s.values.get(&id).cloned();
            let before = s.leased.get(&id).map(|l| l.0.clone()).unwrap_or(before);
            s.leased.insert(id.clone(), (before, w.value.clone()));
            s.values.insert(id, w.value.clone());
        }
        s.dnd_rule = dnd_rule;
        Ok(())
    }

    fn hand_back(
        &mut self,
        owner: &str,
        human: &[SettingWrite],
        dnd_rule: bool,
    ) -> Result<(), LeaseError> {
        self.take_time_and_maybe_fail("hand_back")?;
        let mut s = self.store.borrow_mut();
        s.calls.push(format!("hand_back {owner}"));
        let human_ids: Vec<String> = human.iter().map(SettingWrite::id).collect();
        for w in human {
            s.values.insert(w.id(), w.value.clone());
        }
        let leased = std::mem::take(&mut s.leased);
        for (id, (before, applied)) in leased {
            if human_ids.contains(&id) {
                continue;
            }
            // compare-and-swap: only restore what still holds our applied value
            if s.values.get(&id) == Some(&applied) {
                match before {
                    Some(b) => {
                        s.values.insert(id, b);
                    }
                    None => {
                        s.values.remove(&id);
                    }
                }
            }
        }
        s.dnd_rule = dnd_rule;
        Ok(())
    }
}

/// A planner that always answers the same.
pub struct FixedPlanner(pub Result<Plan, Clash>);

impl Planner for FixedPlanner {
    fn plan(&mut self) -> Result<Plan, Clash> {
        self.0.clone()
    }
}

pub fn ok_planner() -> FixedPlanner {
    FixedPlanner(Ok(Plan {
        vd_id: 2,
        moves: vec![],
    }))
}

pub const KEEP: Caps = Caps {
    s1_passed: true,
    sdk: 35,
};

/// A machine on a fake clock at `t0`, with leases that take `latency` ms per call.
pub fn machine(
    t0: u64,
    latency: u64,
    caps: Caps,
) -> (
    ModeMachine<FakeLeases, FakeClock>,
    FakeClock,
    Rc<RefCell<Store>>,
) {
    let clock = FakeClock::at(t0);
    let leases = FakeLeases::new(clock.clone(), latency);
    let store = leases.store.clone();
    (
        ModeMachine::new(leases, clock.clone(), profiles(), caps),
        clock,
        store,
    )
}
