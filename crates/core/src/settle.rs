//! The settle contract (task 2.2, item 3).
//!
//! "200 ms polls, 2 equal signatures, 3 s cap, then `unsettled`". The tree is read
//! every 200 ms; the screen is *settled* as soon as two consecutive reads have the same signature;
//! if 3 s pass without that, the outcome is [`Settle::Unsettled`] (the last tree is still handed
//! back so a read can be served, flagged). A read that fails resets the run of equal signatures.
//! The clock and the tree source are injected, so tests run in zero real time.

use crate::tree::{parse, ParseError};
use agentroid_adb_link::runner::Clock;

/// Where trees come from: uiautomator, the helper, or a fixture.
pub trait TreeSource {
    /// One raw dump (the XML text), or why it could not be read.
    fn dump(&mut self) -> Result<String, String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleOpts {
    pub poll_ms: u64,
    /// Consecutive equal signatures needed.
    pub equal: u32,
    pub cap_ms: u64,
}

impl Default for SettleOpts {
    fn default() -> Self {
        SettleOpts {
            poll_ms: 200,
            equal: 2,
            cap_ms: 3000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settle {
    Settled {
        signature: u64,
        polls: u32,
        elapsed_ms: u64,
        xml: String,
    },
    Unsettled {
        polls: u32,
        elapsed_ms: u64,
        last_xml: Option<String>,
        last_error: Option<String>,
    },
}

impl Settle {
    pub fn is_settled(&self) -> bool {
        matches!(self, Settle::Settled { .. })
    }
    /// `settled` or `unsettled`, the word that goes in the reply.
    pub fn as_str(&self) -> &'static str {
        if self.is_settled() {
            "settled"
        } else {
            "unsettled"
        }
    }
    pub fn polls(&self) -> u32 {
        match self {
            Settle::Settled { polls, .. } | Settle::Unsettled { polls, .. } => *polls,
        }
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= u64::from(x);
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
        self.0 ^= 0xff; // field separator
        self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
    }
}

/// A hash of everything a person would see change: class, labels, ids, bounds and state flags of
/// every kept element. Not the order of attributes, not whitespace between tags.
pub fn signature(xml: &str) -> Result<u64, ParseError> {
    let tree = parse(xml, false)?;
    let mut h = Fnv::new();
    for e in &tree.elements {
        h.bytes(e.class.as_bytes());
        h.bytes(e.text.as_bytes());
        h.bytes(e.desc.as_bytes());
        h.bytes(e.rid.as_bytes());
        h.bytes(
            format!(
                "{},{},{},{}",
                e.bounds.l, e.bounds.t, e.bounds.r, e.bounds.b
            )
            .as_bytes(),
        );
        h.bytes(e.flags.names().join(",").as_bytes());
    }
    Ok(h.0)
}

/// Poll until the screen is settled or the cap passes.
pub fn settle(source: &mut dyn TreeSource, clock: &dyn Clock, opts: SettleOpts) -> Settle {
    let start = clock.now_ms();
    let mut polls = 0u32;
    let mut prev: Option<u64> = None;
    let mut run = 0u32;
    let mut last_xml: Option<String> = None;
    let mut last_error: Option<String>;
    loop {
        polls += 1;
        match source
            .dump()
            .map_err(|e| e.to_string())
            .and_then(|x| signature(&x).map(|s| (x, s)).map_err(|e| e.to_string()))
        {
            Ok((xml, sig)) => {
                run = if prev == Some(sig) { run + 1 } else { 1 };
                prev = Some(sig);
                last_xml = Some(xml);
                last_error = None;
                if run >= opts.equal.max(1) {
                    let elapsed_ms = clock.now_ms().saturating_sub(start);
                    return Settle::Settled {
                        signature: sig,
                        polls,
                        elapsed_ms,
                        xml: last_xml.unwrap_or_default(),
                    };
                }
            }
            Err(e) => {
                prev = None;
                run = 0;
                last_error = Some(e);
            }
        }
        let elapsed = clock.now_ms().saturating_sub(start);
        if elapsed >= opts.cap_ms {
            return Settle::Unsettled {
                polls,
                elapsed_ms: elapsed,
                last_xml,
                last_error,
            };
        }
        clock.sleep_ms(opts.poll_ms.min(opts.cap_ms - elapsed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{screen, FakeClock, ScriptedSource};

    #[test]
    fn two_equal_signatures_settle_on_the_second_poll() {
        let clock = FakeClock::new();
        let mut src = ScriptedSource::new(&clock, vec![Ok(screen("A")), Ok(screen("A"))], 0);
        let r = settle(&mut src, &clock, SettleOpts::default());
        assert!(r.is_settled());
        assert_eq!(r.polls(), 2);
        match r {
            Settle::Settled { elapsed_ms, .. } => assert_eq!(elapsed_ms, 200),
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_change_restarts_the_run() {
        let clock = FakeClock::new();
        let mut src = ScriptedSource::new(
            &clock,
            vec![
                Ok(screen("A")),
                Ok(screen("B")),
                Ok(screen("C")),
                Ok(screen("C")),
            ],
            0,
        );
        let r = settle(&mut src, &clock, SettleOpts::default());
        assert!(r.is_settled());
        assert_eq!(r.polls(), 4);
        assert_eq!(clock.now_ms(), 600); // 3 sleeps of 200 ms
    }

    #[test]
    fn looping_fixture_times_out_unsettled_at_the_cap() {
        let clock = FakeClock::new();
        let mut src = ScriptedSource::cycling(&clock, vec![Ok(screen("A")), Ok(screen("B"))], 0);
        let r = settle(&mut src, &clock, SettleOpts::default());
        assert_eq!(r.as_str(), "unsettled");
        match r {
            Settle::Unsettled {
                polls,
                elapsed_ms,
                last_xml,
                ..
            } => {
                assert_eq!(elapsed_ms, 3000);
                assert_eq!(polls, 16); // t = 0, 200, ..., 3000
                assert!(last_xml.is_some());
            }
            _ => unreachable!(),
        }
        assert_eq!(clock.now_ms(), 3000);
    }

    #[test]
    fn slow_dumps_still_respect_the_cap() {
        let clock = FakeClock::new();
        // each dump takes 700 ms of fake time and the screen never repeats
        let mut src = ScriptedSource::cycling(
            &clock,
            vec![Ok(screen("A")), Ok(screen("B")), Ok(screen("C"))],
            700,
        );
        let r = settle(&mut src, &clock, SettleOpts::default());
        match r {
            Settle::Unsettled {
                elapsed_ms, polls, ..
            } => {
                assert!((3000..3000 + 700).contains(&elapsed_ms), "{elapsed_ms}");
                assert!(polls <= 5);
            }
            _ => panic!("should not settle"),
        }
    }

    #[test]
    fn a_failed_read_breaks_the_run() {
        let clock = FakeClock::new();
        let mut src = ScriptedSource::new(
            &clock,
            vec![
                Ok(screen("A")),
                Err("null root node".into()),
                Ok(screen("A")),
                Ok(screen("A")),
            ],
            0,
        );
        let r = settle(&mut src, &clock, SettleOpts::default());
        assert!(r.is_settled());
        assert_eq!(r.polls(), 4);
        // and when every read fails the result says why
        let mut bad = ScriptedSource::cycling(&clock, vec![Err("screen asleep".into())], 0);
        match settle(&mut bad, &clock, SettleOpts::default()) {
            Settle::Unsettled {
                last_error,
                last_xml,
                ..
            } => {
                assert_eq!(last_error.as_deref(), Some("screen asleep"));
                assert!(last_xml.is_none());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn signature_ignores_formatting_but_not_content() {
        let a = signature(&screen("A")).unwrap();
        assert_eq!(a, signature(&screen("A")).unwrap());
        assert_ne!(a, signature(&screen("B")).unwrap());
        let spaced = screen("A").replace("/>", " />");
        assert_eq!(a, signature(&spaced).unwrap());
        assert!(signature("nope").is_err());
    }

    #[test]
    fn options_are_the_planned_defaults() {
        let o = SettleOpts::default();
        assert_eq!((o.poll_ms, o.equal, o.cap_ms), (200, 2, 3000));
    }
}
