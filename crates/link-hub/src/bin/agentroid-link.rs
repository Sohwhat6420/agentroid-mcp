//! `agentroid-link`: the hub half of the persistent link, from the command line.
//!
//! ```text
//! agentroid-link link pair <serial> --hub <tailnet IPv4>:<port> [--rekey] [--dry-run]
//!                [--state-dir DIR] [--adb PATH] [--key-target agentroid:...]
//! agentroid-link link serve --bind <tailnet IPv4> [--port 8765] [--state-dir DIR] [--key-target ...]
//! agentroid-link link serve --link-lan --bind <home LAN IPv4> [...]   (wss://, TLS 1.3, pinned certificate)
//! agentroid-link link key [--key-target ...]
//! agentroid-link link devices [--state-dir DIR]
//! ```
//!
//! `link pair` is the command the phone's repair texts name (`agentroid link pair <serial>`).
//! adb runs only through the guarded runner, hidden (CREATE_NO_WINDOW). Exit codes: 0 done,
//! 1 refused or failed, 2 usage.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use agentroid_adb_link::guard::{link_dump_argv, link_pair_argv, Guard, GuardedRunner, S24_SERIAL};
use agentroid_adb_link::runner::AdbRunner;
use link_hub::crypto::{HubKey, HUB_KEY_TARGET};
use link_hub::hub::{Hub, HubConfig};
use link_hub::pair::pair;
use link_hub::store::StateDir;

const USAGE: &str = "usage:
  agentroid-link link pair <serial> --hub <tailnet IPv4>:<port> [--rekey] [--dry-run] [--state-dir DIR] [--adb PATH] [--key-target T]
  agentroid-link link serve [--link-lan] --bind <tailnet IPv4, or with --link-lan a private LAN IPv4> [--port 8765] [--state-dir DIR] [--key-target T]
  agentroid-link link key [--key-target T]
  agentroid-link link devices [--state-dir DIR]";

struct Opts {
    positional: Vec<String>,
    hub: Option<String>,
    bind: Option<String>,
    port: u16,
    rekey: bool,
    dry_run: bool,
    lan: bool,
    state_dir: PathBuf,
    adb: PathBuf,
    key_target: String,
}

