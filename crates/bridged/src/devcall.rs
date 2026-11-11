//! `device.call` (task 2.4, daemon side): every tool of the MCP server, through the one gate, with
//! `require`, `retry` and `verify` enforced here and the `health` member on every reply.
//!
//! One path to the phone. Each step below goes through [`DeviceBackend::call`] (core's gate: the protected phone
//! rule, the allowlist, the device mode) or, for a step core has no call for, is preceded by
//! [`DeviceBackend::admit`] with the nearest call (`set_value` is gated as a `tap`, a crop and a
//! pixel probe as reads), so a refused call reaches nothing. The health member is read from what
//! those same calls already left behind ([`DeviceBackend::health`]): it never sends a command.
//!
//! `retry`: at most [`MAX_RETRY`] more attempts, and only after a failure that repeating can cure:
//! the transport failed, a `set_value` read-back differed, or `verify` did not find its selector.
//! A refusal, bad input, a failed `require` or an input the phone refused is never retried.
//!
//! Lane B owns `doctor`, `push` and `pull_new`: their arms answer "planned" and do nothing.

use crate::proto::*;
use crate::server::{device_key, parse_params, to_value, Daemon};
use agentroid_core::gate::{Call, Target};
use agentroid_core::selector::{resolve, Chosen, Selector, Status};
use agentroid_core::transport::{CoreError, DeviceInfo, Outcome};
use agentroid_core::tree::parse;
use serde_json::{json, Map, Value};

/// How long `verify` waits for its selector (core's `require` poll), per attempt.
const VERIFY_MS: u64 = 2000;
/// How long `require` waits for its selectors before the call is refused.
const REQUIRE_MS: u64 = 5000;
/// The pause between two reads while `require` or `verify` waits (core's own poll interval).
const POLL_MS: u64 = 200;
/// After `type_text`, `verify`'s selector is read again this long after it first appeared, at two delays.
const REREAD_MS: [u64; 2] = [150, 600];

/// Why `verify` failed.
enum VerifyMiss {
    /// The selector never appeared (the detail names it and the reads made).
    Missing(String),
    /// It is on screen, but the tree did not change from before a tap or long press.
    Unchanged,
    /// It appeared and was gone again this many ms after (a `type_text` field rewritten).
    Unstable(u64),
    Read(CoreError),
}

/// The clock `require` and `verify` wait by. They poll with the backend lock released between reads,
/// so a second connection is not held up for the whole wait. The default is real time.
pub struct Waits {
    pub now_ms: Box<dyn Fn() -> u64 + Send + Sync>,
    pub sleep_ms: Box<dyn Fn(u64) + Send + Sync>,
}

impl Waits {
    fn real() -> Waits {
        let t0 = std::time::Instant::now();
        Waits {
            now_ms: Box::new(move || u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)),
            sleep_ms: Box::new(|ms| std::thread::sleep(std::time::Duration::from_millis(ms))),
        }
    }
}

/// Whether repeating a failed call is safe: a read, or a call that sets an absolute state. `retry` repeats
/// only these. A tap, key, swipe, scroll, `type_text`, `open_url` or `start_activity` can double-act (a
/// toggle taps twice, text types twice), and a transport error or a failed `verify` does not say whether
/// the first try landed.
fn idempotent(t: &Tool) -> bool {
    matches!(
        t,
        Tool::Look { .. }
            | Tool::Screenshot { .. }
            | Tool::UiTree { .. }
            | Tool::Getprop { .. }
            | Tool::ListApps { .. }
            | Tool::Wait { .. }
            | Tool::PixelProbe { .. }
            | Tool::SetValue { .. }
            | Tool::OpenApp { .. }
            | Tool::Unlock {}
    )
}
/// The longest text `set_value` will clear key by key.
const CLEAR_MAX: usize = 100;

/// A failed step: the error to send, and whether repeating the call can help.
struct Failure {
    rpc: RpcError,
    retryable: bool,
    reason: &'static str,
}

impl Failure {
    fn hard(rpc: RpcError, reason: &'static str) -> Failure {
        Failure {
            rpc,
            retryable: false,
            reason,
        }
    }
    fn retry(rpc: RpcError, reason: &'static str) -> Failure {
        Failure {
            rpc,
            retryable: true,
            reason,
        }
    }
    fn invalid(msg: impl Into<String>) -> Failure {
        Failure::hard(RpcError::new(INVALID_PARAMS, msg), "invalid")
    }
    fn device(msg: impl Into<String>, reason: &'static str) -> Failure {
        Failure::hard(RpcError::new(DEVICE_ERROR, msg), reason)
    }
}

