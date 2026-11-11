//! What counts as a clash on the keep-working path (rounds 40 and 41).
//! A clash pauses the job ("You win, job pauses"). Package lists are data handed in by the caller
//! (the banking hand list, Play `FINANCE` packages, camera apps, the S1 per-app deny list), never
//! names written in this code.

use std::collections::HashMap;
use std::fmt;

/// Why the job cannot keep working on the virtual display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clash {
    /// The app refuses a secondary display (S1's per-app deny list).
    RefusesSecondaryDisplay { package: String },
    /// The job's app shows a secure (`FLAG_SECURE`) window: it would be black on the hidden screen.
    SecureWindow { package: String },
    /// The same app is in front of the human on display 0.
    SameAppOnDisplay0 { package: String },
    /// A camera app (the camera cannot be shared with the human's screen).
    Camera { package: String },
    /// A banking or payment app (round 36: never on the hidden screen).
    Banking { package: String },
    /// The job's task left the virtual display (for example the human opened the app, so Android
    /// brought the task back to display 0).
    TaskLeftVirtualDisplay { package: String },
}

impl fmt::Display for Clash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Clash::RefusesSecondaryDisplay { package } => {
                write!(f, "{package} refuses a secondary display")
            }
            Clash::SecureWindow { package } => write!(f, "{package} shows a secure window"),
            Clash::SameAppOnDisplay0 { package } => {
                write!(f, "{package} is open on the main screen")
            }
            Clash::Camera { package } => write!(f, "{package} is a camera app"),
            Clash::Banking { package } => write!(f, "{package} is a banking or payment app"),
            Clash::TaskLeftVirtualDisplay { package } => {
                write!(f, "{package} left the hidden screen")
            }
        }
    }
}

/// The package lists, all data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClashPolicy {
    pub refuses_secondary: Vec<String>,
    pub camera: Vec<String>,
    pub banking: Vec<String>,
}

/// One window flagged `SECURE` in `dumpsys window windows`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureWindow {
    pub package: String,
    pub display: u32,
}

fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.split_whitespace().find_map(|w| w.strip_prefix(name))
}

/// `Window{8d3f1a2 u0 com.example/com.example.Main}` -> `com.example`.
fn window_package(head: &str) -> Option<String> {
    let inner = head.split_once("Window{")?.1.split_once('}')?.0;
    let last = inner.split_whitespace().nth(2)?;
    Some(last.split('/').next()?.to_string())
}

/// Every secure window, from `dumpsys window windows`.
pub fn parse_secure_windows(text: &str) -> Vec<SecureWindow> {
    let mut out = Vec::new();
    let mut cur: Option<(String, u32, bool)> = None;
    fn flush(c: Option<(String, u32, bool)>, out: &mut Vec<SecureWindow>) {
        if let Some((package, display, true)) = c {
            out.push(SecureWindow { package, display });
        }
    }
    for raw in text.lines() {
        let t = raw.trim();
        if t.starts_with("Window #") {
            flush(cur.take(), &mut out);
            cur = window_package(t).map(|p| (p, 0, false));
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        if let Some(d) = field(t, "mDisplayId=") {
            c.1 = d.parse().unwrap_or(0);
        }
        if let Some(p) = field(t, "package=") {
            c.0 = p.to_string();
        }
        if let Some(fl) = t.strip_prefix("fl=") {
            if fl.split_whitespace().any(|w| w == "SECURE") {
                c.2 = true;
            }
        }
    }
    flush(cur.take(), &mut out);
    out
}

/// `ActivityRecord{c2d7e10 u0 com.android.settings/.Settings t23}` -> `com.android.settings`.
fn record_package(line: &str) -> Option<String> {
    let inner = line.split_once("ActivityRecord{")?.1.split_once('}')?.0;
    let comp = inner.split_whitespace().nth(2)?;
    Some(comp.split('/').next()?.to_string())
}

/// The resumed package per display, from `dumpsys activity activities`.
pub fn parse_resumed(text: &str) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    let mut display: Option<u32> = None;
    for raw in text.lines() {
        let t = raw.trim();
        if let Some(rest) = t.strip_prefix("Display #") {
            display = rest
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|n| n.parse().ok());
            continue;
        }
        if t.starts_with("ResumedActivity:") || t.starts_with("mResumedActivity:") {
            if let (Some(d), Some(p)) = (display, record_package(t)) {
                out.entry(d).or_insert(p);
            }
        }
    }
    out
}

/// Clash rules that depend only on the package and the policy.
pub fn check_package(package: &str, policy: &ClashPolicy) -> Result<(), Clash> {
    let has = |l: &[String]| l.iter().any(|p| p == package);
    let package = package.to_string();
    if has(&policy.banking) {
        return Err(Clash::Banking { package });
    }
    if has(&policy.camera) {
        return Err(Clash::Camera { package });
    }
    if has(&policy.refuses_secondary) {
        return Err(Clash::RefusesSecondaryDisplay { package });
    }
    Ok(())
}

/// The secure-window rule: any secure window of the job's app, on any display.
pub fn check_secure(package: &str, secure: &[SecureWindow]) -> Result<(), Clash> {
    if secure.iter().any(|w| w.package == package) {
        return Err(Clash::SecureWindow {
            package: package.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_rules_come_from_the_policy_only() {
        let p = ClashPolicy {
            banking: vec!["b.bank".into()],
            camera: vec!["c.cam".into()],
            refuses_secondary: vec!["r.fixed".into()],
        };
        assert_eq!(
            check_package("b.bank", &p),
            Err(Clash::Banking {
                package: "b.bank".into()
            })
        );
        assert_eq!(
            check_package("c.cam", &p),
            Err(Clash::Camera {
                package: "c.cam".into()
            })
        );
        assert_eq!(
            check_package("r.fixed", &p),
            Err(Clash::RefusesSecondaryDisplay {
                package: "r.fixed".into()
            })
        );
        assert_eq!(check_package("x.other", &p), Ok(()));
        assert_eq!(check_package("b.bank", &ClashPolicy::default()), Ok(()));
    }
}
