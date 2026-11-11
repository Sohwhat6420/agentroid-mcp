"""One real task end to end, no model: AndroidWorld OpenAppTaskEval on the emulator, driven through Agentroid.

Proves the whole path (AndroidWorld env -> indexed elements -> JSONAction -> action_map -> Agentroid MCP ->
bridged daemon -> adb -> emulator -> AndroidWorld's own scorer). A scripted Policy reads the app name from the goal.

Run with the AndroidWorld venv, emulator up:
    python -B smoke_openapp.py
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import agentroid_mcp as am
import run_pilot as rp

SCAFFOLD = Path(__file__).resolve().parents[2]
BRIDGED = SCAFFOLD / "target" / "debug" / "bridged.exe"
DEVICE = "emulator-5554"


class OpenThenDone:
    def decide(self, goal, observation, history):
        if not history:
            app = re.search(r"[Oo]pen the (?:app )?(.+?)(?: app)?\.?$", goal)
            return {"action_type": "open_app", "app_name": (app.group(1) if app else goal).strip()}
        return {"action_type": "status", "goal_status": "complete"}


def main() -> int:
    tmp = tempfile.mkdtemp(prefix="aw-smoke-")
    state = Path(tmp) / "state"
    pipe = rf"\\.\pipe\agentroid-aw-smoke-{os.getpid()}"
    proc = subprocess.Popen([str(BRIDGED), "--adb", "auto", "--allow-emulators", "--pipe", pipe, "--state-dir", str(state)],
                            creationflags=0x08000000, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    client = None
    try:
        log = state / "bridged.log"
        for _ in range(100):
            if log.exists() and "listening" in log.read_text(errors="replace"):
                break
            time.sleep(0.1)
        client = am.start_agentroid(pipe=pipe, session="aw-smoke", stderr_path=Path(tmp) / "mcp.err")
        env, reg = rp.make_env()
        try:
            res = rp.run_task(env, "OpenAppTaskEval", reg["OpenAppTaskEval"], OpenThenDone(), am.Executor(client, device=DEVICE), max_steps=5)
        finally:
            env.close()
        print(f"goal={res.goal!r} ended={res.ended} success={res.success} steps={res.steps} error={res.error} wall={res.wall_s}s")
        print("log:", res.log)
        return 0 if res.success else 1
    finally:
        if client:
            client.close()
        subprocess.run([str(BRIDGED), "--stop", "--state-dir", str(state)], creationflags=0x08000000, timeout=30)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


if __name__ == "__main__":
    sys.exit(main())
