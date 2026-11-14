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
| `--protected-ask-window-min` (how long an ask lasts, a daytime Deny holds, a re-allow may come, and the gap between two asks) | 10 | 1 to 60 |
| `--presence-file` and `--presence-key-file` (the overnight rule; both or neither) | none: the overnight rule is off | |
| `--presence-ttl-min` (how fresh the presence file must be) | 15 | 1 to 60 |

The code: `crates/core/src/protected.rs` (the lease, the screen rules, the holds file and the
presence file), `crates/adb-link/src/lease.rs` (the lease book that core and the adb guard share),
`crates/adb-link/src/guard.rs` (`GuardedRunner::with_leases`, `protected_policy`,
`GuardedRunner::with_s24_heads_up`), the heads-up check in
`crates/bridged/assets/idle-free-dump/Dump.java` and `crates/bridged/src/live.rs`, and the wiring
in `crates/core/src/transport.rs` and `crates/bridged/src/{main,server,jobs,devcall,devtools}.rs`.

## How a call on the protected phone goes

The checks run in this order, and the first one that refuses ends the call:

1. **The configured mode wins.** `helper` on the protected phone keeps its usual meaning.
   `read_only` on the protected phone answers the ruled reads only (a few `getprop` keys and the
   approved `doctor` probes), exactly as without the lease. Every other call, reads included
   (`ui_tree`, `screenshot`, `look`), is refused as a protected-phone call, and **nothing is
   asked**: an agent in `read_only` never sends the owner a push. Without `--protected-lease` the
   phone stays `read_only`, whatever it is configured as.
