//! Detector 1 (hub side): the `getevent` device classifier and touch-down detector.
//!
//! * [`parse_devices`] reads `getevent -pl` / `getevent -pli` device descriptions.
//! * [`classify`] picks touchscreens generically: `INPUT_PROP_DIRECT` with `ABS_MT_POSITION_X/Y`
//!   (or single-touch `ABS_X`/`ABS_Y` plus `BTN_TOUCH`), and never a virtual device (bus 0006,
//!   a uhid/uinput/scrcpy/virtual name, or a node that is not `/dev/input/eventN`).
//! * [`Detector`] reads `getevent -lt` lines and emits a [`HumanTouch`] per touch-down frame:
//!   `BTN_TOUCH DOWN`, a new `ABS_MT_TRACKING_ID` other than `ffffffff` (not while hovering), or
//!   `KEY_WAKEUP`/`KEY_POWER` from the touchscreen itself (a double-tap wake, round 32).
//!
//! Why injected input never counts: `input tap` / `input swipe` and scrcpy's default control path go
//! through `InputManager.injectInputEvent`, and `dispatchGesture` through the accessibility gesture
//! injector. None of them writes to an evdev node, so nothing appears on the physical touchscreen's
//! node. scrcpy's UHID mode (`--keyboard=uhid`, `--mouse=uhid`, gamepads) creates its own `/dev/uhid`
//! device: it is added while the stream runs (a hot-plug, excluded), its bus is 0006 and its name
//! says scrcpy. Any one of those three is enough to exclude it.

use std::collections::{HashMap, HashSet};

/// One `add device` block of `getevent -pl(i)`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InputDevice {
    pub node: String,
    pub name: String,
    /// `bus:` (only printed with `-i`), hex.
    pub bus: Option<u16>,
    /// `KEY (0001)` codes.
    pub keys: Vec<String>,
    /// `ABS (0003)` codes.
    pub abs: Vec<String>,
    /// `INPUT_PROP_*`.
    pub props: Vec<String>,
}

impl InputDevice {
    fn has_key(&self, k: &str) -> bool {
        self.keys.iter().any(|x| x == k)
    }
    fn has_abs(&self, k: &str) -> bool {
        self.abs.iter().any(|x| x == k)
    }
    fn has_prop(&self, k: &str) -> bool {
        self.props.iter().any(|x| x == k)
    }
}

/// What a device is, for 2.15.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// A physical touchscreen (or a direct stylus digitiser).
    Touchscreen,
    /// Excluded as virtual, with the reason.
    Virtual(&'static str),
    /// Anything else (keys, touchpads, sensors).
    Other,
}

/// BUS_VIRTUAL in `linux/input.h`: uinput and uhid devices made by software.
pub const BUS_VIRTUAL: u16 = 0x06;

fn is_event_node(p: &str) -> bool {
    p.strip_prefix("/dev/input/event")
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Why a device is virtual, if it is.
pub fn virtual_reason(d: &InputDevice) -> Option<&'static str> {
    if d.bus == Some(BUS_VIRTUAL) {
        return Some("bus 0006 (BUS_VIRTUAL: uinput or uhid)");
    }
    let name = d.name.to_ascii_lowercase();
    for (needle, why) in [
        ("uhid", "uhid device name"),
        ("uinput", "uinput device name"),
        ("scrcpy", "scrcpy device name"),
        ("virtual", "virtual device name"),
    ] {
        if name.contains(needle) {
            return Some(why);
        }
    }
    if !is_event_node(&d.node) {
        return Some("not a /dev/input/eventN node");
    }
    None
}

/// Generic touchscreen classifier (no vendor names).
pub fn classify(d: &InputDevice) -> Class {
    if let Some(why) = virtual_reason(d) {
        return Class::Virtual(why);
    }
    if d.has_prop("INPUT_PROP_ACCELEROMETER") || d.has_prop("INPUT_PROP_POINTER") {
        return Class::Other;
    }
    let multi = d.has_abs("ABS_MT_POSITION_X") && d.has_abs("ABS_MT_POSITION_Y");
    let single = d.has_abs("ABS_X") && d.has_abs("ABS_Y") && d.has_key("BTN_TOUCH");
    if d.has_prop("INPUT_PROP_DIRECT") && (multi || single) {
        Class::Touchscreen
    } else {
        Class::Other
    }
}

