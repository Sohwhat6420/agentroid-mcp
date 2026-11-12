//! Settings lease rows.

use agentroid_adb_link::guard::{GuardedRunner, S24_SERIAL};
use agentroid_core::gate::DeviceMode;
use serde_json::Value;

use super::Rig;
use crate::clock::CareClock;
use crate::fake::FakePhone;
use crate::leases::{render_entries, LeaseStore, Scope};
use crate::phone::AdbPhone;
use crate::settings::{
    agent_bundle, panel_off_refusal, LeaseManager, Owner, RestoreStatus, ScopeReq,
};
use crate::sink::{CareEventKind, MemoryCareSink};

const FULL: DeviceMode = DeviceMode::Full;

/// Originals with awkward bytes; `None` = the key is absent.
fn originals(i: usize) -> Vec<(&'static str, &'static str, Option<String>, String)> {
    let tricky = [
        "128",
        "a b  c",
        "it's",
        "quote\"d",
        "ünïcödé ✓",
        "$HOME;reboot",
        "60.0",
        "-1",
        "back\\slash",
        "`x`",
    ];
    vec![
        (
            "system",
            "screen_brightness",
            Some(format!("{}", 100 + i)),
            "1".into(),
        ),
        (
            "system",
            "screen_brightness_mode",
            Some("1".into()),
            "0".into(),
        ),
        ("system", "peak_refresh_rate", None, "60".into()),
        (
            "secure",
            "miui_refresh_rate",
            Some(tricky[i].to_string()),
            "60".into(),
        ),
        (
            "system",
            "screen_off_timeout",
            Some("60000".into()),
            "1800000".into(),
        ),
        (
            "global",
            "care_test_key",
            if i.is_multiple_of(2) {
                None
            } else {
                Some(format!("{} {}", tricky[(i + 3) % 10], i))
            },
            "x y".into(),
        ),
    ]
}

#[test]
fn leases_restore_byte_exact_10_of_10_including_kill_and_reboot() {
    let mut exact = 0;
    for i in 0..10 {
        let rig = Rig::new(&format!("exact{i}"), "POCO0010");
        let orig = originals(i);
        for (ns, key, v, _) in &orig {
            if let Some(v) = v {
                rig.fake.set(ns, key, v);
            }
        }
        let before = rig.fake.state().settings.clone();
        let owner = Owner {
            pid: 4000 + i as u32,
            job_id: format!("job-{i}"),
            session: "s".into(),
        };
        {
            let mut mgr = rig.manager();
            mgr.pid = owner.pid;
            let phone = AdbPhone::new(&rig.runner, "POCO0010");
            for (ns, key, _, applied) in &orig {
                mgr.apply(&phone, FULL, &owner, ns, key, applied, ScopeReq::Job)
                    .unwrap();
            }
            for (ns, key, _, applied) in &orig {
                assert_eq!(rig.fake.get(ns, key).as_deref(), Some(applied.as_str()));
            }
            if i % 3 == 0 {
                // normal job end
                let r = mgr.restore_job(&phone, FULL, &owner.job_id).unwrap();
                assert!(
                    r.iter().all(|(_, s)| matches!(
                        s,
                        RestoreStatus::Restored | RestoreStatus::Deleted
                    )),
                    "{r:?}"
                );
            }
            // otherwise: the daemon is killed here (the manager is dropped without restoring)
        }
        if i % 3 != 0 {
            rig.pids.kill(owner.pid);
            let mut token = "POCO0010".to_string();
            if i % 3 == 2 {
                // ... and the phone reboots and comes back on a new Wi-Fi token
                rig.fake.reboot();
                token = "192.0.2.78:41234".into();
                rig.fake.state().tokens = vec![token.clone()];
            }
            let mut mgr = rig.manager(); // a new daemon process
            let phone = AdbPhone::new(&rig.runner, token.as_str());
            let rep = mgr.on_reconnect(&phone, FULL).unwrap();
            assert_eq!(rep.restored.len(), orig.len(), "{:?}", rep.restored);
            assert!(rep.reasserted.is_empty());
        }
        let after = rig.fake.state().settings.clone();
        assert_eq!(
            after, before,
            "trial {i}: every byte back, absent keys absent"
        );
        assert!(
            rig.store().entries().unwrap().is_empty(),
            "trial {i}: leases.json is empty"
        );
        exact += 1;
    }
    assert_eq!(exact, 10);
}

