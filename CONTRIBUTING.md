# Contributing to Agentroid

Thank you for looking. Agentroid is pre-release and the code is changing, so please open an issue to talk about anything bigger than a small fix before you write it.

## Ground rules

- **Licence.** By submitting a contribution you agree it is licensed under the Apache License 2.0, as section 5 of the [LICENSE](LICENSE) says, unless you state otherwise.
- **Sign off every commit (DCO).** Agentroid uses the [Developer Certificate of Origin](DCO) instead of a contributor licence agreement. Add a `Signed-off-by: Your Name <you@example.com>` line to each commit message, which `git commit -s` does for you, to certify that you wrote the change or have the right to submit it under the Apache License 2.0. The name and email must match the commit's author. To fix a branch that is missing sign-offs, run `git rebase --signoff main` (or `git commit --amend -s` for the last commit) and force-push your own branch. You can check a branch with `python scripts/check_dco.py main..HEAD`.
- **Do not copy code from projects with an incompatible licence** (for example GPL or AGPL). [NOTICE](NOTICE) records the projects that were studied for ideas only and the code that was adapted with attribution. If you adapt code under Apache-2.0 or MIT, say where it came from in the file and add it to NOTICE.
- **Keep private data out.** Never commit real device serial numbers, IP addresses, tokens, keys, or screenshots and logs from a phone that show personal content. Fixtures and tests use obvious dummy serial numbers and the documentation address ranges (192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24). An example of a placeholder serial: `DEVICE0000EXAMPLE`.
- **Changes that widen what an agent can do get a closer look.** That covers the device gate, the adb guard, the `doctor` allowlist, the `run_script` sandbox, the HTTP listener and the helper app's permissions. Such a change needs tests that show the new behaviour is allowed and that the nearest forbidden neighbour is still refused. Please do not add a tool that takes a free-form shell command.
- **Security problems** go to [SECURITY.md](SECURITY.md), not to a public issue.
- **Be kind.** This project follows the [Contributor Covenant 2.1](CODE_OF_CONDUCT.md). Report a conduct problem through the repository's private reporting (Security tab), as for a vulnerability.

## Setting up

You need Windows 10 or 11 to run the daemon, a recent stable Rust toolchain, and Node.js (the MCP server runs TypeScript directly with `node --experimental-strip-types`; it was developed on Node 22.17.0). The Rust tests do not need a phone. See the Quick start in the [README](README.md) for the full requirements.

```powershell
git clone https://github.com/Sohwhat6420/agentroid-mcp.git agentroid
cd agentroid
```

## Checks to run before you open a pull request

Run the ones that cover the files you changed. All of them must pass on a clean checkout.

**Rust** (repository root):

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**MCP server** (`apps/mcp`):

```powershell
npm ci
npm run typecheck
npm test
```

The tool list has a size budget: at most 27 tools, and the list sent to the model must stay within the token budget set in `apps/mcp/src/budget.ts`. `npm test` checks this with an offline estimate. `npm run budget:ci` measures it with Anthropic's `count_tokens` endpoint and needs an `ANTHROPIC_API_KEY`; you can skip it if you do not have one. If you add or lengthen a tool description, check the budget.

**Job view** (`web/jobview`):

```powershell
npm ci
npm run typecheck
npm run build
npm test
npm run contrast
```

**Schemas** (repository root; needs Python 3 with the `jsonschema` package):

```powershell
python test_schemas.py
python test_runner_receipt.py
```

**Android helper** (`android/`): `android/run-tests.sh` runs the helper's plain-JVM tests and a clean-room audit of its manifest and sources. It needs JDK 17 and the OkHttp, Okio and kotlin-stdlib jars (the versions named in [NOTICE](NOTICE)), which you download from Maven Central yourself and point `HELPER_LIBS` at. The script does not fetch them for you yet.

**Benchmark scripts** (`bench/androidworld`): their tests need AndroidWorld installed separately and currently assume the author's Windows machine, so they are not set up for outside contributors yet.

## Writing and style

- Match the surrounding code. `cargo fmt` and `cargo clippy -D warnings` are the Rust style check.
- Tests belong with the change. Where you add a safety check, add a test that fails when the check is removed.
- Docs and comments use plain, precise British English.

## Pull requests

- One topic per pull request, with a description of what changed and why, and which of the checks above you ran.
- Say how you tested it: an emulator, or a physical device (make and Android version), or only the unit tests.
- A maintainer may ask you to split a large change or add a test before review.

This is a one-person project. I read issues and pull requests when I can, usually within a week, and I may be slower during term.
