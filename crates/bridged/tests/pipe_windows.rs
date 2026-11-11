//! Real named pipe: `\\.\pipe\agentroid-test-<pid>`. Windows only; still no device involved.
#![cfg(windows)]

use agentroid_core::fake::{FakeClock, FakeDevice};
use agentroid_core::transport::Core;
use bridged::pipe::{current_user_sid, PipeListener};
use bridged::registry::Registry;
use bridged::server::{self, CoreBackend, Daemon};
use serde_json::{json, Value};
use std::os::windows::io::AsRawHandle;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::sync::watch;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

struct PipeClient {
    r: BufReader<tokio::io::ReadHalf<NamedPipeClient>>,
    w: tokio::io::WriteHalf<NamedPipeClient>,
}

impl PipeClient {
    async fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.w
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        assert!(self.r.read_line(&mut line).await.unwrap() > 0);
        serde_json::from_str(&line).unwrap()
    }
}

/// Named-pipe clients must retry on ERROR_PIPE_BUSY (231): between two accepts no instance is
/// listening yet. The thin MCP client needs the same loop.
async fn open_retrying(name: &str) -> NamedPipeClient {
    loop {
        match ClientOptions::new().open(name) {
            Ok(c) => return c,
            Err(e) if e.raw_os_error() == Some(231) => {
                tokio::time::sleep(Duration::from_millis(5)).await
            }
            Err(e) => panic!("open pipe: {e}"),
        }
    }
}

async fn connect(name: &str) -> PipeClient {
    let (r, w) = tokio::io::split(open_retrying(name).await);
    PipeClient {
        r: BufReader::new(r),
        w,
    }
}

fn dacl_text(client: &NamedPipeClient) -> String {
    // SAFETY: the handle is live for the call; the out-pointers receive LocalAlloc'd memory freed below.
    unsafe {
        let mut sd = std::ptr::null_mut();
        let err = GetSecurityInfo(
            client.as_raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        assert_eq!(err, 0, "GetSecurityInfo failed: {err}");
        let mut wide: *mut u16 = std::ptr::null_mut();
        assert_ne!(
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut wide,
                std::ptr::null_mut()
            ),
            0
        );
        let mut n = 0;
        while *wide.add(n) != 0 {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(wide, n));
        LocalFree(wide.cast());
        LocalFree(sd);
        s
    }
}

#[test]
fn real_pipe_round_trip_first_instance_and_current_user_only_dacl() {
    let name = format!(r"\\.\pipe\agentroid-test-{}", std::process::id());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(20), async {
            let daemon = Daemon::new(
                Registry::in_memory(),
                Box::new(CoreBackend::new(Core::new(
                    FakeDevice::new(),
                    FakeClock::new(),
                ))),
            );
            let listener = PipeListener::bind(&name).expect("first instance");

            // Nobody can squat the name: a second first-instance claim fails while the daemon holds it.
            assert!(
                PipeListener::bind(&name).is_err(),
                "a second first-instance claim on the same name must fail"
            );

            let (tx, rx) = watch::channel(false);
            let serving = tokio::spawn(server::serve(daemon.clone(), listener, rx));

            let mut a = connect(&name).await;
            let hello = a.call(1, "hello", json!({ "session": "pipe-a" })).await;
            assert_eq!(hello["result"]["holder"], "pipe-a");

            // The kernel object's DACL names exactly the current user.
            let probe = open_retrying(&name).await;
            let dacl = dacl_text(&probe);
            // SDDL writes well-known SIDs as aliases: the built-in Administrator (RID 500, which a
            // GitHub Windows runner runs as) appears as `LA`, not as its full SID.
            let sid = current_user_sid().unwrap();
            let as_alias = sid.ends_with("-500") && dacl.contains(";;;LA)");
            assert!(dacl.contains(&sid) || as_alias, "{dacl}");
            assert_eq!(dacl.matches('(').count(), 1, "one ACE only: {dacl}");
            assert!(dacl.starts_with("D:P"), "protected: {dacl}");
            drop(probe);

            // Two real clients contend for one fake device; the first is named in the refusal.
            let mut b = connect(&name).await;
            b.call(1, "hello", json!({ "session": "pipe-b" })).await;
            let r = a
                .call(2, "lease.acquire", json!({ "device": "DEVICE0000EXAMPLE" }))
                .await;
            assert_eq!(r["result"]["epoch"], 1);
            let r = b
                .call(2, "lease.acquire", json!({ "device": "DEVICE0000EXAMPLE" }))
                .await;
            assert_eq!(r["error"]["code"], -32001);
            assert!(r["error"]["message"].as_str().unwrap().contains("pipe-a"));

            // A client vanishing frees the lease and does not disturb the other connection.
            drop(a);
            let mut freed = false;
            for i in 0..200 {
                let r = b
                    .call(
                        10 + i,
                        "lease.acquire",
                        json!({ "device": "DEVICE0000EXAMPLE" }),
                    )
                    .await;
                if r["result"]["epoch"] == 2 {
                    freed = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(freed, "lease not released after the pipe client vanished");

            tx.send(true).unwrap();
            serving.await.unwrap().unwrap();
            // The listener is gone with the task, so the name can be claimed again.
            drop(b);
            let again = PipeListener::bind(&name);
            assert!(
                again.is_ok(),
                "name is free after the daemon stopped: {:?}",
                again.err()
            );
        })
        .await
        .expect("pipe test timed out");
    });
}
