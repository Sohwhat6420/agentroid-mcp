"""The CI sign-in token reaches `claude` children only (workflow-trigger review, 13 Nov 2026).

    python -B test_token_isolation.py

No network, no emulator, no real token: a dummy value stands in.
"""

from __future__ import annotations

import os
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import claude_session as cs  # noqa: E402

DUMMY = "dummy-not-a-real-token"


class FakeProc:
    def __init__(self):
        self.stdout = iter(())

    def poll(self):
        return None


class TokenIsolation(unittest.TestCase):
    def setUp(self):
        self._saved = os.environ.get(cs.TOKEN_VAR)
        cs._held_token = None
        os.environ[cs.TOKEN_VAR] = DUMMY

    def tearDown(self):
        cs._held_token = None
        os.environ.pop(cs.TOKEN_VAR, None)
        if self._saved is not None:
            os.environ[cs.TOKEN_VAR] = self._saved

    def test_hold_takes_it_out_of_the_environment(self):
        self.assertTrue(cs.hold_token())
        self.assertFalse(cs.TOKEN_VAR in os.environ, "the token is still in os.environ")
        self.assertTrue(cs.hold_token(), "idempotent: still held after a second call")

    def test_an_ordinary_child_no_longer_inherits_it(self):
        # positive control first: before hold_token, a plain child sees it
        out = subprocess.run([sys.executable, "-c", f"import os; print(os.environ.get({cs.TOKEN_VAR!r}, ''))"],
                             capture_output=True, text=True, creationflags=cs.CREATE_NO_WINDOW).stdout.strip()
        self.assertEqual(out, DUMMY)
        cs.hold_token()
        out = subprocess.run([sys.executable, "-c", f"import os; print(os.environ.get({cs.TOKEN_VAR!r}, ''))"],
                             capture_output=True, text=True, creationflags=cs.CREATE_NO_WINDOW).stdout.strip()
        self.assertEqual(out, "", "bridged, the MCP server and android_world are started like this")

    def test_the_claude_child_gets_it_back(self):
        cs.hold_token()
        seen = {}

        def popen(*a, **k):
            seen.update(k)
            return FakeProc()

        cs.ClaudeSession("m", "sys", popen=popen)
        self.assertEqual(seen["env"][cs.TOKEN_VAR], DUMMY)

    def test_with_no_token_the_claude_child_inherits_as_before(self):
        os.environ.pop(cs.TOKEN_VAR, None)
        self.assertFalse(cs.hold_token())
        self.assertIsNone(cs.child_env())


if __name__ == "__main__":
    unittest.main()
