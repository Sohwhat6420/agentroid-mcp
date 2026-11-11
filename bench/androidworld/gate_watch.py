"""Memory-gated start and 85% abort for the scored full run.

    python -B gate_watch.py --until 2026-11-07T07:00 [--start-at-or-below 38] [--abort-above 54.1]
    python -B gate_watch.py --until 2026-11-08T00:00 --name aw-full2c --out-dir results/full-2c --start-at-or-below 99 \
        --run-args "--subset subset-23-fix.json --chunk 15 --backend omniroute --model a,b --policy v2 --fresh-emulator" [--abort-free-gb 8]

With --run-args the run is that run_full.py command line (default: the Sonnet full run, as before). --start-at-or-below 99
starts at the first reading; the abort line (committed memory, and C: free space with --abort-free-gb) still applies.

Every 10 minutes it reads committed memory (TotalVirtualMemorySize - FreeVirtualMemory). When two readings in a row are at
or below the gate, and no emulator is running, results/full-2 does not exist and C: has at least 40 GB free, it starts the
run as the detached process `aw-full2`. While the run is alive it reads every 60 s and, above the abort line, stops the
run by its recorded name and the emulator by emulator_ctl, never by image name. After --until it stops waiting.

Everything it does is appended to results/gate-watch.log. No window is ever shown (detached-run hides its children).
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

HERE = Path(__file__).resolve().parent
LOG = HERE / "results" / "gate-watch.log"
OUT = HERE / "results" / "full-2"
RUN_NAME = "aw-full2"
RUN_ARGS: list[str] | None = None
DETACHED = os.environ.get("AGENTROID_DETACHED_RUN", "detached-run.mjs")  # a node script that runs a command hidden and detached
PY = os.environ.get("AW_PYTHON", sys.executable)  # the python of the venv with android_world installed
NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0
POLL_WAIT_S, POLL_RUN_S = 600, 60
MIN_FREE_GB = 40


def say(msg: str) -> None:
    LOG.parent.mkdir(parents=True, exist_ok=True)
    with LOG.open("a", encoding="utf-8") as f:
        f.write(f"{datetime.now().isoformat(timespec='seconds')} {msg}\n")


def sh(argv: list[str], timeout: float = 60.0) -> subprocess.CompletedProcess:
    return subprocess.run(argv, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout, creationflags=NO_WINDOW)


def committed_gb() -> tuple[float, float]:
    r = sh(["powershell.exe", "-NoProfile", "-Command",
            "$o=Get-CimInstance Win32_OperatingSystem; '{0} {1}' -f $o.TotalVirtualMemorySize,$o.FreeVirtualMemory"])
    total, free = (int(x) for x in r.stdout.split())
    return (total - free) / 1048576, total / 1048576


def emulator_running() -> bool:
    return "qemu-system" in sh(["tasklist"]).stdout.lower()


def run_alive() -> bool:
    out = sh(["node", DETACHED, "status", RUN_NAME]).stdout
    return '"alive":true' in out.replace(" ", "")


def start_run() -> bool:
    tail = RUN_ARGS if RUN_ARGS is not None else ["--out", "results/full-2", "--chunk", "15", "--pool", "own", "--model", "sonnet", "--policy", "v2", "--fresh-emulator"]
    r = sh(["node", DETACHED, "start", RUN_NAME, "--cwd", str(HERE), "--", PY, "-B", "run_full.py", *tail])
    say(f"START {RUN_NAME} exit {r.returncode}: {r.stdout.strip()[:200]}")
    return r.returncode == 0


def abort(reason: str) -> None:
    say(f"ABORT {reason}")
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "STOP").write_text("memory abort\n", encoding="utf-8")
    say(f"stop {RUN_NAME}: " + sh(["node", DETACHED, "stop", RUN_NAME]).stdout.strip()[:120])
    say("emulator_ctl stop: " + sh([PY, "-B", str(HERE / "emulator_ctl.py"), "stop"]).stdout.strip()[:120])


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--until", required=True, help="local ISO time after which it stops waiting, e.g. 2026-11-07T07:00")
    ap.add_argument("--start-at-or-below", type=float, default=38.0)
    ap.add_argument("--abort-above", type=float, default=54.1)
    ap.add_argument("--abort-free-gb", type=float, default=0.0, help="also abort when C: has less than this many GB free (0 = off)")
    ap.add_argument("--name", default="aw-full2")
    ap.add_argument("--out-dir", default="results/full-2")
    ap.add_argument("--run-args", help="the run_full.py arguments, one string; must contain --out <out-dir>")
    a = ap.parse_args(argv)
    global OUT, RUN_NAME, RUN_ARGS
    OUT, RUN_NAME = HERE / a.out_dir, a.name
    if a.run_args:
        RUN_ARGS = a.run_args.split() + ["--out", a.out_dir]
    until = datetime.fromisoformat(a.until)
    low_reads, started = 0, False
    say(f"watching until {until}, start at or below {a.start_at_or_below} GB, abort above {a.abort_above} GB")
    while datetime.now() < until:
        used, total = committed_gb()
        if started:
            if used > a.abort_above:
                abort(f"{used:.1f} of {total:.1f} GB committed")
                return 3
            if a.abort_free_gb and shutil.disk_usage("C:/").free / 2**30 < a.abort_free_gb:
                abort(f"C: below {a.abort_free_gb:.0f} GB free")
                return 3
            if not run_alive():
                say(f"run finished or ended at {used:.1f} GB")
                return 0
            time.sleep(POLL_RUN_S)
            continue
        free_gb = shutil.disk_usage("C:/").free / 2**30
        ok = used <= a.start_at_or_below and not emulator_running() and not OUT.exists() and free_gb >= MIN_FREE_GB
        say(f"{used:.1f} GB committed, C: {free_gb:.0f} GB free, gate {'OPEN' if ok else 'closed'}")
        low_reads = low_reads + 1 if ok else 0
        if low_reads >= 2:
            started = start_run()
            if not started:
                return 2
            continue
        time.sleep(POLL_WAIT_S)
    say("deadline reached without a start" if not started else "deadline reached")
    return 1


if __name__ == "__main__":
    sys.exit(main())
