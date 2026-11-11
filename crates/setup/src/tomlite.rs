//! A small TOML reader for the two files this crate owns (`quirks.toml`, `devices.toml`).
//!
//! Supported: `#` comments, `[a.b."c"]` tables, `[[a]]` arrays of tables, `key = value` with bare or
//! quoted keys, basic and literal strings, integers, booleans and arrays (which may span lines).
//! Anything else (inline tables, dates, floats, multi-line strings) is an error, never a guess.
//! ASSUMPTION: a full TOML crate would add five crates to the lock; these files are ours, so the
//! subset is enough. Writing is done by hand in `setup.rs` (text appended or one section replaced).

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Array(Vec<Value>),
    Table(BTreeMap<String, Value>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_table(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_table()?.get(key)
    }
    /// An array of strings, or `None` if any item is not a string.
    pub fn str_list(&self) -> Option<Vec<String>> {
        self.as_array()?
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TomlError(pub String);

impl std::fmt::Display for TomlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "toml: {}", self.0)
    }
}

impl std::error::Error for TomlError {}

fn err<T>(line: usize, msg: impl std::fmt::Display) -> Result<T, TomlError> {
    Err(TomlError(format!("line {line}: {msg}")))
}

/// Drop a trailing comment that is outside any string.
fn strip_comment(l: &str) -> &str {
    let mut in_basic = false;
    let mut in_literal = false;
    let mut escaped = false;
    for (i, c) in l.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_basic => escaped = true,
            '"' if !in_literal => in_basic = !in_basic,
            '\'' if !in_basic => in_literal = !in_literal,
            '#' if !in_basic && !in_literal => return &l[..i],
            _ => {}
        }
    }
    l
}

/// Split a dotted key path; parts may be quoted.
fn key_path(s: &str, line: usize) -> Result<Vec<String>, TomlError> {
    let mut parts = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let (part, tail) = if let Some(r) = rest.strip_prefix('"') {
            let (v, used) = basic_string(r, line)?;
            (v, &r[used..])
        } else if let Some(r) = rest.strip_prefix('\'') {
            let end = r
                .find('\'')
                .ok_or_else(|| TomlError(format!("line {line}: unterminated key")))?;
            (r[..end].to_string(), &r[end + 1..])
        } else {
            let end = rest.find('.').unwrap_or(rest.len());
            let k = rest[..end].trim();
            if k.is_empty()
                || !k
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return err(line, format!("bad key {k:?}"));
            }
            (k.to_string(), &rest[end..])
        };
        parts.push(part);
        let tail = tail.trim_start();
        rest = match tail.strip_prefix('.') {
            Some(t) => t.trim_start(),
            None if tail.is_empty() => "",
            None => return err(line, format!("junk after key: {tail:?}")),
        };
    }
    if parts.is_empty() {
        return err(line, "empty key");
    }
    Ok(parts)
}

/// Parse a basic string body (after the opening quote). Returns the value and the bytes consumed
/// including the closing quote.
fn basic_string(s: &str, line: usize) -> Result<(String, usize), TomlError> {
    let mut out = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Ok((out, i + 1)),
            '\\' => match chars.next() {
                Some((_, 'n')) => out.push('\n'),
                Some((_, 't')) => out.push('\t'),
                Some((_, 'r')) => out.push('\r'),
                Some((_, '"')) => out.push('"'),
                Some((_, '\\')) => out.push('\\'),
                other => return err(line, format!("unsupported escape {other:?}")),
            },
            '\n' => return err(line, "newline in string"),
            c => out.push(c),
        }
    }
    err(line, "unterminated string")
}

/// Parse one value at the start of `s`; returns it and the unparsed rest.
fn value(s: &str, line: usize) -> Result<(Value, &str), TomlError> {
    let s = s.trim_start();
    if let Some(r) = s.strip_prefix('"') {
        if r.starts_with("\"\"") {
            return err(line, "multi-line strings are not supported");
        }
        let (v, used) = basic_string(r, line)?;
        return Ok((Value::Str(v), &r[used..]));
    }
    if let Some(r) = s.strip_prefix('\'') {
        let end = r
            .find('\'')
            .ok_or_else(|| TomlError(format!("line {line}: unterminated literal string")))?;
        return Ok((Value::Str(r[..end].to_string()), &r[end + 1..]));
    }
    if let Some(mut r) = s.strip_prefix('[') {
        let mut items = Vec::new();
        loop {
            r = r.trim_start();
            if let Some(t) = r.strip_prefix(']') {
                return Ok((Value::Array(items), t));
            }
            let (v, t) = value(r, line)?;
            items.push(v);
            let t = t.trim_start();
            r = if let Some(t) = t.strip_prefix(',') {
                t
            } else if t.starts_with(']') {
                t
            } else {
                return err(line, format!("expected , or ] in array, found {t:?}"));
            };
        }
    }
    if s.starts_with('{') {
        return err(line, "inline tables are not supported");
    }
    let end = s
        .find(|c: char| c == ',' || c == ']' || c.is_whitespace())
        .unwrap_or(s.len());
    let tok = &s[..end];
    let v = match tok {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => match tok.replace('_', "").parse::<i64>() {
            Ok(i) if !tok.is_empty() => Value::Int(i),
            _ => return err(line, format!("unsupported value {tok:?}")),
        },
    };
    Ok((v, &s[end..]))
}

