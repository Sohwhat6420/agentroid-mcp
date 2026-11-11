//! `agentroid-setup`: task 2.7 (`setup`, `doctor`, `--undo`, quirks v0).
//!
//! * [`doctor`]: read-only switch check with exact fixes; flags other UiAutomation owners (V49).
//! * [`setup`]: device profile and `devices.toml` entry in at most 6 prompts; `--dry-run`.
//! * [`journal`]: the undo journal and byte-identical `--undo`.
//! * [`quirks`]: `quirks.toml`, keyed by OEM/skin and SDK range.
//! * [`device`]: the reads, their parsers and the writes, as exact shell lines.
//!
//! Every phone command goes through a `Runner`; the binary hands it adb-link's `GuardedRunner`, so the
//! S24 answers only its approved reads. Nothing here opens a window (adb-link spawns adb with
//! `CREATE_NO_WINDOW`).

pub mod device;
pub mod doctor;
pub mod journal;
pub mod quirks;
pub mod setup;
pub mod sha256;
pub mod tomlite;

#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;
