//! `require` on reads (task 2.2, item 2): a read can name selectors that must be on screen. It polls
//! until every one resolves; on timeout it returns the selectors that were still missing, from the
//! last tree that could be read, so the caller knows what the screen lacked rather than just "timeout".

use crate::selector::{resolve, Resolution, ResolveError, Selector};
use crate::settle::TreeSource;
use crate::tree::parse;
use agentroid_adb_link::runner::Clock;

/// Calls must stay under 110 s (Claude Code backgrounds MCP calls at 2 min).
pub const MAX_TIMEOUT_MS: u64 = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequireOpts {
    pub timeout_ms: u64,
    pub poll_ms: u64,
}

impl Default for RequireOpts {
    fn default() -> Self {
        RequireOpts {
            timeout_ms: 5000,
            poll_ms: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Required {
    /// Every selector resolved; one resolution per selector, in the order given.
    Met {
        xml: String,
        resolutions: Vec<Resolution>,
        polls: u32,
        elapsed_ms: u64,
    },
    /// Timed out. `missing` are the selectors that did not resolve in the last readable tree (all of
    /// them if no tree could be read at all).
    Timeout {
        missing: Vec<Selector>,
        last_xml: Option<String>,
        last_error: Option<String>,
        polls: u32,
        elapsed_ms: u64,
    },
}

impl Required {
    pub fn is_met(&self) -> bool {
        matches!(self, Required::Met { .. })
    }
    /// A line for the reply.
    pub fn describe(&self) -> String {
        match self {
            Required::Met {
                polls, elapsed_ms, ..
            } => format!("require met after {polls} read(s), {elapsed_ms} ms"),
            Required::Timeout {
                missing,
                polls,
                elapsed_ms,
                last_error,
                ..
            } => {
                let m: Vec<String> = missing.iter().map(|s| s.to_string()).collect();
                let why = last_error
                    .as_ref()
                    .map(|e| format!(" (last read error: {e})"))
                    .unwrap_or_default();
                format!(
                    "require TIMED OUT after {polls} read(s), {elapsed_ms} ms; missing: {}{why}",
                    m.join("; ")
                )
            }
        }
    }
}

/// Read until every selector in `requires` resolves, or `opts.timeout_ms` passes.
pub fn read_require(
    source: &mut dyn TreeSource,
    clock: &dyn Clock,
    requires: &[Selector],
    opts: RequireOpts,
) -> Required {
    let timeout = opts.timeout_ms.min(MAX_TIMEOUT_MS);
    let start = clock.now_ms();
    let mut polls = 0u32;
    let mut missing: Vec<Selector> = requires.to_vec();
    let mut last_xml: Option<String> = None;
    let mut last_error: Option<String>;
    loop {
        polls += 1;
        match source.dump() {
            Ok(xml) => match parse(&xml, false) {
                Ok(tree) => {
                    let mut found: Vec<Resolution> = Vec::new();
                    let mut miss: Vec<Selector> = Vec::new();
                    for sel in requires {
                        match resolve(&tree, sel) {
                            Ok(r) => found.push(r),
                            Err(
                                ResolveError::NoMatch(_)
                                | ResolveError::Empty
                                | ResolveError::BadRel(..),
                            ) => miss.push(sel.clone()),
                        }
                    }
                    if miss.is_empty() {
                        return Required::Met {
                            xml,
                            resolutions: found,
                            polls,
                            elapsed_ms: clock.now_ms().saturating_sub(start),
                        };
                    }
                    missing = miss;
                    last_error = None;
                    last_xml = Some(xml);
                }
                Err(e) => last_error = Some(e.to_string()),
            },
            Err(e) => last_error = Some(e),
        }
        let elapsed = clock.now_ms().saturating_sub(start);
        if elapsed >= timeout {
            return Required::Timeout {
                missing,
                last_xml,
                last_error,
                polls,
                elapsed_ms: elapsed,
            };
        }
        clock.sleep_ms(opts.poll_ms.min(timeout - elapsed).max(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{screen, FakeClock, ScriptedSource};

    fn two_buttons(a: bool, b: bool) -> String {
        let mut s = String::from("<hierarchy rotation=\"0\">");
        if a {
            s += "<node class=\"android.widget.Button\" text=\"Save\" resource-id=\"p:id/save\" clickable=\"true\" bounds=\"[0,0][100,100]\"/>";
        }
        if b {
            s += "<node class=\"android.widget.Button\" text=\"Cancel\" resource-id=\"p:id/cancel\" clickable=\"true\" bounds=\"[0,200][100,300]\"/>";
        }
        s + "</hierarchy>"
    }

    #[test]
    fn met_immediately() {
        let clock = FakeClock::new();
        let mut src = ScriptedSource::new(&clock, vec![Ok(two_buttons(true, true))], 0);
        let r = read_require(
            &mut src,
            &clock,
            &[Selector::id("save"), Selector::text("Cancel")],
            RequireOpts::default(),
        );
        match r {
            Required::Met {
                resolutions,
                polls,
                elapsed_ms,
                ..
            } => {
                assert_eq!((resolutions.len(), polls, elapsed_ms), (2, 1, 0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn met_after_the_screen_changes() {
        let clock = FakeClock::new();
        let mut src = ScriptedSource::new(
            &clock,
            vec![
                Ok(two_buttons(true, false)),
                Ok(two_buttons(true, false)),
                Ok(two_buttons(true, true)),
            ],
            0,
        );
        let r = read_require(
            &mut src,
            &clock,
            &[Selector::id("save"), Selector::id("cancel")],
            RequireOpts::default(),
        );
        assert!(r.is_met());
        assert_eq!(clock.now_ms(), 400);
    }

    #[test]
    fn timeout_returns_exactly_the_missing_selectors() {
        let clock = FakeClock::new();
        let mut src = ScriptedSource::cycling(&clock, vec![Ok(two_buttons(true, false))], 0);
        let want = [
            Selector::id("save"),
            Selector::text("Cancel"),
            Selector::desc("Overflow menu"),
        ];
        let r = read_require(
            &mut src,
            &clock,
            &want,
            RequireOpts {
                timeout_ms: 1000,
                poll_ms: 200,
            },
        );
        match &r {
            Required::Timeout {
                missing,
                polls,
                elapsed_ms,
                last_xml,
                last_error,
            } => {
                assert_eq!(
                    missing,
                    &vec![Selector::text("Cancel"), Selector::desc("Overflow menu")]
                );
                assert_eq!((*polls, *elapsed_ms), (6, 1000));
                assert!(last_xml.is_some() && last_error.is_none());
            }
            other => panic!("{other:?}"),
        }
        let d = r.describe();
        assert!(
            d.contains("TIMED OUT") && d.contains("Overflow menu") && !d.contains("id=\"save\""),
            "{d}"
        );
    }

    #[test]
    fn unreadable_screen_reports_all_missing_and_the_error() {
        let clock = FakeClock::new();
        let mut src = ScriptedSource::cycling(&clock, vec![Err("screen asleep".into())], 0);
        let want = [Selector::id("save"), Selector::id("cancel")];
        match read_require(
            &mut src,
            &clock,
            &want,
            RequireOpts {
                timeout_ms: 400,
                poll_ms: 200,
            },
        ) {
            Required::Timeout {
                missing,
                last_error,
                last_xml,
                ..
            } => {
                assert_eq!(missing.len(), 2);
                assert_eq!(last_error.as_deref(), Some("screen asleep"));
                assert!(last_xml.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn timeout_is_capped_below_two_minutes() {
        let clock = FakeClock::new();
        let mut src = ScriptedSource::cycling(&clock, vec![Ok(screen("A"))], 0);
        let r = read_require(
            &mut src,
            &clock,
            &[Selector::id("nope")],
            RequireOpts {
                timeout_ms: 10_000_000,
                poll_ms: 5000,
            },
        );
        match r {
            Required::Timeout { elapsed_ms, .. } => assert_eq!(elapsed_ms, MAX_TIMEOUT_MS),
            other => panic!("{other:?}"),
        }
    }
}
