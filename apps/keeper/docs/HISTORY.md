# How the keeper got here

This is the keeper's timeline from the monorepo it was written in, with the real dates. Nothing was backdated. Agentroid's public repository starts from a single commit, so the commit hashes below come from the keeper's standalone repository and are not in this one. What was removed from that history: phone hardware serials and a home-network address (replaced with `EXAMPLE_SERIAL_*` / `192.0.2.126`), and the build output folder, which held an old throwaway signing key.

| Date | Commit | What happened |
| --- | --- | --- |
| 20 Oct 2026 | `302743d` | **v0.1 spike.** A 21 KB APK that re-enables Wireless debugging, looks up the port with NSD and posts an outbound heartbeat. 30 logic tests. The setup script refuses a protected phone by hardware serial. No phone was connected that day, so nothing ran on a device. |
| 20 Oct 2026 | `22a1591` | Heartbeat contract fixed against the laptop-side watchdog: the serial is passed at setup (an app cannot read `ro.serialno`), and `0`/`1`/`-1` are all accepted for the adb state. |
| 22 Oct 2026 | `c24d8a3` | `KeeperService` is not exported, so `adb` cannot start it ("Requires permission not exported"). Setup now goes through the launcher activity, which forwards the extras. First run on a phone over Wi-Fi adb. |
| 28 Oct 2026 | `4f80c6e` | **v0.2.** Found why the keeper itself died: a test plan ran `am force-stop` on it, and Android delivers neither boot broadcasts nor alarms to a force-stopped package until the user launches it, so the "restore" line (which tried to start the unexported service directly) could never bring it back. Added a self-re-arming exact-alarm chain, direct-boot awareness, a timed pause, and a restore that goes through the launcher activity. |
| 1 Nov 2026 | `f0d5dc9` | **v0.3.** Wi-Fi drop tests (K2) showed slow rejoins with the screen off, so the keeper now wakes the screen briefly while the network is gone. (Later the same day the slow rejoins turned out to be mostly the drop script's own `sleep` stalling while the phone slept, so the nudge may be a safety net rather than a fix.) |
| 1 Nov 2026 | `ee7dbb4` | The nudge timer itself stopped while the CPU slept, so a partial wake lock now covers the outage (it times out on its own after 10 min). |
| 4 Nov 2026 | `ebcf934` | A second phone joined the fleet and was added to the setup script's allow-list. (Public version: the allow-list is now `KEEPER_REFUSE_SERIALS`, a deny-list you set yourself.) |

## What changed for the public version

- The hub URL is optional config; with none set the keeper sends nothing.
- A failed heartbeat logs one short line and moves on (no stack trace, no retry loop).
- `setup-keeper.sh` takes the serial (and optionally the hub) as arguments and has no device list baked in.
- The signing key is generated into `~/.keys` on first build with a random password, never inside the repo. The old repo-local throwaway key is gone from the published history.
- Wording tidied; behaviour unchanged. The 40 logic tests pass.

## Lessons worth keeping

- A foreground service is not enough on HyperOS: it needs the battery allow-list, the Autostart app-ops and an alarm backstop, and even then it can be killed.
- Test scripts lie: two "slow Wi-Fi" findings came from `sleep` inside `adb shell` stalling while the phone suspended. Time things on the phone, not in the shell that drives it.
- You cannot read the real hardware serial from an app; have the computer read it over adb and hand it over.