#[test]
fn absent_keys_are_deleted_on_restore() {
    let rig = Rig::new("absent", "POCO0011");
    let phone = AdbPhone::new(&rig.runner, "POCO0011");
    let mut mgr = rig.manager();
    let owner = Owner::here("j-absent", "s");
    assert_eq!(rig.fake.get("system", "peak_refresh_rate"), None);
    let c = mgr
        .apply(
            &phone,
            FULL,
            &owner,
            "system",
            "peak_refresh_rate",
            "60",
            ScopeReq::Job,
        )
        .unwrap();
    assert_eq!(c.before, "ABSENT");
    let r = mgr.restore_job(&phone, FULL, "j-absent").unwrap();
    assert_eq!(r[0].1, RestoreStatus::Deleted);
    assert_eq!(
        rig.fake.get("system", "peak_refresh_rate"),
        None,
        "deleted, not an empty string"
    );
    assert!(rig
        .fake
        .commands()
        .iter()
        .any(|c| c == "settings delete system peak_refresh_rate"));
    assert!(!rig
        .fake
        .commands()
        .iter()
        .any(|c| c.starts_with("settings put system peak_refresh_rate ''")));
}

#[test]
fn cas_refuses_to_restore_over_a_change_the_user_made() {
    let rig = Rig::new("cas", "POCO0012");
    let phone = AdbPhone::new(&rig.runner, "POCO0012");
    let mut mgr = rig.manager();
    let owner = Owner::here("j-cas", "s");
    rig.fake.set("system", "screen_brightness", "128");
    mgr.apply(
        &phone,
        FULL,
        &owner,
        "system",
        "screen_brightness",
        "1",
        ScopeReq::Job,
    )
    .unwrap();
    rig.fake.set("system", "screen_brightness", "200"); // the user moved the slider
    let r = mgr.restore_job(&phone, FULL, "j-cas").unwrap();
    assert_eq!(r[0].1, RestoreStatus::Left);
    assert_eq!(
        rig.fake.get("system", "screen_brightness").as_deref(),
        Some("200")
    );
    assert!(
        mgr.store.entries().unwrap().is_empty(),
        "the lease ends; the user's value stands"
    );
    let ev = mgr.sink.events.last().unwrap();
    assert_eq!(
        (ev.kind, ev.outcome.as_str()),
        (CareEventKind::Restore, "left")
    );
    // an absent-before key the user then set is left too (not deleted)
    mgr.apply(
        &phone,
        FULL,
        &owner,
        "system",
        "min_refresh_rate",
        "60",
        ScopeReq::Job,
    )
    .unwrap();
    rig.fake.set("system", "min_refresh_rate", "120");
    assert_eq!(
        mgr.restore_job(&phone, FULL, "j-cas").unwrap()[0].1,
        RestoreStatus::Left
    );
    assert_eq!(
        rig.fake.get("system", "min_refresh_rate").as_deref(),
        Some("120")
    );
}

