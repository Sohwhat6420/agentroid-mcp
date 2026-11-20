"""The adapter against the real stack on the fake device: Python Executor -> MCP stdio server (node)
-> bridged --fake (named pipe) -> core -> FakeDevice. No phone, no emulator, no network.

Run: python -B test_integration_fake.py   (needs target/debug/bridged.exe built and node on PATH)
Skipped, loudly, when the binary is missing.
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
CREATE_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0  # creationflags is Windows-only


@unittest.skipUnless(os.name == "nt" and BRIDGED.exists(), f"{BRIDGED} not built (cargo build -p agentroid-bridged)")
class FakeStack(unittest.TestCase):
    proc: subprocess.Popen
    tmp: tempfile.TemporaryDirectory
    client: am.McpClient

    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory(prefix="aw-int-")
        cls.pipe = rf"\\.\pipe\agentroid-aw-test-{os.getpid()}"
        cls.state = Path(cls.tmp.name) / "state"
        cls.proc = subprocess.Popen(
            [str(BRIDGED), "--fake", "--pipe", cls.pipe, "--state-dir", str(cls.state)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, creationflags=CREATE_NO_WINDOW,
        )
        log = cls.state / "bridged.log"
        for _ in range(100):
            if log.exists() and "listening" in log.read_text(errors="replace"):
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("bridged --fake did not start")
        cls.client = am.start_agentroid(pipe=cls.pipe, session="aw-int-test", stderr_path=Path(cls.tmp.name) / "mcp.err")
        # the fake lists the POCO and a read-only S24: act on the one that is not read-only
        listing = cls.client.call("list_devices", {})
        devices = ((listing.structured or {}).get("data") or {}).get("devices") or []
        full = [d["device"] for d in devices if d.get("mode") != "read_only"]
        if len(full) != 1:
            raise RuntimeError(f"expected one acting device, got {devices!r} / {listing.text!r}")
        cls.device = full[0]

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

    def test_server_lists_the_27_tools(self):
        names = [t["name"] for t in self.client.list_tools()]
        self.assertEqual(len(names), 27)
        for needed in ("tap", "type_text", "key", "scroll", "swipe", "open_app", "wait", "ui_tree"):
            self.assertIn(needed, names)

    def test_actions_run_end_to_end_and_the_fake_phone_saw_them(self):
        ex = am.Executor(self.client, device=self.device, sleep=lambda s: None)
        els = [{"bbox_pixels": {"x_min": 48, "y_min": 600, "x_max": 1172, "y_max": 700}, "text": ""}]
        steps = [
            ex.execute({"action_type": "open_app", "app_name": "Settings"}, els, (1220, 2712)),
            ex.execute({"action_type": "input_text", "text": "wifi", "index": 0}, els, (1220, 2712)),
            ex.execute({"action_type": "navigate_back"}, els, (1220, 2712)),
            ex.execute({"action_type": "scroll", "direction": "down"}, els, (1220, 2712)),
            ex.execute({"action_type": "swipe", "direction": "up"}, els, (1220, 2712)),
            ex.execute({"action_type": "wait"}, els, (1220, 2712)),
            ex.execute({"action_type": "click", "x": 100, "y": 100}, els, (1220, 2712)),
        ]
        for s in steps:
            self.assertIsNone(s.error, f"{s.action}: {s.error}")
            self.assertTrue(all(not r.is_error for r in s.results))
        tree = self.client.call("ui_tree", {"device": self.device})
        self.assertFalse(tree.is_error)
        self.assertIn("wifi", tree.text, "the typed text reached the (fake) field")

    def test_harness_actions_make_no_device_call(self):
        ex = am.Executor(self.client, device=self.device, sleep=lambda s: None)
        s = ex.execute({"action_type": "answer", "text": "7"})
        self.assertEqual((s.answer, s.results, s.error), ("7", [], None))
        s = ex.execute({"action_type": "status", "goal_status": "complete"})
        self.assertEqual((s.status, s.results), ("complete", []))

    def test_a_bad_action_is_an_error_row_not_an_exception(self):
        ex = am.Executor(self.client, device=self.device, sleep=lambda s: None)
        s = ex.execute({"action_type": "click", "index": 9}, [], (1220, 2712))
        self.assertIsNotNone(s.error)
        self.assertEqual(s.results, [])


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1], verbosity=2)