fn leading_spaces(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// `KEY (0001): ...` -> Some(("KEY", rest)).
fn type_header(t: &str) -> Option<(&str, &str)> {
    let (head, rest) = t.split_once("):")?;
    let (name, code) = head.split_once(" (")?;
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_uppercase()) || code.len() != 4 {
        return None;
    }
    Some((name, rest))
}

/// Parse `getevent -pl` or `getevent -pli` output into devices, in order.
pub fn parse_devices(text: &str) -> Vec<InputDevice> {
    #[derive(PartialEq)]
    enum Sec {
        None,
        Events,
        Props,
    }
    let mut out: Vec<InputDevice> = Vec::new();
    let mut sec = Sec::None;
    let mut ty = String::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("add device ") {
            let node = rest
                .split_once(':')
                .map(|(_, p)| p.trim().to_string())
                .unwrap_or_default();
            out.push(InputDevice {
                node,
                ..Default::default()
            });
            sec = Sec::None;
            continue;
        }
        let Some(dev) = out.last_mut() else { continue };
        if t.is_empty() {
            continue;
        }
        if leading_spaces(line) <= 2 {
            // a header line of the block
            sec = Sec::None;
            if let Some(v) = t.strip_prefix("bus:") {
                dev.bus = u16::from_str_radix(v.trim(), 16).ok();
            } else if let Some(v) = t.strip_prefix("name:") {
                dev.name = v.trim().trim_matches('"').to_string();
            } else if t == "events:" {
                sec = Sec::Events;
                ty.clear();
            } else if t == "input props:" {
                sec = Sec::Props;
            }
            continue;
        }
        match sec {
            Sec::Events => {
                let body = if let Some((name, rest)) = type_header(t) {
                    ty = name.to_string();
                    rest
                } else {
                    t
                };
                match ty.as_str() {
                    // one code per line, then ": value ..."
                    "ABS" => {
                        if let Some(code) = body.split_whitespace().next() {
                            dev.abs.push(code.trim_end_matches(':').to_string());
                        }
                    }
                    "KEY" => dev.keys.extend(body.split_whitespace().map(str::to_string)),
                    _ => {}
                }
            }
            Sec::Props => {
                dev.props.extend(
                    t.split_whitespace()
                        .filter(|w| w.starts_with("INPUT_PROP_"))
                        .map(str::to_string),
                );
            }
            Sec::None => {}
        }
    }
    out
}

/// What made a frame a human touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchKind {
    /// `BTN_TOUCH DOWN` (finger or stylus contact).
    BtnTouchDown,
    /// A new `ABS_MT_TRACKING_ID` other than `ffffffff`, not hovering.
    NewTrackingId,
    /// `KEY_WAKEUP` / `KEY_POWER` from the touchscreen node: a double-tap wake.
    WakeKey,
}

/// One human touch-down seen on a physical touchscreen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanTouch {
    /// Phone kernel timestamp of the first event of the frame, in microseconds.
    pub at_us: u64,
    pub node: String,
    pub kind: TouchKind,
}

impl HumanTouch {
    pub fn at_ms(&self) -> u64 {
        self.at_us / 1000
    }
}

/// One parsed `getevent -lt` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Event {
        t_us: u64,
        node: String,
        ty: String,
        code: String,
        value: String,
    },
    AddDevice {
        node: String,
    },
    RemoveDevice {
        node: String,
    },
    Name(String),
    Other,
}

fn parse_ts(s: &str) -> Option<u64> {
    let (sec, frac) = s.trim().split_once('.')?;
    let sec: u64 = sec.trim().parse().ok()?;
    let mut f: String = frac.trim().chars().take(6).collect();
    while f.len() < 6 {
        f.push('0');
    }
    Some(sec * 1_000_000 + f.parse::<u64>().ok()?)
}

