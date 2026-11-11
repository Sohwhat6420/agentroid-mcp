//! `agentroid.call(tool, args)` to a core [`Call`], so the call can go through `gate::check`.
//! Only the 17 core tools exist here; anything else (the job tools, `run_script` itself, a typo)
//! is refused as `invalid`. Argument names follow the tool schemas' snake_case.

use agentroid_core::gate::{valid_prop_key, Call, Target};
use agentroid_core::selector::Selector;
use serde_json::{Map, Value};

type Args = Map<String, Value>;

fn s(a: &Args, k: &str) -> Result<String, String> {
    a.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string `{k}`"))
}
fn opt_s(a: &Args, k: &str) -> Result<Option<String>, String> {
    match a.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(v)) => Ok(Some(v.clone())),
        Some(_) => Err(format!("`{k}` must be a string")),
    }
}
fn num(a: &Args, k: &str) -> Result<f64, String> {
    a.get(k)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("missing number `{k}`"))
}
fn opt_u64(a: &Args, k: &str) -> Result<Option<u64>, String> {
    match a.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("`{k}` must be a non-negative integer")),
    }
}
fn opt_bool(a: &Args, k: &str) -> Result<bool, String> {
    match a.get(k) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("`{k}` must be a boolean")),
    }
}
fn u32_or(a: &Args, k: &str, d: u32) -> Result<u32, String> {
    Ok(opt_u64(a, k)?
        .map(|v| v.min(u32::MAX as u64) as u32)
        .unwrap_or(d))
}
fn i32_or(a: &Args, k: &str, d: i32) -> Result<i32, String> {
    Ok(opt_u64(a, k)?
        .map(|v| v.min(i32::MAX as u64) as i32)
        .unwrap_or(d))
}
fn opt_i32(a: &Args, k: &str) -> Result<Option<i32>, String> {
    Ok(opt_u64(a, k)?.map(|v| v.min(i32::MAX as u64) as i32))
}

pub fn parse_selector(v: &Value) -> Result<Selector, String> {
    let a = v.as_object().ok_or("a selector must be an object")?;
    let rel = match a.get("rel") {
        None | Some(Value::Null) => None,
        Some(Value::Array(p)) if p.len() == 2 => {
            let fx = p[0].as_f64().ok_or("rel must be two numbers")?;
            let fy = p[1].as_f64().ok_or("rel must be two numbers")?;
            Some((fx, fy))
        }
        Some(_) => return Err("rel must be two numbers".into()),
    };
    let sel = Selector {
        ref_id: opt_s(a, "ref")?,
        id: opt_s(a, "id")?,
        text: opt_s(a, "text")?,
        desc: opt_s(a, "desc")?,
        near_text: opt_s(a, "near_text")?,
        rel,
    };
    if sel.is_empty() {
        return Err("empty selector".into());
    }
    Ok(sel)
}

/// `{index}`, `{x, y, space?, image_width?}` or `{select: {...}}`, either under `target` or flat.
fn target(a: &Args) -> Result<Target, String> {
    let t = match a.get("target") {
        Some(Value::Object(o)) => o,
        Some(_) => return Err("`target` must be an object".into()),
        None => a,
    };
    if let Some(i) = opt_u64(t, "index")? {
        return Ok(Target::Index(i as usize));
    }
    if t.contains_key("x") || t.contains_key("y") {
        return Ok(Target::Point {
            x: num(t, "x")?,
            y: num(t, "y")?,
            space: opt_s(t, "space")?,
            image_width: opt_i32(t, "image_width")?,
        });
    }
    if let Some(sel) = t.get("select") {
        return Ok(Target::Select(parse_selector(sel)?));
    }
    Err("a target needs `index`, `x`/`y` or `select`".into())
}

