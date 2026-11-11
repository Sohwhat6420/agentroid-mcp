//! `handover`: task 2.15, agent mode and human mode (hub side).
//!
//! * [`touch`]: Detector 1, the `getevent` device classifier and touch-down detector.
//! * [`stream`]: the admitted `getevent` commands (adb-link's guard: full devices only, never the
//!   S24), the line stream (`CREATE_NO_WINDOW`) and the restart/re-map supervisor.
//! * [`mode`]: the mode state machine and the [`mode::SettingsLeases`] seam that `crates/care` fills.
//! * [`vdisplay`] and [`clash`]: the scrcpy virtual-display handover and what counts as a clash.
//! * [`profile`]: the two lease bundles, `profiles/human.json` and `profiles/agent.json`.
//!
//! Nothing here runs adb on its own; tests use fixtures, a fake clock and fake leases.

pub mod clash;
pub mod mode;
pub mod profile;
pub mod stream;
pub mod touch;
pub mod vdisplay;