fn default_state_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("AGENTROID_STATE_DIR") {
        return PathBuf::from(d);
    }
    match std::env::var_os("LOCALAPPDATA") {
        Some(d) => PathBuf::from(d).join("agentroid"),
        None => PathBuf::from(".agentroid"),
    }
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        positional: vec![],
        hub: None,
        bind: None,
        port: 8765,
        rekey: false,
        dry_run: false,
        lan: false,
        state_dir: default_state_dir(),
        adb: std::env::var_os("AGENTROID_ADB")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("adb")),
        key_target: HUB_KEY_TARGET.to_string(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--hub" => o.hub = Some(value("--hub")?),
            "--bind" => o.bind = Some(value("--bind")?),
            "--port" => {
                o.port = value("--port")?
                    .parse()
                    .map_err(|_| "--port must be 0..65535".to_string())?
            }
            "--state-dir" => o.state_dir = PathBuf::from(value("--state-dir")?),
            "--adb" => o.adb = PathBuf::from(value("--adb")?),
            "--key-target" => o.key_target = value("--key-target")?,
            "--rekey" => o.rekey = true,
            "--dry-run" => o.dry_run = true,
            "--link-lan" => o.lan = true,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => o.positional.push(s.to_string()),
        }
    }
    Ok(o)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let o = match parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let words: Vec<&str> = o.positional.iter().map(String::as_str).collect();
    let result = match words.as_slice() {
        ["link", "pair", serial] => cmd_pair(&o, serial),
        ["link", "serve"] => cmd_serve(&o),
        ["link", "key"] => cmd_key(&o),
        ["link", "devices"] => cmd_devices(&o),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_pair(o: &Opts, serial: &str) -> Result<(), String> {
    let Some(hub) = o.hub.as_deref() else {
        return Err(
            "link pair needs --hub <this laptop's tailnet IPv4>:<port> (see `tailscale ip -4`)"
                .into(),
        );
    };
    link_hub::bind::check_phone_hub_address(hub).map_err(|e| e.0)?;
    if o.dry_run {
        // no adb, no key store: the same refusals a real run makes before it touches anything
        if serial.to_ascii_uppercase().contains(S24_SERIAL) {
            return Err(link_hub::pair::PairError::ReadOnlyS24.to_string());
        }
        Guard::new()
            .check(&link_dump_argv(serial))
            .map_err(|e| e.0)?;
        println!(
            "would run (through the guarded runner, after its getprop ro.serialno identity probe):"
        );
        println!("  adb {}", link_dump_argv(serial).join(" "));
        println!(
            "  adb {}",
            link_pair_argv(
                serial,
                "<hub public key SPKI, base64>",
                hub,
                "<ro.serialno>",
                o.rekey
            )
            .join(" ")
        );
        return Ok(());
    }
    let key = HubKey::load_or_create(&o.key_target).map_err(|e| e.0)?;
    let store = StateDir::open(&o.state_dir).map_err(|e| e.to_string())?;
    let runner = GuardedRunner::new(AdbRunner::new(&o.adb));
    let done = pair(&runner, &store, key.spki(), serial, hub, o.rekey, now_ms())
        .map_err(|e| e.to_string())?;
    println!(
        "paired {} (key_id={}); hub key_id={}; the phone will dial ws://{hub}/helper",
        done.binding.serial,
        done.binding.key_id,
        key.key_id()
    );
    Ok(())
}

fn cmd_key(o: &Opts) -> Result<(), String> {
    let key = HubKey::load_or_create(&o.key_target).map_err(|e| e.0)?;
    println!("public_key={}\nkey_id={}", key.spki_b64(), key.key_id());
    Ok(())
}

fn cmd_devices(o: &Opts) -> Result<(), String> {
    let store = StateDir::open(&o.state_dir).map_err(|e| e.to_string())?;
    for b in store.load_devices().map_err(|e| e.to_string())? {
        let epoch = store.current_epoch(&b.serial).unwrap_or(0);
        println!(
            "{} key_id={} last_epoch={epoch}{}",
            b.serial,
            b.key_id,
            if b.revoked { " REVOKED" } else { "" }
        );
    }
    Ok(())
}

fn cmd_serve(o: &Opts) -> Result<(), String> {
    let Some(bind) = o.bind.clone() else {
        return Err(
            "link serve needs --bind <this laptop's tailnet IPv4> (see `tailscale ip -4`)".into(),
        );
    };
    if o.lan {
        link_hub::bind::check_bind_address_lan(&bind).map_err(|e| e.0)?;
    } else {
        link_hub::bind::check_bind_address(&bind).map_err(|e| e.0)?;
    }
    let key = HubKey::load_or_create(&o.key_target).map_err(|e| e.0)?;
    let mut cfg = HubConfig::new(&o.state_dir, bind, o.port, key);
    if o.lan {
        // home Wi-Fi: TLS 1.3 with the certificate the pairing QR pins (made on first use)
        let target = link_hub::tls::tls_target_for(&o.key_target);
        let hub_state = o.state_dir.join("hub-state");
        let (tls, created) = link_hub::tls::HubTls::load_or_create(&target, &hub_state)
            .map_err(|e| e.to_string())?;
        if created {
            eprintln!("new TLS certificate: phones paired over the LAN before now must re-pair");
        }
        eprintln!("TLS fingerprint (the QR's fp)={}", tls.fingerprint_hex());
        cfg.tls = Some(std::sync::Arc::new(tls));
        cfg.lan = true;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async {
        let lan = cfg.lan;
        let hub = Hub::start(cfg).await.map_err(|e| e.to_string())?;
        let url = if lan { hub.url().replacen("ws://", "wss://", 1) } else { hub.url() };
        eprintln!("listening on {url} (hub key_id={}); Ctrl-C stops", hub.hub_key().key_id());
        let mut events = hub.subscribe();
        loop {
            tokio::select! {
                e = events.recv() => match e {
                    Ok(ev) => eprintln!("{ev:?}"),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => eprintln!("(skipped {n} events)"),
                    Err(_) => break,
                },
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        hub.shutdown().await;
        Ok::<(), String>(())
    })
}
