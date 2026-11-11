//! The laptop watcher: every 15 s check the target; if it is gone, try mDNS first, then the last
//! working address, with a TCP reachability probe before every `adb connect`. Full-jitter backoff
//! between recovery attempts. Emits [`Event`]s.
//!
//! One call to [`Watcher::step`] is one check plus at most one recovery round. [`Watcher::run_until`]
//! loops it on the injected clock, so tests drive it on a fake clock with a fake runner and prober.

use std::net::SocketAddr;

use crate::gone::{Cause, DeviceGone, Evidence};
use crate::guard::S24_SERIAL;
use crate::parse::{parse_devices, parse_mdns, phone_id, Listed};
use crate::runner::{args, Clock, Prober, Reach, RunOutput, Runner};

/// What the watcher tells its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The target is listed as `device` (one per healthy check).
    Present,
    /// The target went missing (emitted on the first miss, and again if the likely cause changes).
    Gone { cause: Cause, human_fix: String },
    /// The target is back. `after_ms` counts from the first check that found it gone.
    Reconnected { after_ms: u64 },
}

/// Watcher settings.
#[derive(Debug, Clone)]
pub struct Config {
    /// Phone id (hardware serial), mDNS name, or `ip:port` of the phone to keep attached.
    pub target: String,
    /// Healthy-state check period. 15 s.
    pub interval_ms: u64,
    pub backoff_base_ms: u64,
    pub backoff_cap_ms: u64,
    /// Smallest delay after a failed round, so full jitter never busy-loops adb.
    pub backoff_floor_ms: u64,
    pub probe_timeout_ms: u64,
    pub cmd_timeout_ms: u64,
    /// Last working `ip:port`, if known from an earlier run.
    pub last_addr: Option<String>,
    /// Hardware serial a connected address must report; defaults from `target` when it is a serial or mDNS name.
    pub expect_hw_serial: Option<String>,
    pub seed: u64,
}

impl Config {
    pub fn new(target: impl Into<String>) -> Self {
        let target = target.into();
        let pid = phone_id(&target);
        let expect = if pid.contains(':') { None } else { Some(pid) };
        Self {
            target,
            interval_ms: 15_000,
            backoff_base_ms: 500,
            backoff_cap_ms: 4_000,
            backoff_floor_ms: 100,
            probe_timeout_ms: 2_000,
            cmd_timeout_ms: 10_000,
            last_addr: None,
            expect_hw_serial: expect,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// Small xorshift generator for the jitter (no dependency, seedable for tests).
#[derive(Debug, Clone)]
pub struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x2545_F491_4F6C_DD1D
        } else {
            seed
        })
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform in `0..=upper`.
    pub fn below(&mut self, upper: u64) -> u64 {
        if upper == u64::MAX {
            self.next_u64()
        } else {
            self.next_u64() % (upper + 1)
        }
    }
}

/// The watcher.
pub struct Watcher<R: Runner, P: Prober, C: Clock> {
    runner: R,
    prober: P,
    clock: C,
    cfg: Config,
    rng: XorShift,
    last_addr: Option<String>,
    down_since: Option<u64>,
    attempt: u32,
    last_cause: Option<Cause>,
    last_gone: Option<DeviceGone>,
    last_error: Option<String>,
}

fn looks_like_s24(s: &str) -> bool {
    s.to_ascii_uppercase().contains(S24_SERIAL)
}

impl<R: Runner, P: Prober, C: Clock> Watcher<R, P, C> {
    /// Refuses a target that is the protected phone.
    pub fn new(runner: R, prober: P, clock: C, cfg: Config) -> Result<Self, String> {
        if looks_like_s24(&cfg.target) || cfg.last_addr.as_deref().is_some_and(looks_like_s24) {
            return Err("the protected phone is read-only and never a target".into());
        }
        let rng = XorShift::new(cfg.seed);
        let last_addr = cfg.last_addr.clone();
        Ok(Self {
            runner,
            prober,
            clock,
            cfg,
            rng,
            last_addr,
            down_since: None,
            attempt: 0,
            last_cause: None,
            last_gone: None,
            last_error: None,
        })
    }

