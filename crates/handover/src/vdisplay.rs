//! The scrcpy virtual-display handover, as pure logic: the scrcpy argv, the display id parse, the
//! lifecycle, `am stack list` parsing, which task moves, and the clash checks before and during the
//! keep-working path. Nothing here starts a process or talks to a phone.

use std::collections::HashMap;

use crate::clash::{check_package, check_secure, Clash, ClashPolicy, SecureWindow};

/// The scrcpy command used for the virtual display, plus `--no-window` so nothing ever opens on
/// the user's screen (whether the window-less flag works on 5.0.1 is checked in S1; until then this argv
/// is not run). Destroy-content-on-removal stays on (scrcpy's default): when the display goes, the
/// apps on it close instead of jumping onto the human's screen.
pub fn scrcpy_argv(serial: &str, size: &str, start_app: &str) -> Vec<String> {
    [
        "-s",
        serial,
        &format!("--new-display={size}"),
        "--display-ime-policy=local",
        "--no-vd-system-decorations",
        &format!("--start-app={start_app}"),
        "--no-audio",
        "--no-clipboard-autosync",
        "--max-fps=10",
        "--no-window",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `[server] INFO: New display: 1220x2712/480 (id=2)` -> 2.
pub fn parse_new_display(line: &str) -> Option<u32> {
    let rest = line.split_once("New display:")?.1;
    let id = rest.split_once("(id=")?.1.split_once(')')?.0;
    id.trim().parse().ok()
}

/// The virtual display's lifecycle, driven by scrcpy's output and a health check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lifecycle {
    Spawning,
    Up { id: u32 },
    Failed { why: String },
    Down,
}

impl Lifecycle {
    /// One line of scrcpy's output.
    pub fn on_line(&mut self, line: &str) {
        if let Some(id) = parse_new_display(line) {
            if *self == Lifecycle::Spawning {
                *self = Lifecycle::Up { id };
            }
        } else if line.contains("ERROR:")
            && matches!(self, Lifecycle::Spawning | Lifecycle::Up { .. })
        {
            *self = Lifecycle::Failed {
                why: line.trim().to_string(),
            };
        }
    }

    /// The display ids the phone reports now (from `dumpsys display`): an Up display that is gone
    /// has failed.
    pub fn health(&mut self, present: &[u32]) {
        if let Lifecycle::Up { id } = *self {
            if !present.contains(&id) {
                *self = Lifecycle::Failed {
                    why: format!("display {id} is gone"),
                };
            }
        }
    }

    /// Teardown (the scrcpy process is stopped by its owner).
    pub fn teardown(&mut self) {
        *self = Lifecycle::Down;
    }

    pub fn id(&self) -> Option<u32> {
        match self {
            Lifecycle::Up { id } => Some(*id),
            _ => None,
        }
    }
}

/// One task from `am stack list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub task_id: u32,
    pub root_task_id: u32,
    pub display: u32,
    pub package: String,
    pub visible: bool,
}

fn num_after(s: &str, key: &str) -> Option<u32> {
    let rest = s.split_once(key)?.1;
    rest.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

/// Parse `am stack list` (Android 10-11 `Stack id=`, 12+ `RootTask id=`).
pub fn parse_stack_list(text: &str) -> Vec<Task> {
    let mut out = Vec::new();
    let mut root: Option<(u32, u32)> = None;
    for raw in text.lines() {
        let t = raw.trim();
        if t.starts_with("Stack id=") || t.starts_with("RootTask id=") {
            root = match (num_after(t, "id="), num_after(t, "displayId=")) {
                (Some(r), Some(d)) => Some((r, d)),
                _ => None,
            };
            continue;
        }
        let (Some((root_task_id, display)), Some(rest)) = (root, t.strip_prefix("taskId=")) else {
            continue;
        };
        let Some((id, after)) = rest.split_once(':') else {
            continue;
        };
        let Ok(task_id) = id.trim().parse() else {
            continue;
        };
        let Some(comp) = after.split_whitespace().next() else {
            continue;
        };
        let package = comp.split('/').next().unwrap_or("").to_string();
        out.push(Task {
            task_id,
            root_task_id,
            display,
            package,
            visible: t.contains("visible=true"),
        });
    }
    out
}

/// What the phone looks like at the moment of the handover.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scene {
    pub tasks: Vec<Task>,
    pub secure: Vec<SecureWindow>,
    /// Resumed package per display (`dumpsys activity activities`).
    pub resumed: HashMap<u32, String>,
}