/// Parse one line of `getevent -lt`.
pub fn parse_line(raw: &str) -> Line {
    let line = raw.trim_end_matches('\r');
    let t = line.trim();
    if let Some(rest) = t.strip_prefix("add device ") {
        return rest
            .split_once(':')
            .map(|(_, p)| Line::AddDevice {
                node: p.trim().to_string(),
            })
            .unwrap_or(Line::Other);
    }
    if let Some(rest) = t.strip_prefix("remove device ") {
        return rest
            .split_once(':')
            .map(|(_, p)| Line::RemoveDevice {
                node: p.trim().to_string(),
            })
            .unwrap_or(Line::Other);
    }
    if let Some(v) = t.strip_prefix("name:") {
        return Line::Name(v.trim().trim_matches('"').to_string());
    }
    let Some(rest) = t.strip_prefix('[') else {
        return Line::Other;
    };
    let Some((ts, rest)) = rest.split_once(']') else {
        return Line::Other;
    };
    let Some(t_us) = parse_ts(ts) else {
        return Line::Other;
    };
    // "/dev/input/event5: EV_KEY BTN_TOUCH DOWN". Without the node prefix the event cannot be
    // attributed, so it is never counted.
    let Some((node, ev)) = rest.trim().split_once(": ") else {
        return Line::Other;
    };
    let mut w = ev.split_whitespace();
    match (w.next(), w.next(), w.next()) {
        (Some(ty), Some(code), Some(value)) => Line::Event {
            t_us,
            node: node.trim().to_string(),
            ty: ty.to_string(),
            code: code.to_string(),
            value: value.to_string(),
        },
        _ => Line::Other,
    }
}

#[derive(Debug, Clone, Default)]
struct Frame {
    first_us: Option<u64>,
    btn_touch_down: bool,
    new_tracking: bool,
    wake_key: bool,
    hovering: bool,
}

#[derive(Debug, Clone)]
struct Dev {
    class: Class,
    /// First seen while the stream ran: a physical touchscreen never hot-plugs, so it never counts.
    hotplugged: bool,
    name: String,
}

/// Detector 1: device map plus the event stream state.
#[derive(Debug, Clone, Default)]
pub struct Detector {
    devs: HashMap<String, Dev>,
    frames: HashMap<String, Frame>,
    last_added: Option<String>,
    remap: bool,
    /// Every node excluded so far, with its reason (for the report and the job view).
    pub excluded: Vec<(String, String)>,
}

impl Detector {
    /// Start from a `getevent -pl(i)` map.
    pub fn from_map(pl: &str) -> Self {
        Self::from_map_after(pl, &[])
    }

    /// Start again after a restart: devices that were hot-plugged in an earlier stream (same node
    /// and name, from [`Detector::hot_devices`]) stay excluded.
    pub fn from_map_after(pl: &str, hot: &[(String, String)]) -> Self {
        let mut d = Detector::default();
        for (node, name) in hot {
            d.devs.insert(
                node.clone(),
                Dev {
                    class: Class::Virtual("hot-plugged earlier"),
                    hotplugged: true,
                    name: name.clone(),
                },
            );
        }
        d.load(pl, false);
        d
    }

    /// Every hot-plugged device seen so far, as (node, name).
    pub fn hot_devices(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .devs
            .iter()
            .filter(|(_, d)| d.hotplugged)
            .map(|(n, d)| (n.clone(), d.name.clone()))
            .collect();
        v.sort();
        v
    }

    fn load(&mut self, pl: &str, hot: bool) {
        for dev in parse_devices(pl) {
            // a node keeps its hot-plug mark while the same device holds it
            let known_hot = self
                .devs
                .get(&dev.node)
                .filter(|x| x.name.is_empty() || x.name == dev.name)
                .map(|x| x.hotplugged);
            let hotplugged = known_hot.unwrap_or(hot);
            let class = classify(&dev);
            if let Class::Virtual(why) = &class {
                self.excluded.push((dev.node.clone(), why.to_string()));
            }
            self.devs.insert(
                dev.node.clone(),
                Dev {
                    class,
                    hotplugged,
                    name: dev.name.clone(),
                },
            );
        }
    }