    /// Last address that worked (persist it between runs).
    pub fn last_addr(&self) -> Option<&str> {
        self.last_addr.as_deref()
    }

    /// The structured `device_gone` for the current outage, if the target is down.
    pub fn device_gone(&self) -> Option<&DeviceGone> {
        if self.down_since.is_some() {
            self.last_gone.as_ref()
        } else {
            None
        }
    }

    pub fn is_down(&self) -> bool {
        self.down_since.is_some()
    }

    /// Last adb failure that kept a check from happening (adb missing, server error).
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn run(&self, a: &[&str]) -> Result<RunOutput, String> {
        self.runner
            .run(&args(a), self.cfg.cmd_timeout_ms)
            .map_err(|e| e.to_string())
    }

    fn list(&self) -> Result<Vec<Listed>, String> {
        let out = self.run(&["devices", "-l"])?;
        if !out.ok() {
            return Err(format!(
                "adb devices -l exited {:?}: {}",
                out.code,
                out.stderr.trim()
            ));
        }
        Ok(parse_devices(&out.stdout))
    }

    fn matches(&self, l: &Listed) -> bool {
        if looks_like_s24(&l.serial) {
            return false;
        }
        l.serial == self.cfg.target
            || phone_id(&l.serial) == phone_id(&self.cfg.target)
            || self.last_addr.as_deref() == Some(l.serial.as_str())
    }

    fn mdns_records(&self) -> Vec<String> {
        let Ok(out) = self.run(&["mdns", "services"]) else {
            return Vec::new();
        };
        let want = phone_id(&self.cfg.target);
        let mut addrs = Vec::new();
        for r in parse_mdns(&out.stdout) {
            if looks_like_s24(&r.phone_id) {
                self.runner.deny(&r.addr); // the protected phone's address is never ours
                continue;
            }
            if r.phone_id == want && !addrs.contains(&r.addr) {
                addrs.push(r.addr);
            }
        }
        addrs
    }

    fn backoff(&mut self) -> u64 {
        let shift = self.attempt.min(16);
        let bound = self
            .cfg
            .backoff_cap_ms
            .min(self.cfg.backoff_base_ms.saturating_mul(1u64 << shift));
        self.attempt = self.attempt.saturating_add(1);
        self.rng.below(bound).max(self.cfg.backoff_floor_ms)
    }

    /// One check, and if the target is gone, one recovery round. Returns the events and how long
    /// the caller should wait before the next step.
    pub fn step(&mut self) -> (Vec<Event>, u64) {
        let mut events = Vec::new();
        let listed = match self.list() {
            Ok(l) => {
                self.last_error = None;
                l
            }
            Err(e) => {
                self.last_error = Some(e);
                return (events, self.cfg.interval_ms);
            }
        };

        let mut present: Option<&Listed> = None;
        let mut other_state: Option<String> = None;
        for l in listed.iter().filter(|l| self.matches(l)) {
            if l.state == "device" {
                present.get_or_insert(l);
            } else if other_state.is_none() {
                other_state = Some(l.state.clone());
            }
        }

        if let Some(p) = present {
            if p.network()
                && !p.serial.contains("._adb-tls")
                && p.serial.parse::<SocketAddr>().is_ok()
            {
                self.last_addr = Some(p.serial.clone());
            }
            match self.down_since.take() {
                Some(since) => {
                    events.push(Event::Reconnected {
                        after_ms: self.clock.now_ms().saturating_sub(since),
                    });
                    self.reset_outage();
                }
                None => events.push(Event::Present),
            }
            return (events, self.cfg.interval_ms);
        }

        // Gone.
        let now = self.clock.now_ms();
        if self.down_since.is_none() {
            self.down_since = Some(now);
            self.attempt = 0;
            self.last_cause = None;
        }
        let since = self.down_since.unwrap_or(now);

        let mdns_addrs = self.mdns_records();
        let probed: Vec<(String, Reach)> = mdns_addrs
            .iter()
            .map(|a| (a.clone(), self.prober.probe(a, self.cfg.probe_timeout_ms)))
            .collect();
        let mdns_best = probed
            .iter()
            .find(|(_, r)| *r == Reach::Open)
            .or(probed.first())
            .cloned();
        let last = self.last_addr.clone().map(|a| {
            let r = self.prober.probe(&a, self.cfg.probe_timeout_ms);
            (a, r)
        });
        let gone = DeviceGone::from_evidence(
            &self.cfg.target,
            Evidence {
                listed_state: other_state,
                mdns: mdns_best,
                last: last.clone(),
            },
        );
        if self.last_cause != Some(gone.cause) {
            events.push(Event::Gone {
                cause: gone.cause,
                human_fix: gone.human_fix.clone(),
            });
            self.last_cause = Some(gone.cause);
        }
        let cause = gone.cause;
        self.last_gone = Some(gone);

        if cause != Cause::Unauthorised {
            // mDNS first, then the last working address; only addresses that answered the probe.
            let mut candidates: Vec<String> = probed
                .iter()
                .filter(|(_, r)| *r == Reach::Open)
                .map(|(a, _)| a.clone())
                .collect();
            if let Some((a, Reach::Open)) = &last {
                if !candidates.contains(a) {
                    candidates.push(a.clone());
                }
            }
            for addr in candidates {
                if self.try_connect(&addr, &listed) {
                    self.last_addr = Some(addr);
                    events.push(Event::Reconnected {
                        after_ms: self.clock.now_ms().saturating_sub(since),
                    });
                    self.reset_outage();
                    return (events, self.cfg.interval_ms);
                }
            }
        }
        let delay = self.backoff();
        (events, delay)
    }