/// One `am display move-stack`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub root_task_id: u32,
    pub package: String,
    pub from: u32,
    pub to: u32,
}

impl Move {
    /// The shell words (after `adb -s <serial> shell`).
    pub fn command(&self) -> Vec<String> {
        vec![
            "am".into(),
            "display".into(),
            "move-stack".into(),
            self.root_task_id.to_string(),
            self.to.to_string(),
        ]
    }
}

/// The handover: which root tasks move to the virtual display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub vd_id: u32,
    pub moves: Vec<Move>,
}

/// Plan the keep-working handover for the job's app. Moves the job's own root task(s) from display
/// 0 to the virtual display; the human's apps never move. Clashes (each pauses the job): the app
/// is on the banking, camera or refuses-a-secondary-display list; it shows a secure window; or the
/// human has another task of the same app on display 0 (`job_task` names the job's own task when
/// known; without it, the topmost visible task of the package on display 0 is taken as the job's).
pub fn plan_handover(
    job_pkg: &str,
    job_task: Option<u32>,
    vd_id: u32,
    scene: &Scene,
    policy: &ClashPolicy,
) -> Result<Plan, Clash> {
    check_package(job_pkg, policy)?;
    check_secure(job_pkg, &scene.secure)?;
    let on_d0: Vec<&Task> = scene
        .tasks
        .iter()
        .filter(|t| t.display == 0 && t.package == job_pkg)
        .collect();
    let mine: Option<u32> = job_task.or_else(|| {
        on_d0
            .iter()
            .find(|t| t.visible)
            .or(on_d0.first())
            .map(|t| t.task_id)
    });
    if on_d0.iter().any(|t| Some(t.task_id) != mine) {
        return Err(Clash::SameAppOnDisplay0 {
            package: job_pkg.to_string(),
        });
    }
    let mut moves: Vec<Move> = Vec::new();
    for t in on_d0 {
        if !moves.iter().any(|m| m.root_task_id == t.root_task_id) {
            moves.push(Move {
                root_task_id: t.root_task_id,
                package: t.package.clone(),
                from: 0,
                to: vd_id,
            });
        }
    }
    Ok(Plan { vd_id, moves })
}

/// While the job works out of sight: the human opened the same app on display 0, the job's task
/// left the virtual display, or a secure window appeared.
pub fn check_ongoing(job_pkg: &str, vd_id: u32, scene: &Scene) -> Result<(), Clash> {
    if scene.resumed.get(&0).is_some_and(|p| p == job_pkg) {
        return Err(Clash::SameAppOnDisplay0 {
            package: job_pkg.to_string(),
        });
    }
    check_secure(job_pkg, &scene.secure)?;
    if !scene
        .tasks
        .iter()
        .any(|t| t.display == vd_id && t.package == job_pkg)
    {
        return Err(Clash::TaskLeftVirtualDisplay {
            package: job_pkg.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_display_id() {
        assert_eq!(
            parse_new_display("[server] INFO: New display: 1220x2712/480 (id=2)"),
            Some(2)
        );
        assert_eq!(
            parse_new_display("INFO: New display: 1220x2712/480 (id=17)"),
            Some(17)
        );
        assert_eq!(parse_new_display("INFO: Device: phone"), None);
    }

    #[test]
    fn lifecycle_up_failed_down() {
        let mut l = Lifecycle::Spawning;
        l.on_line("[server] INFO: New display: 1220x2712/480 (id=2)");
        assert_eq!(l.id(), Some(2));
        l.health(&[0, 2]);
        assert_eq!(l, Lifecycle::Up { id: 2 });
        l.health(&[0]);
        assert!(matches!(l, Lifecycle::Failed { .. }));
        l.teardown();
        assert_eq!(l, Lifecycle::Down);
        let mut e = Lifecycle::Spawning;
        e.on_line("ERROR: Could not create display");
        assert!(matches!(e, Lifecycle::Failed { .. }));
    }

    #[test]
    fn scrcpy_argv_never_opens_a_window() {
        let a = scrcpy_argv("PLACEHOLDER", "1220x2712/480", "com.example.app");
        assert!(a.contains(&"--no-window".to_string()));
        assert!(a.contains(&"--new-display=1220x2712/480".to_string()));
        assert!(!a.iter().any(|w| w.contains("no-vd-destroy-content")));
    }
}
