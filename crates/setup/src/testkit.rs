//! Test doubles. `Recorded` answers from a fixture exactly as written. `FakePhone` starts from a
//! fixture and keeps state, so setup's writes and undo's reverts change what later reads return.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use agentroid_adb_link::runner::{RunError, RunOutput, Runner};

use crate::device::{self, HELPER_PKG, WSS};

pub fn parse_fixture(text: &str) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut cur: Option<String> = None;
    for line in text.lines() {
        if let Some(cmd) = line.strip_prefix("### ") {
            map.insert(cmd.trim().to_string(), String::new());
            cur = Some(cmd.trim().to_string());
        } else if let Some(c) = &cur {
            let e = map.get_mut(c).unwrap();
            e.push_str(line);
            e.push('\n');
        }
    }
    map
}

fn ok(s: impl Into<String>) -> Result<RunOutput, RunError> {
    Ok(RunOutput {
        code: Some(0),
        stdout: s.into(),
        stderr: String::new(),
    })
}

fn fail(msg: &str) -> Result<RunOutput, RunError> {
    Ok(RunOutput {
        code: Some(1),
        stdout: String::new(),
        stderr: msg.into(),
    })
}

/// The shell line of an argv (`-s T shell <line>`), or the argv joined for server commands.
pub fn line_of(a: &[String]) -> String {
    if a.len() == 4 && a[0] == "-s" && a[2] == "shell" {
        a[3].clone()
    } else {
        a.join(" ")
    }
}

/// Static fixture runner. `serial` overrides `getprop ro.serialno` (for the protected phone tests).
pub struct Recorded {
    pub sections: HashMap<String, String>,
    pub serial: Option<String>,
    pub calls: RefCell<Vec<Vec<String>>>,
}

impl Recorded {
    pub fn new(fixture: &str) -> Self {
        Recorded {
            sections: parse_fixture(fixture),
            serial: None,
            calls: RefCell::new(vec![]),
        }
    }
    pub fn lines(&self) -> Vec<String> {
        self.calls.borrow().iter().map(|a| line_of(a)).collect()
    }
}

impl Runner for Recorded {
    fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
        self.calls.borrow_mut().push(a.to_vec());
        let l = line_of(a);
        if l == "getprop ro.serialno" {
            if let Some(s) = &self.serial {
                return ok(format!("{s}\n"));
            }
        }
        match self.sections.get(&l) {
            Some(out) => ok(out.clone()),
            None => fail("not found"),
        }
    }
}