    fn reset_outage(&mut self) {
        self.down_since = None;
        self.attempt = 0;
        self.last_cause = None;
        self.last_gone = None;
    }

    /// Connect to one address, check who answered, and confirm adb lists it as `device`.
    fn try_connect(&mut self, addr: &str, listed: &[Listed]) -> bool {
        if addr.parse::<SocketAddr>().is_err() {
            return false;
        }
        if listed
            .iter()
            .any(|l| l.serial == addr && l.state != "device")
        {
            let _ = self.run(&["disconnect", addr]);
        }
        let Ok(out) = self.run(&["connect", addr]) else {
            return false;
        };
        let text = format!("{} {}", out.stdout, out.stderr).to_lowercase();
        if !out.ok()
            || !text.contains("connected to")
            || text.contains("failed")
            || text.contains("cannot")
        {
            return false;
        }
        if !self.identity_ok(addr) {
            return false;
        }
        matches!(self.list(), Ok(l) if l.iter().any(|x| x.serial == addr && x.state == "device"))
    }

    /// Ask the phone at `addr` for its hardware serial. The protected phone, a wrong phone, or no answer: drop the link.
    fn identity_ok(&mut self, addr: &str) -> bool {
        let hw = match self.run(&["-s", addr, "shell", "getprop ro.serialno"]) {
            Ok(o) if o.ok() => o.stdout.trim().to_string(),
            _ => String::new(),
        };
        let good = if hw.is_empty() {
            false
        } else if hw.eq_ignore_ascii_case(S24_SERIAL) {
            if self.last_addr.as_deref() == Some(addr) {
                self.last_addr = None;
            }
            false
        } else {
            self.cfg
                .expect_hw_serial
                .as_deref()
                .is_none_or(|e| e.eq_ignore_ascii_case(&hw))
        };
        if !good {
            // Disconnect first: once the address is denied the guard would refuse this too.
            let _ = self.run(&["disconnect", addr]);
            if hw.eq_ignore_ascii_case(S24_SERIAL) {
                self.runner.deny(addr);
            }
        }
        good
    }

