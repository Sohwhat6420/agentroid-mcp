//! Wire types of the IPC (JSON-RPC 2.0, one JSON document per line). Every request and response
//! type derives `JsonSchema`; [`ipc_schema`] collects them into the one file CI turns into
//! TypeScript (`generated/ipc.schema.json`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// JSON-RPC 2.0 spec codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
// Application codes (the -32000..-32099 server-error range).
/// The device is leased by someone else; `data` is the current [`Lease`].
pub const LEASE_HELD: i64 = -32001;
/// The caller does not hold the lease the call needs; `data` is the current [`Lease`] if any.
pub const NOT_HOLDER: i64 = -32002;
/// The device-mode gate refused the call (read_only, S24, allowlist).
pub const REFUSED: i64 = -32003;
/// The device or transport failed the call.
pub const DEVICE_ERROR: i64 = -32004;
/// No connected device matches.
pub const UNKNOWN_DEVICE: i64 = -32005;
/// The daemon could not persist its state (the lease is not granted, or a run directory is not writable).
pub const STATE_ERROR: i64 = -32006;
/// The job's state does not allow this (already running, already ended, another job on the device).
pub const JOB_ERROR: i64 = -32007;
/// No job with that run id, in memory or in the jobs directory.
pub const UNKNOWN_JOB: i64 = -32008;

/// Method names, in the order of the capability list.
pub const METHODS: [&str; 18] = [
    "hello",
    "devices.list",
    "lease.acquire",
    "lease.release",
    "lease.status",
    "device.call",
    "job.start",
    "job.status",
    "job.pause",
    "job.resume",
    "job.stop",
    "job.receipt",
    "job.dry_run",
    "job.run_script",
    // the link hub in the daemon (2.12 + 2.14): pairing state, link health, signed segments
    "link.pair_status",
    "link.health",
    "link.segment",
    "link.segment_report",
];

/// A JSON-RPC request or notification (no `id`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// String, number or null; absent for notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC response: exactly one of `result` and `error`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: Value, result: Value) -> Self {
        RpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: Value, error: RpcError) -> Self {
        RpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

// ---- hello ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct HelloParams {
    /// A label for this session (for example the Claude Code session); it becomes the lease holder name.
    #[serde(default)]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HelloResult {
    pub name: String,
    pub version: String,
    pub protocol: String,
    pub capabilities: Vec<String>,
    /// The holder name this connection will use for leases.
    pub holder: String,
}

// ---- devices.list ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DevicesListParams {}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeviceRow {
    /// Lease key: the hardware serial, or the adb serial when none is known.
    pub device: String,
    pub serial: String,
    pub hw_serial: String,
    pub model: String,
    /// `full`, `helper` or `read_only` (the protected phone is always `read_only`).
    pub mode: String,
    /// Can the phone be leased: on the allowlist (`--allow-serial`, `allow.txt`, `--allow-emulators`), or
    /// the protected phone (always `read_only`, ruled reads only). A listed phone that is not allowed is
    /// visible but every call and lease on it is refused. Older daemons do not send the field; a reader
    /// treats a missing value as allowed.
    #[serde(default = "yes")]
    pub allowed: bool,
    /// Current lease holder, if any.
    pub holder: Option<String>,
    /// care's settings leases in force on this phone (2.13; the `changes_in_force` items of
    /// `schemas/job-view.json`). Empty when none, or when the daemon has no care configured.
    #[serde(default)]
    pub changes_in_force: Vec<ChangeRow>,
}

/// Who holds a settings lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangeOwner {
    pub pid: u32,
    pub job_id: String,
    pub session: String,
}

/// One setting changed under a care lease: `key`, `before`, `applied`, `owner` (plus where and how long).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangeRow {
    pub key: String,
    /// `system`, `secure`, `global` or `vendor`.
    pub ns: String,
    pub before: String,
    pub applied: String,
    pub owner: ChangeOwner,
    /// `session`, `job`, `until` (`permanent` is R2 and refused in Phase 2).
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    pub ts: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DevicesListResult {
    pub devices: Vec<DeviceRow>,
}

