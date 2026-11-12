//! `bridged`: task 2.3 (daemon, JSON-RPC IPC, generated types) and 2.8 (registry v0,
//! single-holder lease).
//!
//! One daemon owns the devices, the leases and the registry; every MCP process is a thin client on
//! the named pipe `\\.\pipe\agentroid`.
//!
//! The library crates run inside the daemon: the link hub ([`link`]; its
//! listener lives in `crates/link-hub`, tailnet or 127.0.0.1 only), care's heat guard, battery
//! floor and settings leases ([`jobcare`]), agent/human mode and the take-over ([`takeover`]),
//! signed segments (`link.segment`), and the job view's HTTP side ([`jobview`]: this crate's one
//! TCP listener, 127.0.0.1 and in hub mode the tailnet, never `0.0.0.0` or the LAN).

pub mod allow;
pub mod awake;
pub mod devcall;
pub mod devtools;
pub mod diagnose;
pub mod jobcare;
pub mod jobs;
pub mod jobview;
pub mod link;
pub mod listener;
pub mod live;
pub mod probe;
pub mod proto;
pub mod registry;
pub mod runner;
pub mod server;
pub mod sha256;
pub mod takeover;

#[cfg(windows)]
pub mod pipe;
#[cfg(unix)]
pub mod unix_socket;

/// Default daemon address: a named pipe on Windows, a Unix socket elsewhere (`--pipe` overrides both).
#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\agentroid";
#[cfg(not(windows))]
pub const PIPE_NAME: &str = "/tmp/agentroid.sock";
