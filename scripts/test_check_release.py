"""Tests for check_release.py: python -B scripts/test_check_release.py"""
import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import check_release as cr  # noqa: E402

REAL_ROOT = cr.ROOT

FILES = [
    "Cargo.toml",
    "Cargo.lock",
    "apps/mcp/package.json",
    "apps/mcp/package-lock.json",
    "apps/mcp/src/version.ts",
    "web/jobview/package.json",
    "web/jobview/package-lock.json",
    "CHANGELOG.md",
]


class Release(unittest.TestCase):
    def setUp(self):
        self.fresh()

    def fresh(self):
        """A new copy of the release files in a temporary folder, and check_release pointed at it."""
        if getattr(self, "tmp", None) is not None:
            shutil.rmtree(self.tmp)
        self.tmp = Path(tempfile.mkdtemp())
        for rel in FILES:
            (self.tmp / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(REAL_ROOT / rel, self.tmp / rel)
        shutil.copytree(REAL_ROOT / "docs" / "release-notes", self.tmp / "docs" / "release-notes")
        cr.ROOT = self.tmp
        self.version = cr.problems()[0]

    def tearDown(self):
        cr.ROOT = REAL_ROOT
        shutil.rmtree(self.tmp)
        self.tmp = None

    def edit(self, rel, old, new):
        p = self.tmp / rel
        text = p.read_text(encoding="utf-8")
        self.assertIn(old, text)
        p.write_text(text.replace(old, new, 1), encoding="utf-8")

    def test_the_repository_agrees_with_itself(self):
        self.assertEqual(cr.problems(), (self.version, []))
        self.assertEqual(cr.problems(f"v{self.version}"), (self.version, []))

    def test_a_wrong_tag_fails(self):
        _, out = cr.problems("v9.9.9")
        self.assertTrue(any("does not match" in o for o in out), out)
        self.assertTrue(cr.problems(self.version)[1], "a tag without the v fails too")

    def test_each_file_that_drifts_is_named(self):
        cases = [
            ("apps/mcp/package.json", "apps/mcp/package.json"),
            ("web/jobview/package.json", "web/jobview/package.json"),
            ("apps/mcp/src/version.ts", "apps/mcp/src/version.ts"),
        ]
        for rel, where in cases:
            with self.subTest(rel=rel):
                self.fresh()
                self.edit(rel, f'"{self.version}"', '"0.0.1"')
                out = cr.problems()[1]
                self.assertTrue(any(o.startswith(where) for o in out), out)

    def test_a_crate_left_behind_in_cargo_lock_is_named(self):
        self.edit("Cargo.lock", f'name = "agentroid-core"\nversion = "{self.version}"', 'name = "agentroid-core"\nversion = "0.0.1"')
        out = cr.problems()[1]
        self.assertEqual(out, [f"Cargo.lock agentroid-core: '0.0.1', want '{self.version}'"])

    def test_a_lock_file_root_left_behind_is_named(self):
        p = self.tmp / "apps/mcp/package-lock.json"
        d = json.loads(p.read_text(encoding="utf-8"))
        d["packages"][""]["version"] = "0.0.1"
        p.write_text(json.dumps(d), encoding="utf-8")
        out = cr.problems()[1]
        self.assertEqual(len(out), 1)
        self.assertIn("packages['']", out[0])

    def test_a_tag_with_no_release_notes_fails(self):
        (self.tmp / f"docs/release-notes/v{self.version}.md").unlink()
        out = cr.problems(f"v{self.version}")[1]
        self.assertEqual(len(out), 1)
        self.assertIn("is missing", out[0])
        self.assertEqual(cr.problems()[1], [], "no tag, no notes needed")

    def test_a_missing_changelog_section_fails(self):
        self.edit("CHANGELOG.md", f"## [{self.version}]", "## [later]")
        self.assertIn(f"CHANGELOG.md: no '## [{self.version}]' section", cr.problems()[1])


if __name__ == "__main__":
    unittest.main()
