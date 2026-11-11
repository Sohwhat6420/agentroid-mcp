#!/usr/bin/env python
"""Check that every commit in a range carries a Developer Certificate of Origin sign-off.

    python scripts/check_dco.py [<revision range>]      (default: HEAD)
    python scripts/check_dco.py main..HEAD

A commit passes when its message has a `Signed-off-by: Name <email>` line whose name and
email both match the commit's author (case-insensitive). Merge commits are skipped.
Exit codes: 0 every commit signed off, 1 at least one commit is not, 2 git failed.
Standard library only. Starts one `git` child process with no console window.
"""
from __future__ import annotations

import re
import subprocess
import sys

SIGNOFF = re.compile(r"^Signed-off-by:\s*(.+?)\s*<([^<>\s]+)>\s*$", re.I | re.M)
FIELD = "\x1f"
RECORD = "\x1e"


def parse_log(raw):
    """Yield (sha, author_name, author_email, parents, message) from `git log -z`-style output."""
    for rec in raw.split(RECORD):
        rec = rec.strip("\n")
        if not rec:
            continue
        sha, name, email, parents, message = rec.split(FIELD, 4)
        yield sha, name, email, parents.split(), message


def signed_off(name, email, message):
    for m in SIGNOFF.finditer(message):
        if m.group(1).strip().lower() == name.strip().lower() and m.group(2).lower() == email.lower():
            return True
    return False


def failures(raw):
    """Return [(sha, reason)] for every non-merge commit without a matching sign-off."""
    bad = []
    for sha, name, email, parents, message in parse_log(raw):
        if len(parents) > 1:
            continue
        if not SIGNOFF.search(message):
            bad.append((sha, "no Signed-off-by line"))
        elif not signed_off(name, email, message):
            bad.append((sha, "Signed-off-by does not match the commit author"))
    return bad


def git_log(rev):
    flags = subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0
    fmt = "%H" + FIELD + "%an" + FIELD + "%ae" + FIELD + "%P" + FIELD + "%B" + RECORD
    out = subprocess.run(
        ["git", "log", "--format=" + fmt, rev],
        capture_output=True, text=True, encoding="utf-8", creationflags=flags,
    )
    if out.returncode != 0:
        sys.stderr.write(out.stderr)
        return None
    return out.stdout


def main(argv):
    rev = argv[1] if len(argv) > 1 else "HEAD"
    raw = git_log(rev)
    if raw is None:
        return 2
    bad = failures(raw)
    for sha, why in bad:
        print(f"{sha[:12]}  {why}")
    if bad:
        print(f"{len(bad)} commit(s) need a sign-off: git commit -s, or git rebase --signoff.")
        return 1
    print("every commit is signed off")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
