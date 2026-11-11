//! JSON lines between the parent and the child (one object per line, `t` is the tag).
//!
//! ```text
//! parent -> child  {"t":"start","script":"...","limits":{...}}          (once, after job assignment)
//! child  -> parent {"t":"hello","in_job":true}
//! child  -> parent {"t":"call","id":1,"tool":"tap","args":{...}}        (blocks for the reply)
//! child  -> parent {"t":"shell","id":2,"cmd":"getprop ro.serialno"}
//! parent -> child  {"t":"reply","id":1,"ok":false,"error":"refused: ..."}
//! child  -> parent {"t":"log","line":"..."}
//! child  -> parent {"t":"done","ok":true,"value":"<json text>"}       (then the child exits)
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildLimits {
    pub js_mem_bytes: usize,
    pub cpu_ms: u64,
    pub max_sleep_ms: u64,
    pub max_stdout_lines: usize,
    pub max_line_chars: usize,
    pub max_value_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToChild {
    Start {
        script: String,
        limits: ChildLimits,
    },
    Reply {
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromChild {
    Hello {
        in_job: bool,
    },
    Call {
        id: u64,
        tool: String,
        args: Value,
    },
    Shell {
        id: u64,
        cmd: String,
    },
    Log {
        line: String,
    },
    /// `kind` on failure: `exception`, `memory`, `cpu_deadline`, `sandbox`. `value` is JSON text
    /// (absent for `undefined`).
    Done {
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_single_lines() {
        let msgs = vec![
            FromChild::Hello { in_job: true },
            FromChild::Call {
                id: 1,
                tool: "tap".into(),
                args: serde_json::json!({"x": 1, "y": "a\nb"}),
            },
            FromChild::Shell {
                id: 2,
                cmd: "getprop a\nreboot".into(),
            },
            FromChild::Log {
                line: "x\ny".into(),
            },
            FromChild::Done {
                ok: false,
                kind: Some("memory".into()),
                value: None,
                error: Some("out of memory".into()),
            },
        ];
        for m in msgs {
            let s = serde_json::to_string(&m).unwrap();
            assert!(!s.contains('\n'), "{s}");
            assert_eq!(serde_json::from_str::<FromChild>(&s).unwrap(), m);
        }
        let r = ToChild::Reply {
            id: 3,
            ok: true,
            value: Some(serde_json::json!({"a": [1]})),
            error: None,
        };
        assert_eq!(
            serde_json::from_str::<ToChild>(&serde_json::to_string(&r).unwrap()).unwrap(),
            r
        );
    }
}
