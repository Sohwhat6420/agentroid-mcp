#!/usr/bin/env python
"""Check that every version in the repository agrees, and with a release tag if one is given.

    python scripts/check_release.py            every version agrees, and CHANGELOG.md has its section
    python scripts/check_release.py v1.0.0     ... and the tag is v<that version>, with notes in
                                               docs/release-notes/v1.0.0.md

Reads Cargo.toml ([workspace.package] version), the workspace crates in Cargo.lock,
apps/mcp/package.json and its lock file, apps/mcp/src/version.ts, web/jobview/package.json and
CHANGELOG.md. Exit codes: 0 all agree, 1 a mismatch (each one is printed), 2 a file is missing.
Standard library only; starts no child process.
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def read(rel):
    try:
        return (ROOT / rel).read_text(encoding="utf-8")
    except OSError as e:
        print(f"check_release: cannot read {rel}: {e}", file=sys.stderr)
        sys.exit(2)


def versions():
    """Yield (where, version) for every place that states the release version."""
    cargo = read("Cargo.toml")
    m = re.search(r"\[workspace\.package\][^\[]*?^version\s*=\s*\"([^\"]+)\"", cargo, re.M | re.S)
    yield "Cargo.toml [workspace.package]", m.group(1) if m else None
    lock = read("Cargo.lock")
    for name, ver in re.findall(r"^name = \"(agentroid-[^\"]+)\"\nversion = \"([^\"]+)\"", lock, re.M):
        yield f"Cargo.lock {name}", ver
    for rel in ("apps/mcp/package.json", "web/jobview/package.json"):
        yield rel, json.loads(read(rel)).get("version")
    for rel in ("apps/mcp/package-lock.json", "web/jobview/package-lock.json"):
        d = json.loads(read(rel))
        yield rel, d.get("version")
        yield f"{rel} packages['']", d.get("packages", {}).get("", {}).get("version")
    m = re.search(r"export const VERSION = \"([^\"]+)\"", read("apps/mcp/src/version.ts"))
    yield "apps/mcp/src/version.ts", m.group(1) if m else None


def problems(tag=None):
    found = list(versions())
    want = found[0][1]
    out = [f"{where}: {v!r}, want {want!r}" for where, v in found if v != want]
    if want is not None and not re.search(rf"^## \[?{re.escape(want)}\]?", read("CHANGELOG.md"), re.M):
        out.append(f"CHANGELOG.md: no '## [{want}]' section")
    if tag is not None and tag != f"v{want}":
        out.append(f"tag {tag!r} does not match the version {want!r} (want v{want})")
    if tag is not None and not (ROOT / "docs" / "release-notes" / f"{tag}.md").is_file():
        out.append(f"docs/release-notes/{tag}.md is missing: the draft release takes its notes from it")
    return want, out


def main(argv):
    tag = argv[1] if len(argv) > 1 else None
    want, out = problems(tag)
    for p in out:
        print(f"check_release: {p}", file=sys.stderr)
    if out:
        return 1
    print(f"check_release: every version is {want}" + (f", and the tag {tag} matches" if tag else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
