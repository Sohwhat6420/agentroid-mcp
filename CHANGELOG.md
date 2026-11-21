# Changelog

Each release, newest first. From 1.0.0 the versions follow [semantic versioning](https://semver.org): the MCP tool names and their inputs, the result shape (`ok`, `data`, `error`, header, receipt) and the error kinds change only in a new major version. The daemon's own JSON-RPC methods, the flow file format and the Rust crates' APIs are internal and may change in a minor release.

## [1.0.0] - unreleased (draft)

The first tagged release. Before it, Agentroid was a preview at 0.1.0 with no tags.

### What is in it

- **The MCP server** (`apps/mcp`): 27 tools over stdio, 23 device tools and 4 job tools, and an optional Streamable HTTP listener for a tailnet with one bearer token per device and mode. A `minimal` profile lists 8 tools. The full list is in [docs/REFERENCE.md](docs/REFERENCE.md#tools).
- **The daemon** (`bridged`, Windows only): the device allowlist, the deny-by-default adb guard, device modes, the screen guard with the optional approval channel, the hash-chained audit log, leases, stay-awake and jobs with a journal, pause, resume, record and replay.
- **Setup and checks**: `agentroid-setup doctor`, the connection keeper (`apps/keeper`) and the Android helper (built and tested, not yet linked to the live transport).

### Changed in this release

- **Typed errors.** The 13 tool-result error kinds now live in `apps/mcp/src/errors.ts`, each with what it means and whether repeating the call can help, and [docs/REFERENCE.md](docs/REFERENCE.md#errors) carries the same table. Errors thrown at start-up have stable codes: `ConfigError` (`config`), `BindRefused` (`bind_refused`, a kind of `ConfigError`), `TokenError` (`token`) and `CredstoreError` (`credstore`). Their messages are unchanged.
- **The run id in an error's text.** An error result's text now ends `error kind: <kind>, run <run id>`, so a refusal read from the text alone can be matched to its receipt (part of [#32](https://github.com/Sohwhat6420/agentroid-mcp/issues/32); the health fields of a refused call are unchanged).
- **One version.** The MCP server reports its version from `apps/mcp/src/version.ts` in place of a hard-coded string, and a test keeps it equal to `package.json`, the Cargo workspace and the docs.
- **Docs that match the code.** docs/REFERENCE.md gains a tool table and an error table made from the code (`npm run docs` in `apps/mcp`), and a table of every `AGENTROID_` environment variable. `npm test` now fails when the tool counts, the tables, the environment variables, the version or a relative link in the docs drift from the code.
- **CI.** A Linux job runs `cargo fmt`, `clippy` and `cargo test` for the whole workspace (the Windows-only parts are compiled out there), and a release-check job checks that a `v*` tag matches every version in the repository before the release build runs. The release workflow still makes a **draft** GitHub release only.
- `apps/mcp/package.json` declares `"engines": {"node": ">=22.6.0"}`, the floor the docs already gave.

### Known limits

The limits listed in the README's [Status and limits](README.md#status-and-limits) all still apply: Windows only for the daemon, no image bytes sent to the agent, `pixel_probe` and `look`'s `element` and `ocr` not implemented, per-device modes not settable from configuration on the stdio path, and Claude Code the only MCP client tried. Open bugs are on the [issue list](https://github.com/Sohwhat6420/agentroid-mcp/issues).
