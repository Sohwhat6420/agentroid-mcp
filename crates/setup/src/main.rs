//! `agentroid-setup`: the command line for task 2.7.
//!
//! ```text
//! agentroid-setup [setup] [--device T] [--mode full|helper|read_only] [--name N] [--dry-run] [--yes]
//! agentroid-setup doctor [--device T] [--mode M]
//! agentroid-setup --undo [--device T]
//! common: [--config-dir D] [--adb PATH] [--quirks FILE]
//! ```
//! Every adb command goes through adb-link's `GuardedRunner` around `AdbRunner`, which spawns adb with
//! `CREATE_NO_WINDOW`. Exit codes: 0 fine, 1 doctor found something or setup left work, 2 error.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use agentroid_adb_link::guard::GuardedRunner;
use agentroid_adb_link::parse::parse_devices;
use agentroid_adb_link::runner::{AdbRunner, Runner};
use agentroid_core::gate::{is_s24, DeviceMode};
use setup::quirks::Quirks;
use setup::setup::{configured_mode, Options, Prompter, Question};

struct Stdin;

impl Prompter for Stdin {
    fn ask(&mut self, q: &Question) -> String {
        print!("{} [{}] ", q.text, q.default);
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        line
    }
}

#[derive(Default)]
struct Args {
    cmd: String,
    device: Option<String>,
    mode: Option<DeviceMode>,
    name: Option<String>,
    dry_run: bool,
    yes: bool,
    config_dir: Option<PathBuf>,
    adb: Option<PathBuf>,
    quirks: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        cmd: "setup".into(),
        ..Default::default()
    };
    let mut it = std::env::args().skip(1);
    while let Some(w) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match w.as_str() {
            "setup" | "doctor" => a.cmd = w.clone(),
            "--undo" | "undo" => a.cmd = "undo".into(),
            "--device" | "-s" => a.device = Some(val("--device")?),
            "--mode" => {
                let m = val("--mode")?;
                a.mode = Some(DeviceMode::parse(&m).ok_or_else(|| format!("unknown mode {m}"))?);
            }
            "--name" => a.name = Some(val("--name")?),
            "--dry-run" => a.dry_run = true,
            "--yes" | "-y" => a.yes = true,
            "--config-dir" => a.config_dir = Some(PathBuf::from(val("--config-dir")?)),
            "--adb" => a.adb = Some(PathBuf::from(val("--adb")?)),
            "--quirks" => a.quirks = Some(PathBuf::from(val("--quirks")?)),
            "--help" | "-h" => a.cmd = "help".into(),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn default_config_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("agentroid"))
        .unwrap_or_else(|| PathBuf::from("agentroid-config"))
}

fn only_device(runner: &dyn Runner) -> Result<String, String> {
    let out = runner
        .run(&["devices".to_string(), "-l".to_string()], 10_000)
        .map_err(|e| e.to_string())?;
    let ready: Vec<_> = parse_devices(&out.stdout)
        .into_iter()
        .filter(|d| d.state == "device")
        .collect();
    match ready.as_slice() {
        [one] => Ok(one.serial.clone()),
        [] => Err("no phone connected".into()),
        _ => Err("several phones connected: name one with --device".into()),
    }
}

fn run() -> Result<i32, String> {
    let a = parse_args()?;
    if a.cmd == "help" {
        println!("agentroid-setup [setup|doctor|--undo] [--device T] [--mode M] [--name N] [--dry-run] [--yes] [--config-dir D] [--adb PATH] [--quirks FILE]");
        return Ok(0);
    }
    let quirks = match &a.quirks {
        Some(p) => Quirks::load(p).map_err(|e| e.to_string())?,
        None => Quirks::builtin(),
    };
    let config_dir = a.config_dir.clone().unwrap_or_else(default_config_dir);
    let runner = GuardedRunner::new(AdbRunner::new(
        a.adb.clone().unwrap_or_else(|| PathBuf::from("adb")),
    ));
    match a.cmd.as_str() {
        "doctor" => {
            let target = match &a.device {
                Some(d) => d.clone(),
                None => only_device(&runner)?,
            };
            let mode = a.mode.unwrap_or_else(|| {
                if is_s24(&target) {
                    DeviceMode::ReadOnly
                } else {
                    // devices.toml is keyed by hardware serial; the adb name is often the same over USB
                    configured_mode(&config_dir, &target).unwrap_or(DeviceMode::Full)
                }
            });
            let rep = setup::doctor::doctor(&runner, &target, mode, &quirks)
                .map_err(|e| e.to_string())?;
            print!("{}", rep.render());
            Ok(if rep.findings.is_empty() { 0 } else { 1 })
        }
        "undo" => {
            let rep = setup::journal::undo(&config_dir, Some(&runner), a.device.as_deref())
                .map_err(|e| e.to_string())?;
            println!(
                "undid run {}: {} files restored, {} removed, {} phone settings reverted",
                rep.run, rep.files_restored, rep.files_removed, rep.device_reverted
            );
            for n in rep.notes {
                println!("  note: {n}");
            }
            Ok(0)
        }
        _ => {
            let opts = Options {
                config_dir: config_dir.clone(),
                target: a.device.clone(),
                mode: a.mode,
                name: a.name.clone(),
                dry_run: a.dry_run,
                yes: a.yes,
            };
            let out = setup::setup::setup(&runner, &quirks, &mut Stdin, &opts)
                .map_err(|e| e.to_string())?;
            print!("{}", out.render(&config_dir));
            Ok(if out.remaining.is_empty() { 0 } else { 1 })
        }
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("agentroid-setup: {e}");
            std::process::exit(2);
        }
    }
}
