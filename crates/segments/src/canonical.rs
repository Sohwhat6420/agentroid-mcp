//! The canonical byte form of a segment, the bytes the hub signs and the phone verifies.
//!
//! Java twin: `android/runner/src/main/java/dev/agentroid/helper/runner/Canonical.java`. The two must agree byte for
//! byte; `schemas/fixtures/segment/canonical.json` pins the output and both test suites check it.
//!
//! Rules (a strict subset of RFC 8785 JCS, chosen so no float formatting is ever needed):
//! - no whitespace anywhere;
//! - object members sorted by the UTF-8 bytes of their keys; duplicate keys cannot occur (both parsers refuse or merge);
//! - strings: `"` as `\"`, `\` as `\\`, U+0008 `\b`, U+0009 `\t`, U+000A `\n`, U+000C `\f`, U+000D `\r`, every other
//!   U+0000..U+001F as `\u00xx` (lower-case hex); everything else, `/` and non-ASCII included, as raw UTF-8;
//! - numbers: integers in the i64 range only, in plain decimal; any fraction or exponent, or anything outside i64, is
//!   refused (a segment never needs one);
//! - `true`, `false`, `null` as literals.
//!
//! Signed bytes = UTF-8 of [`DOMAIN`] followed by the canonical form of the `segment` object (domain separation, as the
//! link does with `agentroid-link/v1/...`).

use serde_json::Value;
use std::fmt;

/// Prefix of every signed segment message: a domain label and one LF.
pub const DOMAIN: &str = "agentroid-segment/v1\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonError {
    /// A number with a fraction or exponent, or outside i64, at this JSON-pointer-like path.
    NotAnInteger(String),
}

impl fmt::Display for CanonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CanonError::NotAnInteger(p) => {
                write!(f, "{p}: only i64 integers are allowed in a segment")
            }
        }
    }
}

impl std::error::Error for CanonError {}

/// The canonical text of any JSON value.
pub fn to_canonical(v: &Value) -> Result<String, CanonError> {
    let mut out = String::new();
    write(v, "", &mut out)?;
    Ok(out)
}

/// The bytes the hub signs for this `segment` object.
pub fn signed_bytes(segment: &Value) -> Result<Vec<u8>, CanonError> {
    let mut b = DOMAIN.as_bytes().to_vec();
    b.extend_from_slice(to_canonical(segment)?.as_bytes());
    Ok(b)
}

fn write(v: &Value, path: &str, out: &mut String) -> Result<(), CanonError> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => match n.as_i64() {
            Some(i) => out.push_str(&i.to_string()),
            None => {
                return Err(CanonError::NotAnInteger(if path.is_empty() {
                    "/".into()
                } else {
                    path.into()
                }))
            }
        },
        Value::String(s) => string(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(x, &format!("{path}/{i}"), out)?;
            }
            out.push(']');
        }
        Value::Object(m) => {
            // Sorted by UTF-8 bytes regardless of the map's own order (serde_json may preserve insertion order).
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                string(k, out);
                out.push(':');
                write(&m[k.as_str()], &format!("{path}/{k}"), out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_drops_whitespace() {
        let v = json!({"b": 1, "a": [true, null, {"z": "x", "y": -2}], "A": "q"});
        assert_eq!(
            to_canonical(&v).unwrap(),
            r#"{"A":"q","a":[true,null,{"y":-2,"z":"x"}],"b":1}"#
        );
    }

    #[test]
    fn keys_sort_by_utf8_bytes_not_utf16_units() {
        // U+FFFD (EF BF BD) before U+1F600 (F0 9F 98 80) in UTF-8 order; UTF-16 order would put the emoji first.
        let v = json!({"\u{1F600}": "e", "\u{FFFD}": "r", "b": 1, "A": true});
        assert_eq!(
            to_canonical(&v).unwrap(),
            "{\"A\":true,\"b\":1,\"\u{FFFD}\":\"r\",\"\u{1F600}\":\"e\"}"
        );
    }

    #[test]
    fn escapes_exactly_the_jcs_set() {
        let v = json!("q\"b\\s/\u{8}\t\n\u{c}\r\u{1}\u{1f}é☕😀");
        assert_eq!(
            to_canonical(&v).unwrap(),
            "\"q\\\"b\\\\s/\\b\\t\\n\\f\\r\\u0001\\u001fé☕😀\""
        );
    }

    #[test]
    fn refuses_floats_and_huge_numbers() {
        assert!(to_canonical(&json!({"a": 1.5})).is_err());
        assert!(to_canonical(&serde_json::from_str::<Value>("{\"a\":3.0}").unwrap()).is_err());
        assert!(to_canonical(&json!({"a": u64::MAX})).is_err());
        assert_eq!(
            to_canonical(&json!(i64::MIN)).unwrap(),
            i64::MIN.to_string()
        );
    }

    #[test]
    fn signed_bytes_carry_the_domain() {
        let b = signed_bytes(&json!({"a": 1})).unwrap();
        assert_eq!(b, b"agentroid-segment/v1\n{\"a\":1}".to_vec());
    }
}
