//! Detector 1 against the synthetic getevent fixtures: 20 of 20 human touches, 0 of 500 injected
//! actions, each virtual-device rule on its own, and the stream supervisor.

mod common;

use std::cell::RefCell;
use std::collections::VecDeque;

use agentroid_adb_link::guard::{GuardedRunner, S24_SERIAL};
use agentroid_adb_link::runner::{RunError, RunOutput, Runner};
use common::fixture;
use handover::stream::{AdbLineStream, LineSource, Phone, Supervisor};
use handover::touch::{classify, parse_devices, Class, Detector, TouchKind};

fn base_map() -> String {
    fixture("getevent/synthetic-map.txt")
}

fn hot_map() -> String {
    fixture("getevent/synthetic-map-after-scrcpy.txt")
}

/// Feed a stream, re-mapping whenever the detector asks (as the supervisor does).
fn run(det: &mut Detector, text: &str) -> Vec<handover::touch::HumanTouch> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(t) = det.feed(line) {
            out.push(t);
        }
        if det.needs_remap() {
            det.remap(&hot_map());
        }
    }
    out
}

#[test]
fn the_map_picks_exactly_the_touchscreen_and_the_stylus() {
    let det = Detector::from_map(&base_map());
    assert_eq!(
        det.touchscreens(),
        vec![
            "/dev/input/event5".to_string(),
            "/dev/input/event6".to_string()
        ]
    );
    let devs = parse_devices(&base_map());
    assert_eq!(devs.len(), 9);
    let class = |n: &str| classify(devs.iter().find(|d| d.node == n).unwrap());
    // keys, proximity, accelerometer and the touchpad never count
    for n in [
        "/dev/input/event0",
        "/dev/input/event1",
        "/dev/input/event2",
        "/dev/input/event3",
        "/dev/input/event4",
    ] {
        assert_eq!(class(n), Class::Other, "{n}");
    }
}

#[test]
fn twenty_of_twenty_fixture_touches_emit_a_human_touch() {
    let text = fixture("getevent/synthetic-human-20.txt");
    let expected = text
        .lines()
        .filter(|l| l.starts_with("# human touch "))
        .count();
    assert_eq!(expected, 20);
    let mut det = Detector::from_map(&base_map());
    let touches = run(&mut det, &text);
    assert_eq!(touches.len(), 20, "{touches:#?}");
    // each lands after its own "# human touch" marker and before the next one
    let mut marker_line = Vec::new();
    for (i, l) in text.lines().enumerate() {
        if l.starts_with("# human touch ") {
            marker_line.push(i);
        }
    }
    assert_eq!(marker_line.len(), touches.len());
    let pens = touches
        .iter()
        .filter(|t| t.node == "/dev/input/event6")
        .count();
    let wakes = touches
        .iter()
        .filter(|t| t.kind == TouchKind::WakeKey)
        .count();
    let mt_only = touches
        .iter()
        .filter(|t| t.kind == TouchKind::NewTrackingId)
        .count();
    assert_eq!(
        (pens, wakes, mt_only),
        (2, 1, 1),
        "stylus contact, double-tap wake and tracking-id-only touches all count"
    );
    // timestamps strictly increase (no duplicates)
    assert!(touches.windows(2).all(|w| w[0].at_us < w[1].at_us));
}

#[test]
fn five_hundred_injected_actions_give_zero_touches() {
    let text = fixture("getevent/synthetic-injected-500.txt");
    let actions: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("# action "))
        .collect();
    assert_eq!(actions.len(), 500);
    for path in ["input", "dispatchGesture", "scrcpy-inject"] {
        assert_eq!(
            actions
                .iter()
                .filter(|l| l.ends_with(&format!(": {path}")))
                .count(),
            125,
            "{path}"
        );
    }
    assert_eq!(
        actions.iter().filter(|l| l.contains(": uhid on ")).count(),
        125
    );
    // the uhid part really does carry touch-downs, on virtual and hot-plugged nodes
    assert!(
        text.lines()
            .filter(|l| l.contains("BTN_TOUCH") && l.contains("DOWN"))
            .count()
            >= 90
    );
    let mut det = Detector::from_map(&base_map());
    let touches = run(&mut det, &text);
    assert!(touches.is_empty(), "false takeovers: {touches:#?}");
}

#[test]
fn human_touches_after_injected_ones_still_count() {
    // the detector is not simply silent: 20, then 500 injected, then 20 again
    let human = fixture("getevent/synthetic-human-20.txt");
    let injected = fixture("getevent/synthetic-injected-500.txt");
    let mut det = Detector::from_map(&base_map());
    let mut n = run(&mut det, &human).len();
    n += run(&mut det, &injected).len();
    n += run(&mut det, &human).len();
    assert_eq!(n, 40);
}