    /// Re-map after an `add device` line (run `getevent -pli` again). Devices first seen while the
    /// stream ran stay excluded however they describe themselves.
    pub fn remap(&mut self, pl: &str) {
        self.load(pl, true);
        self.remap = false;
    }

    /// True when an `add device` was seen and [`Detector::remap`] has not run since.
    pub fn needs_remap(&self) -> bool {
        self.remap
    }

    /// Nodes that count as touchscreens now.
    pub fn touchscreens(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .devs
            .iter()
            .filter(|(_, d)| d.class == Class::Touchscreen && !d.hotplugged)
            .map(|(n, _)| n.clone())
            .collect();
        v.sort();
        v
    }

    /// True when `node` counts as a touchscreen now (a sample source for the clock map: the lines of
    /// other input nodes do not carry the touchscreen's report timing).
    pub fn is_touchscreen(&self, node: &str) -> bool {
        self.counts(node)
    }

    fn counts(&self, node: &str) -> bool {
        self.devs
            .get(node)
            .is_some_and(|d| d.class == Class::Touchscreen && !d.hotplugged)
    }

    /// Feed one `getevent -lt` line; returns a touch when a frame closes with a touch-down.
    pub fn feed(&mut self, raw: &str) -> Option<HumanTouch> {
        match parse_line(raw) {
            Line::AddDevice { node } => {
                // unknown until re-mapped, and never a touchscreen this stream
                self.devs.insert(
                    node.clone(),
                    Dev {
                        class: Class::Virtual("added while streaming"),
                        hotplugged: true,
                        name: String::new(),
                    },
                );
                self.excluded
                    .push((node.clone(), "added while streaming (hot-plug)".into()));
                self.last_added = Some(node);
                self.remap = true;
                None
            }
            Line::RemoveDevice { node } => {
                self.devs.remove(&node);
                self.frames.remove(&node);
                None
            }
            Line::Name(name) => {
                // the name line that follows an `add device`
                if let Some(d) = self.last_added.take().and_then(|n| self.devs.get_mut(&n)) {
                    d.name = name;
                }
                None
            }
            Line::Other => None,
            Line::Event {
                t_us,
                node,
                ty,
                code,
                value,
            } => {
                if !self.counts(&node) {
                    return None;
                }
                let f = self.frames.entry(node.clone()).or_default();
                match (ty.as_str(), code.as_str()) {
                    ("EV_SYN", "SYN_REPORT") => {
                        let f = std::mem::take(f);
                        let kind = if f.btn_touch_down {
                            Some(TouchKind::BtnTouchDown)
                        } else if f.wake_key {
                            Some(TouchKind::WakeKey)
                        } else if f.new_tracking && !f.hovering {
                            Some(TouchKind::NewTrackingId)
                        } else {
                            None
                        };
                        return kind.map(|kind| HumanTouch {
                            at_us: f.first_us.unwrap_or(t_us),
                            node,
                            kind,
                        });
                    }
                    ("EV_KEY", "BTN_TOUCH") if value == "DOWN" => {
                        f.btn_touch_down = true;
                        f.first_us.get_or_insert(t_us);
                    }
                    ("EV_KEY", "KEY_WAKEUP" | "KEY_POWER") if value == "DOWN" => {
                        f.wake_key = true;
                        f.first_us.get_or_insert(t_us);
                    }
                    ("EV_ABS", "ABS_MT_TRACKING_ID") if !value.eq_ignore_ascii_case("ffffffff") => {
                        f.new_tracking = true;
                        f.first_us.get_or_insert(t_us);
                    }
                    ("EV_ABS", "ABS_MT_DISTANCE" | "ABS_DISTANCE")
                        if u32::from_str_radix(&value, 16).is_ok_and(|v| v > 0) =>
                    {
                        f.hovering = true;
                    }
                    _ => {}
                }
                None
            }
        }
    }

    /// Feed many lines and collect the touches.
    pub fn feed_all(&mut self, text: &str) -> Vec<HumanTouch> {
        text.lines().filter_map(|l| self.feed(l)).collect()
    }

    /// The set of node names currently known.
    pub fn nodes(&self) -> HashSet<String> {
        self.devs.keys().cloned().collect()
    }
}

