//! Wiring 2 (25 Oct), item 2 and item 4: the phone's clock and the hub's clock are two clocks.
//! A touch is stamped on the phone's clock, the mode machine's timers (the 2-minute reclaim rule,
//! the "older than the agent's hold" rule) run on the hub's, and `touch::ClockMap` is the only
//! thing between them. Every test here builds a phone clock that is far from the hub's and fails
//! if the conversion is skipped.

mod common;

use common::{machine, ok_planner, KEEP};
use handover::mode::{Action, Mode, PhoneState, Source};
use handover::touch::{parse_line, ClockMap, Detector, Line, PhoneClock, PhoneClocks};

const OFF: PhoneState = PhoneState {
    display0_on: false,
    locked: true,
    media_playing: false,
    in_call: false,
};
const RECLAIM_IDLE_MS: u64 = 120_000;
const RECLAIM_TOLERANCE_MS: u64 = 5_000;

fn map() -> String {
    common::fixture("getevent/synthetic-map.txt")
}

/// The phone's `getevent -lt` clock reading (microseconds) at hub time `hub_ms`, for a phone whose
/// clock runs `skew_ms` ahead of the hub's (negative: behind).
fn phone_us(hub_ms: u64, skew_ms: i64) -> u64 {
    u64::try_from((i128::from(hub_ms) + i128::from(skew_ms)) * 1000)
        .expect("a phone clock cannot be negative")
}

fn stamp(us: u64) -> String {
    format!("[{:>5}.{:06}]", us / 1_000_000, us % 1_000_000)
}

/// A finger tapping the touchscreen (event5) at phone time `us`, then lifting 70 ms later.
fn tap(us: u64) -> String {
    let (d, u) = (stamp(us), stamp(us + 70_000));
    format!(
        "{d} /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   00000041\n{d} /dev/input/event5: EV_KEY       BTN_TOUCH            DOWN\n{d} /dev/input/event5: EV_SYN       SYN_REPORT           00000000\n\
         {u} /dev/input/event5: EV_ABS       ABS_MT_TRACKING_ID   ffffffff\n{u} /dev/input/event5: EV_KEY       BTN_TOUCH            UP\n{u} /dev/input/event5: EV_SYN       SYN_REPORT           00000000\n"
    )
}

/// A lone volume-key event: not a touch, but it carries the phone's clock (a sample for the map).
fn key(us: u64) -> String {
    let s = stamp(us);
    format!("{s} /dev/input/event1: EV_KEY       KEY_VOLUMEUP         DOWN\n{s} /dev/input/event1: EV_SYN       SYN_REPORT           00000000\n")
}

/// What the hub does with a chunk of the live stream that reaches it at hub time `arrive`:
/// samples the offset with the chunk's newest line, then places every touch on the hub clock.
/// With `convert == false` it keeps the phone's timestamp as if it were hub time (the bug).
fn live(
    det: &mut Detector,
    clocks: &mut PhoneClocks,
    chunk: &str,
    arrive: u64,
    convert: bool,
) -> Vec<u64> {
    for l in chunk.lines() {
        if let Line::Event { t_us, .. } = parse_line(l) {
            clocks.observe(PhoneClock::Kernel, t_us, arrive);
        }
    }
    det.feed_all(chunk)
        .iter()
        .map(|t| {
            if convert {
                clocks
                    .try_to_hub_ms(PhoneClock::Kernel, t.at_us)
                    .expect("calibrated by this chunk")
            } else {
                t.at_ms()
            }
        })
        .collect()
}

fn run_reclaim(skew_ms: i64, hub_start: u64, convert: bool) -> (Option<u64>, Option<u64>, Mode) {
    let (mut m, clock, _store) = machine(hub_start, 200, KEEP);
    m.start_job("job-1");
    let mut det = Detector::from_map(&map());
    let mut clocks = PhoneClocks::default();
    // the phone has been streaming for a while: harmless lines calibrate the map
    for k in 0..5u64 {
        let hub = hub_start + 1_000 * k;
        clock.set(hub + 30);
        live(
            &mut det,
            &mut clocks,
            &key(phone_us(hub, skew_ms)),
            hub + 30,
            convert,
        );
    }
    // a person taps at hub time T; the lines reach the hub 40 ms later
    let t = hub_start + 8_000;
    clock.set(t + 40);
    let at = live(
        &mut det,
        &mut clocks,
        &tap(phone_us(t, skew_ms)),
        t + 40,
        convert,
    );
    assert_eq!(at.len(), 1, "one touch-down");
    let acts = m.human_touch(at[0], Source::Getevent, &mut ok_planner());
    if !acts
        .iter()
        .any(|a| matches!(a, Action::StopInjection { .. }))
    {
        return (None, None, m.mode());
    }
    // the screen is off, nobody touches: the 10 s warning must come ~2 min after the touch
    let mut warn = None;
    let mut reclaimed = None;
    let mut now = t + 40;
    while now < t + 300_000 && reclaimed.is_none() {
        now += 250;
        clock.set(now);
        for a in m.tick(OFF) {
            match a {
                Action::ShowDot { .. } if warn.is_none() => warn = Some(now),
                Action::Reclaimed { at } => reclaimed = Some(at),
                _ => {}
            }
        }
    }
    (warn.map(|w| w - t), reclaimed.map(|r| r - t), m.mode())
}