    /// Loop [`step`](Self::step) on the clock. `f` sees each step's events and returns true to stop.
    pub fn run_until<F: FnMut(&[Event]) -> bool>(&mut self, mut f: F) {
        loop {
            let (events, delay) = self.step();
            if f(&events) {
                return;
            }
            self.clock.sleep_ms(delay);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::GuardedRunner;
    use crate::runner::{RunError, RunOutput};
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    const POCO: &str = "DEVICE0000EXAMPLE";
    const A1: &str = "192.0.2.78:37123";
    const OLD: &str = "192.0.2.246:37123";

    struct World {
        clock: Rc<Cell<u64>>,
        connected: HashSet<String>,
        /// addr -> (reach before `at`, `at`): Open from `at` on.
        rules: HashMap<String, (Reach, u64)>,
        hw: HashMap<String, String>,
        mdns: Vec<(String, String)>,
        extra_list: Vec<String>,
        log: Vec<String>,
        connect_fail: u32,
        drop_at: Option<u64>,
        down_for: u64,
    }

    impl World {
        fn now(&self) -> u64 {
            self.clock.get()
        }
        fn script(&mut self) {
            if let Some(d) = self.drop_at {
                if self.now() >= d {
                    self.drop_at = None;
                    self.connected.clear();
                    self.rules
                        .insert(A1.into(), (Reach::Unreachable, d + self.down_for));
                }
            }
        }
        fn reach(&self, a: &str) -> Reach {
            match self.rules.get(a) {
                Some((before, at)) => {
                    if self.now() >= *at {
                        Reach::Open
                    } else {
                        *before
                    }
                }
                None => Reach::Unreachable,
            }
        }
    }

    type Shared = Rc<RefCell<World>>;

    struct FakeRunner(Shared);
    struct FakeProber(Shared);
    #[derive(Clone)]
    struct FakeClock(Rc<Cell<u64>>);

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
        fn sleep_ms(&self, ms: u64) {
            self.0.set(self.0.get() + ms);
        }
    }

    impl Prober for FakeProber {
        fn probe(&self, addr: &str, _t: u64) -> Reach {
            let mut w = self.0.borrow_mut();
            w.script();
            w.log.push(format!("probe {addr}"));
            w.reach(addr)
        }
    }

    fn ok(s: String) -> Result<RunOutput, RunError> {
        Ok(RunOutput {
            code: Some(0),
            stdout: s,
            stderr: String::new(),
        })
    }

    impl Runner for FakeRunner {
        fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
            let mut w = self.0.borrow_mut();
            w.script();
            w.log.push(a.join(" "));
            let s: Vec<&str> = a.iter().map(String::as_str).collect();
            match s.as_slice() {
                ["devices", "-l"] => {
                    let mut o = String::from("List of devices attached\n");
                    let mut addrs: Vec<&String> = w.connected.iter().collect();
                    addrs.sort();
                    for addr in addrs {
                        o.push_str(&format!("{addr:<22} device product:peridot model:POCO_F6 device:peridot transport_id:3\n"));
                    }
                    for l in &w.extra_list {
                        o.push_str(l);
                        o.push('\n');
                    }
                    ok(o)
                }
                ["mdns", "services"] => {
                    let mut o = String::from("List of discovered mdns services\n");
                    for (name, addr) in &w.mdns {
                        o.push_str(&format!("{name}\t_adb-tls-connect._tcp\t{addr}\n"));
                    }
                    ok(o)
                }
                ["connect", addr] => {
                    if w.reach(addr) == Reach::Open && w.connect_fail == 0 {
                        w.connected.insert(addr.to_string());
                        ok(format!("connected to {addr}\n"))
                    } else {
                        if w.connect_fail > 0 && w.reach(addr) == Reach::Open {
                            w.connect_fail -= 1;
                        }
                        ok(format!(
                            "failed to connect to '{addr}': Connection refused\n"
                        ))
                    }
                }
                ["disconnect", addr] => {
                    w.connected.remove(*addr);
                    ok(format!("disconnected {addr}\n"))
                }
                ["-s", addr, "shell", "getprop ro.serialno"] => {
                    let hw = w.hw.get(*addr).cloned().unwrap_or_else(|| POCO.to_string());
                    ok(format!("{hw}\n"))
                }
                other => Err(RunError::Io(format!("fake runner: unexpected {other:?}"))),
            }
        }
    }