pub fn parse_call(tool: &str, args: &Value) -> Result<Call, String> {
    let empty = Map::new();
    let a = match args {
        Value::Null => &empty,
        Value::Object(o) => o,
        _ => return Err("args must be an object".into()),
    };
    let call = match tool {
        "list_devices" => Call::ListDevices,
        "screenshot" => Call::Screenshot {
            max_width: i32_or(a, "max_width", 720)?,
        },
        "ui_tree" => {
            let require = match a.get("require") {
                None | Some(Value::Null) => vec![],
                Some(Value::Array(v)) => v.iter().map(parse_selector).collect::<Result<_, _>>()?,
                Some(_) => return Err("`require` must be a list of selectors".into()),
            };
            Call::UiTree {
                interactive_only: opt_bool(a, "interactive_only")?,
                require,
                timeout_ms: opt_u64(a, "timeout_ms")?,
            }
        }
        "look" => Call::Look {
            max_width: i32_or(a, "max_width", 720)?,
        },
        "list_apps" => Call::ListApps {
            filter: opt_s(a, "filter")?.unwrap_or_default(),
        },
        "wait" => {
            let seconds = num(a, "seconds")?;
            if !(0.0..=30.0).contains(&seconds) {
                return Err("`seconds` must be within 0..=30".into());
            }
            Call::Wait { seconds }
        }
        "getprop" => {
            let key = s(a, "key")?;
            if !valid_prop_key(&key) {
                return Err(format!("bad property key {key:?}"));
            }
            Call::Getprop { key }
        }
        "tap" => Call::Tap { target: target(a)? },
        "long_press" => Call::LongPress {
            target: target(a)?,
            ms: u32_or(a, "ms", 800)?,
        },
        "swipe" => Call::Swipe {
            x1: num(a, "x1")?,
            y1: num(a, "y1")?,
            x2: num(a, "x2")?,
            y2: num(a, "y2")?,
            ms: u32_or(a, "ms", 300)?,
            space: opt_s(a, "space")?,
            image_width: opt_i32(a, "image_width")?,
        },
        "scroll" => Call::Scroll {
            direction: s(a, "direction")?,
        },
        "type_text" => Call::TypeText {
            text: s(a, "text")?,
            index: opt_u64(a, "index")?.map(|i| i as usize),
            submit: opt_bool(a, "submit")?,
        },
        "key" => Call::Key {
            name: s(a, "name")?,
        },
        "unlock" => Call::Unlock,
        "open_app" => Call::OpenApp {
            package: s(a, "package")?,
        },
        "open_url" => Call::OpenUrl {
            uri: s(a, "uri")?,
            package: opt_s(a, "package")?,
        },
        "start_activity" => Call::StartActivity {
            component: s(a, "component")?,
            action: opt_s(a, "action")?,
            data: opt_s(a, "data")?,
        },
        other => {
            return Err(format!(
                "unknown tool {:?}: run_script can call only the core tools",
                other.chars().take(64).collect::<String>()
            ))
        }
    };
    Ok(call)
}

/// Arguments that parse to each [`Call::samples`] entry's tool (for tests and the daemon's selftest).
pub fn sample_args(tool: &str) -> Value {
    use serde_json::json;
    match tool {
        "wait" => json!({"seconds": 0}),
        "getprop" => json!({"key": "ro.product.brand"}),
        "tap" => json!({"x": 10, "y": 10, "space": "device"}),
        "long_press" => json!({"target": {"index": 0}, "ms": 800}),
        "swipe" => json!({"x1": 1, "y1": 1, "x2": 2, "y2": 2}),
        "scroll" => json!({"direction": "down"}),
        "type_text" => json!({"text": "hi"}),
        "key" => json!({"name": "back"}),
        "open_app" => json!({"package": "com.android.settings"}),
        "open_url" => json!({"uri": "https://example.com"}),
        "start_activity" => json!({"component": "com.android.settings/.Settings"}),
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_core_tool_parses_from_its_sample_args() {
        for c in Call::samples() {
            let parsed = parse_call(c.name(), &sample_args(c.name()))
                .unwrap_or_else(|e| panic!("{}: {e}", c.name()));
            assert_eq!(parsed.name(), c.name());
            assert_eq!(parsed.is_acting(), c.is_acting());
        }
    }

    #[test]
    fn targets() {
        assert_eq!(
            parse_call("tap", &json!({"index": 3})).unwrap(),
            Call::Tap {
                target: Target::Index(3)
            }
        );
        assert_eq!(
            parse_call("tap", &json!({"target": {"select": {"text": "OK"}}})).unwrap(),
            Call::Tap {
                target: Target::Select(Selector::text("OK"))
            }
        );
        assert!(parse_call("tap", &json!({})).is_err());
        assert!(parse_call("tap", &json!({"select": {}})).is_err());
    }

    #[test]
    fn unknown_tools_and_bad_args_are_refused() {
        for t in ["run_script", "job_start", "shell", "adb", ""] {
            assert!(parse_call(t, &json!({})).is_err(), "{t}");
        }
        assert!(parse_call("getprop", &json!({"key": "a;reboot"})).is_err());
        assert!(parse_call("wait", &json!({"seconds": 1e9})).is_err());
        assert!(parse_call("tap", &json!([1, 2])).is_err());
    }
}
