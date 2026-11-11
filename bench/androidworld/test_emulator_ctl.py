"""emulator_ctl with a fake process runner and clock: nothing is started or stopped.

Run: python -B test_emulator_ctl.py
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

import emulator_ctl as ec


class Names(unittest.TestCase):
    def test_names_count_up_and_start_over_when_unknown(self):
        self.assertEqual(ec.next_name(None), "aw-emu-1")
        self.assertEqual(ec.next_name("aw-emu-1"), "aw-emu-2")
        self.assertEqual(ec.next_name("aw-emu-9"), "aw-emu-10")
        self.assertEqual(ec.next_name("aw-emu7"), "aw-emu-1")


class Fake:
    """A subprocess.run stand-in that answers adb queries from a script and records every call."""

    def __init__(self, boot_after=0, pm_ok=True):
        self.calls, self.boot_after, self.pm_ok, self.boot_polls = [], boot_after, pm_ok, 0

    def __call__(self, argv, **kw):
        self.calls.append(argv)
        if "sys.boot_completed" in argv:
            self.boot_polls += 1
            return SimpleNamespace(returncode=0, stdout="1\n" if self.boot_polls > self.boot_after else "\n", stderr="")
        if "path" in argv:
            return SimpleNamespace(returncode=0, stdout="package:/system/framework/framework-res.apk\n" if self.pm_ok else "", stderr="")
        return SimpleNamespace(returncode=0, stdout="", stderr="")


class Boot(unittest.TestCase):
    def test_it_polls_until_boot_completed_and_the_package_manager_answers(self):
        fake, now = Fake(boot_after=2), [0.0]
        took = ec.wait_boot(run=fake, sleep=lambda s: now.__setitem__(0, now[0] + s), clock=lambda: now[0])
        self.assertEqual(fake.boot_polls, 3)
        self.assertEqual(took, 10.0)

    def test_it_gives_up_with_a_clear_error(self):
        fake, now = Fake(boot_after=10**6), [0.0]
        with self.assertRaises(TimeoutError):
            ec.wait_boot(timeout_s=20, run=fake, sleep=lambda s: now.__setitem__(0, now[0] + s), clock=lambda: now[0])


class Control(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self._old = ec.NAME_FILE
        ec.NAME_FILE = Path(self.tmp.name) / "emulator.name"

    def tearDown(self):
        ec.NAME_FILE = self._old
        self.tmp.cleanup()

    def test_stop_ends_only_the_recorded_supervisor(self):
        ec.NAME_FILE.write_text("aw-emu-3", encoding="utf-8")
        fake = Fake()
        ec.stop(fake)
        self.assertEqual(len(fake.calls), 1)
        self.assertEqual(fake.calls[0][-2:], ["stop", "aw-emu-3"])

    def test_stop_with_nothing_recorded_does_nothing(self):
        fake = Fake()
        self.assertEqual(ec.stop(fake), [])
        self.assertEqual(fake.calls, [])

    def test_restart_stops_then_starts_a_new_name_and_waits(self):
        ec.NAME_FILE.write_text("aw-emu-3", encoding="utf-8")
        fake = Fake()
        name = ec.restart(run=fake, sleep=lambda s: None)
        self.assertEqual(name, "aw-emu-4")
        kinds = [c[2] for c in fake.calls if c[0] == "node"]
        self.assertEqual(kinds, ["stop", "start"])
        self.assertEqual(ec.NAME_FILE.read_text(encoding="utf-8"), "aw-emu-4")


if __name__ == "__main__":
    import sys
    unittest.main(argv=sys.argv[:1], verbosity=2)