#[test]
fn permanent_is_refused_as_r2_and_read_only_refuses_every_scope() {
    let rig = Rig::new("refuse", "POCO0013");
    let phone = AdbPhone::new(&rig.runner, "POCO0013");
    let mut mgr = rig.manager();
    let owner = Owner::here("j-r", "s");
    let e = mgr
        .apply(
            &phone,
            FULL,
            &owner,
            "system",
            "screen_brightness",
            "1",
            ScopeReq::Permanent,
        )
        .unwrap_err();
    assert_eq!(e.class(), Some("R2"));
    assert!(e.to_string().contains("R2"), "the result says R2: {e}");
    assert!(rig.fake.commands().is_empty(), "nothing reached the phone");
    let ev = mgr.sink.events.last().unwrap();
    assert_eq!(
        (ev.kind, ev.class.as_deref()),
        (CareEventKind::Refused, Some("R2"))
    );

    let until = rig.clock.unix_ms() + 3_600_000;
    for scope in [
        ScopeReq::Session,
        ScopeReq::Job,
        ScopeReq::Until(until),
        ScopeReq::Permanent,
    ] {
        let e = mgr
            .apply(
                &phone,
                DeviceMode::ReadOnly,
                &owner,
                "system",
                "screen_brightness",
                "1",
                scope,
            )
            .unwrap_err();
        assert_eq!(e.class(), Some("read_only"), "{scope:?}: {e}");
    }
    assert!(rig.fake.commands().is_empty(), "read_only: not even a read");

    // The protected phone is read_only whatever it is configured as, by name ...
    let s24 = FakePhone::new(S24_SERIAL);
    let g = GuardedRunner::new(s24.clone());
    let p = AdbPhone::new(&g, S24_SERIAL);
    for scope in [
        ScopeReq::Session,
        ScopeReq::Job,
        ScopeReq::Until(until),
        ScopeReq::Permanent,
    ] {
        assert_eq!(
            mgr.apply(&p, FULL, &owner, "system", "screen_brightness", "1", scope)
                .unwrap_err()
                .class(),
            Some("read_only")
        );
    }
    // ... and behind an address, found by its ro.serialno (only the identity probe went out).
    s24.state().tokens = vec!["192.0.2.99:40000".into()];
    let p = AdbPhone::new(&g, "192.0.2.99:40000");
    for scope in [ScopeReq::Session, ScopeReq::Job, ScopeReq::Until(until)] {
        assert_eq!(
            mgr.apply(&p, FULL, &owner, "system", "screen_brightness", "1", scope)
                .unwrap_err()
                .class(),
            Some("read_only")
        );
    }
    assert!(
        s24.state().log.iter().all(|l| l == "getprop ro.serialno"),
        "{:?}",
        s24.state().log
    );
    assert!(s24.state().settings.is_empty() && mgr.store.entries().unwrap().is_empty());
    // helper mode has no shell
    assert!(mgr
        .apply(
            &phone,
            DeviceMode::Helper,
            &owner,
            "system",
            "screen_brightness",
            "1",
            ScopeReq::Job
        )
        .is_err());
}

