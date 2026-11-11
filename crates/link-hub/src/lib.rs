//! `link-hub`: the laptop end of the persistent phone link.
//!
//! The phone DIALS IN to `ws://<tailnet address>:<port>/helper` (protocol v1, `schemas/link.json`);
//! this crate never dials a phone. Modules:
//!
//! * [`bind`]: the listener refuses every address but one literal tailnet IPv4 (or 127.0.0.1 for
//!   tests); the rules and messages are those of `apps/mcp/src/http-wiring.ts` `checkBindAddress`.
//! * [`crypto`]: strict base64, P-256 SPKI, DER ECDSA signatures, the two signed layouts, and the
//!   hub key pair (private half only in Windows Credential Manager, through `credstore`).
//! * [`proto`]: a strict validator for every link.json message, the exact fix texts, frame builders.
//! * [`store`]: `hub-state/` (device bindings, per-device epochs, the request-id response cache),
//!   every file written atomically.
//! * [`hub`]: the WebSocket server and the API the daemon will call: [`hub::Hub::send_request`],
//!   [`hub::Hub::status`], [`hub::Hub::events`].
//! * [`pair`]: `agentroid-link link pair <serial>` over the guarded adb runner.
//! * [`paircode`]: one-time codes and the `agentroid://pair` QR link for pairing with no adb.
//! * [`power`]: `PowerCreateRequest`/`PowerSetRequest(PowerRequestSystemRequired)` while a job runs (V13).
//! * [`tls`]: TLS 1.3 for `--link-lan` (home Wi-Fi): a self-signed P-256 certificate pinned by the QR's `fp`.
//! * [`clock`]: the time seam (a fake clock drives the 10-minute offline rule in tests).

pub mod bind;
pub mod clock;
pub mod crypto;
pub mod hub;
pub mod pair;
pub mod paircode;
pub mod power;
pub mod proto;
pub mod store;
pub mod tls;

pub use hub::{DeviceStatus, EpochSource, Hub, HubConfig, HubError, HubEvent, Response};
