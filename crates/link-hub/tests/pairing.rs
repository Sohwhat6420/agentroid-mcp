//! `link pair <serial>` through the real GuardedRunner over a fake adb: exactly the two allowed
//! argv reach adb (besides identity probes), the key is bound to ro.serialno, and the protected phone is
//! refused however it is named. No adb is run and no device is touched.

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use agentroid_adb_link::guard::{link_dump_argv, GuardedRunner, S24_SERIAL};
use agentroid_adb_link::runner::{RunError, RunOutput, Runner};
use common::*;
use link_hub::crypto::b64;
use link_hub::pair::{pair, PairError};
use link_hub::store::StateDir;

const HUB: &str = "100.101.102.103:8765";

/// A fake adb: answers getprop with the hardware serial, the dump with the phone key, and the
/// PAIR broadcast as ConfigReceiver does. Records every argv it is given.
struct FakeAdb {
    hw_serial: String,
    key: RefCell<PhoneKey>,
    rekey_to: Option<u8>,
    calls: Log,
}

type Log = Rc<RefCell<Vec<Vec<String>>>>;

impl FakeAdb {
    fn new(hw: &str, seed: u8) -> Self {
        Self {
            hw_serial: hw.into(),
            key: RefCell::new(phone_key(seed)),
            rekey_to: None,
            calls: Rc::new(RefCell::new(vec![])),
        }
    }
    fn dump(&self) -> String {
        let k = self.key.borrow();
        format!(
            "SERVICE dev.agentroid.helper/.link.LinkService 1a2b3c pid=4242 user=0\n  Client:\n    agentroid-link v1\n    alg=SHA256withECDSA\n    public_key={}\n    key_id={}\n",
            b64(&k.spki),
            k.key_id
        )
    }
}

fn ok(stdout: String) -> Result<RunOutput, RunError> {
    Ok(RunOutput {
        code: Some(0),
        stdout,
        stderr: String::new(),
    })
}

fn is_probe(a: &[String]) -> bool {
    let tail = a[2..].join(" ");
    a.len() >= 3 && a[0] == "-s" && tail == "shell getprop ro.serialno"
}

impl Runner for FakeAdb {
    fn run(&self, args: &[String], _timeout_ms: u64) -> Result<RunOutput, RunError> {
        self.calls.borrow_mut().push(args.to_vec());
        if is_probe(args) {
            return ok(format!("{}\n", self.hw_serial));
        }
        let w: Vec<&str> = args[2..].iter().map(String::as_str).collect();
        if w.starts_with(&["shell", "dumpsys"]) {
            return ok(self.dump());
        }
        if w.starts_with(&["shell", "am", "broadcast"]) {
            if w.ends_with(&["--ez", "rekey", "true"]) {
                if let Some(s) = self.rekey_to {
                    *self.key.borrow_mut() = phone_key(s);
                }
            }
            let kid = self.key.borrow().key_id.clone();
            return ok(format!(
                "Broadcasting: Intent {{ act=dev.agentroid.helper.PAIR flg=0x400000 cmp=dev.agentroid.helper/.link.ConfigReceiver (has extras) }}\nBroadcast completed: result=-1, data=\"paired key_id={kid}\"\n"
            ));
        }
        panic!("unexpected adb argv {args:?}");
    }
}

fn s(a: &[&str]) -> Vec<String> {
    a.iter().map(|x| x.to_string()).collect()
}