/// The protected phone's one exception (12 Nov 2026): with the manager's opt-in AND the opt-in runner, the protected phone may
/// lease and restore the two screen keys, by name or behind an address; nothing else; no other
/// read_only phone; and without either opt-in nothing changes.
#[test]
fn the_s24_exception_is_two_screen_keys_on_the_s24_only() {
    let rig = Rig::new("s24-awake", "POCO0014");
    let owner = Owner::here("session-awake", "awake:s24");
    let s24 = FakePhone::new(S24_SERIAL);
    s24.set("system", "screen_off_timeout", "30000");
    s24.set("global", "stay_on_while_plugged_in", "0");
    let awake_runner = GuardedRunner::with_s24_screen_awake(s24.clone());
    let plain_runner = GuardedRunner::new(s24.clone());
    let ro = DeviceMode::ReadOnly;

    // without the manager's opt-in: refused as read_only, before a read
    let mut off = rig.manager();
    let p = AdbPhone::new(&awake_runner, S24_SERIAL);
    let e = off
        .apply(
            &p,
            ro,
            &owner,
            "system",
            "screen_off_timeout",
            "1800000",
            ScopeReq::Session,
        )
        .unwrap_err();
    assert_eq!(e.class(), Some("read_only"));
    // with it, but through the ordinary runner: the guard refuses the read, nothing is written
    let mut on = rig.manager();
    on.allow_s24_screen_awake();
    let p_plain = AdbPhone::new(&plain_runner, S24_SERIAL);
    assert!(on
        .apply(
            &p_plain,
            ro,
            &owner,
            "system",
            "screen_off_timeout",
            "1800000",
            ScopeReq::Session
        )
        .is_err());
    assert_eq!(
        s24.get("system", "screen_off_timeout").as_deref(),
        Some("30000")
    );
    assert!(on.store.entries().unwrap().is_empty());

    // both opt-ins, behind an address (found by ro.serialno): the two keys lease, others refuse
    s24.state().tokens = vec!["192.0.2.99:40000".into()];
    let p = AdbPhone::new(&awake_runner, "192.0.2.99:40000");
    for (ns, k, v) in [
        ("system", "screen_off_timeout", "1800000"),
        ("global", "stay_on_while_plugged_in", "7"),
    ] {
        on.apply(&p, ro, &owner, ns, k, v, ScopeReq::Session)
            .unwrap_or_else(|e| panic!("{ns}/{k}: {e}"));
        assert_eq!(s24.get(ns, k).as_deref(), Some(v));
    }
    for (ns, k, v) in [
        ("system", "screen_brightness", "1"),
        ("secure", "lockscreen.disabled", "1"),
        ("global", "adb_enabled", "1"),
    ] {
        let e = on
            .apply(&p, ro, &owner, ns, k, v, ScopeReq::Session)
            .unwrap_err();
        assert_eq!(e.class(), Some("read_only"), "{ns}/{k}: {e}");
        assert_eq!(s24.get(ns, k), None, "{ns}/{k} was written");
    }
    // the session ends: both come back by compare-and-swap, and the store is empty
    let rs = on.restore_session(&p, ro, "awake:s24").unwrap();
    assert_eq!(rs.len(), 2);
    assert!(
        rs.iter().all(|(_, st)| *st == RestoreStatus::Restored),
        "{rs:?}"
    );
    assert_eq!(
        s24.get("system", "screen_off_timeout").as_deref(),
        Some("30000")
    );
    assert_eq!(
        s24.get("global", "stay_on_while_plugged_in").as_deref(),
        Some("0")
    );
    assert!(on.store.entries().unwrap().is_empty());
    // only settings get/put on the two keys (and the identity probe) ever reached the protected phone
    for l in s24.state().log.iter() {
        assert!(
            l == "getprop ro.serialno"
                || l.ends_with("system screen_off_timeout")
                || l.ends_with("global stay_on_while_plugged_in")
                || l.contains("system screen_off_timeout ")
                || l.contains("global stay_on_while_plugged_in "),
            "{l}"
        );
    }

    // any other read_only phone stays refused, opt-ins or not, and gets only the identity probe
    let other = FakePhone::new("ROPHONE01");
    let other_runner = GuardedRunner::with_s24_screen_awake(other.clone());
    let po = AdbPhone::new(&other_runner, "ROPHONE01");
    let e = on
        .apply(
            &po,
            ro,
            &owner,
            "system",
            "screen_off_timeout",
            "1800000",
            ScopeReq::Session,
        )
        .unwrap_err();
    assert_eq!(e.class(), Some("read_only"));
    assert!(other.commands().is_empty(), "{:?}", other.commands());
}

/// The item schema of `schemas/job-view.json` `changes_in_force`, checked by hand.
fn check_job_view_item(v: &Value) {
    let o = v.as_object().unwrap();
    let allowed = [
        "key", "ns", "before", "applied", "owner", "scope", "expires", "ts",
    ];
    assert!(o.keys().all(|k| allowed.contains(&k.as_str())), "{o:?}");
    for k in ["key", "before", "applied", "owner"] {
        assert!(o.contains_key(k), "{k} missing");
    }
    assert!(["system", "secure", "global", "vendor"].contains(&o["ns"].as_str().unwrap()));
    let owner = o["owner"].as_object().unwrap();
    assert_eq!(
        owner.keys().map(String::as_str).collect::<Vec<_>>(),
        ["job_id", "pid", "session"]
    );
    assert!(["session", "job", "until", "permanent"].contains(&o["scope"].as_str().unwrap()));
    assert!(crate::clock::parse_rfc3339(o["ts"].as_str().unwrap()).is_some());
}