/// How far a new `hub_ms - phone_ms` sample may sit from the stored offset before it is treated as a
/// break in the phone's clock (it suspended, it rebooted) or a bad line, not as transport jitter.
pub const CLOCK_BREAK_MS: i64 = 2_000;

/// Maps phone kernel timestamps onto the hub's millisecond clock. The offset is the smallest
/// `hub_ms - phone_ms` seen (the least-delayed line), so transport delay never moves events later.
///
/// The offset only ever drifts by jitter. A sample more than [`CLOCK_BREAK_MS`] from it, either way,
/// is not believed on its own: the kernel clock stops while the phone suspends, a reboot restarts it,
/// and one garbled line can read anything. It becomes a candidate; a second sample that agrees with
/// the candidate (within the same bound) re-anchors the map, and any sample that agrees with the stored
/// offset again throws the candidate away. Until then the old offset stays, and the caller must not
/// trust a mapped time that falls outside what the report's own timing allows (see `Daemon::phone_touch`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ClockMap {
    offset: Option<i64>,
    candidate: Option<i64>,
    breaks: u32,
}

impl ClockMap {
    pub fn observe(&mut self, phone_us: u64, hub_ms: u64) {
        let o = hub_ms as i64 - (phone_us / 1000) as i64;
        let Some(x) = self.offset else {
            self.offset = Some(o);
            return;
        };
        if (o - x).abs() <= CLOCK_BREAK_MS {
            self.offset = Some(x.min(o));
            self.candidate = None;
        } else if self
            .candidate
            .is_some_and(|c| (o - c).abs() <= CLOCK_BREAK_MS)
        {
            // two samples agree on a new offset: the phone's clock really moved
            self.offset = Some(self.candidate.unwrap_or(o).min(o));
            self.candidate = None;
            self.breaks += 1;
        } else {
            self.candidate = Some(o);
        }
    }

    /// True when a sample at (`phone_us`, `hub_ms`) sits within `tol_ms` of the stored offset, or the
    /// map has none yet.
    pub fn plausible(&self, phone_us: u64, hub_ms: u64, tol_ms: i64) -> bool {
        let o = hub_ms as i64 - (phone_us / 1000) as i64;
        self.offset.is_none_or(|x| (o - x).abs() <= tol_ms)
    }

    /// How many times the map re-anchored after a clock break.
    pub fn breaks(&self) -> u32 {
        self.breaks
    }

    /// The phone time on the hub clock (or the phone time itself before any observation).
    pub fn to_hub_ms(&self, phone_us: u64) -> u64 {
        ((phone_us / 1000) as i64 + self.offset.unwrap_or(0)).max(0) as u64
    }

    /// True once a pair (phone time, hub time) has been seen.
    pub fn calibrated(&self) -> bool {
        self.offset.is_some()
    }

    /// The stored `hub_ms - phone_ms` offset (the least-delayed one), if any.
    pub fn offset_ms(&self) -> Option<i64> {
        self.offset
    }

    /// Like [`ClockMap::to_hub_ms`], but `None` before any observation: an uncalibrated map must
    /// never pass a phone timestamp off as hub time (the two clocks start at unrelated zeros).
    pub fn try_to_hub_ms(&self, phone_us: u64) -> Option<u64> {
        self.offset
            .map(|o| ((phone_us / 1000) as i64 + o).max(0) as u64)
    }
}

/// Which phone clock a timestamp is on. The two are different clocks, so each gets its own
/// [`ClockMap`]: `getevent -t` prints the kernel's input-event time (`CLOCK_MONOTONIC`, which stops
/// while the phone sleeps); the overlay and `HumanTouchFlag` use `SystemClock.elapsedRealtime`
/// (`CLOCK_BOOTTIME`, which does not).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PhoneClock {
    Kernel,
    Elapsed,
}

/// One [`ClockMap`] per phone clock for one phone.
#[derive(Debug, Clone, Copy, Default)]
pub struct PhoneClocks {
    kernel: ClockMap,
    elapsed: ClockMap,
}

