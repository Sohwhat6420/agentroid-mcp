# The protected-device lease

The protected phone (`S24_SERIAL` in `crates/adb-link/src/guard.rs`, `PROTECTED0000EXAMPLE` in
this repository) is `read_only` by default: it answers a fixed set of reads and nothing else. The
protected-device lease lets an agent work on it once the owner has allowed it on a push.

**The gate is approval, not content.** Nothing on the phone is refused for what it shows: any app,
any screen, any file path. What the lease guards is **who may take the phone over, and when**: an
agent gets it only after the owner taps Allow (or, overnight, under the owner's standing rule), and
a Deny ends it. Agentroid's aim is efficiency, not lockdown, so the gate asks once and then gets
out of the way. One Allow opens a lease for a burst of work (30 minutes by default). It does not
ask again on every tap.

## Rulings

The owner's rulings for this design, word for word (14 Nov 2026):

> "No dont block anything from being seen, forget what i said, just send a notification for
> approval before s24u is taken over"

> "Overnight still skips it (Recommended)"

So the earlier content rules (protected apps, their web hosts and file paths, the keyguard, the
notification shade, Recents, notification history, and the heads-up check) are gone. The approval
push, the engineering around it (holds, one ask per phone, the audit log), the fixed refusals that
protect the phone itself (the PIN, `unlock`, code, raw shell, link segments) and the overnight rule
stay.

## Turning it on

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

Asks go to the push channel at priority 4; the quiet notices (the lease-open notice and the
overnight notice) at priority 2.

The code: `crates/core/src/protected.rs` (the lease, the PIN rule, the holds file and the presence
file), `crates/adb-link/src/lease.rs` (the lease book that core and the adb guard share),
`crates/adb-link/src/guard.rs` (`GuardedRunner::with_leases`, `protected_policy`,
`code_push_refusal`, `GuardedRunner::with_s24_helper`), the dump helper's integrity check in
`crates/bridged/src/live.rs`, and the wiring in `crates/core/src/transport.rs` and
`crates/bridged/src/{main,server,jobs,devcall,devtools}.rs`.

## How a call on the protected phone goes

The checks run in this order, and the first one that refuses ends the call:

1. **The configured mode wins.** `helper` on the protected phone keeps its usual meaning.
   `read_only` on the protected phone answers the ruled reads only (a few `getprop` keys and the
   approved `doctor` probes), exactly as without the lease. Every other call, reads included
   (`ui_tree`, `screenshot`, `look`), is refused as a protected-phone call, and **nothing is
   asked**: an agent in `read_only` never sends the owner a push. Without `--protected-lease` the
   phone stays `read_only`, whatever it is configured as.
2. **Fixed refusals, before any lease.** These protect the phone itself, not anything it shows.
   They are refused under any lease and never cause an ask:
   - `unlock`, so the owner's PIN is never typed;
   - raw shell, `run_script` and link segments;
   - **code pushed to the phone** (rule `code`): a push whose phone path, or whose laptop file,
     ends in `.dex`, `.apk`, `.apks`, `.apex`, `.jar`, `.so`, `.odex`, `.vdex`, `.oat`, `.art`,
     `.xapk` or `.aab` (any case), and any push into `/data/local/tmp` (where `app_process` and the
     shell run code from) or onto the dump helper's path. The path is normalised first (`\`
     becomes `/`; empty, `.` and `..` segments are resolved), so `/sdcard/../data/local/tmp/x` is
     caught. Core checks the phone path; the adb guard checks both sides again. Pulls are reads and
     are not refused.
3. **The daemon lease.** The caller must hold the daemon's device lease on the protected phone
   (`lease.acquire`). The protected lease's holder is **the caller's own** daemon lease (its
   holder name and epoch): `device.call` and the devtools calls name their connection, and a job
   names its connection and the epoch it started under. A job from an earlier grant, or a call
   from a connection that does not hold the phone, has no holder and is refused.