#[test]
fn changes_in_force_lists_the_lease_while_active_and_is_empty_after_restore() {
    let rig = Rig::new("cif", "POCO0014");
    let phone = AdbPhone::new(&rig.runner, "POCO0014");
    let mut mgr = rig.manager();
    let owner = Owner::here("j-cif", "sess-1");
    rig.fake.set("system", "screen_brightness", "128");
    assert!(mgr.changes_in_force("POCO0014").unwrap().is_empty());
    mgr.apply(
        &phone,
        FULL,
        &owner,
        "system",
        "screen_brightness",
        "1",
        ScopeReq::Job,
    )
    .unwrap();
    mgr.apply(
        &phone,
        FULL,
        &owner,
        "system",
        "peak_refresh_rate",
        "60",
        ScopeReq::Job,
    )
    .unwrap();
    let list = mgr.changes_in_force("POCO0014").unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(
        (
            list[0].key.as_str(),
            list[0].before.as_str(),
            list[0].applied.as_str()
        ),
        ("screen_brightness", "128", "1")
    );
    assert_eq!(
        (
            list[1].key.as_str(),
            list[1].before.as_str(),
            list[1].applied.as_str()
        ),
        ("peak_refresh_rate", "ABSENT", "60")
    );
    assert_eq!(list[0].owner, owner);
    for c in &list {
        check_job_view_item(&serde_json::to_value(c).unwrap());
    }
    assert!(
        mgr.changes_in_force("OTHER").unwrap().is_empty(),
        "per phone"
    );
    mgr.restore_job(&phone, FULL, "j-cif").unwrap();
    assert!(mgr.changes_in_force("POCO0014").unwrap().is_empty());
}

#[test]
fn one_journal_entry_per_change_and_per_restore() {
    let rig = Rig::new("journal", "POCO0015");
    let phone = AdbPhone::new(&rig.runner, "POCO0015");
    let mut mgr = rig.manager();
    let owner = Owner::here("j-jr", "s");
    let profile_keys = vec!["miui_refresh_rate".to_string()];
    let applied = mgr
        .apply_agent_bundle(&phone, FULL, &owner, &profile_keys)
        .unwrap();
    assert_eq!(applied.len(), 6);
    assert_eq!(mgr.sink.count(CareEventKind::Change), 6);
    assert_eq!(mgr.sink.events.len(), 6);
    mgr.restore_job(&phone, FULL, "j-jr").unwrap();
    assert_eq!(mgr.sink.count(CareEventKind::Restore), 6);
    assert_eq!(mgr.sink.events.len(), 12);
    for ev in &mgr.sink.events {
        let post = ev.post();
        assert!(post["kind"].as_str().unwrap().starts_with("care."));
        assert_eq!(ev.status(), "done");
        assert!(!ev.step_id().is_empty());
        let line =
            serde_json::json!({"step_id": ev.step_id(), "intent": ev.intent(), "post": post})
                .to_string();
        assert!(
            line.len() < 1024,
            "a record stays under 1 KB: {}",
            line.len()
        );
    }
}

#[test]
fn a_drifted_setting_is_reasserted_and_still_restored_to_the_original() {
    let rig = Rig::new("reassert", "POCO0016");
    let phone = AdbPhone::new(&rig.runner, "POCO0016");
    let mut mgr = rig.manager();
    let owner = Owner::here("j-ra", "s");
    rig.fake.set("system", "peak_refresh_rate", "120");
    mgr.apply(
        &phone,
        FULL,
        &owner,
        "system",
        "peak_refresh_rate",
        "60",
        ScopeReq::Job,
    )
    .unwrap();
    assert!(
        mgr.reassert(&phone, FULL, Some("j-ra")).unwrap().is_empty(),
        "nothing drifted"
    );
    rig.fake.set("system", "peak_refresh_rate", "120"); // the skin reset it
    assert_eq!(
        mgr.reassert(&phone, FULL, Some("j-ra")).unwrap(),
        ["system/peak_refresh_rate"]
    );
    assert_eq!(
        rig.fake.get("system", "peak_refresh_rate").as_deref(),
        Some("60")
    );
    assert_eq!(mgr.sink.count(CareEventKind::Reassert), 1);
    // a second apply of the same key keeps the ORIGINAL before
    mgr.apply(
        &phone,
        FULL,
        &owner,
        "system",
        "peak_refresh_rate",
        "90",
        ScopeReq::Job,
    )
    .unwrap();
    assert_eq!(mgr.changes_in_force("POCO0016").unwrap()[0].before, "120");
    mgr.restore_job(&phone, FULL, "j-ra").unwrap();
    assert_eq!(
        rig.fake.get("system", "peak_refresh_rate").as_deref(),
        Some("120")
    );
}