// ---- leases (2.8): the record is exactly {device, holder, epoch} ----

/// Mirrors `schemas/registry-lease.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub device: String,
    pub holder: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeaseAcquireParams {
    pub device: String,
}

pub type LeaseAcquireResult = Lease;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeaseReleaseParams {
    pub device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeaseReleaseResult {
    pub released: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LeaseStatusParams {
    /// Only this device; default all leases.
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeaseStatusResult {
    pub leases: Vec<Lease>,
}

// ---- device.call: pass-through to core ----

/// One tool call as `device.call` carries it: the tool and its arguments (flattened, so the JSON
/// is `{"tool": "tap", "x": 1, ...}`) plus the three options every tool takes (2.4), which the
/// daemon enforces:
/// - `require`: selectors that must be on screen before the call (else nothing is done);
/// - `retry`: at most [`MAX_RETRY`] more attempts when the call fails on the transport or its `verify` fails;
/// - `verify`: one selector that must be on screen after the call.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolCall {
    #[serde(flatten)]
    pub tool: Tool,
    /// Selector objects (`id`, `text`, `desc`, `near_text`, `rel`), at most [`MAX_REQUIRE`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require: Vec<Value>,
    /// Extra attempts, 0 to [`MAX_RETRY`]; more is refused as invalid params.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<Value>,
}

/// The most extra attempts `retry` may ask for.
pub const MAX_RETRY: u32 = 3;
/// The most selectors `require` may carry.
pub const MAX_REQUIRE: usize = 8;

/// The tools of `device.call` (the 23 MCP tools minus the three the job methods and `lease` serve).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum Tool {
    /// Screenshot plus UI tree (a read). The `element` and `ocr` options are not implemented in this build (always
    /// return an error).
    Look {
        #[serde(default)]
        max_width: Option<i32>,
        #[serde(default)]
        element: Option<Value>,
        #[serde(default)]
        ocr: Option<bool>,
        /// A snapshot id from an earlier read's header: answer with only what changed since.
        #[serde(default)]
        since: Option<String>,
    },
    /// A screenshot (a read).
    Screenshot {
        #[serde(default)]
        max_width: Option<i32>,
    },
    /// Press and hold a tree index or a point.
    LongPress {
        #[serde(default)]
        index: Option<usize>,
        #[serde(default)]
        x: Option<f64>,
        #[serde(default)]
        y: Option<f64>,
        #[serde(default)]
        space: Option<String>,
        #[serde(default)]
        ms: Option<u32>,
        /// A stable ref (`r3fa91c`) from a tree listing: it names the same element after a scroll.
        #[serde(default, rename = "ref")]
        r#ref: Option<String>,
    },
    Swipe {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        #[serde(default)]
        ms: Option<u32>,
        #[serde(default)]
        space: Option<String>,
    },
    /// `up`, `down`, `left` or `right`.
    Scroll {
        direction: String,
    },
    TypeText {
        text: String,
        #[serde(default)]
        index: Option<usize>,
        #[serde(default)]
        submit: Option<bool>,
        /// A stable ref (`r3fa91c`) from a tree listing: it names the same element after a scroll.
        #[serde(default, rename = "ref")]
        r#ref: Option<String>,
    },
    Key {
        name: String,
    },
    Unlock {},
    OpenApp {
        package: String,
    },
    OpenUrl {
        uri: String,
        #[serde(default)]
        package: Option<String>,
    },
    StartActivity {
        component: String,
        #[serde(default)]
        action: Option<String>,
        #[serde(default)]
        data: Option<String>,
    },
    /// Installed packages containing `filter` (a read).
    ListApps {
        #[serde(default)]
        filter: Option<String>,
    },
    /// Wait (core caps one wait at 10 s).
    Wait {
        seconds: f64,
    },
    /// Set a text field to `value` (text fields only) and read it back from the tree. Targets: `element` (a selector) or `index`.
    SetValue {
        #[serde(default)]
        element: Option<Value>,
        #[serde(default)]
        index: Option<usize>,
        value: Value,
    },
    /// The colour of one pixel (a read). Not implemented in this build (always returns an error).
    PixelProbe {
        x: f64,
        y: f64,
        #[serde(default)]
        space: Option<String>,
    },
    /// PLANNED, not implemented (doctor `diagnose`).
    Doctor {
        #[serde(default)]
        diagnose: Option<String>,
    },
    /// PLANNED, not implemented.
    PullNew {
        dir: String,
    },
    /// PLANNED, not implemented.
    Push {
        local: String,
        phone_path: String,
    },
    /// The UI tree (a read).
    UiTree {
        #[serde(default)]
        interactive_only: Option<bool>,
        /// A snapshot id from an earlier read's header: answer with only what changed since.
        #[serde(default)]
        since: Option<String>,
    },
    /// `adb shell getprop <key>` (a read; the protected phone answers only `ro.serialno`).
    Getprop {
        key: String,
    },
    /// Tap a tree index, or a point (`x`, `y` in `space`).
    Tap {
        #[serde(default)]
        index: Option<usize>,
        #[serde(default)]
        x: Option<f64>,
        #[serde(default)]
        y: Option<f64>,
        #[serde(default)]
        space: Option<String>,
        /// A stable ref (`r3fa91c`) from a tree listing: it names the same element after a scroll.
        #[serde(default, rename = "ref")]
        r#ref: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeviceCallParams {
    pub device: String,
    pub call: ToolCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeviceCallResult {
    pub tool: String,
    pub text: String,
    /// Size of the screenshot, when the call took one (bytes are not carried over this channel yet).
    pub image_bytes: Option<usize>,
    /// The health member (2.4): `alias`, `model`, `mode`, `battery`, `powered`, `temp_c`, `screen`,
    /// `lock`, `foreground {package, title}` and `age_s`, as `apps/mcp/src/header.ts` reads it. Only what
    /// the call path itself saw: a field nobody read is absent, never guessed. Never absent from the reply.
    #[serde(default)]
    pub health: Value,
    /// Attempts made (1 when nothing was retried).
    #[serde(default)]
    pub attempts: u32,
    /// `ocr` when `text` was read from an element crop by OCR on the laptop (no image in the result).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `pixel_probe`: the colour, 0 to 255 each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rgb: Option<[u8; 3]>,
    /// `set_value`: the value read back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

// ---- jobs (task 2.10b): every job method needs the caller to hold the device's lease ----

/// `job.start`. The job runs under the caller's lease and stops before its next acting step if
/// that lease is lost. The caller releases the lease when it is done with the device.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobStartParams {
    pub device: String,
    /// An inline flow: a `schemas/flow.json` object, or the same flow as YAML text. Exactly one of
    /// `flow` and `steps` is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<Value>,
    /// `run_steps`: an ordered list of flow steps (`schemas/flow.json` `$defs/step`; a step with no
    /// `id` is numbered `s1`, `s2`, ...), run in order, stopping at the first failure. Needs `app`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<Value>>,
    /// With `steps`: the package the steps run in (the engine relaunches it if it leaves the screen
    /// and treats it as the one expected app).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Merged into the flow's `vars` (same names win here).
    #[serde(default)]
    pub vars: Option<serde_json::Map<String, Value>>,
    /// The phone's OEM (`xiaomi`), for OEM-scoped pop-up rules; unknown means they never apply.
    #[serde(default)]
    pub oem: Option<String>,
    #[serde(default)]
    pub sdk: Option<u32>,
    /// The per-job pop-up switch; default true.
    #[serde(default)]
    pub auto_dismiss: Option<bool>,
    /// Record the run: if it ends `done`, `<run id>.flow.yaml` (a flow `job.start` replays) and
    /// `<run id>.maestro.yaml` (a Maestro export) are written beside the run directory. Secret
    /// variables' values are never saved. Default false.
    #[serde(default)]
    pub record: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobStartResult {
    pub run_id: String,
    pub status: String,
    /// `journal.ndjson` of this run.
    pub journal: String,
    /// `progress.ndjson` of this run (2.11a tails it).
    pub progress: String,
}

/// `job.status`, `job.pause`, `job.stop`, `job.receipt`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobRef {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobResumeParams {
    pub run_id: String,
    /// A person confirmed: lets a `checkpoint` step that waits for a person pass. Default false.
    #[serde(default)]
    pub human_confirmed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobStatusResult {
    pub run_id: String,
    pub device: String,
    pub flow: String,
    /// `running`, `paused`, `stuck`, `stopped`, `done` or `error` (`schemas/job-view.json`).
    pub status: String,
    pub step_id: Option<String>,
    pub steps_planned: u32,
    pub steps_done: u32,
    pub retries: u32,
    /// One short line: progress, step, last error, health.
    pub line: String,
    pub last_error: Option<String>,
    /// Why it paused (`unknown_overlay`, `unreadable_screen`, `unverified_intent`, ...).
    pub pause_reason: Option<String>,
    /// Records in the journal: the journal position.
    pub journal_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobStopResult {
    pub run_id: String,
    pub status: String,
    /// Records in the journal when it stopped.
    pub journal_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobReceiptResult {
    pub run_id: String,
    /// Exactly `schemas/receipt.json`.
    pub receipt: Value,
    /// The receipt panel's fields `receipt.json` has no slot for: outcome, steps retried / not run,
    /// time split into work / cooling / waiting, settings changed and restored, pop-ups dismissed.
    pub detail: Value,
    /// One row per top-level step of the job's flow, in order: `{step_id, status, note}` where
    /// status is `done`, `failed`, `interrupted`, `running` or `not_run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<Value>>,
    /// For a job started with `record` that ended `done`: `{flow, maestro, steps, maestro_skipped}`,
    /// the two files written and what the Maestro export left as comments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SavedTreeParam {
    pub name: String,
    /// A uiautomator dump.
    pub xml: String,
}

/// `job.dry_run`: the flow against saved trees; the phone is not touched.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobDryRunParams {
    pub device: String,
    pub flow: Value,
    pub trees: Vec<SavedTreeParam>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DryRunRow {
    pub step_id: String,
    pub kind: String,
    pub field: String,
    pub selector: String,
    pub row: Option<String>,
    pub verdict: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DryRunOverlay {
    pub tree: String,
    pub verdict: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobDryRunResult {
    pub flow: String,
    pub matched: usize,
    pub rows: Vec<DryRunRow>,
    pub overlays: Vec<DryRunOverlay>,
    /// The same report as text.
    pub text: String,
}

/// `job.run_script`: JavaScript in the slice-c sandbox (`full` devices only; on any other device
/// every binding is refused, and each refusal is journalled).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunScriptParams {
    pub device: String,
    pub script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunScriptResult {
    pub run_id: String,
    /// `ok`, `script_error`, `memory_limit`, `cpu_deadline`, `wall_deadline`, `call_cap`, `crashed`, `sandbox_error`.
    pub outcome: String,
    pub value: Value,
    pub error: Option<String>,
    pub stdout: Vec<String>,
    pub stdout_truncated: bool,
    /// The sandbox's refusal records, as returned.
    pub refusals: Vec<Value>,
    pub calls: u32,
    pub wall_ms: u64,
    pub cpu_ms: u64,
    /// `journal.ndjson` of this script run.
    pub journal: String,
    /// Refusal records written to that journal (equals `refusals.len()` unless the disk failed).
    pub journalled_refusals: usize,
}

// ---- the link hub (2.12 / 2.14, in the daemon) ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct LinkPairStatusParams {
    /// Only this device; default every paired phone.
    #[serde(default)]
    pub device: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkPairRow {
    pub device: String,
    /// The phone key's id (pinned at pairing).
    pub key_id: String,
    pub revoked: bool,
    /// The link epoch (one counter with the 2.8 lease epoch).
    pub epoch: Option<u64>,
    pub connected: bool,
    pub online: bool,
    pub last_seen_ms: Option<u64>,
    pub pending_requests: usize,
    /// A newer hub owns the phone.
    pub superseded: bool,
    /// The phone asked to be paired again.
    pub needs_repair: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkPairStatusResult {
    pub devices: Vec<LinkPairRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkHealthParams {
    pub device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkHealthResult {
    pub device: String,
    /// `link` when the paired link is up and not revoked, else `adb` (the fallback, under the same lease).
    pub transport: String,
    pub paired: bool,
    pub connected: bool,
    pub online: bool,
    pub link_epoch: Option<u64>,
    pub lease_epoch: Option<u64>,
    /// One line: why this transport.
    pub reason: String,
}

/// `link.segment`: build, classify (Phase 2: R2+ and require_confirmation are refused; there is no
/// approval channel), sign with the hub key and send one runner segment (`schemas/segment.json`)
/// on the link's `segment` frame. Lease holder only; `read_only` devices are refused.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkSegmentParams {
    pub device: String,
    pub segment_id: String,
    pub run_id: String,
    /// `schemas/segment.json` steps (without the classification: the hub computes it).
    pub steps: Vec<Value>,
    /// The flow owns the app it drives (an owned app's own steps can be R1). Default false.
    #[serde(default)]
    pub owned: Option<bool>,
    /// Seconds until the segment expires; default 300.
    #[serde(default)]
    pub expires_in_s: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkSegmentResult {
    pub segment_id: String,
    pub epoch: u64,
    /// The segment's class, computed by the hub (`R0` or `R1`).
    pub classification: String,
    /// The hub answered from its request cache: nothing was sent twice.
    pub from_hub_cache: bool,
    /// The phone's JSON-RPC answer to the segment.
    pub response: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkSegmentReportParams {
    pub device: String,
    pub segment_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkSegmentReportResult {
    pub segment_id: String,
    /// The supervisor found nothing wrong with the phone's report.
    pub ok: bool,
    pub outcome: String,
    pub findings: Vec<String>,
    pub report: Value,
}

/// Root of the generated schema: lists every request and response type so one file defines the lot.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct IpcApi {
    pub rpc_request: RpcRequest,
    pub rpc_response: RpcResponse,
    pub hello: (HelloParams, HelloResult),
    pub devices_list: (DevicesListParams, DevicesListResult),
    pub lease_acquire: (LeaseAcquireParams, LeaseAcquireResult),
    pub lease_release: (LeaseReleaseParams, LeaseReleaseResult),
    pub lease_status: (LeaseStatusParams, LeaseStatusResult),
    pub device_call: (DeviceCallParams, DeviceCallResult),
    pub job_start: (JobStartParams, JobStartResult),
    pub job_status: (JobRef, JobStatusResult),
    pub job_pause: (JobRef, JobStatusResult),
    pub job_resume: (JobResumeParams, JobStatusResult),
    pub job_stop: (JobRef, JobStopResult),
    pub job_receipt: (JobRef, JobReceiptResult),
    pub job_dry_run: (JobDryRunParams, JobDryRunResult),
    pub job_run_script: (RunScriptParams, RunScriptResult),
    pub link_pair_status: (LinkPairStatusParams, LinkPairStatusResult),
    pub link_health: (LinkHealthParams, LinkHealthResult),
    pub link_segment: (LinkSegmentParams, LinkSegmentResult),
    pub link_segment_report: (LinkSegmentReportParams, LinkSegmentReportResult),
}

/// The generated schema as pretty JSON text (trailing newline).
pub fn ipc_schema() -> String {
    let schema = schemars::schema_for!(IpcApi);
    let mut s = serde_json::to_string_pretty(&schema).expect("schema serializes");
    s.push('\n');
    s
}