/// Split a shell line into words, honouring single quotes (the only quoting setup emits).
fn words(line: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut in_q = false;
    let mut esc = false;
    for c in line.chars() {
        if esc {
            cur.push(c);
            have = true;
            esc = false;
            continue;
        }
        match c {
            '\\' if !in_q => esc = true,
            '\'' => {
                in_q = !in_q;
                have = true;
            }
            ' ' if !in_q => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if in_q {
        return None;
    }
    if have {
        out.push(cur);
    }
    Some(out)
}

/// What the phone holds that setup may change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneState {
    pub settings: BTreeMap<String, BTreeMap<String, String>>,
    pub whitelist: BTreeSet<String>,
    pub appops: BTreeMap<String, String>,
    pub wss: bool,
}

pub struct FakePhone {
    pub sections: HashMap<String, String>,
    pub state: RefCell<PhoneState>,
    pub calls: RefCell<Vec<Vec<String>>>,
    /// Keys whose writes the phone silently drops (Android 13+ restricted settings).
    pub drop_keys: RefCell<BTreeSet<String>>,
}

pub const WRITE_PREFIXES: [&str; 6] = [
    "settings put ",
    "settings delete ",
    "dumpsys deviceidle whitelist ",
    "cmd appops set ",
    "pm grant ",
    "pm revoke ",
];

impl FakePhone {
    pub fn new(fixture: &str) -> Self {
        let sections = parse_fixture(fixture);
        let mut settings = BTreeMap::new();
        for ns in ["global", "secure", "system"] {
            settings.insert(
                ns.to_string(),
                device::settings_map(
                    sections
                        .get(&format!("settings list {ns}"))
                        .map_or("", |s| s),
                ),
            );
        }
        let whitelist =
            device::whitelist_user(sections.get("dumpsys deviceidle").map_or("", |s| s));
        let appops = device::appops_map(
            sections
                .get(&format!("cmd appops get {HELPER_PKG}"))
                .map_or("", |s| s),
        );
        let wss = device::wss_granted(
            sections
                .get(&format!("dumpsys package {HELPER_PKG}"))
                .map_or("", |s| s),
        );
        FakePhone {
            sections,
            state: RefCell::new(PhoneState {
                settings,
                whitelist,
                appops,
                wss,
            }),
            calls: RefCell::new(vec![]),
            drop_keys: RefCell::new(BTreeSet::new()),
        }
    }

    pub fn snapshot(&self) -> PhoneState {
        self.state.borrow().clone()
    }

    pub fn lines(&self) -> Vec<String> {
        self.calls.borrow().iter().map(|a| line_of(a)).collect()
    }

    pub fn writes(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| WRITE_PREFIXES.iter().any(|p| l.starts_with(p)))
            .collect()
    }

    fn deviceidle(&self) -> String {
        let base = self
            .sections
            .get("dumpsys deviceidle")
            .cloned()
            .unwrap_or_default();
        let mut out = String::new();
        let mut skipping = false;
        for l in base.lines() {
            if skipping {
                if l.starts_with("    ") {
                    continue;
                }
                skipping = false;
            }
            out.push_str(l);
            out.push('\n');
            if l.trim() == "Whitelist user apps:" {
                for p in &self.state.borrow().whitelist {
                    out.push_str(&format!("    {p}\n"));
                }
                skipping = true;
            }
        }
        out
    }

    fn appops_out(&self) -> String {
        self.state
            .borrow()
            .appops
            .iter()
            .map(|(k, v)| format!("{k}: {v}\n"))
            .collect()
    }

    fn package_out(&self) -> String {
        let mut s = format!(
            "Packages:\n  Package [{HELPER_PKG}] (7d1c0e2):\n    userId=10301\n    install permissions:\n      android.permission.INTERNET: granted=true\n"
        );
        if self.state.borrow().wss {
            s.push_str(&format!("      {WSS}: granted=true\n"));
        }
        s
    }

    fn write(&self, line: &str) -> Result<RunOutput, RunError> {
        let Some(w) = words(line) else {
            return fail("bad quoting");
        };
        let w: Vec<&str> = w.iter().map(String::as_str).collect();
        let mut st = self.state.borrow_mut();
        match w.as_slice() {
            ["settings", "put", ns, key, value] => {
                if self.drop_keys.borrow().contains(*key) {
                    return ok("");
                }
                st.settings
                    .entry(ns.to_string())
                    .or_default()
                    .insert(key.to_string(), value.to_string());
            }
            ["settings", "delete", ns, key] => {
                st.settings.entry(ns.to_string()).or_default().remove(*key);
            }
            ["dumpsys", "deviceidle", "whitelist", arg] => {
                if let Some(p) = arg.strip_prefix('+') {
                    st.whitelist.insert(p.to_string());
                } else if let Some(p) = arg.strip_prefix('-') {
                    st.whitelist.remove(p);
                } else {
                    return fail("bad whitelist arg");
                }
            }
            ["cmd", "appops", "set", pkg, op, mode] if *pkg == HELPER_PKG => {
                let key = if op.chars().all(|c| c.is_ascii_digit()) {
                    format!("MIUIOP({op})")
                } else {
                    op.to_string()
                };
                if *mode == "default" {
                    st.appops.remove(&key);
                } else {
                    st.appops.insert(key, mode.to_string());
                }
            }
            ["pm", "grant", pkg, perm] if *pkg == HELPER_PKG && *perm == WSS => st.wss = true,
            ["pm", "revoke", pkg, perm] if *pkg == HELPER_PKG && *perm == WSS => st.wss = false,
            _ => {
                return Ok(RunOutput {
                    code: Some(255),
                    stdout: String::new(),
                    stderr: format!("fake: unknown write {line}"),
                })
            }
        }
        ok("")
    }
}

impl Runner for FakePhone {
    fn run(&self, a: &[String], _t: u64) -> Result<RunOutput, RunError> {
        self.calls.borrow_mut().push(a.to_vec());
        let l = line_of(a);
        if WRITE_PREFIXES.iter().any(|p| l.starts_with(p)) {
            return self.write(&l);
        }
        if let Some(ns) = l.strip_prefix("settings list ") {
            let st = self.state.borrow();
            return match st.settings.get(ns) {
                Some(m) => ok(m
                    .iter()
                    .map(|(k, v)| format!("{k}={v}\n"))
                    .collect::<String>()),
                None => fail("bad namespace"),
            };
        }
        if l == "dumpsys deviceidle" {
            return ok(self.deviceidle());
        }
        if l == format!("cmd appops get {HELPER_PKG}") {
            return ok(self.appops_out());
        }
        if l == format!("dumpsys package {HELPER_PKG}") {
            return ok(self.package_out());
        }
        match self.sections.get(&l) {
            Some(out) => ok(out.clone()),
            None => fail("not found"),
        }
    }
}

/// A `Runner` over a shared phone, so a test prompter can act on the phone mid-setup.
pub struct Shared(pub Rc<FakePhone>);

impl Runner for Shared {
    fn run(&self, a: &[String], t: u64) -> Result<RunOutput, RunError> {
        self.0.run(a, t)
    }
}

#[test]
fn words_undo_sh_quote() {
    for v in ["plain", "a'b", "a b", "x:y/z", "''"] {
        let line = format!("settings put secure k {}", device::sh_quote(v));
        assert_eq!(words(&line).unwrap().last().unwrap(), v, "{v}");
    }
}