#[test]
fn session_and_until_scopes_end_at_their_own_time() {
    let rig = Rig::new("scopes", "POCO0017");
    let phone = AdbPhone::new(&rig.runner, "POCO0017");
    let mut mgr = rig.manager();
    let owner = Owner::here("j-sc", "sess-A");
    rig.fake.set("system", "screen_brightness", "128");
    rig.fake.set("global", "stay_on_while_plugged_in", "0");
    mgr.apply(
        &phone,
        FULL,
        &owner,
        "system",
        "screen_brightness",
        "1",
        ScopeReq::Session,
    )
    .unwrap();
    let until = rig.clock.unix_ms() + 10 * 60_000;
    mgr.apply(
        &phone,
        FULL,
        &owner,
        "global",
        "stay_on_while_plugged_in",
        "7",
        ScopeReq::Until(until),
    )
    .unwrap();
    // job end touches neither
    assert!(mgr.restore_job(&phone, FULL, "j-sc").unwrap().is_empty());
    // until has not passed
    assert!(mgr.restore_expired(&phone, FULL).unwrap().is_empty());
    rig.clock.advance(10 * 60_000);
    let r = mgr.restore_expired(&phone, FULL).unwrap();
    assert_eq!((r.len(), &r[0].1), (1, &RestoreStatus::Restored));
    assert_eq!(
        rig.fake
            .get("global", "stay_on_while_plugged_in")
            .as_deref(),
        Some("0")
    );
    // another session's end does not touch it; its own does
    assert!(mgr
        .restore_session(&phone, FULL, "sess-B")
        .unwrap()
        .is_empty());
    assert_eq!(
        mgr.restore_session(&phone, FULL, "sess-A").unwrap().len(),
        1
    );
    assert_eq!(
        rig.fake.get("system", "screen_brightness").as_deref(),
        Some("128")
    );
    // an until in the past is refused
    assert!(mgr
        .apply(
            &phone,
            FULL,
            &owner,
            "system",
            "screen_brightness",
            "1",
            ScopeReq::Until(rig.clock.unix_ms())
        )
        .is_err());
}

#[test]
fn stale_restore_leaves_live_owners_alone() {
    let rig = Rig::new("stale", "POCO0018");
    let phone = AdbPhone::new(&rig.runner, "POCO0018");
    let mut mgr = rig.manager();
    rig.fake.set("system", "screen_brightness", "128");
    rig.fake.set("system", "screen_brightness_mode", "1");
    let live = Owner {
        pid: 111,
        job_id: "live".into(),
        session: "s".into(),
    };
    let dead = Owner {
        pid: 222,
        job_id: "dead".into(),
        session: "s".into(),
    };
    mgr.apply(
        &phone,
        FULL,
        &live,
        "system",
        "screen_brightness",
        "1",
        ScopeReq::Job,
    )
    .unwrap();
    mgr.apply(
        &phone,
        FULL,
        &dead,
        "system",
        "screen_brightness_mode",
        "0",
        ScopeReq::Job,
    )
    .unwrap();
    rig.pids.kill(222);
    let r = mgr.restore_stale(&phone, FULL).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0.job_id, "dead");
    assert_eq!(
        rig.fake.get("system", "screen_brightness").as_deref(),
        Some("1"),
        "the live job's lease stays"
    );
    assert_eq!(
        rig.fake.get("system", "screen_brightness_mode").as_deref(),
        Some("1")
    );
}

