"""Diagnostic (11 Nov): which step of run_pilot's real_run blinds the AndroidWorld accessibility forwarder?

    python -B triage/probe_bridged.py [--serial emulator-5554]

Mirrors real_run: make_env first (count), then bridged.exe + the MCP client (count), then one Agentroid `screen`
read, which runs `uiautomator dump` (count), then a harmless tap on an empty status-bar spot (count), then
refresh_env (count). Drives only the emulator named by --serial.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(HERE))
import agentroid_mcp as am  # noqa: E402
import run_pilot as rp  # noqa: E402

NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0  # creationflags is Windows-only


def count(env, label: str) -> int:
    n = len(list(env.get_state(wait_to_stabilize=False).ui_elements))
    print(f"{label}: elements={n}", flush=True)
    return n


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--serial", default="emulator-5554")
    a = ap.parse_args()
    env, registry = rp.make_env()
    count(env, "1 make_env")
    bridged = HERE.parents[1] / "target" / "debug" / "bridged.exe"
    tmp = Path(tempfile.mkdtemp(prefix="aw-probe-"))
    state = tmp / "state"
    pipe = rf"\\.\pipe\agentroid-aw-probe-{os.getpid()}"
    proc = subprocess.Popen([str(bridged), "--adb", "auto", "--allow-emulators", "--pipe", pipe, "--state-dir", str(state)],
                            creationflags=NO_WINDOW, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    client = None
    try:
        log = state / "bridged.log"
        for _ in range(100):
            if log.exists() and "listening" in log.read_text(errors="replace"):
                break
            time.sleep(0.1)
        client = am.start_agentroid(pipe=pipe, session="aw-probe", stderr_path=tmp / "mcp.err")
        time.sleep(2)
        count(env, "2 bridged + MCP up")
        task_cls = registry["SystemBluetoothTurnOn"]
        task = task_cls(task_cls.generate_random_params())
        task.initialize_task(env)
        els = list(env.get_state().ui_elements)
        print(f"3 after task init: elements={len(els)} {[getattr(e, 'text', None) for e in els[:9]]}", flush=True)
        ex = am.Executor(client, device=a.serial)
        log1 = ex.execute({"action_type": "click", "index": 7}, els, tuple(env.logical_screen_size))
        print(f"   tap 7: error={log1.error} status={log1.status}", flush=True)
        for i in range(4):
            time.sleep(2)
            count(env, f"4.{i} after the Agentroid tap")
        if hasattr(env, "refresh_env"):
            env.refresh_env()
            time.sleep(3)
            count(env, "4 after refresh_env")
        print("bridged.log:\n" + log.read_text(errors="replace"), flush=True)
    finally:
        if client:
            client.close()
        subprocess.run([str(bridged), "--stop", "--state-dir", str(state)], creationflags=NO_WINDOW, timeout=30)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
        env.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