/// What one successful attempt produced.
#[derive(Default)]
struct Done {
    tool: String,
    text: String,
    image_bytes: Option<usize>,
    source: Option<String>,
    rgb: Option<[u8; 3]>,
    value: Option<Value>,
}

fn map_core(tool: &str, e: CoreError) -> Failure {
    match e {
        CoreError::Refused(r) => Failure::hard(
            RpcError::new(REFUSED, r.to_string()).with_data(json!({ "tool": tool })),
            "refused",
        ),
        e @ (CoreError::Invalid(_) | CoreError::Frame(_) | CoreError::Resolve(_)) => {
            Failure::hard(RpcError::new(INVALID_PARAMS, e.to_string()), "invalid")
        }
        CoreError::Transport(t) => Failure::retry(
            RpcError::new(DEVICE_ERROR, CoreError::Transport(t).to_string()),
            "transport",
        ),
        e @ CoreError::RequireTimeout { .. } => Failure::device(e.to_string(), "require"),
        e => Failure::device(e.to_string(), "device"),
    }
}

/// A gate refusal of a composite tool, worded for the tool the caller named (the check ran on a stand-in call).
fn map_admit(tool: &str, e: CoreError) -> Failure {
    match e {
        CoreError::Refused(r) => {
            let msg = match r {
                agentroid_core::gate::Refusal::S24 { .. } => {
                    format!("{tool} refused: the protected phone is read-only and never a target")
                }
                agentroid_core::gate::Refusal::ReadOnly { .. } => {
                    format!("{tool} refused: this device is read_only, so it only answers reads")
                }
                agentroid_core::gate::Refusal::NeedsAdbShell { .. } => format!(
                    "{tool} refused: this device is in helper mode and {tool} needs adb shell"
                ),
                other => other.to_string(),
            };
            Failure::hard(
                RpcError::new(REFUSED, msg).with_data(json!({ "tool": tool })),
                "refused",
            )
        }
        other => map_core(tool, other),
    }
}

/// `{id|text|desc|near_text|rel}`: the same keys as core's [`Selector`]; anything else is refused.
pub fn parse_selector(v: &Value) -> Result<Selector, String> {
    let Value::Object(o) = v else {
        return Err(
            "a selector must be an object with ref, id, text, desc, near_text or rel".into(),
        );
    };
    let mut s = Selector::default();
    for (k, val) in o {
        let text = || {
            val.as_str()
                .map(str::to_string)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| format!("selector key {k:?} must be a non-empty string"))
        };
        match k.as_str() {
            "ref" => s.ref_id = Some(text()?),
            "id" => s.id = Some(text()?),
            "text" => s.text = Some(text()?),
            "desc" => s.desc = Some(text()?),
            "near_text" => s.near_text = Some(text()?),
            "rel" => {
                let a = val
                    .as_array()
                    .filter(|a| a.len() == 2)
                    .and_then(|a| Some((a[0].as_f64()?, a[1].as_f64()?)));
                s.rel = Some(
                    a.filter(|(x, y)| (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y))
                        .ok_or("selector key \"rel\" must be [fx, fy], each 0 to 1")?,
                );
            }
            other => {
                return Err(format!(
                    "unknown selector key {other:?} (use ref, id, text, desc, near_text, rel)"
                ))
            }
        }
    }
    if s.is_empty() {
        return Err("an empty selector matches nothing".into());
    }
    Ok(s)
}

fn target(
    index: Option<usize>,
    x: Option<f64>,
    y: Option<f64>,
    space: &Option<String>,
    r: &Option<String>,
) -> Option<Target> {
    match (index, x, y, space, r) {
        (Some(i), None, None, None, None) => Some(Target::Index(i)),
        (None, Some(x), Some(y), _, None) => Some(Target::Point {
            x,
            y,
            space: space.clone(),
            image_width: None,
        }),
        (None, None, None, None, Some(r)) if !r.trim().is_empty() => {
            Some(Target::Select(Selector::ref_id(r)))
        }
        _ => None,
    }
}

