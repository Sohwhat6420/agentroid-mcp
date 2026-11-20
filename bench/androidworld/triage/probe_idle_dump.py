"""Positive control for the 11 Nov unreadable_screen refusals (proved before run 3).

Hypothesis: `uiautomator dump` waits for the UI to go idle, and a running stopwatch never does, so the dump
fails ("could not get idle state") and the screen guard, unable to read the screen, refuses the tap.

Boots a fresh emulator (emulator_ctl), opens Clock > Stopwatch, starts it, then times three dumps while it runs
and three after pausing it. Prints one JSON object. Run with the AndroidWorld venv:

    python -B triage/probe_idle_dump.py [--fallback CMD]   (--fallback: a second read command to try while running)
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import emulator_ctl  # noqa: E402

NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0  # creationflags is Windows-only
SERIAL = emulator_ctl.SERIAL


def adb(*args: str, timeout: float = 40.0) -> subprocess.CompletedProcess:
    return subprocess.run([emulator_ctl.adb_path(), "-s", SERIAL, *args], capture_output=True, text=True,
                          encoding="utf-8", errors="replace", timeout=timeout, creationflags=NO_WINDOW)


def dump() -> dict:
    t0 = time.monotonic()
    try:
        r = adb("exec-out", "uiautomator", "dump", "/dev/tty", timeout=40)
        out = (r.stdout or "") + (r.stderr or "")
    except subprocess.TimeoutExpired:
        out = "(timed out after 40 s)"
    ok = "<hierarchy" in out
    return {"ok": ok, "s": round(time.monotonic() - t0, 2), "msg": "" if ok else out.strip()[:200], "xml": out if ok else ""}


def centre(xml: str, label: str) -> tuple[int, int] | None:
    for m in re.finditer(r'<node [^>]*>', xml):
        node = m.group(0)
        if f'text="{label}"' in node or f'content-desc="{label}"' in node:
            b = re.search(r'bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', node)
            if b:
                x1, y1, x2, y2 = map(int, b.groups())
                return (x1 + x2) // 2, (y1 + y2) // 2
    return None


def tap_label(label: str) -> tuple[int, int] | None:
    d = dump()
    xy = centre(d["xml"], label) if d["ok"] else None
    if not xy:
        return None
    adb("shell", "input", "tap", str(xy[0]), str(xy[1]))
    time.sleep(1.5)
    return xy


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--fallback", default=None, help="a shell command line run with `adb exec-out` while the stopwatch runs")
    ap.add_argument("--no-restart", action="store_true", help="use the emulator already running")
    a = ap.parse_args(argv)
    res: dict = {"serial": SERIAL}
    if not a.no_restart:
        res["emulator"] = emulator_ctl.restart()
    res["sdk"] = adb("shell", "getprop", "ro.build.version.sdk").stdout.strip()
    res["uiautomator_jar"] = adb("shell", "ls", "/system/framework/uiautomator.jar").stdout.strip()
    adb("shell", "am", "start", "-n", "com.google.android.deskclock/com.android.deskclock.DeskClock")
    time.sleep(3)
    res["tap_stopwatch_tab"] = tap_label("Stopwatch")
    res["tap_start"] = tap_label("Start")
    time.sleep(1)
    res["running"] = [{k: v for k, v in dump().items() if k != "xml"} for _ in range(3)]
    if a.fallback:
        t0 = time.monotonic()
        r = adb("exec-out", *a.fallback.split(), timeout=40)
        out = (r.stdout or "") + (r.stderr or "")
        res["fallback_running"] = {"ok": "<hierarchy" in out, "s": round(time.monotonic() - t0, 2),
                                   "has_pause": 'text="Pause"' in out or 'content-desc="Pause"' in out,
                                   "msg": "" if "<hierarchy" in out else out.strip()[:300]}
    # pause it by tapping where Start was (the same button toggles): reading the screen to find "Pause" is
    # exactly what fails while it runs. Then the control dumps on the still screen.
    if res["tap_start"]:
        adb("shell", "input", "tap", str(res["tap_start"][0]), str(res["tap_start"][1]))
        time.sleep(1.5)
    time.sleep(1)
    res["paused"] = [{k: v for k, v in dump().items() if k != "xml"} for _ in range(3)]
    print(json.dumps(res, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