#[test]
fn pairing_sends_exactly_the_two_allowed_argv_and_binds_ro_serialno() {
    let dir = TempDir::new("pair-ok");
    let store = StateDir::open(dir.path()).unwrap();
    let hub = hub_key();
    let (runner, log) = rig(FakeAdb::new("POCOX6HW0001", 31));
    // the user names the phone by its Wi-Fi adb address; the key is bound to ro.serialno
    let sel = "100.70.1.2:41555";
    let done = pair(&runner, &store, hub.spki(), sel, HUB, false, 1234).unwrap();

    let hub_b64 = hub.spki_b64();
    let dump = s(&[
        "-s",
        sel,
        "shell",
        "dumpsys",
        "activity",
        "service",
        "dev.agentroid.helper/.link.LinkService",
    ]);
    let pair_cmd = s(&[
        "-s",
        sel,
        "shell",
        "am",
        "broadcast",
        "-a",
        "dev.agentroid.helper.PAIR",
        "-n",
        "dev.agentroid.helper/.link.ConfigReceiver",
        "--es",
        "hub_key",
        &hub_b64,
        "--es",
        "hub",
        HUB,
        "--es",
        "serial",
        "POCOX6HW0001",
    ]);
    // what reached adb, besides `shell getprop ro.serialno` identity probes: exactly these two
    let calls = calls(&log);
    let real: Vec<Vec<String>> = calls.iter().filter(|a| !is_probe(a)).cloned().collect();
    assert_eq!(real, vec![dump.clone(), pair_cmd.clone()]);
    assert!(calls.iter().filter(|a| is_probe(a)).all(|a| a[1] == sel));
    assert_eq!(
        done.commands
            .iter()
            .filter(|a| !is_probe(a))
            .cloned()
            .collect::<Vec<_>>(),
        vec![dump, pair_cmd]
    );

    let phone = phone_key(31);
    assert_eq!(done.binding.serial, "POCOX6HW0001");
    assert_eq!(done.binding.key_id, phone.key_id);
    let rows = store.load_devices().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (
            rows[0].serial.as_str(),
            rows[0].spki.as_slice(),
            rows[0].paired_at_ms
        ),
        ("POCOX6HW0001", phone.spki.as_slice(), 1234)
    );
    let text = std::fs::read_to_string(store.devices_path()).unwrap();
    assert!(text.contains(&phone.key_id) && text.contains("POCOX6HW0001"));
}

#[test]
fn rekey_adds_the_optional_suffix_and_binds_the_new_key() {
    let dir = TempDir::new("pair-rekey");
    let store = StateDir::open(dir.path()).unwrap();
    let hub = hub_key();
    let mut fake = FakeAdb::new("POCOX6HW0001", 32);
    fake.rekey_to = Some(33);
    let (runner, log) = rig(fake);
    let done = pair(&runner, &store, hub.spki(), "POCOX6HW0001", HUB, true, 1).unwrap();
    let real: Vec<Vec<String>> = calls(&log).into_iter().filter(|a| !is_probe(a)).collect();
    assert_eq!(real.len(), 3, "dump, pair --ez rekey true, dump again");
    assert_eq!(real[0], link_dump_argv("POCOX6HW0001"));
    assert_eq!(real[1][real[1].len() - 3..], s(&["--ez", "rekey", "true"]));
    assert_eq!(real[2], link_dump_argv("POCOX6HW0001"));
    assert_eq!(done.binding.key_id, phone_key(33).key_id);
}

#[test]
fn the_s24_is_never_paired_by_serial() {
    let dir = TempDir::new("pair-s24");
    let store = StateDir::open(dir.path()).unwrap();
    for sel in [
        S24_SERIAL,
        "protected0000example",
        "adb-PROTECTED0000EXAMPLE-abc123._adb-tls-connect._tcp",
    ] {
        let (runner, log) = rig(FakeAdb::new(S24_SERIAL, 34));
        let r = pair(&runner, &store, hub_key().spki(), sel, HUB, false, 1);
        assert!(matches!(r, Err(PairError::ReadOnlyS24)), "{sel}: {r:?}");
        assert!(calls(&log).is_empty(), "{sel}: adb was not even asked");
    }
    assert!(store.load_devices().unwrap().is_empty());
}

#[test]
fn the_s24_is_never_paired_behind_an_address() {
    let dir = TempDir::new("pair-s24-ip");
    let store = StateDir::open(dir.path()).unwrap();
    let (runner, log) = rig(FakeAdb::new(S24_SERIAL, 35));
    let sel = "192.0.2.50:5555";
    let r = pair(&runner, &store, hub_key().spki(), sel, HUB, false, 1);
    assert!(matches!(r, Err(PairError::ReadOnlyS24)), "{r:?}");
    let calls1 = calls(&log);
    assert_eq!(
        calls1.len(),
        1,
        "the pairing's own getprop probe only: {calls1:?}"
    );
    assert!(
        calls1.iter().all(|a| is_probe(a)),
        "only identity probes reached adb: {calls1:?}"
    );
    assert!(store.load_devices().unwrap().is_empty());
    // and the guard itself refuses the link shapes for that address from now on, and for any
    // other address that turns out to be the protected phone
    assert!(matches!(
        runner.run(&link_dump_argv(sel), 1000),
        Err(RunError::Refused(_))
    ));
    let (r2, log2) = rig(FakeAdb::new(S24_SERIAL, 35));
    let e = r2
        .run(&link_dump_argv("192.0.2.51:5555"), 1000)
        .unwrap_err();
    assert!(e.to_string().contains("S24"), "{e}");
    assert!(calls(&log2).iter().all(|a| is_probe(a)));
}

