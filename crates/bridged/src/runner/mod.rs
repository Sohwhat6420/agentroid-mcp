//! The 2.10 job runner, slice a: the flow engine.
//!
//! * [`flow`]: YAML flows, validated against `schemas/flow.json`.
//! * [`engine`]: runs a flow on a `Transport` through core's gate, with the caps, the cycle
//!   detector, app relaunch and the settle contract.
//! * [`popups`]: the pop-up rulebook (`popups.yaml`) and the dismissal policy.
//! * [`dryrun`]: an offline dry run against saved trees.
//! * [`sink`]: progress events (`schemas/progress.json`), journal records (`schemas/journal.json`)
//!   and checkpoints, behind traits that slice b persists.
//!
//! * [`journal`] (slice b): the run directory, NDJSON with group-commit fsync, checkpoints.
//! * [`resume`] (slice b): read a journal back (torn tail truncated, corrupt middle refused) and
//!   decide where to continue; an intent with no outcome is re-verified, never re-sent.
//! * [`receipt`] (slice b): the end-of-job receipt (`schemas/receipt.json` plus the panel detail).
//!
//! * [`flight`] (2.11a): the flight recorder (the last 10 trees and 3 frames per job, dumped to
//!   `<run>/flight/` on `stuck`) and the 7-day retention pruned at job start.
//!
//! * [`queue`] (PQ-1): the scheduled action queue: times, screen conditions, one wake-up per stop,
//!   `resume(run_id)`; no model calls.
//!
//! Slice c (`run_script`) is `crates/sandbox`; the daemon wiring is `crate::jobs`.

pub mod dryrun;
pub mod engine;
pub mod flight;
pub mod flow;
pub mod journal;
pub mod popups;
pub mod queue;
pub mod receipt;
pub mod record;
pub mod resume;
pub mod sink;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_b;
#[cfg(test)]
mod tests_queue;
#[cfg(test)]
mod tests_sup;