fn table_at<'a>(
    root: &'a mut BTreeMap<String, Value>,
    path: &[String],
    line: usize,
) -> Result<&'a mut BTreeMap<String, Value>, TomlError> {
    let mut cur = root;
    for p in path {
        let entry = cur
            .entry(p.clone())
            .or_insert_with(|| Value::Table(BTreeMap::new()));
        cur = match entry {
            Value::Table(t) => t,
            Value::Array(a) => match a.last_mut() {
                Some(Value::Table(t)) => t,
                _ => return err(line, format!("{p} is not a table")),
            },
            _ => return err(line, format!("{p} is not a table")),
        };
    }
    Ok(cur)
}

/// Parse a document into its root table.
pub fn parse(text: &str) -> Result<Value, TomlError> {
    let mut root: BTreeMap<String, Value> = BTreeMap::new();
    let mut current: Vec<String> = Vec::new();
    let mut seen_tables: Vec<Vec<String>> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let lineno = i + 1;
        let mut l = strip_comment(lines[i]).trim().to_string();
        i += 1;
        if l.is_empty() {
            continue;
        }
        if let Some(inner) = l.strip_prefix("[[").and_then(|r| r.strip_suffix("]]")) {
            let path = key_path(inner, lineno)?;
            let (last, parent) = path.split_last().expect("non-empty");
            let t = table_at(&mut root, parent, lineno)?;
            match t
                .entry(last.clone())
                .or_insert_with(|| Value::Array(Vec::new()))
            {
                Value::Array(a) => a.push(Value::Table(BTreeMap::new())),
                _ => return err(lineno, format!("{last} is not an array of tables")),
            }
            current = path;
            continue;
        }
        if let Some(inner) = l.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            let path = key_path(inner, lineno)?;
            if seen_tables.contains(&path) {
                return err(lineno, format!("table {inner} defined twice"));
            }
            seen_tables.push(path.clone());
            table_at(&mut root, &path, lineno)?;
            current = path;
            continue;
        }
        // an array value may continue over several lines: join until the brackets balance
        let depth = |s: &str| {
            let mut d = 0i32;
            let (mut inb, mut inl) = (false, false);
            let mut esc = false;
            for c in s.chars() {
                match c {
                    _ if esc => esc = false,
                    '\\' if inb => esc = true,
                    '"' if !inl => inb = !inb,
                    '\'' if !inb => inl = !inl,
                    '[' if !inb && !inl => d += 1,
                    ']' if !inb && !inl => d -= 1,
                    _ => {}
                }
            }
            d
        };
        while depth(&l) > 0 && i < lines.len() {
            l.push(' ');
            l.push_str(strip_comment(lines[i]).trim());
            i += 1;
        }
        let Some((k, v)) = l.split_once('=') else {
            return err(lineno, format!("expected key = value, found {l:?}"));
        };
        let path = key_path(k, lineno)?;
        let (val, rest) = value(v, lineno)?;
        if !rest.trim().is_empty() {
            return err(lineno, format!("junk after value: {:?}", rest.trim()));
        }
        let (last, parent) = path.split_last().expect("non-empty");
        let mut full = current.clone();
        full.extend_from_slice(parent);
        let t = table_at(&mut root, &full, lineno)?;
        if t.insert(last.clone(), val).is_some() {
            return err(lineno, format!("key {last} defined twice"));
        }
    }
    Ok(Value::Table(root))
}

/// Quote a string as a TOML basic string.
pub fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_subset() {
        let doc = r#"
# comment
schema = 1
[devices."ABC-123"]
name = "POCO \"X6\" Pro" # trailing
mode = 'full'
sdk = 35
ok = true
list = ["a", "b",
  "c"]

[[quirk]]
id = "one"
[[quirk]]
id = "two"
ops = []
"#;
        let v = parse(doc).unwrap();
        assert_eq!(v.get("schema").unwrap().as_int(), Some(1));
        let d = v.get("devices").unwrap().get("ABC-123").unwrap();
        assert_eq!(d.get("name").unwrap().as_str(), Some("POCO \"X6\" Pro"));
        assert_eq!(d.get("mode").unwrap().as_str(), Some("full"));
        assert_eq!(d.get("sdk").unwrap().as_int(), Some(35));
        assert_eq!(d.get("ok").unwrap().as_bool(), Some(true));
        assert_eq!(d.get("list").unwrap().str_list().unwrap(), ["a", "b", "c"]);
        let q = v.get("quirk").unwrap().as_array().unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[1].get("id").unwrap().as_str(), Some("two"));
        assert_eq!(
            q[1].get("ops").unwrap().str_list().unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn refuses_what_it_does_not_know() {
        for bad in [
            "a = {x = 1}",
            "a = 1.5",
            "a = \"\"\"x\"\"\"",
            "a = 1\na = 2",
            "[t]\n[t]",
            "a = \"x",
            "= 1",
            "a = 1 2",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn quote_round_trips() {
        for s in ["plain", "with \"quotes\"", "back\\slash", "tab\there"] {
            let v = parse(&format!("k = {}", quote(s))).unwrap();
            assert_eq!(v.get("k").unwrap().as_str(), Some(s));
        }
    }
}