#[test]
fn bad_arguments_are_refused_before_adb() {
    let dir = TempDir::new("pair-bad");
    let store = StateDir::open(dir.path()).unwrap();
    for (sel, hub) in [
        ("POCO1", "127.0.0.1:8765"),
        ("POCO1", "192.168.1.2:8765"),
        ("POCO1", "0.0.0.0:8765"),
        ("-d", HUB),
        ("two words", HUB),
        ("", HUB),
    ] {
        let (runner, log) = rig(FakeAdb::new("POCO1", 36));
        assert!(
            matches!(
                pair(&runner, &store, hub_key().spki(), sel, hub, false, 1),
                Err(PairError::BadArgument(_))
            ),
            "{sel} {hub}"
        );
        assert!(calls(&log).is_empty());
    }
}

#[test]
fn a_mismatched_reply_stores_nothing() {
    let dir = TempDir::new("pair-mismatch");
    let store = StateDir::open(dir.path()).unwrap();
    let fake = FakeAdb::new("POCO1", 37);
    // a phone whose PAIR reply names another key id than its dump shows
    struct Liar(FakeAdb);
    impl Runner for Liar {
        fn run(&self, args: &[String], t: u64) -> Result<RunOutput, RunError> {
            let mut out = self.0.run(args, t)?;
            if args.iter().any(|a| a == "broadcast") {
                out.stdout = out
                    .stdout
                    .replace(&phone_key(37).key_id, &phone_key(38).key_id);
            }
            Ok(out)
        }
    }
    let runner = GuardedRunner::new(Liar(fake));
    let r = pair(&runner, &store, hub_key().spki(), "POCO1", HUB, false, 1);
    assert!(
        matches!(r, Err(PairError::Phone(ref m)) if m.contains("nothing was stored")),
        "{r:?}"
    );
    assert!(store.load_devices().unwrap().is_empty());
}

/// The CLI's `--dry-run` prints the two shapes and refuses the protected phone, without adb or the key store.
#[test]
fn the_cli_dry_run_shows_the_shapes_and_refuses_the_s24() {
    let run = |args: &[&str]| {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_agentroid-link"));
        c.args(args).env("AGENTROID_ADB", "Z:\\no-such-adb.exe");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(agentroid_adb_link::runner::CREATE_NO_WINDOW);
        }
        c.output().unwrap()
    };
    let ok = run(&["link", "pair", "POCO1", "--hub", HUB, "--dry-run"]);
    assert_eq!(ok.status.code(), Some(0));
    let out = String::from_utf8_lossy(&ok.stdout);
    assert!(
        out.contains(
            "adb -s POCO1 shell dumpsys activity service dev.agentroid.helper/.link.LinkService"
        ),
        "{out}"
    );
    assert!(out.contains("adb -s POCO1 shell am broadcast -a dev.agentroid.helper.PAIR -n dev.agentroid.helper/.link.ConfigReceiver --es hub_key"), "{out}");
    let s24 = run(&["link", "pair", S24_SERIAL, "--hub", HUB, "--dry-run"]);
    assert_eq!(s24.status.code(), Some(1));
    let err = String::from_utf8_lossy(&s24.stderr);
    assert!(
        err.contains("S24") || err.contains("protected phone"),
        "{err}"
    );
    let lan = run(&[
        "link",
        "pair",
        "POCO1",
        "--hub",
        "192.168.1.2:8765",
        "--dry-run",
    ]);
    assert_eq!(lan.status.code(), Some(1));
    let usage = run(&["link", "frobnicate"]);
    assert_eq!(usage.status.code(), Some(2));
}

/// A GuardedRunner over the fake adb, and the fake's call log (shared with it).
fn rig(fake: FakeAdb) -> (GuardedRunner<FakeAdb>, Log) {
    let log = fake.calls.clone();
    (GuardedRunner::new(fake), log)
}

fn calls(log: &Log) -> Vec<Vec<String>> {
    log.borrow().clone()
}