#[test]
fn a_crash_between_the_record_and_the_put_cannot_leak_and_an_unreachable_phone_keeps_its_lease() {
    let rig = Rig::new("crashput", "POCO0019");
    let phone = AdbPhone::new(&rig.runner, "POCO0019");
    let mut mgr = rig.manager();
    let owner = Owner::here("j-cp", "s");
    rig.fake.set("system", "screen_brightness", "128");
    rig.fake.state().fail_prefixes = vec!["settings put".into()];
    assert!(mgr
        .apply(
            &phone,
            FULL,
            &owner,
            "system",
            "screen_brightness",
            "1",
            ScopeReq::Job
        )
        .is_err());
    assert_eq!(
        mgr.store.entries().unwrap().len(),
        1,
        "the record was written first"
    );
    rig.fake.state().fail_prefixes.clear();
    // restore: the phone never got the value, so CAS leaves it (it is the original anyway)
    assert_eq!(
        mgr.restore_job(&phone, FULL, "j-cp").unwrap()[0].1,
        RestoreStatus::Left
    );
    assert_eq!(
        rig.fake.get("system", "screen_brightness").as_deref(),
        Some("128")
    );

    mgr.apply(
        &phone,
        FULL,
        &owner,
        "system",
        "screen_brightness",
        "1",
        ScopeReq::Job,
    )
    .unwrap();
    rig.fake.state().offline = true;
    assert!(mgr.restore_job(&phone, FULL, "j-cp").is_err());
    assert_eq!(mgr.store.entries().unwrap().len(), 1, "kept for later");
    rig.fake.state().offline = false;
    rig.fake.state().fail_prefixes = vec!["settings put".into()];
    assert_eq!(
        mgr.restore_job(&phone, FULL, "j-cp").unwrap()[0].1,
        RestoreStatus::Unreachable
    );
    assert_eq!(
        mgr.store.entries().unwrap().len(),
        1,
        "an unverified restore keeps the lease"
    );
    rig.fake.state().fail_prefixes.clear();
    assert_eq!(
        mgr.restore_job(&phone, FULL, "j-cp").unwrap()[0].1,
        RestoreStatus::Restored
    );
    assert_eq!(
        rig.fake.get("system", "screen_brightness").as_deref(),
        Some("128")
    );
}

#[test]
fn never_panel_off_while_working() {
    assert!(panel_off_refusal("system", "screen_brightness", "0").is_some());
    assert!(panel_off_refusal("system", "screen_brightness", "x").is_some());
    assert!(panel_off_refusal("system", "screen_brightness", "1").is_none());
    assert!(panel_off_refusal("system", "screen_off_timeout", "15000").is_some());
    assert!(panel_off_refusal("system", "screen_off_timeout", "1800000").is_none());
    for (ns, key, v) in agent_bundle(&["miui_refresh_rate".into(), "user_refresh_rate".into()]) {
        assert!(panel_off_refusal(ns, key, &v).is_none(), "{key}={v}");
    }
    assert_eq!(
        agent_bundle(&[]).len(),
        5,
        "vendor keys only when the profile lists them"
    );
    assert_eq!(agent_bundle(&["user_refresh_rate".into()]).len(), 6);
    let rig = Rig::new("paneloff", "POCO0020");
    let phone = AdbPhone::new(&rig.runner, "POCO0020");
    let mut mgr = rig.manager();
    let owner = Owner::here("j-po", "s");
    assert!(mgr
        .apply(
            &phone,
            FULL,
            &owner,
            "system",
            "screen_brightness",
            "0",
            ScopeReq::Job
        )
        .is_err());
    assert!(mgr
        .apply(
            &phone,
            FULL,
            &owner,
            "system",
            "screen_off_timeout",
            "10000",
            ScopeReq::Job
        )
        .is_err());
    assert!(rig.fake.commands().is_empty());
    // bad names never reach the shell
    assert!(mgr
        .apply(
            &phone,
            FULL,
            &owner,
            "system",
            "x;reboot",
            "1",
            ScopeReq::Job
        )
        .is_err());
    assert!(mgr
        .apply(&phone, FULL, &owner, "vendor", "x", "1", ScopeReq::Job)
        .is_err());
    assert!(mgr
        .apply(&phone, FULL, &owner, "system", "k", "ABSENT", ScopeReq::Job)
        .is_err());
    assert!(mgr
        .apply(&phone, FULL, &owner, "system", "k", "a\nb", ScopeReq::Job)
        .is_err());
    assert!(rig.fake.commands().is_empty());
}