    type TestWatcher = Watcher<GuardedRunner<FakeRunner>, FakeProber, FakeClock>;

    fn rig(last: Option<&str>) -> (TestWatcher, Shared, Rc<Cell<u64>>) {
        let clock = Rc::new(Cell::new(1_000_000));
        let w = Rc::new(RefCell::new(World {
            clock: clock.clone(),
            connected: HashSet::new(),
            rules: HashMap::new(),
            hw: HashMap::new(),
            mdns: vec![],
            extra_list: vec![],
            log: vec![],
            connect_fail: 0,
            drop_at: None,
            down_for: 0,
        }));
        let mut cfg = Config::new(POCO);
        cfg.last_addr = last.map(str::to_string);
        let watcher = Watcher::new(
            GuardedRunner::new(FakeRunner(w.clone())),
            FakeProber(w.clone()),
            FakeClock(clock.clone()),
            cfg,
        )
        .unwrap();
        (watcher, w, clock)
    }

    fn mdns_name() -> String {
        format!("adb-{POCO}-abc123 (2)._adb-tls-connect._tcp")
    }

    fn connects(w: &Shared) -> Vec<String> {
        w.borrow()
            .log
            .iter()
            .filter(|l| l.starts_with("connect "))
            .cloned()
            .collect()
    }

    fn pos(w: &Shared, what: &str) -> usize {
        w.borrow()
            .log
            .iter()
            .position(|l| l == what)
            .unwrap_or_else(|| panic!("{what} not in log"))
    }

    #[test]
    fn present_is_a_heartbeat_every_15s() {
        let (mut wt, w, _) = rig(Some(A1));
        w.borrow_mut().connected.insert(A1.into());
        let (ev, delay) = wt.step();
        assert_eq!(ev, vec![Event::Present]);
        assert_eq!(delay, 15_000);
        assert!(!wt.is_down());
    }

    #[test]
    fn rejects_the_s24_as_target() {
        let clock = Rc::new(Cell::new(0));
        let w = Rc::new(RefCell::new(World {
            clock: clock.clone(),
            connected: HashSet::new(),
            rules: HashMap::new(),
            hw: HashMap::new(),
            mdns: vec![],
            extra_list: vec![],
            log: vec![],
            connect_fail: 0,
            drop_at: None,
            down_for: 0,
        }));
        let r = Watcher::new(
            FakeRunner(w.clone()),
            FakeProber(w.clone()),
            FakeClock(clock),
            Config::new(S24_SERIAL),
        );
        assert!(r.is_err());
    }

    #[test]
    fn mdns_is_tried_before_the_last_working_address() {
        let (mut wt, w, _) = rig(Some(A1));
        let new_addr = "192.0.2.80:41000";
        {
            let mut wb = w.borrow_mut();
            wb.mdns.push((mdns_name(), new_addr.into()));
            wb.rules.insert(new_addr.into(), (Reach::Unreachable, 0));
            wb.rules.insert(A1.into(), (Reach::Unreachable, 0));
        }
        let (ev, _) = wt.step();
        assert!(matches!(ev.last(), Some(Event::Reconnected { .. })));
        assert_eq!(connects(&w), vec![format!("connect {new_addr}")]);
        assert_eq!(wt.last_addr(), Some(new_addr));
    }

    #[test]
    fn stale_mdns_ip_falls_back_to_last_working_address() {
        let (mut wt, w, _) = rig(Some(A1));
        {
            let mut wb = w.borrow_mut();
            wb.mdns.push((mdns_name(), OLD.into())); // mDNS announces the old IP: nothing there
            wb.rules.insert(A1.into(), (Reach::Unreachable, 0)); // the phone is really at A1
        }
        let (ev, _) = wt.step();
        match &ev[0] {
            Event::Gone { cause, human_fix } => {
                assert_eq!(*cause, Cause::StaleMdnsIp);
                assert!(!human_fix.is_empty());
            }
            e => panic!("expected Gone first, got {e:?}"),
        }
        assert!(matches!(ev[1], Event::Reconnected { .. }));
        assert_eq!(
            connects(&w),
            vec![format!("connect {A1}")],
            "must never connect to the stale IP"
        );
        // reachability probe came before the connect
        assert!(pos(&w, &format!("probe {A1}")) < pos(&w, &format!("connect {A1}")));
        assert!(pos(&w, "mdns services") < pos(&w, &format!("connect {A1}")));
        assert_eq!(wt.last_addr(), Some(A1));
    }