#[test]
fn a_phone_clock_hours_ahead_still_gets_its_two_minute_reclaim_and_ten_second_warning() {
    // the phone has been up 3 h longer than the laptop's daemon
    let (warn, reclaimed, mode) = run_reclaim(3 * 3_600_000, 1_000_000, true);
    let warn = warn.expect("the warning was shown");
    assert!(
        (RECLAIM_IDLE_MS - RECLAIM_TOLERANCE_MS..=RECLAIM_IDLE_MS + RECLAIM_TOLERANCE_MS)
            .contains(&warn),
        "the warning came {warn} ms after the touch, not after about 2 minutes"
    );
    let reclaimed = reclaimed.expect("reclaimed");
    assert!(
        (9_000..=11_000).contains(&(reclaimed - warn)),
        "reclaim {} ms after the warning",
        reclaimed - warn
    );
    assert_eq!(mode, Mode::Agent);
}

#[test]
fn a_phone_clock_behind_the_hub_still_takes_over_on_a_touch() {
    // the phone rebooted 30 minutes after the laptop: its clock reads far less than the hub's
    let (warn, _, _) = run_reclaim(-1_800_000, 3_600_000, true);
    assert!(
        warn.is_some(),
        "the touch was seen and the machine ran its reclaim rule"
    );
}

#[test]
fn the_unconverted_phone_stamp_would_break_both_clocks() {
    // The same two runs with the conversion skipped: this is what the converted runs above guard
    // against. A clock ahead makes the last touch lie in the hub's future, so the idle time never
    // reaches 2 minutes and the reclaim never comes; a clock behind makes the touch older than
    // the agent's own hold, so the person's touch is ignored.
    let (warn, reclaimed, _) = run_reclaim(3 * 3_600_000, 1_000_000, false);
    assert_eq!(
        (warn, reclaimed),
        (None, None),
        "ahead: the 2-minute rule never fires"
    );
    let (_, _, mode) = run_reclaim(-1_800_000, 3_600_000, false);
    assert_eq!(
        mode,
        Mode::Agent,
        "behind: the touch was dropped as older than the agent's hold"
    );
}

#[test]
fn the_offset_is_the_least_delayed_one_and_a_slow_line_never_moves_a_touch_later() {
    let skew = 50_000_000i64;
    let mut c = ClockMap::default();
    // three lines of the same stream reach the hub 400, 25 and 180 ms late
    for (hub, delay) in [(10_000u64, 400u64), (11_000, 25), (12_000, 180)] {
        c.observe(phone_us(hub, skew), hub + delay);
    }
    // a touch at hub time 11_500 lands within the smallest delay (25 ms), never at +400
    let at = c.to_hub_ms(phone_us(11_500, skew));
    assert!((11_500..=11_525).contains(&at), "{at}");
    assert!(c.calibrated());
}

// ------------------------------------------------------------------ clock breaks (review fixes)

/// A calibrated map for a phone whose clock runs `skew_ms` ahead, sampled every second for 5 s.
fn calibrated(skew_ms: i64) -> ClockMap {
    let mut c = ClockMap::default();
    for k in 0..5u64 {
        c.observe(
            phone_us(1_000_000 + 1_000 * k, skew_ms),
            1_000_000 + 1_000 * k + 30,
        );
    }
    c
}

#[test]
fn a_suspend_stops_the_kernel_clock_and_two_agreeing_samples_re_anchor_the_map() {
    let mut c = calibrated(0);
    // the phone slept for 10 minutes: its clock stopped, the hub's did not
    let hub = 1_000_000 + 4_000 + 600_000;
    let phone_ms = 1_000_000 + 4_000 + 500; // half a second of awake time since
    c.observe(phone_ms * 1000, hub + 30);
    assert_eq!(c.breaks(), 0, "one odd sample is only a candidate");
    assert!(
        c.to_hub_ms(phone_ms * 1000) < hub - 500_000,
        "the old offset still stands until a second sample agrees"
    );
    c.observe((phone_ms + 1_000) * 1000, hub + 1_030);
    assert_eq!(c.breaks(), 1);
    let m = c.to_hub_ms((phone_ms + 1_000) * 1000);
    assert!(
        (hub + 1_000..=hub + 1_030).contains(&m),
        "re-anchored: {m} vs {}",
        hub + 1_000
    );
}

#[test]
fn a_reboot_restarts_the_clock_near_zero_and_the_map_follows() {
    let mut c = calibrated(3 * 3_600_000);
    let hub = 1_100_000u64;
    c.observe(5_000_000, hub);
    c.observe(6_000_000, hub + 1_000);
    assert_eq!(c.breaks(), 1);
    assert_eq!(c.to_hub_ms(7_000_000), hub + 2_000);
}

#[test]
fn one_garbled_line_is_forgotten_and_the_next_good_sample_clears_it() {
    let mut c = calibrated(0);
    let before = c.offset_ms();
    c.observe(phone_us(1_005_000, 5 * 3_600_000), 1_005_030); // five hours ahead
    assert_eq!(c.offset_ms(), before, "not believed alone");
    c.observe(phone_us(1_006_000, 0), 1_006_030);
    assert_eq!(
        c.offset_ms(),
        before,
        "a good sample brings the candidate to nothing"
    );
    assert_eq!(c.breaks(), 0);
    // two garbled lines that disagree with each other never re-anchor either
    c.observe(phone_us(1_007_000, 5 * 3_600_000), 1_007_030);
    c.observe(phone_us(1_008_000, 9 * 3_600_000), 1_008_030);
    assert_eq!(c.offset_ms(), before);
    assert!(c.plausible(phone_us(1_009_000, 0), 1_009_030, 2_000));
    assert!(!c.plausible(phone_us(1_009_000, 5 * 3_600_000), 1_009_030, 15_000));
}