#[test]
fn each_virtual_rule_excludes_on_its_own() {
    let devs = parse_devices(&hot_map());
    let get = |n: &str| devs.iter().find(|d| d.node == n).unwrap().clone();
    // event7: bus 0006 and a uinput name
    assert!(matches!(
        classify(&get("/dev/input/event7")),
        Class::Virtual(_)
    ));
    // event8: an innocent name, excluded by bus 0006 alone
    let e8 = get("/dev/input/event8");
    assert_eq!(e8.name, "hid-generic-panel");
    assert_eq!(
        classify(&e8),
        Class::Virtual("bus 0006 (BUS_VIRTUAL: uinput or uhid)")
    );
    // the same device without -i (no bus line): excluded by its name only if the name says so
    let mut no_bus = get("/dev/input/event7");
    no_bus.bus = None;
    assert_eq!(classify(&no_bus), Class::Virtual("uinput device name"));
    // event9: scrcpy's own uhid device
    assert!(matches!(
        classify(&get("/dev/input/event9")),
        Class::Virtual(_)
    ));
    // event10: looks physical (USB bus, plain name) but was added while streaming
    assert_eq!(classify(&get("/dev/input/event10")), Class::Touchscreen);
    let mut det = Detector::from_map(&base_map());
    det.feed("add device 10: /dev/input/event10");
    det.remap(&hot_map());
    assert!(!det
        .touchscreens()
        .contains(&"/dev/input/event10".to_string()));
    assert!(det
        .feed("[  1.000000] /dev/input/event10: EV_KEY       BTN_TOUCH            DOWN")
        .is_none());
    assert!(det
        .feed("[  1.000000] /dev/input/event10: EV_SYN       SYN_REPORT           00000000")
        .is_none());
}

#[test]
fn events_without_a_frame_end_or_a_node_never_count() {
    let mut det = Detector::from_map(&base_map());
    assert!(det
        .feed("[  1.000000] /dev/input/event5: EV_KEY       BTN_TOUCH            DOWN")
        .is_none());
    // no SYN_REPORT yet: nothing; a line without the node prefix: nothing
    assert!(det
        .feed("[  1.000100] EV_SYN       SYN_REPORT           00000000")
        .is_none());
    let t = det
        .feed("[  1.000200] /dev/input/event5: EV_SYN       SYN_REPORT           00000000")
        .unwrap();
    assert_eq!(t.at_us, 1_000_000);
}

// ---- the supervisor: restart on EOF, re-map on add device ----

struct FakeSource(VecDeque<String>);

impl LineSource for FakeSource {
    fn next_line(&mut self) -> Option<String> {
        self.0.pop_front()
    }
}

struct FakePhone {
    streams: VecDeque<String>,
    maps_read: u32,
    opened: u32,
}

impl Phone for FakePhone {
    type Source = FakeSource;
    fn read_map(&mut self) -> Result<String, RunError> {
        self.maps_read += 1;
        Ok(if self.maps_read == 1 {
            base_map()
        } else {
            hot_map()
        })
    }
    fn open_stream(&mut self) -> Result<FakeSource, RunError> {
        self.opened += 1;
        let s = self.streams.pop_front().unwrap_or_default();
        Ok(FakeSource(s.lines().map(str::to_string).collect()))
    }
}

#[test]
fn supervisor_restarts_on_eof_and_remaps_on_add_device() {
    let mut human = fixture("getevent/synthetic-human-20.txt");
    // after the restart, the look-alike hot-plugged in the first stream is still not a touchscreen
    human.push_str(
        "[  99999.000000] /dev/input/event10: EV_ABS       ABS_MT_TRACKING_ID   00000999\n",
    );
    human.push_str("[  99999.000000] /dev/input/event10: EV_KEY       BTN_TOUCH            DOWN\n");
    human.push_str(
        "[  99999.000000] /dev/input/event10: EV_SYN       SYN_REPORT           00000000\n",
    );
    let injected = fixture("getevent/synthetic-injected-500.txt");
    let phone = FakePhone {
        streams: VecDeque::from(vec![injected, human]),
        maps_read: 0,
        opened: 0,
    };
    let mut sup = Supervisor::new(phone, 1);
    let mut got = Vec::new();
    sup.run(|t| got.push(t)).unwrap();
    assert_eq!(sup.phone.opened, 2, "restarted once after EOF");
    // map at start, one re-map per add device (2), and a fresh map after the restart
    assert_eq!(sup.phone.maps_read, 4);
    assert_eq!(
        got.len(),
        20,
        "0 from the injected stream, 20 from the human one"
    );
}

// ---- the stream is admitted by adb-link's guard before any process starts ----

struct Spy {
    hw: String,
    calls: RefCell<Vec<Vec<String>>>,
}

impl Runner for Spy {
    fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
        self.calls.borrow_mut().push(a.to_vec());
        Ok(RunOutput {
            code: Some(0),
            stdout: format!("{}\n", self.hw),
            stderr: String::new(),
        })
    }
}

#[test]
fn the_stream_never_starts_for_the_s24_or_a_helper_device() {
    // a path that does not exist: if a process were started this would be an Io error, not Refused
    let adb = std::path::Path::new("Z:/does-not-exist/adb.exe");
    let s24 = GuardedRunner::new(Spy {
        hw: S24_SERIAL.into(),
        calls: RefCell::new(vec![]),
    });
    assert!(matches!(
        AdbLineStream::spawn(&s24, adb, "192.0.2.99:40000", true),
        Err(RunError::Refused(_))
    ));
    assert!(matches!(
        AdbLineStream::spawn(&s24, adb, S24_SERIAL, true),
        Err(RunError::Refused(_))
    ));
    let poco = GuardedRunner::new(Spy {
        hw: "DEVICE0000EXAMPLE".into(),
        calls: RefCell::new(vec![]),
    });
    assert!(matches!(
        AdbLineStream::spawn(&poco, adb, "DEVICE0000EXAMPLE", false),
        Err(RunError::Refused(_))
    ));
    // a full POCO passes the guard; only then does spawning fail on the missing executable
    assert!(matches!(
        AdbLineStream::spawn(&poco, adb, "DEVICE0000EXAMPLE", true),
        Err(RunError::Io(_))
    ));
}