    fn gone_cause(ev: &[Event]) -> (Cause, String) {
        for e in ev {
            if let Event::Gone { cause, human_fix } = e {
                return (*cause, human_fix.clone());
            }
        }
        panic!("no Gone event in {ev:?}");
    }

    #[test]
    fn device_gone_in_every_down_state() {
        // wifi_off: nothing answers
        let (mut wt, w, _) = rig(Some(A1));
        w.borrow_mut().mdns.push((mdns_name(), A1.into()));
        w.borrow_mut()
            .rules
            .insert(A1.into(), (Reach::Unreachable, u64::MAX));
        let (ev, _) = wt.step();
        assert_eq!(gone_cause(&ev).0, Cause::WifiOff);
        assert!(
            connects(&w).is_empty(),
            "no adb connect when the probe fails"
        );
        assert!(wt
            .device_gone()
            .unwrap()
            .to_json()
            .contains("\"cause\":\"wifi_off\""));
        // the same cause is not announced again
        let (ev2, delay) = wt.step();
        assert!(ev2.is_empty());
        assert!((100..=4_000).contains(&delay));

        // wireless_debugging_off: host up, port closed
        let (mut wt, w, _) = rig(Some(A1));
        w.borrow_mut()
            .rules
            .insert(A1.into(), (Reach::Refused, u64::MAX));
        let (ev, _) = wt.step();
        let (c, fix) = gone_cause(&ev);
        assert_eq!(c, Cause::WirelessDebuggingOff);
        assert!(fix.contains("Wireless debugging"));
        assert!(connects(&w).is_empty());

        // stale_mdns_ip (connect keeps failing so it stays down)
        let (mut wt, w, _) = rig(Some(A1));
        w.borrow_mut().mdns.push((mdns_name(), OLD.into()));
        w.borrow_mut()
            .rules
            .insert(A1.into(), (Reach::Unreachable, 0));
        w.borrow_mut().connect_fail = 99;
        let (ev, _) = wt.step();
        assert_eq!(gone_cause(&ev).0, Cause::StaleMdnsIp);
        assert!(wt.is_down());

        // unauthorised: listed, no connect attempt
        let (mut wt, w, _) = rig(Some(A1));
        w.borrow_mut()
            .extra_list
            .push(format!("{}    unauthorized transport_id:5", mdns_name()));
        w.borrow_mut()
            .rules
            .insert(A1.into(), (Reach::Unreachable, 0));
        let (ev, _) = wt.step();
        let (c, fix) = gone_cause(&ev);
        assert_eq!(c, Cause::Unauthorised);
        assert!(fix.contains("tap Allow"));
        assert!(connects(&w).is_empty());

        // offline
        let (mut wt, w, _) = rig(Some(A1));
        w.borrow_mut()
            .extra_list
            .push(format!("{A1}    offline transport_id:6"));
        w.borrow_mut()
            .rules
            .insert(A1.into(), (Reach::Unreachable, u64::MAX));
        let (ev, _) = wt.step();
        let (c, fix) = gone_cause(&ev);
        assert_eq!(c, Cause::Offline);
        assert!(fix.contains("unplug and replug"));

        // unknown: no mDNS record, no remembered address
        let (mut wt, _w, _) = rig(None);
        let (ev, _) = wt.step();
        assert_eq!(gone_cause(&ev).0, Cause::Unknown);
        assert!(wt.device_gone().is_some());
    }