/// The core call for a tool core serves as one call. `require` rides natively only on `ui_tree`.
fn to_core_call(t: &Tool, require: &[Selector]) -> Result<Call, Failure> {
    Ok(match t {
        Tool::Look { max_width, .. } => Call::Look {
            max_width: max_width.unwrap_or(720),
        },
        Tool::Screenshot { max_width } => Call::Screenshot {
            max_width: max_width.unwrap_or(720),
        },
        Tool::UiTree {
            interactive_only, ..
        } => Call::UiTree {
            interactive_only: interactive_only.unwrap_or(false),
            require: require.to_vec(),
            timeout_ms: None,
        },
        Tool::Getprop { key } => Call::Getprop { key: key.clone() },
        Tool::Tap {
            index,
            x,
            y,
            space,
            r#ref,
        } => Call::Tap {
            target: target(*index, *x, *y, space, r#ref).ok_or_else(|| {
                Failure::invalid("tap needs one of: index, ref, or x and y (with optional space)")
            })?,
        },
        Tool::LongPress {
            index,
            x,
            y,
            space,
            ms,
            r#ref,
        } => Call::LongPress {
            target: target(*index, *x, *y, space, r#ref).ok_or_else(|| {
                Failure::invalid(
                    "long_press needs one of: index, ref, or x and y (with optional space)",
                )
            })?,
            ms: ms.unwrap_or(800),
        },
        Tool::Swipe {
            x1,
            y1,
            x2,
            y2,
            ms,
            space,
        } => Call::Swipe {
            x1: *x1,
            y1: *y1,
            x2: *x2,
            y2: *y2,
            ms: ms.unwrap_or(300),
            space: space.clone(),
            image_width: None,
        },
        Tool::Scroll { direction } => Call::Scroll {
            direction: direction.clone(),
        },
        Tool::TypeText {
            text,
            index,
            submit,
            ..
        } => Call::TypeText {
            text: text.clone(),
            index: *index,
            submit: submit.unwrap_or(false),
        },
        Tool::Key { name } => Call::Key { name: name.clone() },
        Tool::Unlock {} => Call::Unlock,
        Tool::OpenApp { package } => Call::OpenApp {
            package: package.clone(),
        },
        Tool::OpenUrl { uri, package } => Call::OpenUrl {
            uri: uri.clone(),
            package: package.clone(),
        },
        Tool::StartActivity {
            component,
            action,
            data,
        } => Call::StartActivity {
            component: component.clone(),
            action: action.clone(),
            data: data.clone(),
        },
        Tool::ListApps { filter } => Call::ListApps {
            filter: filter.clone().unwrap_or_default(),
        },
        Tool::Wait { seconds } => Call::Wait { seconds: *seconds },
        Tool::SetValue { .. }
        | Tool::PixelProbe { .. }
        | Tool::Doctor { .. }
        | Tool::PullNew { .. }
        | Tool::Push { .. } => return Err(Failure::invalid("not a single core call")),
    })
}

fn tool_name(t: &Tool) -> &'static str {
    match t {
        Tool::Look { .. } => "look",
        Tool::Screenshot { .. } => "screenshot",
        Tool::LongPress { .. } => "long_press",
        Tool::Swipe { .. } => "swipe",
        Tool::Scroll { .. } => "scroll",
        Tool::TypeText { .. } => "type_text",
        Tool::Key { .. } => "key",
        Tool::Unlock {} => "unlock",
        Tool::OpenApp { .. } => "open_app",
        Tool::OpenUrl { .. } => "open_url",
        Tool::StartActivity { .. } => "start_activity",
        Tool::ListApps { .. } => "list_apps",
        Tool::Wait { .. } => "wait",
        Tool::SetValue { .. } => "set_value",
        Tool::PixelProbe { .. } => "pixel_probe",
        Tool::Doctor { .. } => "doctor",
        Tool::PullNew { .. } => "pull_new",
        Tool::Push { .. } => "push",
        Tool::UiTree { .. } => "ui_tree",
        Tool::Getprop { .. } => "getprop",
        Tool::Tap { .. } => "tap",
    }
}

fn value_text(v: &Value) -> Result<String, Failure> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err(Failure::invalid(
            "set_value: value must be a string or a number",
        )),
    }
}

impl Daemon {
    /// `device.call`: resolve, check the lease, run the call with its options, attach `health`.
    pub(crate) fn device_call(&self, conn_id: u64, params: &Value) -> Result<Value, RpcError> {
        let p: DeviceCallParams = parse_params(params)?;
        let dev = self.resolve(&p.device)?;
        let key = device_key(&dev);
        self.require_holder(&key, conn_id)?;
        match self.run_tool(&dev, &key, &p.call) {
            Ok((d, attempts)) => {
                let health = self.health_member(&dev);
                to_value(&DeviceCallResult {
                    tool: d.tool,
                    text: d.text,
                    image_bytes: d.image_bytes,
                    health,
                    attempts,
                    source: d.source,
                    rgb: d.rgb,
                    value: d.value,
                })
            }
            Err(mut e) => {
                // the refusal (and every other error from here on) carries the health the daemon could give
                let mut data = match e.data.take() {
                    Some(Value::Object(m)) => m,
                    _ => Map::new(),
                };
                data.insert("health".into(), self.health_member(&dev));
                e.data = Some(Value::Object(data));
                Err(e)
            }
        }
    }

