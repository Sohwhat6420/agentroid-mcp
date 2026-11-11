# Security policy

Agentroid lets an AI agent operate Android devices, so a flaw in its checks can let an agent do something its owner did not intend. Please report such flaws privately.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: open the repository's **Security** tab and choose **Report a vulnerability**. Please do not open a public issue or pull request for a security problem.

**One exception: screen-guard word-list misses on test screens.** If you find a hostile screen (a UI tree you built or recorded on your own test device) that gets a payment, reset, credential or security-setting action past the screen guard, a public issue with the screen dump is welcome. [docs/SAFETY.md](docs/SAFETY.md) explains why: the guard's word lists are public, the misses are published, and more blind cases make it better. Report privately instead if the bypass works against a real person's phone, account or money, or if it gets past the approval channel, the audit log or the device allowlist.

Agentroid is a one-person project. I aim to acknowledge a report within a week, but that is a best effort, not a promise, and I cannot offer a bounty.

Include, if you can:

- what you did, what you expected and what happened, with the commit you tested;
- the tool calls or adb commands involved, and the device mode (`full`, `helper` or `read_only`);
- whether it needs a particular Android version, phone make or network setup;
- a proof of concept on an emulator or on a device you own.

Do not include real access tokens, real device serial numbers or screenshots of someone else's phone. If a token may have leaked, revoke it first (`node --experimental-strip-types src/token-admin.ts revoke <hardware-serial> <mode>` in `apps/mcp`) and say so in the report.

## Supported versions

Agentroid is pre-release (version 0.1.0). Only the latest commit on the default branch is supported. Once a release is tagged, the latest release is supported as well.

## What counts as a vulnerability

These are in scope, because the project's safety model promises them (see the "Safety model" section of the [README](README.md) and [docs/SAFETY.md](docs/SAFETY.md)):

- a call that reaches a device that is not on the allowlist, or that reaches a device in a way the gate should have refused (for example an acting call on a `read_only` device, or a gesture-and-key-only `helper` device running a call that needs `adb shell`);
- an adb command that gets past the guard when it should be refused (for example `kill-server`, `start-server`, `keygen`, `pair`, `reconnect`, an untargeted per-device command, or a `connect` or `disconnect` that names no single address);
- a shell metacharacter, quote or other way to get an arbitrary command through `doctor` or the `agentroid.shell` function in `run_script`;
- an escape from the `run_script` sandbox (QuickJS in a child process under a Windows Job Object), such as file, network or `eval` access, or going past its memory or CPU limits;
- a way for another user or another machine to use the daemon's named pipe, or to reach the HTTP listener without a valid token, from outside the tailnet address it binds, or with a token meant for another device or mode;
- a flaw in the phone link's authentication (the P-256 challenge and response between the helper app and the hub) that lets a different phone or a different hub take the other's place;
- the helper app opening a listening socket, using a permission it should not have, or exposing a component without the permission that is supposed to guard it;
- a secret (a bearer token, the hub key) written to a log, a URL, a command line or a file in clear text;
- a path traversal or injection in flow replay (`job_start` with a flow file name) or in the saved recordings;
- a way to approve an `asked` action other than an exact `allow <id>` answer on the approval channel, within its window (for example text on the phone's screen, a forged or replayed answer, or an answer for another request);
- a way to change, remove or reorder audit-log records, or to act without a record being written first, that `agentroid audit verify` does not catch (with the anchor, for a log rewritten from scratch).

## Known limits that are not vulnerabilities

These are documented design limits of the current build. A report that only restates one of them will be closed, though a clear improvement suggestion is welcome as an ordinary issue.

- **The screen guard reads words, not pictures.** It knows the wording in its tables (16 languages plus Tagalog payment words) and the ids and descriptions of on-screen elements. It misses a button with no readable text (a game canvas, an icon-only button with a meaningless id), a language or phrasing it does not have, a one-time code typed into an ordinary chat box that never mentions a code, and a payment sheet that appears within 2 seconds of the last screen read. The known misses and all four blind rounds are listed in [docs/SAFETY.md](docs/SAFETY.md). Use a `read_only` device, an emulator, or a phone with no payment methods signed in when the stakes are high.
- **Flow risk labels come from the flow's author.** A flow with any step labelled R2 or above is refused, but the label is written by whoever wrote the flow, which may be the agent. Every step still passes the screen guard, which is the real control.
- **A slow action after a late Allow.** The approval window (90 s) is shorter than the MCP call timeout (100 s), so an Allow cannot arrive after the agent was told "timeout". An Allow near the end of the window followed by a slow action can still finish after the agent stopped waiting. While an approval is pending, a second client waits.
- **The audit log's head file sits beside the log.** Someone who can rewrite both can rebuild the chain; only an anchor (`--audit-anchor`) kept off the machine catches that, and Agentroid cannot tell whether the anchor path really leaves the machine. By default a failed anchor write is logged and the action goes ahead; `--audit-anchor-required` refuses it instead.
- **The outbox and inbox are folders, not a sandbox.** `push` reads only `files\outbox` and `pull_new` writes only `files\inbox`, but anything your Windows user puts in the outbox can be pushed by a `full` token, and a pulled file is whatever the phone holds.
- **Any process running as your Windows user can use the daemon's pipe.** The pipe admits the current user only; it does not separate programs that run as you.
- **The phone link is authenticated but not encrypted by Agentroid.** It is meant to run over a private network such as a tailnet and uses cleartext `ws://`, which the helper app allows only towards tailnet hosts.
- **The run journal is not tamper-evident.** It is an append-only file, written before each acting step is sent. The separate audit log of guard decisions is hash-chained; the journal is not.
- **A prompt-injected agent can still use the access it has.** Screen content is untrusted input to the model. The safety checks limit what a misled agent can do; they cannot stop it from doing the permitted things badly.
- **Two optional outbound paths, both to ntfy.** Agentroid sends no telemetry. (a) If you create `%USERPROFILE%\.agentroid
tfy-topic.txt`, the MCP server posts a short message (run id, flow name, step counts, no screen content) when a job finishes or needs you. (b) If you start the daemon with `--approval-topic-file <file>`, each `asked` action posts an approval request to that topic (default server `https://ntfy.sh`, or `--ntfy-base <url>`): it carries the tool, the phone's hardware serial, the guard's reason, which quotes the matched on-screen label, and the target label. Treat both topic names as secrets, prefer a self-hosted ntfy server, and do not install the ntfy app on the phone being driven: its Allow button would be on the agent's own screen.
- **Windows only, pre-release.** Device modes cannot yet be set from configuration: every allowed device runs as `full` over the local pipe (the HTTP listener does enforce a mode per access token).

## Testing responsibly

Test on an emulator or on devices you own. Do not run tests against other people's phones, networks or accounts, and do not use Agentroid to bypass a lock screen or app protections on a device you do not own.

## Dependencies

Report a vulnerability in a dependency to that project. If it also affects Agentroid in a way the project can mitigate, tell us as well. The third-party components and their licences are listed in [NOTICE](NOTICE).