    #[test]
    fn s24_is_never_connected_from_mdns() {
        let (mut wt, w, _) = rig(None);
        let s24_addr = "192.0.2.99:40000";
        {
            let mut wb = w.borrow_mut();
            wb.mdns.push((
                format!("adb-{S24_SERIAL}-abc123._adb-tls-connect._tcp"),
                s24_addr.into(),
            ));
            wb.rules.insert(s24_addr.into(), (Reach::Unreachable, 0));
        }
        let (ev, _) = wt.step();
        assert!(!ev.iter().any(|e| matches!(e, Event::Reconnected { .. })));
        assert!(connects(&w).is_empty());
    }

    #[test]
    fn address_reassigned_to_the_s24_is_dropped_and_never_retried() {
        let (mut wt, w, _) = rig(Some(A1));
        {
            let mut wb = w.borrow_mut();
            wb.rules.insert(A1.into(), (Reach::Unreachable, 0));
            wb.hw.insert(A1.into(), S24_SERIAL.into());
        }
        let (ev, _) = wt.step();
        assert!(!ev.iter().any(|e| matches!(e, Event::Reconnected { .. })));
        assert_eq!(connects(&w).len(), 1);
        assert!(
            w.borrow().connected.is_empty(),
            "link to the protected phone must be dropped again"
        );
        assert!(w.borrow().log.contains(&format!("disconnect {A1}")));
        assert_eq!(wt.last_addr(), None);
        for _ in 0..3 {
            wt.step();
        }
        assert_eq!(connects(&w).len(), 1);
    }

    #[test]
    fn wrong_phone_at_last_address_is_dropped() {
        let (mut wt, w, _) = rig(Some(A1));
        {
            let mut wb = w.borrow_mut();
            wb.rules.insert(A1.into(), (Reach::Unreachable, 0));
            wb.hw.insert(A1.into(), "SOMEOTHERPHONE".into());
        }
        let (ev, _) = wt.step();
        assert!(!ev.iter().any(|e| matches!(e, Event::Reconnected { .. })));
        assert!(w.borrow().connected.is_empty());
    }

    struct Cycle {
        /// `Reconnected.after_ms`: from the first check that found the phone gone.
        after_ms: u64,
        /// From the physical drop (includes the up-to-15 s detection wait).
        from_drop_ms: u64,
        /// From the moment the phone was reachable again (includes detection if it came back first).
        from_up_ms: u64,
        /// Recovery lag once both detected and reachable: what backoff alone costs.
        lag_ms: u64,
        gone_seen: bool,
    }

    /// Drop the link `offset` ms from now; the phone is unreachable for `down_for` ms.
    fn cycle(
        wt: &mut TestWatcher,
        w: &Shared,
        clock: &Rc<Cell<u64>>,
        offset: u64,
        down_for: u64,
        fail_first: u32,
    ) -> Cycle {
        let drop_at = clock.get() + offset;
        {
            let mut wb = w.borrow_mut();
            wb.drop_at = Some(drop_at);
            wb.down_for = down_for;
            wb.connect_fail = fail_first;
        }
        let mut out = Cycle {
            after_ms: u64::MAX,
            from_drop_ms: u64::MAX,
            from_up_ms: u64::MAX,
            lag_ms: u64::MAX,
            gone_seen: false,
        };
        let mut detected_at = 0;
        wt.run_until(|events| {
            for e in events {
                match e {
                    Event::Gone { human_fix, .. } => {
                        out.gone_seen = !human_fix.is_empty();
                        detected_at = clock.get();
                    }
                    Event::Reconnected { after_ms } => {
                        out.after_ms = *after_ms;
                        out.from_drop_ms = clock.get() - drop_at;
                        out.from_up_ms = clock.get().saturating_sub(drop_at + down_for);
                        out.lag_ms = clock
                            .get()
                            .saturating_sub(detected_at.max(drop_at + down_for));
                        return true;
                    }
                    Event::Present => {}
                }
            }
            false
        });
        out
    }

    fn rig_up() -> (TestWatcher, Shared, Rc<Cell<u64>>) {
        let (wt, w, clock) = rig(Some(A1));
        {
            let mut wb = w.borrow_mut();
            wb.connected.insert(A1.into());
            wb.mdns.push((mdns_name(), A1.into()));
            wb.rules.insert(A1.into(), (Reach::Open, 0));
        }
        (wt, w, clock)
    }