4. **The protected lease**, in `Protected::admit`:
   - **Closes first.** Every close the lease book has queued (released, connection closed,
     expired) is written to the audit log before anything else. A close that cannot be written
     stays queued, with every later one behind it, and the call is refused (`Refusal::Audit`)
     until it is written.
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
     once, with no ask. The owner gets a notice that has only a **Deny** button: "Overnight: Agent
     session: NAME took over the protected phone for N min (the presence file says asleep). Tap
     Deny to end it." If the notice cannot be sent, no lease opens.
   - **Otherwise** one ask is sent, with **Allow** and **Deny** buttons: "Agent session: NAME, job
     RUN asks to take over the protected phone for N min (first call: TOOL). Allow covers every
     call for N min; Deny refuses." It is not sent if an ask went out within the last ask window
     (then the call is refused, and the next ask may go once the window has passed). The call is
     refused as pending, and nothing is sent to the phone. The agent sees a 6-character
     reference, never the request id.
   - **After an Allow** one quiet notice is sent with the same request id: "Lease on the protected
     phone open until HH:MM SGT (N min)", in Singapore time (UTC+8, no daylight saving), with a
     **Deny** button. Its Deny closes the lease. If it cannot be sent, the lease still opens (the
     owner allowed it) and `lease:notice_unsent` is logged; the owner can still reply `deny <id>`.
5. **The screen lock** (`crates/core/src/screenlock.rs`), for acting calls: see "One Allow, both
   leases" below.
6. **The PIN rule.** While a PIN or passcode prompt is on screen, every digit key, every typed
   digit and every touch on a digit key is refused, one at a time, so "12" and then "34" is
   refused as well as "1234". This is the one screen rule left, and it guards the owner's PIN, not
   what the screen shows.
7. **The usual guards.** The screen guard (D3) and the file guard run as on any phone. An action
   the screen guard would ask about is asked as usual under a daytime lease. Under an overnight
   lease it is refused, because nobody is awake to answer.

### The holder's name in the push

The holder name comes from the agent's own connection, so the push never shows it raw
(`protected::push_holder`): words that look like a link (a scheme such as `https://`, `www.`, or a
dot and a slash) are dropped, control characters and runs of spaces are taken out, the rest is cut
to 24 characters, and it is shown after "Agent session:" so it never reads as the daemon's own
words. A name with nothing left reads "Agent session: (no name)".

### Screen reads per call

