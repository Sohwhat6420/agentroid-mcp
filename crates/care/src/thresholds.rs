//! The one set of thresholds ("Thresholds, one set everywhere: 42/37 °C with a
//! 60 min rest cap; bench jobs pause at 45 °C"). Every module
//! reads [`THRESHOLDS`]; no other number for these exists in the crate.
//!
//! Temperatures are in tenths of a degree Celsius (deci-°C), as `dumpsys battery` reports them,
//! so the resume comparison is exact integer arithmetic: 369 resumes, 370 does not.

/// The threshold table. One instance: [`THRESHOLDS`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// Pause when the **battery** temperature is at or above this (42.0 °C).
    pub pause_battery_dc: i32,
    /// Bench jobs pause here instead (45.0 °C).
    pub bench_pause_battery_dc: i32,
    /// Resume only when the battery temperature is **strictly below** this (37.0 °C).
    pub resume_below_dc: i32,
    /// A rest longer than this (60 min) pushes and stops the job.
    pub rest_cap_ms: u64,
    /// Poll interval while working. Must keep "pause within 30 s" (see `pause_deadline_ms`).
    pub poll_working_ms: u64,
    /// Poll interval while resting (phone-guard used 60 s).
    pub poll_resting_ms: u64,
    /// The measured check: an injected 43 °C must pause within this.
    pub pause_deadline_ms: u64,
    /// Battery floor, percent, applied only when the phone is not `powered`.
    pub battery_floor_pct: i32,
    /// The charge cap the floor must read correctly through (a capped phone is powered but not "charging").
    pub charge_cap_pct: i32,
    /// Skin temperature at which proportional slowdown starts (39.0 °C) ...
    pub skin_slow_from_dc: i32,
    /// ... and at which it is full (42.0 °C).
    pub skin_slow_full_dc: i32,
    /// Thermal headroom (Android `getThermalHeadroom`, 1.0 = severe throttling) where slowdown starts ...
    pub headroom_slow_from: f32,
    /// ... and where it is full.
    pub headroom_slow_full: f32,
    /// Full slowdown: at most one call per this many ms (1 call a second).
    pub max_pace_ms: u64,
    /// "Never panel-off while working": a leased `screen_off_timeout` may not be shorter than this.
    pub min_screen_timeout_ms: u64,
    /// "Dim to minimum": the brightness the agent lease applies. 0 is refused (panel off on some panels).
    pub dim_minimum: &'static str,
    /// The refresh rate the agent lease applies.
    pub refresh_hz: &'static str,
    /// The screen timeout the agent lease applies while working (30 min).
    pub working_screen_timeout_ms: u64,
}

/// The table.
pub const THRESHOLDS: Thresholds = Thresholds {
    pause_battery_dc: 420,
    bench_pause_battery_dc: 450,
    resume_below_dc: 370,
    rest_cap_ms: 60 * 60 * 1000,
    poll_working_ms: 10_000,
    poll_resting_ms: 60_000,
    pause_deadline_ms: 30_000,
    battery_floor_pct: 12,
    charge_cap_pct: 80,
    skin_slow_from_dc: 390,
    skin_slow_full_dc: 420,
    headroom_slow_from: 0.70,
    headroom_slow_full: 1.00,
    max_pace_ms: 1_000,
    min_screen_timeout_ms: 10 * 60 * 1000,
    dim_minimum: "1",
    refresh_hz: "60",
    working_screen_timeout_ms: 30 * 60 * 1000,
};

// The table's internal rules, checked at compile time: an inconsistent edit does not build.
const _: () = {
    let t = THRESHOLDS;
    assert!(
        t.resume_below_dc < t.pause_battery_dc && t.pause_battery_dc < t.bench_pause_battery_dc
    );
    assert!(
        t.poll_working_ms <= t.pause_deadline_ms,
        "a working poll must fit inside the 30 s pause deadline"
    );
    assert!(t.poll_resting_ms <= t.rest_cap_ms);
    assert!(t.skin_slow_from_dc < t.skin_slow_full_dc);
    assert!(t.headroom_slow_from < t.headroom_slow_full);
    assert!(
        t.max_pace_ms <= 1_000,
        "full slowdown is at most 1 call a second"
    );
    assert!(t.working_screen_timeout_ms >= t.min_screen_timeout_ms);
    assert!(t.battery_floor_pct < t.charge_cap_pct);
};

#[cfg(test)]
mod tests {
    use super::THRESHOLDS as T;

    #[test]
    fn the_table_holds_the_plans_numbers() {
        assert_eq!(
            (
                T.pause_battery_dc,
                T.resume_below_dc,
                T.bench_pause_battery_dc
            ),
            (420, 370, 450)
        );
        assert_eq!(T.rest_cap_ms, 3_600_000);
        assert_eq!(T.battery_floor_pct, 12);
        assert_eq!(T.pause_deadline_ms, 30_000);
        assert_ne!(T.dim_minimum, "0");
        assert_eq!(T.refresh_hz, "60");
    }
}