2. **Fixed refusals, before any lease.** These are refused under any lease and never cause an ask:
   - `unlock`, so the owner's PIN is never typed;
   - raw shell, `run_script` and link segments;
   - opening Gmail, Gmail Go or WhatsApp, a `mailto:`, or a `whatsapp:` link;
   - `open_url` naming one of the protected apps' web hosts (`mail.google.com`,
     `inbox.google.com`, `gmail.com`, `web.whatsapp.com`, `wa.me`, `whatsapp.com`), in any case and
     **whatever the package** (a browser shows the same mail and chats);
   - `type_text` (and `set_value`, which is sent as a `type_text`) whose text contains one of those
     hosts: typed into an address bar it would open the same pages. The match is a plain
     substring, so it is broader than it needs to be (`kiwa.media` contains `wa.me`);
   - a file path naming WhatsApp or Gmail (`whatsapp`, `gmail` or `com.google.android.gm`
     anywhere in it). The path is normalised first: `\` becomes `/`, and empty and `.` segments
     are dropped. The comparison ignores case. The name is looked for anywhere in the path, so a
     `..` cannot hide it. The legacy `/sdcard/WhatsApp/` folder is caught too. So is
     `com.google.android.gms`, which is broader than it needs to be.
3. **The daemon lease.** The caller must hold the daemon's device lease on the protected phone
   (`lease.acquire`). The protected lease's holder is **the caller's own** daemon lease (its
   holder name and epoch): `device.call` and the devtools calls name their connection, and a job
   names its connection and the epoch it started under. A job from an earlier grant, or a call
   from a connection that does not hold the phone, has no holder and is refused.
4. **The protected lease**, in `Protected::admit`:
   - **An open lease** admits the call when it belongs to this holder. A lease of another holder
     is closed (and logged), and the new holder starts from nothing. Every 10 seconds the lease's
     push is polled for a Deny.
   - **A hold** (after a Deny, or an ask that ended unanswered) refuses without asking, **whoever
     holds the phone**: a new holder inherits it.
   - **The pending ask.** At most one ask is out per phone, whoever it is for. It is checked
     without waiting. For its own holder: Allow opens the lease and Deny starts a hold. For
     another holder: while it is open the call is refused without a push; an Allow meant for a
     holder that no longer holds the phone opens nothing (`lease:stale`).
   - **Overnight** (only with a signed, fresh presence file saying `asleep`): a lease opens at
     once. The owner gets a notice that has only a **Deny** button. If the notice cannot be sent,
     no lease opens.
   - **Otherwise** one ask is sent, with **Allow** and **Deny** buttons, naming the holder, the job
     (if any) and the lease length, unless an ask went out within the last ask window (then the
     call is refused, and the next ask may go once the window has passed). The call is refused as
     pending, and nothing is sent to the phone. The agent sees a 6-character reference, never the
     request id.
   - **After an Allow** one quiet notice (push priority 2; asks go at 4) is sent with the same
     request id: "Lease on the protected phone open until HH:MM UTC (N min)", with a **Deny**
     button. Its Deny closes the lease. If it cannot be sent, the lease still opens (the owner
     allowed it) and `lease:notice_unsent` is logged; the owner can still reply `deny <id>`.
5. **The screen lock** (`crates/core/src/screenlock.rs`), for acting calls: see "One Allow, both
   leases" below.
6. **The screen rules** (below), on every read and every act under the lease.
7. **The usual guards.** The screen guard (D3) and the file guard run as on any phone. An action
   the screen guard would ask about is asked as usual under a daytime lease. Under an overnight
   lease it is refused, because nobody is awake to answer.

`open_app` is checked twice: the name as given (step 2), and, under the lease, the package it
resolves to. So a short name such as `atsap`, which resolves to WhatsApp, is refused before
anything is launched.

Core binds the adb name it checked to the lease. The adb guard (`GuardedRunner::with_leases`) lets
a command to the protected phone through only when its `-s` selector is a name bound under the
open lease. It refuses everything else, as before. Even under the lease, the guard still applies
the protected policy (`protected_policy`):

- only `shell`, `exec-out`, `push` and `pull` are sent;
- nothing that touches the helper's link;
- a `shell` or `exec-out` line goes only if it is one of **the exact shapes core sends**
  (`protected_shell_allowed`), and everything else is refused:
  - word for word: the screen size and rotation read, the wakefulness read, `pm list packages`,
    `screencap -p`, `uiautomator dump /dev/tty`, and the fallback screen read;
  - `input [-d N] tap|swipe|keyevent` with plain words, `input text` with core's escaped text,
    `getprop KEY`, `monkey -p PKG -c android.intent.category.LAUNCHER 1`, and `am start -W` with
    1 to 16 quoted arguments.

So `reboot`, `setprop`, `su`, `getevent`, `sendevent`, `svc`, `dumpsys notification`, package
installs and every chained line (`;`, `&&`, `|` outside the exact reads) are refused because they
are not on the list, not because a list of bad words names them.

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
- **Notification history, a notification log, or a notification-listener screen**, in any
  package: a resource id naming one (`notification_history`, `notification_log`,
  `notification_listener`, `notification_access` and their spellings), or a title bar (a
  toolbar, action bar or app bar) titled "Notification history", "Notification log",
  "Notification access" or "Device & app notifications". A list row with the same words is not a
  title bar, so the notification settings list stays readable.
- **Any node outside the launcher that names a protected app** (WhatsApp, Gmail, or Gmail's web
  hosts) in its text or description: a status-bar icon, a toast, a share sheet, a Settings app
  list, a browser tab. The launcher's own icon is the one exception (a touch on it is still
  refused).
- **Under the finger:** a tap, long press, swipe or indexed type whose point falls inside a node
  naming a protected app.
- **A PIN prompt:** while a PIN or passcode prompt is on screen, every digit key and every typed
  digit is refused, one at a time. So "12" and then "34" is refused as well as "1234".
- **A heads-up notification** (screenshots and `look` only; see below).

**One tree per call.** The tree read before a call (`protected_pre`) is the one the screen guard
judges and the one `ui_tree` returns, so a tap and a `ui_tree` read the protected screen once each
(round 1: twice each). A screenshot and `look` still read it a second time after the shot.

**Every settle poll is checked**, not only the first, so a job that settles while WhatsApp comes to
the front stops there. A screenshot is checked twice: the tree is read before the shot, and read
again after it. If either read shows a protected screen, or the screen cannot be read, the image is
dropped.

### Heads-up notifications

A heads-up notification is drawn in a System UI window, not the active window that
`uiautomator dump` reads, so the tree check cannot see it, and a screenshot would capture its
text. Before a screenshot (and `look`, and a job's flight-recorder frames, which are screenshots)
core asks whether one is showing; if one is, it waits 1.5 s and asks again, twice, and then
refuses. It asks once more after the shot, and drops the image if one appeared. If the check
cannot be answered, the image is refused too.

The check changes **no phone setting**. It is one exact read, sent only by its own runner:

```
adb -s <the protected phone> exec-out "CLASSPATH=/data/local/tmp/agentroid-dump.dex:/system/framework/uiautomator.jar app_process /system/bin dev.agentroid.dump.Dump --heads-up"
```

- It runs the same dump helper as the fallback screen read (pushed to `/data/local/tmp` once per
  daemon run). In `--heads-up` mode the helper connects as the shell's `UiAutomation`, asks for
  every window on screen (`FLAG_RETRIEVE_INTERACTIVE_WINDOWS`, on its own connection only), and
  walks the visible nodes of `com.android.systemui` for a notification row's resource id
  (`expandableNotificationRow`, `notification_row`, `notification_content`,
  `notification_main_column`, `heads_up`, `headsup`). It prints one line,
  `agentroid-hun: yes`, `no` or `unknown`, and nothing of what the notification says.
- Only `GuardedRunner::with_s24_heads_up` lets the line through, and only as `exec-out` plus this
  one argument (`Checked::is_s24_heads_up`); the plain guard, the leased guard, `doctor` and the
  MCP tools refuse it. It is not in `S24_READS`. The constant is `S24_HEADS_UP_READ`.
- The transport keeps only the yes or no (`live::parse_heads_up`); `unknown`, an error, or any
  other output is no answer, and the helper is pushed again next time.

**It needs the owner's yes before it is merged, and a check on a real phone.** Which resource ids a
heads-up carries on the protected phone's System UI has not been seen on that phone: a check that
says `no` while one shows would let its text into a screenshot. Until it has been tried there,
treat a screenshot under the lease as possibly showing a heads-up.

## One Allow, both leases

The screen lock (`crates/core/src/screenlock.rs`) has its own lease: it asks before acting on a
phone that is in use. Without care, the owner would tap Allow for the protected lease and then
Allow again for the screen lock. So:

- **An Allowed lease** (an Allow, or a re-allow) skips the screen lock's in-use check for the
  protected phone. Core records the lease as the screen lock's lease for that phone (so the
  screen lock counts the agent's own actions as the agent's, not the owner's), and it drops that
  phone's pending tails. One Allow covers both. **An `ask-screen` No still refuses**: while the
  owner's last `ask-screen` answer was No, no screen-changing call goes, Allow or not.
- **An overnight lease never skips it.** Overnight the owner allowed nothing, so the screen lock's
  own check runs on every acting call: the moment the owner picks the phone up (in use: a focused
  app, the keyboard, a recent touch), the agent stops.

The reverse does not hold: an `ask-screen` Yes does not open the protected lease. `ask-screen`'s
answer is an unsigned file on the laptop, so any local process could write it. The protected
lease's answer comes over the approval channel's separate inbound topic.

## Deny, holds and re-allow

- **A Deny on a daytime ask, on a daytime lease, or on the lease-open notice,** closes the lease
  and holds for the ask window (10 minutes by default). Calls are refused without asking. After
  the window the next call asks again.
- **A Deny overnight** (on the overnight notice) closes the lease and holds **until presence is no
  longer `asleep`**: the presence file says another state, freshly signed. Time passing does not
  end it. A stale, missing or badly signed file does not end it either, because the owner is not
  known to be awake. The overnight rule never reopens a lease while that hold stands.
- **A hold is for every holder.** Another holder, or the same lane under a new grant, inherits it.
- **An explicit re-allow** ends a hold at once, but only for **the holder and epoch that were
  denied**, and only **within the ask window after the Deny**. The owner replies `allow <id>` (with
  the denied request's id) on the inbound topic, and the lease opens as an approved one. An allow
  dated before the Deny, after the window, or picked up by another holder's call is no re-allow.
- **A Deny counts once.** Each Deny event is applied once, so an old Deny cannot close a lease that
  the owner re-allowed later. An answer is used up only by the request it names: another ask's
  poll (the POCO's own approval ask, say) never swallows the protected lease's Deny.
- **If the push channel cannot be read** while a lease is open, that is not taken as "no Deny".
  The failure is audited (`lease:deny_poll`). After 2 minutes without a successful read, the lease
  closes (`lease:deny_unreadable`).
- **An ask that expires unanswered** starts a daytime hold of one ask window, so an agent cannot
  send the owner a new push every few seconds. Separately, no ask goes out within the ask window
  of the last one, for any holder.

### The holds file

A hold outlives a restart. It is written to `protected-holds.json` in the daemon's `--state-dir`
whenever it starts or ends, by writing a temporary file beside it and renaming it over the old
one, and it is read when the daemon starts:

```json
{"version": 1, "hold": {"request_id": "...", "created_ms": 0, "expires_ms": 0,
  "holder": {"name": "lane-a", "epoch": 7}, "denied_at_ms": 0,
  "until": "window" | "awake" | "presence_change", "until_ms": 0, "presence_state": null}}