    #[test]
    fn twenty_disconnect_reconnect_cycles_19_within_10s() {
        let (mut wt, w, clock) = rig_up();
        let mut within = 0;
        let mut worst_after = 0;
        let mut worst_drop = 0;
        let mut worst_up = 0;
        let mut within_drop = 0;
        for i in 0..20u64 {
            let offset = (i * 4_337) % 15_000;
            // phone back after 0.3 .. 5 s, with one slow outlier (cycle 7: 13 s) and every 4th
            // cycle's first connect failing even though the port is open
            let down_for = if i == 7 {
                13_000
            } else {
                300 + (i * 700) % 4_700
            };
            let c = cycle(
                &mut wt,
                &w,
                &clock,
                offset,
                down_for,
                if i % 4 == 3 { 1 } else { 0 },
            );
            assert!(c.gone_seen, "cycle {i}: no device_gone with a fix");
            assert!(c.after_ms != u64::MAX, "cycle {i}: never reconnected");
            if c.after_ms <= 10_000 {
                within += 1;
            }
            if c.from_drop_ms <= 10_000 {
                within_drop += 1;
            }
            worst_after = worst_after.max(c.after_ms);
            worst_drop = worst_drop.max(c.from_drop_ms);
            assert!(
                c.lag_ms <= 4_000 + 100,
                "cycle {i}: backoff lag {} ms",
                c.lag_ms
            );
            worst_up = worst_up.max(c.from_up_ms);
        }
        println!(
            "cycles: {within}/20 within 10 s of detection (worst {worst_after} ms); {within_drop}/20 within 10 s of the physical drop (worst {worst_drop} ms); worst {worst_up} ms after the phone was reachable again"
        );
        assert!(within >= 19, "only {within}/20 within 10 s");
    }

    #[test]
    fn wifi_kill_reattaches_within_60s_of_return_10_of_10() {
        let (mut wt, w, clock) = rig_up();
        for i in 0..10u64 {
            let down_for = 5_000 + i * 4_000; // 5 s .. 41 s
            let c = cycle(&mut wt, &w, &clock, (i * 1_900) % 15_000, down_for, 0);
            assert!(
                c.from_up_ms <= 60_000,
                "cycle {i}: {} ms after Wi-Fi returned",
                c.from_up_ms
            );
            assert!(c.gone_seen);
        }
    }

    #[test]
    fn backoff_is_full_jitter_within_bounds_and_grows_to_the_cap() {
        let (mut wt, _w, _) = rig(None);
        let mut max_seen = 0;
        for i in 0..200 {
            wt.attempt = i % 8;
            let bound = 4_000u64.min(500 << (i % 8));
            let d = wt.backoff();
            assert!(
                d >= 100 && d <= bound.max(100),
                "attempt {}: {d} > {bound}",
                i % 8
            );
            max_seen = max_seen.max(d);
        }
        assert!(
            max_seen > 2_000,
            "jitter should spread up to the cap, max was {max_seen}"
        );
    }

    #[test]
    fn adb_failure_is_not_reported_as_the_phone_being_gone() {
        struct Broken;
        impl Runner for Broken {
            fn run(&self, _a: &[String], _t: u64) -> Result<RunOutput, RunError> {
                Err(RunError::Io("cannot start adb".into()))
            }
        }
        let clock = Rc::new(Cell::new(0));
        let w = Rc::new(RefCell::new(World {
            clock: clock.clone(),
            connected: HashSet::new(),
            rules: HashMap::new(),
            hw: HashMap::new(),
            mdns: vec![],
            extra_list: vec![],
            log: vec![],
            connect_fail: 0,
            drop_at: None,
            down_for: 0,
        }));
        let mut wt =
            Watcher::new(Broken, FakeProber(w), FakeClock(clock), Config::new(POCO)).unwrap();
        let (ev, _) = wt.step();
        assert!(ev.is_empty());
        assert!(wt.last_error().unwrap().contains("cannot start adb"));
        assert!(!wt.is_down());
    }
}