Only a call that could enter a digit (`type_text`, `tap`, `long_press`, `swipe`, and a `key` that is
not back, home or escape) reads the screen for the PIN check, and it uses the stored tree when that
is fresh (within the screen guard's tree age) and nothing has acted since. The screen guard and a
selector then use the same tree. So:

| Call | Screen dumps, round 2 | Screen dumps, now |
| --- | --- | --- |
| `tap` | 1 | 1, or 0 on a fresh, clean stored tree |
| `ui_tree` | 1 | 1 |
| `ui_tree` with `require` (met on the first poll) | 2 | 1 |
| `screenshot` | 2, plus 2 heads-up checks | 0 |
| `look` | 2, plus 2 heads-up checks | 1 |
| `type_text` by index | 2 | 1 |
| `key back`, `scroll`, `open_app` | 0 | 0 |

`crates/core/tests/protected_lease.rs`, `each_call_reads_the_protected_screen_at_most_once`, holds
these numbers.

## The adb guard under the lease

Core binds the adb name it checked to the lease. The adb guard (`GuardedRunner::with_leases`) lets
a command to the protected phone through only when its `-s` selector is a name bound under the
open lease. It refuses everything else, as before. Even under the lease, the guard still applies
the protected policy (`protected_policy`):

- only `shell`, `exec-out`, `push` and `pull` are sent;
- nothing that touches the helper's link;
- no push of code (`code_push_refusal`, as in step 2, on the laptop file and the phone path);
- a `shell` or `exec-out` line goes only if it is one of **the exact shapes core sends**
  (`protected_shell_allowed`), and everything else is refused:
  - word for word: the screen size and rotation read, the wakefulness read, `pm list packages`,
    `screencap -p`, `uiautomator dump /dev/tty`, and the fallback screen read;
  - `input [-d N] tap|swipe|keyevent` with plain words, `input text` with core's escaped text,
    `getprop KEY`, `monkey -p PKG -c android.intent.category.LAUNCHER 1`, and `am start -W` with
    1 to 16 quoted arguments;
  - the file tools' reads (push's read-back and `pull_new`'s listing, size and hash), each with
    one path word that is not empty, at most 1024 characters and does not start with `-`:
    `sha256sum <path>`, `ls -1tp <dir>` and `stat -c %s <path>`.

So `reboot`, `setprop`, `su`, `getevent`, `sendevent`, `svc`, `dumpsys notification`, package
installs and every chained line (`;`, `&&`, `|` outside the exact reads) are refused because they
are not on the list, not because a list of bad words names them.

The guard therefore never trusts a lease on its own. If `getprop ro.serialno` fails and core takes
the phone for an ordinary device, core binds nothing, and the guard refuses the phone.

### The dump helper's integrity

`uiautomator dump` gives up on a screen that never goes idle, and the live transport then falls
back to its dump helper (`crates/bridged/assets/idle-free-dump/`), run with `app_process` from
`/data/local/tmp/agentroid-dump.dex`. That fallback runs on any phone, the protected one included,
so the helper is code that runs on the protected phone, and it is checked before it runs there:

- The daemon keeps its own copy in its state directory (`agentroid-dump.dex`), written only when
  the bytes differ, by a temporary file and a rename.
- Before the helper runs on the protected phone, the transport reads its hash on the phone through
  the helper's own runner (`GuardedRunner::with_s24_helper`), as exactly
  `shell sha256sum /data/local/tmp/agentroid-dump.dex` (`DUMP_HELPER_HASH_READ`). Only a first
  line of 64 lower-case hex digits and the helper's path, and nothing else, is a hash
  (`live::parse_helper_hash`).
- If it matches the hash of the copy built into the daemon, the helper runs. If it does not (or
  there is no file), the state directory's copy is pushed through the same runner, as exactly
  `push <the staged copy> /data/local/tmp/agentroid-dump.dex`, and the hash is read again. A
  second mismatch, or a hash that cannot be read, refuses: the helper never runs unchecked.
- A match holds for 5 minutes (`HELPER_RECHECK`) and for the life of the process; a failed run
  clears it.
- Those two lines are the only ones the helper's runner adds, and only under an open lease. Every
  other runner refuses the push (no other code can be pushed, step 2). The hash read is the file
  tools' `sha256sum <path>` shape, so the leased guard sends it too, as a read; the plain guard,
  `doctor` and the MCP tools refuse it.

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
- **An explicit re-allow** ends a hold after a Deny at once, but only for **the holder and epoch
  that were denied**, and only **within the ask window after the Deny**. The owner replies
  `allow <id>` (with the denied request's id) on the inbound topic, and the lease opens as an
  approved one. An allow dated before the Deny, after the window, or picked up by another holder's
  call is no re-allow.
- **An ask that ends unanswered** starts a daytime hold of one ask window, so an agent cannot send
  the owner a new push every few seconds, and **no Allow lifts it**: a late Allow is no answer.
  Separately, no ask goes out within the ask window of the last one, for any holder.
- **An ask is strict about its end.** An Allow dated after the ask's window never opens the
  phone, even when it arrives while the ask still looks open on the laptop's clock (a phone clock
  that runs ahead). Other approval asks keep their 60-second grace.
- **A Deny counts once, across restarts too.** Each Deny event is applied once, so an old Deny
  cannot close a lease that the owner re-allowed later. The Deny event ids seen are kept in the
  holds file (the newest 64) and handed back to the approver at the first call after a restart.
  An answer is used up only by the request it names: another ask's poll (the POCO's own approval
  ask, say) never swallows the protected lease's Deny.
- **If the push channel cannot be read** while a lease is open, that is not taken as "no Deny".
  The failure is audited (`lease:deny_poll`). After 2 minutes without a successful read, the lease
  closes (`lease:deny_unreadable`).

### The holds file

A hold outlives a restart. It is written to `protected-holds.json` in the daemon's `--state-dir`
whenever it starts or ends, by writing a temporary file beside it and renaming
it over the old one, and it is read when the daemon starts:

