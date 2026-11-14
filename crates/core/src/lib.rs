//! `core`: task 2.2 (transports, selectors, coordinate frames, settle).
//!
//! Pure logic first, behind traits, so every test runs with no phone:
//!
//! * [`tree`]: uiautomator XML dump parser with borrowed labels.
//! * [`selector`]: the one matcher (`id`, `text`, `desc`, `near_text`, relative coordinates last)
//!   with resolution disclosure, shared by live tools and replay.
//! * [`refs`]: stable element refs and `since` snapshot diffs.
//! * [`require`]: `require` on reads; on timeout, the selectors that were missing.
//! * [`settle`]: the settle contract (200 ms polls, 2 equal signatures, 3 s cap, then `unsettled`).
//! * [`frame`]: coordinate frames (screen, window, virtual display) and clamping.
//! * [`gate`]: the tool calls and the device-mode gate (`full` / `helper` / `read_only`, S24 never a target).
//! * [`shell`]: quoting, key names, intents and unlock helpers ported from phone-hands.
//! * [`transport`]: the `Transport` seam and `Core`, which serves every call.
//! * [`fake`] and [`selftest`]: a fake phone and the 31-check selftest scaffold.
//! * [`screenguard`] (D3): the deterministic payment / destructive / credential screen classifier the
//!   gate consults before an acting call lands (deny by default).
//! * [`audit`] (D3): the hash-chained audit log and `agentroid audit verify`; rotation and the
//!   off-machine anchor (D3b).
//! * [`approval`] (D3b): the approval channel that turns an `asked` verdict into a human yes
//!   (strict grammar, one shot, 90 s, deny on timeout; ntfy now, pluggable).
//! * [`shellguard`] and [`fileguard`] (D3b): raw shell lines, `push` and `pull` through the same
//!   deny-by-default rules and the same audit log.
//! * [`protected`]: the protected-device lease (opt-in): one owner's Allow, or a signed and fresh
//!   presence file overnight, opens a time-boxed lease on the protected phone. The gate is the
//!   owner's approval, not what the screen shows; PIN digits, `unlock` and code pushes stay
//!   refused under any lease.
//!
//! Nothing here starts adb or touches a device; the live transport is a later task.

pub mod approval;
pub mod askscreen;
pub mod audit;
pub mod fake;
pub mod fileguard;
pub mod frame;
pub mod gate;
pub mod protected;
pub mod refs;
pub mod require;
pub mod screenguard;
pub mod screenlock;
pub mod selector;
pub mod selftest;
pub mod settle;
pub mod shell;
pub mod shellguard;
pub mod transport;
pub mod tree;

/// Crate name, used by tests and diagnostics.
pub fn name() -> &'static str {
    "core"
}

#[cfg(test)]
mod tests {
    #[test]
    fn name_is_stable() {
        assert_eq!(super::name(), "core");
    }
}