    /// Name a phone for people (the header's `alias`). Not persisted.
    pub fn set_alias(&self, key: &str, alias: &str) {
        self.aliases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.to_string(), alias.to_string());
    }

    /// The `health` member: identity, plus whatever the call path has already seen. No phone traffic.
    pub(crate) fn health_member(&self, dev: &DeviceInfo) -> Value {
        let alias = self
            .aliases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&device_key(dev))
            .cloned();
        let (hint, mode) = {
            let mut b = self.backend();
            (b.health(dev), b.mode_of(dev))
        };
        let mut m = Map::new();
        if let Some(a) = alias {
            m.insert("alias".into(), json!(a));
        }
        if !dev.model.is_empty() {
            m.insert("model".into(), json!(dev.model));
        }
        m.insert("mode".into(), json!(mode.as_str()));
        if let Some(b) = hint.battery {
            m.insert("battery".into(), json!(b.min(100)));
        }
        if let Some(p) = hint.powered {
            m.insert("powered".into(), json!(p));
        }
        if let Some(t) = hint.temp_c {
            m.insert("temp_c".into(), json!(t));
        }
        if let Some(s) = &hint.screen {
            m.insert("screen".into(), json!(s));
        }
        if let Some(l) = &hint.lock {
            m.insert("lock".into(), json!(l));
        }
        if hint.foreground_package.is_some() || hint.foreground_title.is_some() {
            let mut f = Map::new();
            if let Some(p) = &hint.foreground_package {
                f.insert("package".into(), json!(p));
            }
            if let Some(t) = &hint.foreground_title {
                f.insert("title".into(), json!(t));
            }
            m.insert("foreground".into(), Value::Object(f));
        }
        if !hint.is_empty() {
            m.insert("age_s".into(), json!((hint.age_s * 10.0).round() / 10.0));
        }
        Value::Object(m)
    }

    /// Validate the options, then run the attempts.
    fn run_tool(
        &self,
        dev: &DeviceInfo,
        key: &str,
        call: &ToolCall,
    ) -> Result<(Done, u32), RpcError> {
        let retry = call.retry.unwrap_or(0);
        if retry > MAX_RETRY {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!("retry must be 0 to {MAX_RETRY}, got {retry}"),
            ));
        }
        if call.require.len() > MAX_REQUIRE {
            return Err(RpcError::new(
                INVALID_PARAMS,
                format!(
                    "require takes at most {MAX_REQUIRE} selectors, got {}",
                    call.require.len()
                ),
            ));
        }
        let require: Vec<Selector> = call
            .require
            .iter()
            .map(parse_selector)
            .collect::<Result<_, _>>()
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("require: {e}")))?;
        let verify = call
            .verify
            .as_ref()
            .map(parse_selector)
            .transpose()
            .map_err(|e| RpcError::new(INVALID_PARAMS, format!("verify: {e}")))?;
        let name = tool_name(&call.tool);
        if matches!(
            call.tool,
            Tool::Doctor { .. } | Tool::PullNew { .. } | Tool::Push { .. }
        ) {
            return Err(RpcError::new(
                METHOD_NOT_FOUND,
                format!("tool {name} is planned: not implemented in this bridged yet"),
            )
            .with_data(json!({ "tool": name, "planned": true })));
        }
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            match self.attempt(dev, key, &call.tool, &require, verify.as_ref()) {
                Ok(d) => return Ok((d, attempts)),
                Err(f) if f.retryable && attempts <= retry && idempotent(&call.tool) => continue,
                Err(mut f) => {
                    let mut data = match f.rpc.data.take() {
                        Some(Value::Object(m)) => m,
                        _ => Map::new(),
                    };
                    data.insert("tool".into(), json!(name));
                    data.insert("attempts".into(), json!(attempts));
                    data.insert("reason".into(), json!(f.reason));
                    if f.retryable && retry > 0 && !idempotent(&call.tool) {
                        // the first try may have landed: repeating a tap or a key could act twice
                        data.insert(
                            "not_retried".into(),
                            json!(format!("{name} is not idempotent")),
                        );
                        f.rpc.message = format!("{} (not retried: {name} could act twice; look at the screen, then decide)", f.rpc.message);
                    }
                    f.rpc.data = Some(Value::Object(data));
                    if attempts > 1 {
                        f.rpc.message =
                            format!("{} (gave up after {attempts} attempts)", f.rpc.message);
                    }
                    return Err(f.rpc);
                }
            }
        }
    }

    /// Read the tree until every selector resolves or `total_ms` passes. One read per backend lock: the lock
    /// is free while this waits, so another connection is served between two reads.
    fn wait_tree(
        &self,
        key: &str,
        interactive_only: bool,
        sels: &[Selector],
        total_ms: u64,
    ) -> Result<Outcome, CoreError> {
        let w = self.waits.get_or_init(Waits::real);
        let start = (w.now_ms)();
        let mut polls = 0u32;
        loop {
            polls += 1;
            let call = Call::UiTree {
                interactive_only,
                require: sels.to_vec(),
                timeout_ms: Some(0),
            };
            let read = self.backend().call(key, &call);
            match read {
                Err(CoreError::RequireTimeout { missing, .. }) => {
                    let elapsed = (w.now_ms)().saturating_sub(start);
                    if elapsed >= total_ms {
                        let names: Vec<String> = missing.iter().map(ToString::to_string).collect();
                        let detail = format!(
                            "require TIMED OUT after {polls} read(s), {elapsed} ms; missing: {}",
                            names.join("; ")
                        );
                        return Err(CoreError::RequireTimeout { missing, detail });
                    }
                    (w.sleep_ms)(POLL_MS.min(total_ms - elapsed).max(1));
                }
                other => return other,
            }
        }
    }

    /// Replace how `require` and `verify` wait (tests use a virtual clock). Once; later calls are ignored.
    pub fn set_waits(&self, w: Waits) {
        let _ = self.waits.set(w);
    }

    fn core(&self, key: &str, call: &Call) -> Result<agentroid_core::transport::Outcome, Failure> {
        self.backend()
            .call(key, call)
            .map_err(|e| map_core(call.name(), e))
    }

    /// One attempt: `require`, the call itself, then `verify`.
    fn attempt(
        &self,
        dev: &DeviceInfo,
        key: &str,
        tool: &Tool,
        require: &[Selector],
        verify: Option<&Selector>,
    ) -> Result<Done, Failure> {
        let name = tool_name(tool);
        let own_require = matches!(tool, Tool::UiTree { .. });
        if !require.is_empty() && !own_require {
            // the gate first: a refused call must not even read the screen
            let stand_in = match tool {
                Tool::SetValue { .. } => Call::Tap {
                    target: Target::Index(0),
                },
                Tool::PixelProbe { .. } => Call::Screenshot { max_width: 720 },
                other => to_core_call(other, &[])?,
            };
            self.backend()
                .admit(key, &stand_in)
                .map_err(|e| map_admit(name, e))?;
            self.wait_tree(key, false, require, REQUIRE_MS)
                .map_err(|e| match e {
                    CoreError::RequireTimeout { detail, .. } => Failure::device(
                        format!("require failed, nothing was done: {detail}"),
                        "require",
                    ),
                    e => map_core(name, e),
                })?;
        }
        // verify v2: a tap or long press that asks to be verified must also change the screen, so the
        // tree is fingerprinted before the call (the gate first: a refused call reads nothing)
        let before =
            if verify.is_some() && matches!(tool, Tool::Tap { .. } | Tool::LongPress { .. }) {
                self.backend()
                    .admit(key, &to_core_call(tool, &[])?)
                    .map_err(|e| map_admit(name, e))?;
                self.core(
                    key,
                    &Call::UiTree {
                        interactive_only: false,
                        require: vec![],
                        timeout_ms: None,
                    },
                )?;
                Some(
                    self.latest_fingerprint(dev)
                        .ok_or_else(|| Failure::device("the screen could not be read", "device"))?,
                )
            } else {
                None
            };
        let mut done = match tool {
            Tool::SetValue { element, index, value } => self.set_value(dev, key, element.as_ref(), *index, value)?,
            Tool::PixelProbe { x, y, space } => self.pixel_probe(dev, key, *x, *y, space.as_deref())?,
            Tool::Look { since: Some(_), element: Some(_), .. } | Tool::Look { since: Some(_), ocr: Some(true), .. } => {
                return Err(Failure::invalid("since applies to the whole tree: it cannot be combined with element or ocr"))
            }
            Tool::Look { element: Some(_), .. } | Tool::Look { ocr: Some(true), .. } => self.look_element(dev, key, tool)?,
            Tool::UiTree { since: Some(_), .. } if !require.is_empty() => return Err(Failure::invalid("since cannot be combined with require: read the tree with require, then ask for the diff")),
            Tool::UiTree { interactive_only, .. } if !require.is_empty() => {
                let o = self.wait_tree(key, interactive_only.unwrap_or(false), require, REQUIRE_MS).map_err(|e| map_core("ui_tree", e))?;
                Done { tool: name.to_string(), text: o.text, image_bytes: o.image.map(|i| i.len()), ..Done::default() }
            }
            other => {
                let call = match other {
                    Tool::TypeText { text, index, submit, r#ref: Some(r) } => {
                        if index.is_some() {
                            return Err(Failure::invalid("type_text takes index or ref, not both"));
                        }
                        let i = self.index_of_ref(key, dev, r)?;
                        Call::TypeText { text: text.clone(), index: Some(i), submit: submit.unwrap_or(false) }
                    }
                    other => to_core_call(other, require)?,
                };
                let o = self.core(key, &call)?;
                let mut done = Done { tool: name.to_string(), text: o.text, image_bytes: o.image.map(|i| i.len()), ..Done::default() };
                if let Tool::Look { since: Some(since), .. } | Tool::UiTree { since: Some(since), .. } = other {
                    done.text = self.splice_diff(dev, &done.text, since)?;
                }
                done
            }
        };
        if let Some(v) = verify {
            let stable = matches!(tool, Tool::TypeText { .. });
            let fail = |msg: String| Failure::retry(RpcError::new(DEVICE_ERROR, msg), "verify");
            match self.wait_verify(key, dev, v, before, stable) {
                Ok(()) => {}
                Err(VerifyMiss::Missing(detail)) => return Err(fail(format!("verify failed: the call ran but the screen does not show the expected element ({detail}). Look again before repeating it."))),
                Err(VerifyMiss::Unchanged) => return Err(fail("verify failed: the call ran and the expected element is on screen, but the screen did not change (its tree is the same as before the call), so it may have hit nothing, or it did something the tree cannot show (a clipboard copy, a tab that was already selected). Look again before repeating it: a repeat can act twice.".into())),
                Err(VerifyMiss::Unstable(ms)) => return Err(fail(format!("verify failed: the expected element was on screen at first but not {ms} ms later (the keyboard or the app changed the field). Look again before repeating it."))),
                Err(VerifyMiss::Read(e)) => return Err(Failure::device(format!("verify could not read the screen: {e}"), "verify")),
            }
            done.text.push_str("\nverify: met");
            if before.is_some() {
                done.text.push_str(" (the screen changed)");
            } else if stable {
                done.text.push_str(" (still there 150 and 600 ms later)");
            }
        }
        Ok(done)
    }

    /// The structural fingerprint of the tree the latest read stored (`None` when there is none).
    fn latest_fingerprint(&self, dev: &DeviceInfo) -> Option<u64> {
        let (xml, io) = self.backend().latest_tree(dev)?;
        parse(&xml, io)
            .ok()
            .map(|t| agentroid_core::refs::fingerprint(&t))
    }

    /// Poll for `verify`'s selector, one read per backend lock, for [`VERIFY_MS`]. With `before` (a tap or
    /// long press) the tree must also differ from that fingerprint; with `stable` (`type_text`) the
    /// selector must still be there [`REREAD_MS`] after it first appeared, so a keyboard that rewrites
    /// the field a moment later is caught.
    fn wait_verify(
        &self,
        key: &str,
        dev: &DeviceInfo,
        sel: &Selector,
        before: Option<u64>,
        stable: bool,
    ) -> Result<(), VerifyMiss> {
        let w = self.waits.get_or_init(Waits::real);
        let start = (w.now_ms)();
        let mut polls = 0u32;
        loop {
            polls += 1;
            let call = Call::UiTree {
                interactive_only: false,
                require: vec![sel.clone()],
                timeout_ms: Some(0),
            };
            let read = self.backend().call(key, &call);
            let present = match read {
                Ok(_) => true,
                Err(CoreError::RequireTimeout { .. }) => false,
                Err(e) => return Err(VerifyMiss::Read(e)),
            };
            if present && before.is_none_or(|b| self.latest_fingerprint(dev) != Some(b)) {
                break;
            }
            let elapsed = (w.now_ms)().saturating_sub(start);
            if elapsed >= VERIFY_MS {
                return Err(if present {
                    VerifyMiss::Unchanged
                } else {
                    VerifyMiss::Missing(format!(
                        "require TIMED OUT after {polls} read(s), {elapsed} ms; missing: {sel}"
                    ))
                });
            }
            (w.sleep_ms)(POLL_MS.min(VERIFY_MS - elapsed).max(1));
        }
        if stable {
            let mut waited = 0;
            for at in REREAD_MS {
                (w.sleep_ms)(at - waited);
                waited = at;
                let call = Call::UiTree {
                    interactive_only: false,
                    require: vec![sel.clone()],
                    timeout_ms: Some(0),
                };
                match self.backend().call(key, &call) {
                    Ok(_) => {}
                    Err(CoreError::RequireTimeout { .. }) => return Err(VerifyMiss::Unstable(at)),
                    Err(e) => return Err(VerifyMiss::Read(e)),
                }
            }
        }
        Ok(())
    }

    /// Read the tree afresh and find the element a stable ref names: its index in the tree that read stored.
    fn index_of_ref(&self, key: &str, dev: &DeviceInfo, r: &str) -> Result<usize, Failure> {
        self.core(
            key,
            &Call::UiTree {
                interactive_only: false,
                require: vec![],
                timeout_ms: None,
            },
        )?;
        let (xml, io) = self
            .backend()
            .latest_tree(dev)
            .ok_or_else(|| Failure::device("the screen could not be read", "device"))?;
        let tree = parse(&xml, io).map_err(|e| Failure::invalid(e.to_string()))?;
        agentroid_core::refs::find(&tree, r).ok_or_else(|| {
            Failure::invalid(format!(
                "ref {r:?} is not on the screen now, nothing was touched. Read the tree again."
            ))
        })
    }

    /// Replace the listing of a tree read (`text` as core rendered it) by the diff since `since`: the
    /// lines up to and including the one that names the snapshot stay, the rest is the diff.
    fn splice_diff(&self, dev: &DeviceInfo, text: &str, since: &str) -> Result<String, Failure> {
        let Some(at) = text
            .find("(tree from ")
            .and_then(|i| text[i..].find('\n').map(|n| i + n))
        else {
            // the screen could not be read (asleep): nothing new was read, so there is nothing to diff
            return Ok(format!("{text}\n(since {since}: no new tree was read)"));
        };
        let diff = self
            .backend()
            .snapshot_diff(dev, since)
            .map_err(|e| map_core("since", e))?;
        Ok(format!("{}\n{diff}", &text[..at]))
    }

    /// Resolve a selector against a fresh tree: the element's index in the tree that read stored, its
    /// bounds, class and text.
    fn find(
        &self,
        dev: &DeviceInfo,
        key: &str,
        sel: &Selector,
    ) -> Result<(usize, agentroid_core::tree::Bounds, String, String), Failure> {
        self.core(
            key,
            &Call::UiTree {
                interactive_only: false,
                require: vec![],
                timeout_ms: None,
            },
        )?;
        let (xml, io) = self
            .backend()
            .latest_tree(dev)
            .ok_or_else(|| Failure::device("the screen could not be read", "device"))?;
        let tree = parse(&xml, io).map_err(|e| Failure::invalid(e.to_string()))?;
        let res = resolve(&tree, sel).map_err(|e| Failure::invalid(e.to_string()))?;
        if res.status == Status::Ambiguous {
            return Err(Failure::invalid(format!(
                "selector is ambiguous, nothing was touched: {}",
                res.describe()
            )));
        }
        match res.chosen {
            Some(Chosen::Element(a)) => {
                let el = tree
                    .element(a.index)
                    .ok_or_else(|| Failure::invalid("the matched element vanished"))?;
                Ok((a.index, el.bounds, el.class.clone(), el.text.to_string()))
            }
            _ => Err(Failure::invalid(
                "this needs an element; a rel point is not one",
            )),
        }
    }

    fn set_value(
        &self,
        dev: &DeviceInfo,
        key: &str,
        element: Option<&Value>,
        index: Option<usize>,
        value: &Value,
    ) -> Result<Done, Failure> {
        let want = value_text(value)?;
        let sel = element
            .map(parse_selector)
            .transpose()
            .map_err(|e| Failure::invalid(format!("element: {e}")))?;
        if sel.is_some() == index.is_some() {
            return Err(Failure::invalid(
                "set_value needs exactly one of element and index",
            ));
        }
        // set_value acts: gate it as a tap before anything is read
        self.backend()
            .admit(
                key,
                &Call::Tap {
                    target: Target::Index(0),
                },
            )
            .map_err(|e| map_admit("set_value", e))?;
        let (idx, class, old) = match (&sel, index) {
            (Some(s), _) => {
                let (i, _, c, t) = self.find(dev, key, s)?;
                (i, c, t)
            }
            (None, Some(i)) => {
                let (xml, io) = self.backend().latest_tree(dev).ok_or_else(|| {
                    Failure::invalid(
                        "No UI tree yet. Call ui_tree or look first, then set_value by index.",
                    )
                })?;
                let tree = parse(&xml, io).map_err(|e| Failure::invalid(e.to_string()))?;
                let el = tree.element(i).ok_or_else(|| {
                    Failure::invalid(format!(
                        "Index {i} is out of range (0-{}). Call ui_tree again.",
                        tree.elements.len().saturating_sub(1)
                    ))
                })?;
                (i, el.class.clone(), el.text.to_string())
            }
            (None, None) => {
                return Err(Failure::invalid(
                    "set_value needs exactly one of element and index",
                ))
            }
        };
        if !class.contains("EditText") {
            return Err(Failure::device(
                format!("set_value on a {class} needs the stored calibration and the OCR read-back from the phone (CP-D); only text fields are set by this daemon today"),
                "unsupported",
            ));
        }
        self.core(
            key,
            &Call::Tap {
                target: Target::Index(idx),
            },
        )?;
        if !old.is_empty() {
            if old.chars().count() > CLEAR_MAX {
                return Err(Failure::device(format!("set_value would have to delete {} characters one key at a time; clear the field first", old.chars().count()), "unsupported"));
            }
            self.core(
                key,
                &Call::Key {
                    name: "KEYCODE_MOVE_END".into(),
                },
            )?;
            for _ in 0..old.chars().count() {
                self.core(
                    key,
                    &Call::Key {
                        name: "KEYCODE_DEL".into(),
                    },
                )?;
            }
        }
        self.core(
            key,
            &Call::TypeText {
                text: want.clone(),
                index: None,
                submit: false,
            },
        )?;
        // the read-back: a fresh tree, the same element
        let read = match &sel {
            Some(s) => self.find(dev, key, s)?.3,
            None => {
                self.core(
                    key,
                    &Call::UiTree {
                        interactive_only: false,
                        require: vec![],
                        timeout_ms: None,
                    },
                )?;
                let (xml, io) = self
                    .backend()
                    .latest_tree(dev)
                    .ok_or_else(|| Failure::device("the screen could not be read", "device"))?;
                let tree = parse(&xml, io).map_err(|e| Failure::invalid(e.to_string()))?;
                tree.element(idx)
                    .map(|e| e.text.to_string())
                    .unwrap_or_default()
            }
        };
        if read != want {
            return Err(Failure::retry(
                RpcError::new(
                    DEVICE_ERROR,
                    format!("set_value read back {read:?}, wanted {want:?}"),
                ),
                "read_back",
            ));
        }
        Ok(Done {
            tool: "set_value".into(),
            text: format!("set [{idx}] to {want:?}; read back {read:?}"),
            value: Some(json!(read)),
            ..Done::default()
        })
    }

    fn pixel_probe(
        &self,
        dev: &DeviceInfo,
        key: &str,
        x: f64,
        y: f64,
        space: Option<&str>,
    ) -> Result<Done, Failure> {
        self.backend()
            .admit(key, &Call::Screenshot { max_width: 720 })
            .map_err(|e| map_admit("pixel_probe", e))?;
        let rgb = self
            .backend()
            .pixel(dev, x, y, space)
            .map_err(|e| map_core("pixel_probe", e))?;
        Ok(Done {
            tool: "pixel_probe".into(),
            text: format!(
                "pixel ({x},{y}) = rgb({},{},{}) #{:02x}{:02x}{:02x}",
                rgb[0], rgb[1], rgb[2], rgb[0], rgb[1], rgb[2]
            ),
            rgb: Some(rgb),
            ..Done::default()
        })
    }

    /// `look` with an element crop, and with `ocr` the crop read as text (the crop never leaves the laptop).
    fn look_element(&self, dev: &DeviceInfo, key: &str, tool: &Tool) -> Result<Done, Failure> {
        let Tool::Look { element, ocr, .. } = tool else {
            return Err(Failure::invalid("not a look"));
        };
        let ocr = ocr.unwrap_or(false);
        let Some(el) = element else {
            return Err(Failure::invalid("look with ocr needs an element to crop"));
        };
        let sel = parse_selector(el).map_err(|e| Failure::invalid(format!("element: {e}")))?;
        self.backend()
            .admit(key, &Call::Look { max_width: 720 })
            .map_err(|e| map_admit("look", e))?;
        let (_, b, class, _) = self.find(dev, key, &sel)?;
        let out = self
            .backend()
            .crop(dev, b, ocr)
            .map_err(|e| map_core("look", e))?;
        if ocr {
            let text = out
                .text
                .ok_or_else(|| Failure::device("the OCR engine returned no text", "device"))?;
            // text only: no image goes back, so no image tokens
            Ok(Done {
                tool: "look".into(),
                text,
                image_bytes: None,
                source: Some("ocr".into()),
                ..Done::default()
            })
        } else {
            Ok(Done {
                tool: "look".into(),
                text: format!("cropped {class} [{},{}][{},{}]", b.l, b.t, b.r, b.b),
                image_bytes: out.image_bytes,
                ..Done::default()
            })
        }
    }
}
