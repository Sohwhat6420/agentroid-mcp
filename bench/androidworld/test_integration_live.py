"""The live adb transport, as far as it can be proven without a phone: bridged --adb auto starts, the
MCP server reaches it, and list_devices answers from the real adb (an empty list is a pass).

With an emulator running and `--allow-emulators` the same file's second test drives it; that test
is skipped unless AGENTROID_LIVE_DEVICE names an adb serial (e.g. emulator-5554). It acts on the
named device only. Never set it to a real phone's serial.

Run: python -B test_integration_live.py
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

import agentroid_mcp as am

SCAFFOLD = Path(__file__).resolve().parents[2]
BRIDGED = SCAFFOLD / "target" / "debug" / "bridged.exe"
CREATE_NO_WINDOW = 0x08000000
LIVE_DEVICE = os.environ.get("AGENTROID_LIVE_DEVICE", "")


@unittest.skipUnless(os.name == "nt" and BRIDGED.exists(), f"{BRIDGED} not built")
class LiveDaemon(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory(prefix="aw-live-")
        cls.pipe = rf"\\.\pipe\agentroid-aw-live-{os.getpid()}"
        cls.state = Path(cls.tmp.name) / "state"
        extra = ["--allow-emulators"] if LIVE_DEVICE.startswith("emulator-") else []
        cls.proc = subprocess.Popen(
            [str(BRIDGED), "--adb", "auto", *extra, "--pipe", cls.pipe, "--state-dir", str(cls.state)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, creationflags=CREATE_NO_WINDOW,
        )
        log = cls.state / "bridged.log"
        for _ in range(100):
            if log.exists() and "listening" in log.read_text(errors="replace"):
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("bridged --adb auto did not start")
        cls.client = am.start_agentroid(pipe=cls.pipe, session="aw-live-test", stderr_path=Path(cls.tmp.name) / "mcp.err")

    @classmethod
    def tearDownClass(cls) -> None:
        cls.client.close()
        subprocess.run([str(BRIDGED), "--stop", "--state-dir", str(cls.state)], creationflags=CREATE_NO_WINDOW, timeout=30)
        try:
            cls.proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            cls.proc.kill()  # our own recorded child
            cls.proc.wait()
        cls.tmp.cleanup()

    def test_list_devices_answers_from_real_adb(self):
        r = self.client.call("list_devices", {})
        self.assertFalse(r.is_error, r.text)
        self.assertIsInstance(((r.structured or {}).get("data") or {}).get("devices"), list)

    @unittest.skipUnless(LIVE_DEVICE.startswith("emulator-"), "set AGENTROID_LIVE_DEVICE=emulator-5554 with an emulator running")
    def test_an_emulator_can_be_read_and_tapped(self):
        tree = self.client.call("ui_tree", {"device": LIVE_DEVICE})
        self.assertFalse(tree.is_error, tree.text)
        self.assertIn("snapshot", tree.text)
        ex = am.Executor(self.client, device=LIVE_DEVICE)
        step = ex.execute({"action_type": "navigate_home"})
        self.assertIsNone(step.error, step.error)


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1], verbosity=2)