```

`"hold": null` is no hold. `until` says what ends it: `window` at `until_ms`, `awake` when a
trusted presence file says a state other than `asleep`, `presence_change` when a trusted presence
file's state differs from `presence_state` (the first trusted state seen, if none was known).

**A holds file that cannot be read** (not JSON, another version, a malformed hold, or an I/O error
other than "not found") fails closed: the phone is held until a trusted presence file's state
changes. The overnight rule does not open a lease while it holds, and no re-allow lifts it (the
denied request is not known). The owner can also end it by deleting the file while the daemon is
stopped. A missing file is no hold.

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
- An overnight lease ends as soon as the presence file stops saying a fresh, signed `asleep`, and
  every acting call under it passes the screen lock's own in-use check first.

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

## Known limits

These are not fixed by this design, and are recorded so nobody relies on them:

- **Same-user processes.** The daemon, its key files (`--presence-key-file`, the approval topic
  files) and its state directory belong to the owner's Windows account. Any process running as
  that account can read the topic files and forge an Allow on the inbound topic, sign its own
  presence file, or edit the holds file. The fix is a separate Windows account for `bridged`,
  whose files the owner's other processes cannot read; that is a deployment change, not a code
  one.
- **The outbound push carries the inbound URL.** The Allow and Deny buttons must post to the
  inbound topic, so the outbound push (which any reader of the outbound topic sees) contains the
  inbound topic's URL. Anyone who can read the outbound topic can therefore post an answer. The
  separate topic stops an agent that only has the outbound topic file; it does not stop one that
  reads the push itself. The 6-character reference narrows what such a reader can answer, but it
  is not a secret from it.

## Holder and job

The protected lease belongs to the daemon lease that opened it (holder name and epoch), and a
call's holder is always the caller's own (see step 3). It closes when:

- the holder's connection goes (`release_conn`);
- the holder releases the phone (`lease.release`);
- another holder's call reaches it;
- it expires;
- an overnight lease's presence file stops saying `asleep`.

A job runs under its caller's epoch, and the daemon names the job's run id for that epoch while
the job runs. The ask and the lease therefore say which job they are for, and the name goes when
the job ends.

## The audit log

Every lease event is an audit entry (`allowed`, `blocked` or `asked`), with these rules:

`lease:asked`, `lease:opened`, `lease:overnight`, `lease:reallowed`, `lease:denied`,
`lease:expired`, `lease:closed`, `lease:stale`, `lease:hold_ended`, `lease:notice_unsent`,
`lease:deny_poll`, `lease:deny_unreadable`.

- **Before it opens.** `lease:opened`, `lease:overnight` and `lease:reallowed` are written before
  the lease opens. If the write fails, the call is refused and no lease opens (a failed re-allow
  write leaves the hold in place).
- **Every close**, with its reason: `released`, `connection closed` and `expired` (from the lease
  book's queue of closes), another holder, an overnight lease whose presence ended, a Deny, a Deny
  channel that could not be read. A release is logged at once; a dropped connection at once when
  the backend is free, else at the next call; an expiry at the next call.

If an audit entry cannot be written, the call is refused. None of these writes is ignored.

## Tests

`crates/core/tests/protected_lease.rs` (core and the approver together, with a manual clock),
`crates/bridged/tests/protected_lease.rs` (through the daemon: holder, connection close,
release, jobs, files, the close audits), `live::tests::behind_the_guard_one_allow_lets_core_act_on_the_protected_phone_and_nothing_else_rides_on_it`
(core's lease and the adb guard together over a scripted adb),
`live::tests::the_heads_up_check_keeps_only_a_yes_or_no_and_runs_through_its_own_runner`,
`bridged/tests/devtools.rs` (`doctor` refuses the single-purpose reads), and the unit tests in
`protected.rs`, `lease.rs`, `guard.rs`, `approval.rs`, `bridged/src/server.rs` (the caller's
holder) and `bridged/src/main.rs`.