/// A file phone-guard wrote (Python `json.dump(indent=1)`, its field order and `%z` stamp).
const PHONE_GUARD_FILE: &str = "[\n {\n  \"key\": \"screen_brightness\",\n  \"namespace\": \"system\",\n  \"before\": \"128\",\n  \"applied\": \"20\",\n  \"owner_pid\": 4242,\n  \"job_id\": \"live-1\",\n  \"ts\": \"2026-10-19T10:00:00+0200\",\n  \"serial\": \"192.0.2.78:37123\",\n  \"ro_serial\": \"POCO0021\"\n },\n {\n  \"key\": \"temp\",\n  \"namespace\": \"battery_override\",\n  \"before\": \"ABSENT\",\n  \"applied\": \"set\",\n  \"owner_pid\": 4242,\n  \"job_id\": \"battery-temp\",\n  \"ts\": \"2026-10-19T10:00:01+0200\",\n  \"serial\": \"POCO0021\",\n  \"ro_serial\": \"POCO0021\"\n }\n]";

#[test]
fn leases_json_is_phone_guards_format_both_ways() {
    let rig = Rig::new("format", "POCO0021");
    let store = rig.store();
    std::fs::write(&store.path, PHONE_GUARD_FILE).unwrap();
    let entries = store.entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].ident(), "POCO0021");
    assert_eq!(
        render_entries(&entries),
        PHONE_GUARD_FILE,
        "re-rendered byte for byte as phone-guard writes it"
    );

    // What this crate writes for a job lease has exactly phone-guard's keys, in its order.
    let phone = AdbPhone::new(&rig.runner, "POCO0021");
    let mut mgr = LeaseManager::new(
        LeaseStore::new(rig.dir.join("own.json")),
        MemoryCareSink::default(),
        rig.clock.clone(),
        rig.pids.clone(),
    );
    mgr.apply(
        &phone,
        FULL,
        &Owner {
            pid: 7,
            job_id: "j".into(),
            session: String::new(),
        },
        "system",
        "screen_brightness",
        "1",
        ScopeReq::Job,
    )
    .unwrap();
    let text = std::fs::read_to_string(rig.dir.join("own.json")).unwrap();
    let keys: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix('"').and_then(|r| r.split('"').next()))
        .collect();
    assert_eq!(
        keys,
        [
            "key",
            "namespace",
            "before",
            "applied",
            "owner_pid",
            "job_id",
            "ts",
            "serial",
            "ro_serial"
        ]
    );
    assert!(
        text.starts_with("[\n {\n  \"key\": "),
        "python indent=1 layout"
    );
    // other scopes add only the optional keys
    mgr.apply(
        &phone,
        FULL,
        &Owner {
            pid: 7,
            job_id: "j".into(),
            session: "s1".into(),
        },
        "system",
        "screen_brightness_mode",
        "0",
        ScopeReq::Session,
    )
    .unwrap();
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(rig.dir.join("own.json")).unwrap()).unwrap();
    assert_eq!(v[1]["scope"], "session");
    assert_eq!(v[1]["session"], "s1");
    assert!(v[0].get("scope").is_none());

    // The schemas/leases.json shape is read too; unknown keys survive a rewrite.
    let schema_shape = r#"[{"key":"x","ns":"secure","before":"0","applied":"1","owner":{"pid":2,"job_id":"j2","session":"s"},"ts":"2026-10-19T10:00:01Z","future_field":42}]"#;
    let s2 = LeaseStore::new(rig.dir.join("schema.json"));
    std::fs::write(&s2.path, schema_shape).unwrap();
    let e = &s2.entries().unwrap()[0];
    assert_eq!(
        (
            e.namespace.as_str(),
            e.owner_pid,
            e.job_id.as_str(),
            e.session.as_deref()
        ),
        ("secure", 2, "j2", Some("s"))
    );
    assert_eq!(e.scope(), Scope::Job);
    s2.upsert(crate::leases::LeaseEntry {
        key: "y".into(),
        ..e.clone()
    })
    .unwrap();
    let back: Value = serde_json::from_str(&std::fs::read_to_string(&s2.path).unwrap()).unwrap();
    assert_eq!(back[0]["future_field"], 42);

    // atomic writes leave no temp or lock files behind; garbage is never overwritten
    let names: Vec<String> = std::fs::read_dir(&rig.dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .all(|n| !n.ends_with(".tmp") && !n.ends_with(".lock")),
        "{names:?}"
    );
    std::fs::write(&store.path, "{not json").unwrap();
    assert!(store.upsert(entries[0].clone()).is_err());
    assert_eq!(std::fs::read_to_string(&store.path).unwrap(), "{not json");
}
