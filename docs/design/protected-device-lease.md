# The protected-device lease

The protected phone (`S24_SERIAL` in `crates/adb-link/src/guard.rs`, `PROTECTED0000EXAMPLE` in
this repository) is `read_only` by default: it answers a fixed set of reads and nothing else. The
protected-device lease lets an agent work on it after the owner allows it once. Agentroid's aim
is efficiency, not lockdown, so the gate asks once and then gets out of the way. One Allow opens a
lease for a burst of work (30 minutes by default). It does not ask again on every tap.

The lease is **off by default**. `bridged --protected-lease` turns it on, and the daemon refuses
to start with it unless:

- `--adb` is given (the live transport);
- `--bench-approve` is not given;
- there is a real approval channel: `--approval-topic-file` and `--approval-inbound-file`,
  naming two *different* topics (see "Answers cannot be forged by reading the push" below).

Its other flags need `--protected-lease`; on their own the daemon refuses them.

| Flag | Default | Clamp |
| --- | --- | --- |
| `--protected-lease-min` | 30 | 1 to 120 |
| `--protected-ask-window-min` (how long an ask, or a daytime Deny, lasts) | 10 | 1 to 60 |
| `--presence-file` and `--presence-key-file` (the overnight rule; both or neither) | none: the overnight rule is off | |
| `--presence-ttl-min` (how fresh the presence file must be) | 15 | 1 to 60 |

The code: `crates/core/src/protected.rs` (the lease, the screen rules and the presence file),
`crates/adb-link/src/lease.rs` (the lease book that core and the adb guard share),
`crates/adb-link/src/guard.rs` (`GuardedRunner::with_leases`), and the wiring in
`crates/core/src/transport.rs` and `crates/bridged/src/{main,server,jobs,devtools}.rs`.

## How a call on the protected phone goes

The checks run in this order, and the first one that refuses ends the call:

1. **The configured mode wins.** `read_only` or `helper` on the protected phone keeps its usual
   meaning. A read-only call is answered as it always was, and nothing is asked. Without
   `--protected-lease` the phone stays `read_only`, whatever it is configured as.
