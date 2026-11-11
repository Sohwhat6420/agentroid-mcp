//! Time, injected. The guard uses the monotonic clock; lease records carry wall-clock RFC 3339
//! stamps. Tests use [`FakeClock`] and never sleep.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub trait CareClock: Send + Sync {
    /// Monotonic milliseconds.
    fn mono_ms(&self) -> u64;
    /// Unix milliseconds (wall clock).
    fn unix_ms(&self) -> u64;
}

/// The real clock.
#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        SystemClock {
            start: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl CareClock for SystemClock {
    fn mono_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Test clock: only moves when told to.
#[derive(Debug)]
pub struct FakeClock {
    mono: AtomicU64,
    unix: AtomicU64,
}

impl FakeClock {
    /// Starts at mono 0 and the given wall-clock time.
    pub fn new(unix_ms: u64) -> Self {
        FakeClock {
            mono: AtomicU64::new(0),
            unix: AtomicU64::new(unix_ms),
        }
    }
    pub fn advance(&self, ms: u64) {
        self.mono.fetch_add(ms, Ordering::SeqCst);
        self.unix.fetch_add(ms, Ordering::SeqCst);
    }
}

impl CareClock for FakeClock {
    fn mono_ms(&self) -> u64 {
        self.mono.load(Ordering::SeqCst)
    }
    fn unix_ms(&self) -> u64 {
        self.unix.load(Ordering::SeqCst)
    }
}

/// 2026-10-20T00:00:00Z, a convenient test epoch.
pub const TEST_EPOCH_MS: u64 = 1_792_454_400_000;

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix ms to `YYYY-MM-DDTHH:MM:SSZ` (seconds precision, like phone-guard's stamps; the `Z` form
/// matches the `ts` pattern of `schemas/job-view.json`).
pub fn rfc3339_utc(unix_ms: u64) -> String {
    let secs = (unix_ms / 1000) as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

fn num(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// Parse `YYYY-MM-DDTHH:MM:SS(.fff)?(Z|+HH:MM|-HH:MM|+HHMM)` to unix ms. phone-guard's own
/// `%z` stamps (`+0200`, no colon) are accepted too.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() < 20 || !s.is_ascii() {
        return None;
    }
    let (date, rest) = s.split_at(10);
    let b = date.as_bytes();
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, mo, d) = (num(&date[0..4])?, num(&date[5..7])?, num(&date[8..10])?);
    let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix('t'))?;
    if rest.len() < 9 || rest.as_bytes()[2] != b':' || rest.as_bytes()[5] != b':' {
        return None;
    }
    let (h, mi, se) = (num(&rest[0..2])?, num(&rest[3..5])?, num(&rest[6..8])?);
    let mut tail = &rest[8..];
    let mut frac_ms = 0i64;
    if let Some(t) = tail.strip_prefix('.') {
        let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        let ms3: String = format!("{digits:0<3}").chars().take(3).collect();
        frac_ms = ms3.parse().ok()?;
        tail = &t[digits.len()..];
    }
    let offset_min = match tail {
        "Z" | "z" => 0,
        _ => {
            let sign = match tail.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let t = &tail[1..];
            let (oh, om) = match t.len() {
                5 if t.as_bytes()[2] == b':' => (num(&t[0..2])?, num(&t[3..5])?),
                4 => (num(&t[0..2])?, num(&t[2..4])?),
                _ => return None,
            };
            sign * (oh * 60 + om)
        }
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset_min * 60;
    u64::try_from(secs * 1000 + frac_ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_round_trip() {
        assert_eq!(rfc3339_utc(TEST_EPOCH_MS), "2026-10-20T00:00:00Z");
        assert_eq!(parse_rfc3339("2026-10-20T00:00:00Z"), Some(TEST_EPOCH_MS));
        assert_eq!(
            parse_rfc3339("2026-10-20T08:00:00+08:00"),
            Some(TEST_EPOCH_MS)
        );
        assert_eq!(
            parse_rfc3339("2026-10-20T02:00:00+0200"),
            Some(TEST_EPOCH_MS)
        );
        assert_eq!(
            parse_rfc3339("2026-10-20T00:00:00.250Z"),
            Some(TEST_EPOCH_MS + 250)
        );
        for bad in [
            "",
            "2026-10-20",
            "2026-13-20T00:00:00Z",
            "2026-10-20T00:00:00",
            "2026-10-20T00:00:00+8",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
        let c = FakeClock::new(TEST_EPOCH_MS);
        c.advance(61_000);
        assert_eq!(rfc3339_utc(c.unix_ms()), "2026-10-20T00:01:01Z");
        assert_eq!(c.mono_ms(), 61_000);
    }
}
