"""Start, wait for and stop the AndroidWorld emulator, so a long run can use a fresh one for every chunk.

Why: on 27 Oct the one emulator of the first full run grew from 14 GB to 21 GB of private memory in about two hours and
then stopped answering adb (every `adb shell` timed out), which ended every later chunk at its setup. A fresh emulator per
chunk bounds that.

    python -B emulator_ctl.py restart [--name aw-emuN]    stop what we started, start a new one, wait until it has booted
    python -B emulator_ctl.py stop
    python -B emulator_ctl.py status

Only the process we recorded is stopped: the launcher PID in `<sdk>/emulator.pid` (written by start-emulator.ps1) and the
detached supervisor named in `emulator.name` beside this file. Nothing is stopped by image name. The supervisor is
`detached-run.mjs` (a hidden window; no console ever appears).
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
SDK = Path(os.environ.get("ANDROID_SDK_ROOT") or os.environ.get("ANDROID_HOME") or Path.home() / "AndroidSdk")
PID_FILE = SDK / "emulator.pid"
NAME_FILE = HERE / "results" / "emulator.name"
# a node script that runs a command hidden and detached; the device copy unless the env var names another
DETACHED = os.environ.get("AGENTROID_DETACHED_RUN") or str(Path.home() / ".claude" / "scripts" / "detached-run.mjs")
START_SCRIPT = HERE / "start-emulator.ps1"
SERIAL = "emulator-5554"
NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0


def adb_path() -> str:
    pkgs = Path.home() / "AppData" / "Local" / "Microsoft" / "WinGet" / "Packages"
    for pkg in pkgs.glob("Genymobile.scrcpy*"):
        for exe in pkg.glob("*/adb.exe"):
            return str(exe)
    return "adb"


def _run(argv: list[str], timeout: float = 30.0, run=subprocess.run) -> subprocess.CompletedProcess:
    return run(argv, capture_output=True, text=True, timeout=timeout, creationflags=NO_WINDOW)


def next_name(previous: str | None) -> str:
    """aw-emu-<n+1> after aw-emu-<n>; a first name when there is none."""
    if previous and previous.startswith("aw-emu-") and previous[7:].isdigit():
        return f"aw-emu-{int(previous[7:]) + 1}"
    return "aw-emu-1"


def stop(run=subprocess.run) -> list[str]:
    """Stop the recorded supervisor (which ends its emulator tree). Returns what was done."""
    done = []
    if NAME_FILE.exists():
        name = NAME_FILE.read_text(encoding="utf-8").strip()
        r = _run(["node", DETACHED, "stop", name], run=run)
        done.append(f"detached-run stop {name}: exit {r.returncode}")
    return done


def booted(run=subprocess.run) -> bool:
    try:
        r = _run([adb_path(), "-s", SERIAL, "shell", "getprop", "sys.boot_completed"], timeout=15, run=run)
    except subprocess.TimeoutExpired:
        return False
    return r.stdout.strip() == "1"


def start(name: str, run=subprocess.run) -> None:
    NAME_FILE.parent.mkdir(parents=True, exist_ok=True)
    r = _run(["node", DETACHED, "start", name, "--", "powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(START_SCRIPT)], run=run)
    if r.returncode != 0:
        raise RuntimeError(f"detached-run start {name} failed: {r.stdout} {r.stderr}")
    NAME_FILE.write_text(name, encoding="utf-8")


def wait_boot(timeout_s: float = 300.0, run=subprocess.run, sleep=time.sleep, clock=time.monotonic) -> float:
    """Seconds until sys.boot_completed is 1 and the package manager answers; raises on timeout."""
    t0 = clock()
    while clock() - t0 < timeout_s:
        if booted(run):
            try:
                r = _run([adb_path(), "-s", SERIAL, "shell", "pm", "path", "android"], timeout=15, run=run)
                if r.stdout.startswith("package:"):
                    return clock() - t0
            except subprocess.TimeoutExpired:
                pass
        sleep(5)
    raise TimeoutError(f"{SERIAL} did not finish booting in {timeout_s:.0f} s")


def restart(run=subprocess.run, sleep=time.sleep) -> str:
    previous = NAME_FILE.read_text(encoding="utf-8").strip() if NAME_FILE.exists() else None
    stop(run)
    sleep(5)
    name = next_name(previous)
    start(name, run)
    wait_boot(run=run, sleep=sleep)
    sleep(20)  # the launcher and a11y services settle after boot_completed
    return name


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["restart", "stop", "status"])
    a = ap.parse_args(argv)
    if a.cmd == "stop":
        print("\n".join(stop()) or "nothing recorded")
    elif a.cmd == "status":
        print("booted" if booted() else "not answering")
    else:
        print("started", restart())
    return 0


if __name__ == "__main__":
    sys.exit(main())
