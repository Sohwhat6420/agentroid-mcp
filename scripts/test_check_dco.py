"""Tests for check_dco.py: python -B scripts/test_check_dco.py"""
import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import check_dco as dco  # noqa: E402

F, R = dco.FIELD, dco.RECORD


def rec(sha, name, email, parents, msg):
    return F.join([sha, name, email, parents, msg]) + R


class Logic(unittest.TestCase):
    def test_matching_signoff_passes(self):
        raw = rec("a" * 40, "Ada L", "ada@example.org", "p1", "fix\n\nSigned-off-by: Ada L <ada@example.org>\n")
        self.assertEqual(dco.failures(raw), [])

    def test_case_is_ignored(self):
        raw = rec("a" * 40, "Ada L", "Ada@Example.org", "p1", "x\n\nsigned-off-by: ada l <ADA@example.org>\n")
        self.assertEqual(dco.failures(raw), [])

    def test_missing_signoff_fails(self):
        raw = rec("b" * 40, "Ada L", "ada@example.org", "p1", "fix\n")
        self.assertEqual(dco.failures(raw), [("b" * 40, "no Signed-off-by line")])

    def test_other_persons_signoff_fails(self):
        raw = rec("c" * 40, "Ada L", "ada@example.org", "p1", "x\n\nSigned-off-by: Bob <bob@example.org>\n")
        self.assertEqual(dco.failures(raw)[0][1], "Signed-off-by does not match the commit author")

    def test_same_name_other_email_fails(self):
        raw = rec("d" * 40, "Ada L", "ada@example.org", "p1", "x\n\nSigned-off-by: Ada L <other@example.org>\n")
        self.assertEqual(len(dco.failures(raw)), 1)

    def test_merge_commits_are_skipped(self):
        raw = rec("e" * 40, "Ada L", "ada@example.org", "p1 p2", "Merge branch x\n")
        self.assertEqual(dco.failures(raw), [])

    def test_one_bad_among_good(self):
        raw = (rec("1" * 40, "A", "a@x.org", "p", "m\n\nSigned-off-by: A <a@x.org>\n")
               + rec("2" * 40, "A", "a@x.org", "p", "m\n"))
        self.assertEqual([s for s, _ in dco.failures(raw)], ["2" * 40])


class RealRepo(unittest.TestCase):
    """Positive and negative control against a real throwaway repository."""

    def run_git(self, cwd, *args):
        flags = subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0
        subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, creationflags=flags)

    def test_exit_codes(self):
        with tempfile.TemporaryDirectory() as d:
            g = lambda *a: self.run_git(d, "-c", "user.name=Ada L", "-c", "user.email=ada@example.org", *a)
            g("init", "-q")
            with open(os.path.join(d, "f"), "w") as fh:
                fh.write("1")
            g("add", "f")
            g("commit", "-q", "-s", "-m", "signed")
            old = os.getcwd()
            os.chdir(d)
            try:
                self.assertEqual(dco.main(["x", "HEAD"]), 0)
                with open("f", "w") as fh:
                    fh.write("2")
                g("commit", "-q", "-a", "-m", "unsigned")
                self.assertEqual(dco.main(["x", "HEAD"]), 1)
                self.assertEqual(dco.main(["x", "no-such-ref"]), 2)
            finally:
                os.chdir(old)


if __name__ == "__main__":
    unittest.main()