```json
{"version": 2, "hold": {"request_id": "...", "created_ms": 0, "expires_ms": 0,
  "holder": {"name": "lane-a", "epoch": 7}, "denied_at_ms": 0,
  "until": "window" | "awake" | "presence_change", "until_ms": 0, "presence_state": null,
  "reallow": true},
 "seen_denies": ["..."]}
```

`"hold": null` is no hold. `until` says what ends it: `window` at `until_ms`, `awake` when a
trusted presence file says a state other than `asleep`, `presence_change` when a trusted presence
file's state differs from `presence_state` (the first trusted state seen, if none was known).
`reallow` is true only for a hold after a Deny. A version-1 file (round 2) is still read: its hold
cannot be re-allowed, and it lists no Deny events.

**A holds file that cannot be read** (not JSON, another version, a malformed hold or Deny list, or
an I/O error other than "not found") fails closed: the phone is held until a trusted presence
file's state changes. The overnight rule does not open a lease while it holds, and no re-allow
lifts it (the denied request is not known). The owner can also end it by deleting the file while
the daemon is stopped. A missing file is no hold.

## The overnight rule

The owner allowed the protected phone to be used overnight without being asked ("Overnight still
skips it"). The rule opens a lease only when a **presence file** says the owner is asleep, and
only when that file can be trusted:

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

## Answers cannot be forged by reading the push

With `--protected-lease` the approval channel requires a separate inbound topic. It checks this on
every send and every poll (`NtfyConfig::require_separate_inbound`), and the daemon checks it again
at start-up. An Allow posted to the outbound topic, which any reader of the push can see, is never
read. An HMAC on the answers is not possible here, because ntfy's action buttons post a fixed body.
The agent is told only a 6-character reference, and the audit log records only that reference, so
an agent cannot learn the full request id it would need to answer.

## Known limits

These are not fixed by this design, and are recorded so nobody relies on them:

- **Same-user forgery.** The daemon, its key files (`--presence-key-file`, the approval topic
  files) and its state directory belong to the owner's Windows account. Any process running as
  that account can read the topic files and forge an Allow on the inbound topic, sign its own
  presence file, or edit the holds file. The fix is a separate Windows account for `bridged`,
  whose files the owner's other processes cannot read; that is a deployment change, not a code
  one, and the owner's default for now is not to make it.
- **Direct adb.** The lease and the guard govern what goes through Agentroid. A process on the
  laptop that runs `adb` itself, outside the daemon, is not stopped by either.
- **Deleting the holds file and restarting.** A same-user process that deletes
  `protected-holds.json` while the daemon is stopped ends a hold, and the Deny events kept in it
  are forgotten (the same-user limit above, in its simplest form).
- **The push carries the full request id.** The Allow and Deny buttons must post `allow <id>` and
  `deny <id>` to the inbound topic, so the outbound push (which any reader of the outbound topic
  sees) contains the inbound topic's URL and the full request id. Anyone who can read the outbound
  topic can therefore post an answer. The separate topic stops an agent that only has the outbound
  topic file; it does not stop one that reads the push itself. The 6-character reference narrows
  what an agent learns from Agentroid, but it is not a secret from a reader of the push.

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
  the backend is free, else at the next call; an expiry at the next call. **A close is never
  dropped**: one that cannot be written goes back to the front of the queue, and calls on the
  protected phone are refused until it is written.

If an audit entry cannot be written, the call is refused. None of these writes is ignored.

## Tests

`crates/core/tests/protected_lease.rs` (core and the approver together, with a manual clock: the
ask and the notice, no content rule, holds and restarts, one ask per phone, the strict late Allow,
the close queue, the PIN rule, the dumps per call), `crates/bridged/tests/protected_lease.rs`
(through the daemon: holder, connection close, release, jobs, files, the close audits),
`live::tests::behind_the_guard_one_allow_lets_core_act_on_the_protected_phone_and_nothing_else_rides_on_it`
(core's lease and the adb guard together over a scripted adb), the `live::tests` of the dump
helper's integrity, `bridged/tests/devtools.rs` (`doctor` refuses the single-purpose reads), and
the unit tests in `protected.rs`, `lease.rs`, `guard.rs`, `approval.rs`, `bridged/src/server.rs`
(the caller's holder) and `bridged/src/main.rs`.