2. **Fixed refusals, before any lease.** These are refused under any lease and never cause an ask:
   - `unlock`, so the owner's PIN is never typed;
   - raw shell, `run_script` and link segments;
   - opening Gmail, Gmail Go or WhatsApp, a `mailto:`, a `wa.me` link or a `whatsapp:` link;
   - a file path naming WhatsApp or Gmail (`whatsapp`, `gmail` or `com.google.android.gm`
     anywhere in it). The path is normalised first: `\` becomes `/`, and empty and `.` segments
     are dropped. The comparison ignores case. The name is looked for anywhere in the path, so a
     `..` cannot hide it. The legacy `/sdcard/WhatsApp/` folder is caught too. So is
     `com.google.android.gms`, which is broader than it needs to be.
3. **The daemon lease.** The caller must hold the daemon's device lease on the protected phone
   (`lease.acquire`). Its holder name and epoch are the protected lease's holder.
4. **The protected lease**, in `Protected::admit`:
   - **An open lease** admits the call when it belongs to this holder. A lease of another holder
     is closed, and the new holder starts from nothing. Every 10 seconds the lease's push is
     polled for a Deny.
   - **A hold** (after a Deny) refuses without asking.
   - **A pending ask** is checked without waiting. Allow opens the lease and Deny starts a hold.
     A pending ask belongs to the holder it was sent for: another holder's Allow never opens one
     for it.
   - **Overnight** (only with a signed, fresh presence file saying `asleep`): a lease opens at
     once. The owner gets a notice that has only a **Deny** button. If the notice cannot be sent,
     no lease opens.
   - **Otherwise** one ask is sent, with **Allow** and **Deny** buttons, naming the holder, the job
     (if any) and the lease length. The call is refused as pending, and nothing is sent to the
     phone. The agent sees a 6-character reference, never the request id.
5. **The screen rules** (below), on every read and every act under the lease.
6. **The usual guards.** The screen guard (D3) and the file guard run as on any phone. An action
   the screen guard would ask about is asked as usual under a daytime lease. Under an overnight
   lease it is refused, because nobody is awake to answer.

Core binds the adb name it checked to the lease. The adb guard (`GuardedRunner::with_leases`) lets
a command to the protected phone through only when its `-s` selector is a name bound under the
open lease. It refuses everything else, as before. Even under the lease, the guard still applies
the protected policy:

- only `shell`, `exec-out`, `push` and `pull` are sent;
- nothing that touches the helper's link;
- no `reboot`, `setprop`, `su`, `getevent` or `sendevent`;
- no `svc usb`, `svc wifi` or `svc data`;
- no package installs.

The guard therefore never trusts a lease on its own. If `getprop ro.serialno` fails and core takes
the phone for an ordinary device, core binds nothing, and the guard refuses the phone.

## The screen rules

Under the lease, a read (`look`, `screenshot`, `ui_tree`, `settle`) or an act on the protected
phone is refused when the screen shows any of these:

- **A protected app** in front: Gmail (`com.google.android.gm` and every `com.google.android.gm.*`
  package, Gmail Go included) or WhatsApp (any package containing `whatsapp`).
- **The keyguard, the notification shade or Recents.** All three belong to `com.android.systemui`
  (or the launcher), not to the app whose messages they preview, so a package check alone would
  miss them. The markers are resource ids: keyguard, notification rows and panels, and Recents and
  overview task views. An escape key (back, home, escape) is still allowed there.
- **A system bar or notification that names a protected app.**
- **Under the finger:** a tap, long press, swipe or indexed type whose point falls inside a
  protected app's node.
- **A PIN prompt:** while a PIN or passcode prompt is on screen, every digit key and every typed
  digit is refused, one at a time. So "12" and then "34" is refused as well as "1234".

**Every settle poll is checked**, not only the first, so a job that settles while WhatsApp comes to
the front stops there. A screenshot is checked twice: the tree is read before the shot, and read
again after it. If either read shows a protected screen, or the screen cannot be read, the image is
dropped.

## One Allow, both leases

The screen lock (`crates/core/src/screenlock.rs`) has its own lease: it asks before acting on a
phone that is in use. Without care, the owner would tap Allow for the protected lease and then
Allow again for the screen lock. Under an open protected lease, core skips the screen lock's
in-use check for the protected phone. It records the lease as the screen lock's lease for that
phone (so the screen lock counts the agent's own actions as the agent's, not the owner's), and it
drops that phone's pending tails. So one Allow covers both.

The reverse does not hold: an `ask-screen` Yes does not open the protected lease. `ask-screen`'s
answer is an unsigned file on the laptop, so any local process could write it. The protected
lease's answer comes over the approval channel's separate inbound topic.

## Deny, holds and re-allow

- **A Deny on a daytime ask, or on a daytime lease,** closes the lease and holds for the ask
  window (10 minutes by default). Calls are refused without asking. After the window the next
  call asks again.
- **A Deny overnight** (on the overnight notice) closes the lease and holds **until presence is no
  longer `asleep`**: the presence file says another state, freshly signed. Time passing does not
  end it. A stale, missing or badly signed file does not end it either, because the owner is not
  known to be awake. The overnight rule never reopens a lease while that hold stands.
- **An explicit re-allow** ends any hold at once. The owner replies `allow <id>` (with the denied
  request's id) on the inbound topic, and the lease opens as an approved one.
- **A Deny counts once.** Each Deny event is applied once, so an old Deny cannot close a lease that
  the owner re-allowed later.
- **If the push channel cannot be read** while a lease is open, that is not taken as "no Deny".
  The failure is audited (`lease:deny_poll`). After 2 minutes without a successful read, the lease
  closes (`lease:deny_unreadable`).
- **An ask that expires unanswered** starts a daytime hold of one ask window, so an agent cannot
  send the owner a new push every few seconds.

## The overnight rule

The owner allowed the protected phone to be used overnight without being asked. The rule is kept,
but it opens a lease only when a **presence file** says the owner is asleep, and only when that
file can be trusted:

- It is JSON: `{"state": "asleep", "written_at": <Unix ms>, "mac": "<hex>"}`. The `mac` is
  HMAC-SHA256, keyed with the contents of `--presence-key-file` (at least 16 bytes once trimmed),
  over `agentroid-presence-v1\n<state>\n<written_at>`.
- `written_at` must be no older than the TTL (15 minutes by default). It must also be no more than
  2 minutes in the future.
- A missing key, an unreadable, malformed, unsigned, badly signed, stale or future-dated file
  turns the overnight rule **off**. The lease then asks, as in the day. It never fails open.
- An overnight lease ends as soon as the presence file stops saying a fresh, signed `asleep`.

Whatever writes the presence file (the owner's presence tracker) must rewrite it more often than
the TTL.

**What the signature does not stop.** The key is a file on the laptop. A process running as the
same user that can read the key file can sign its own `asleep`. Keep the key where agents' working
folders and tools cannot read it (outside any repository, readable by the presence writer and the
daemon only). The signature closes the old hole, in which any process could write `asleep` in
plain text and last night's file counted all day. It is not a defence against malware running as
the owner.

## Answers cannot be forged by reading the push

With `--protected-lease` the approval channel requires a separate inbound topic. It checks this on
every send and every poll (`NtfyConfig::require_separate_inbound`), and the daemon checks it again
at start-up. An Allow posted to the outbound topic, which any reader of the push can see, is never
read. An HMAC on the answers is not possible here, because ntfy's action buttons post a fixed body.
The agent is told only a 6-character reference, and the audit log records only that reference, so
an agent cannot learn the full request id it would need to answer.

## Holder and job

The protected lease belongs to the daemon lease that opened it (holder name and epoch). It
closes when:

- the holder's connection goes (`release_conn`);
- the holder releases the phone (`lease.release`);
- another holder's call reaches it;
- it expires.

A job runs under its caller's epoch, and the daemon names the job's run id for that epoch while
the job runs. The ask and the lease therefore say which job they are for, and the name goes when
the job ends.

## The audit log

Every lease event is an audit entry (`allowed`, `blocked` or `asked`), with these rules:

`lease:asked`, `lease:opened`, `lease:overnight`, `lease:reallowed`, `lease:denied`,
`lease:expired`, `lease:closed`, `lease:stale`, `lease:deny_poll`, `lease:deny_unreadable`.

If the audit entry cannot be written, the call is refused. None of these writes is ignored.

## Tests

`crates/core/tests/protected_lease.rs` (core and the approver together, with a manual clock),
`crates/bridged/tests/protected_lease.rs` (through the daemon: holder, connection close,
release, jobs, files), `live::tests::behind_the_guard_one_allow_lets_core_act_on_the_protected_phone_and_nothing_else_rides_on_it`
(core's lease and the adb guard together over a scripted adb), and the unit tests in
`protected.rs`, `lease.rs`, `guard.rs`, `approval.rs` and `bridged/src/main.rs`.