impl PhoneClocks {
    pub fn map(&self, c: PhoneClock) -> &ClockMap {
        match c {
            PhoneClock::Kernel => &self.kernel,
            PhoneClock::Elapsed => &self.elapsed,
        }
    }

    pub fn observe(&mut self, c: PhoneClock, phone_us: u64, hub_ms: u64) {
        match c {
            PhoneClock::Kernel => self.kernel.observe(phone_us, hub_ms),
            PhoneClock::Elapsed => self.elapsed.observe(phone_us, hub_ms),
        }
    }

    pub fn try_to_hub_ms(&self, c: PhoneClock, phone_us: u64) -> Option<u64> {
        self.map(c).try_to_hub_ms(phone_us)
    }

    pub fn plausible(&self, c: PhoneClock, phone_us: u64, hub_ms: u64, tol_ms: i64) -> bool {
        self.map(c).plausible(phone_us, hub_ms, tol_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_multi_type_block() {
        let devs = parse_devices(
            "add device 1: /dev/input/event5\n  bus:      0000\n  name:     \"ts\"\n  events:\n    KEY (0001): BTN_TOUCH             BTN_TOOL_FINGER\n    ABS (0003): ABS_MT_POSITION_X     : value 0, min 0, max 1219\n                ABS_MT_POSITION_Y     : value 0, min 0, max 2711\n  input props:\n    INPUT_PROP_DIRECT\n",
        );
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0].bus, Some(0));
        assert_eq!(devs[0].keys, vec!["BTN_TOUCH", "BTN_TOOL_FINGER"]);
        assert_eq!(devs[0].abs, vec!["ABS_MT_POSITION_X", "ABS_MT_POSITION_Y"]);
        assert_eq!(classify(&devs[0]), Class::Touchscreen);
    }

    #[test]
    fn parses_event_lines() {
        assert_eq!(
            parse_line(
                "[   12345.000100] /dev/input/event5: EV_KEY       BTN_TOUCH            DOWN"
            ),
            Line::Event {
                t_us: 12_345_000_100,
                node: "/dev/input/event5".into(),
                ty: "EV_KEY".into(),
                code: "BTN_TOUCH".into(),
                value: "DOWN".into()
            }
        );
        assert_eq!(
            parse_line("[   1.5] /dev/input/event5: EV_SYN SYN_REPORT 00000000"),
            Line::Event {
                t_us: 1_500_000,
                node: "/dev/input/event5".into(),
                ty: "EV_SYN".into(),
                code: "SYN_REPORT".into(),
                value: "00000000".into()
            }
        );
        // no node prefix: never attributed
        assert_eq!(parse_line("[   1.5] EV_KEY BTN_TOUCH DOWN"), Line::Other);
        assert_eq!(
            parse_line("add device 9: /dev/input/event9"),
            Line::AddDevice {
                node: "/dev/input/event9".into()
            }
        );
    }

    #[test]
    fn clock_map_keeps_the_least_delayed_offset() {
        let mut c = ClockMap::default();
        c.observe(10_000_000, 50_100); // 100 ms late
        c.observe(11_000_000, 51_020); // 20 ms late
        assert_eq!(c.to_hub_ms(12_000_000), 52_020);
    }

    #[test]
    fn an_uncalibrated_map_refuses_to_convert_and_the_two_phone_clocks_do_not_mix() {
        let mut c = ClockMap::default();
        assert!(!c.calibrated());
        assert_eq!(c.try_to_hub_ms(5_000_000), None);
        c.observe(5_000_000, 105_000);
        assert_eq!(
            (c.calibrated(), c.offset_ms(), c.try_to_hub_ms(6_000_000)),
            (true, Some(100_000), Some(106_000))
        );
        let mut p = PhoneClocks::default();
        p.observe(PhoneClock::Kernel, 5_000_000, 105_000);
        assert_eq!(
            p.try_to_hub_ms(PhoneClock::Kernel, 6_000_000),
            Some(106_000)
        );
        assert_eq!(
            p.try_to_hub_ms(PhoneClock::Elapsed, 6_000_000),
            None,
            "the elapsed-realtime clock was never observed"
        );
    }
}
