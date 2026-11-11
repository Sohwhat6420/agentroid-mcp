//! Task 2.14, hub side: on-phone runner segments.
//!
//! - [`classes`]: the hand-written safe list (`runner/safe-classes.json`) and the per-step `safety_decision`
//!   (allowed / require_confirmation / blocked; Phase 2 blocks require_confirmation).
//! - [`segment`]: the `schemas/segment.json` types, the hub's P-256 key, build (refuses any step that is not R0/R1),
//!   sign and verify.
//! - [`canonical`]: the canonical byte form, identical to `android/runner` `Canonical.java` (fixtures prove it).
//! - [`supervisor`]: admits segments before sending and checks the phone's reports.
//!
//! Nothing here spawns a process, opens a socket or touches a device.

pub mod canonical;
pub mod classes;
pub mod segment;
pub mod supervisor;
pub mod time;

pub use classes::{Decision, Phase, Risk, SafeClasses, SafetyDecision};
pub use segment::{
    build, build_and_sign, sign, sign_with, verify, BuildRefusal, HubKey, Post, Refusal, Segment,
    SegmentRequest, SignedSegment, Step, VerifyContext,
};
pub use supervisor::{Finding, Report, StepReport, Supervisor, Verdict};

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;
