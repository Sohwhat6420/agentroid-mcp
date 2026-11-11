"""Diagnostic (11 Nov): does the AndroidWorld env return a UI tree on this emulator right now?

    python -B triage/probe_tree.py

Builds the env exactly as run_pilot does, reads the state five times (3 s apart), with and without
wait_to_stabilize, and prints the element counts, then one refresh_env and five more reads. Drives nothing.
"""

from __future__ import annotations

import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import run_pilot as rp  # noqa: E402


def main() -> int:
    env, _registry = rp.make_env()
    for i in range(3):
        s = env.get_state()
        print(f"default read {i}: elements={len(list(s.ui_elements))}", flush=True)
    for i in range(5):
        s = env.get_state(wait_to_stabilize=False)
        px = None if s.pixels is None else getattr(s.pixels, "shape", "?")
        print(f"read {i}: elements={len(list(s.ui_elements))} pixels={px}", flush=True)
        time.sleep(3)
    if hasattr(env, "refresh_env"):
        print("refresh_env ...", flush=True)
        env.refresh_env()
        time.sleep(5)
        for i in range(5):
            s = env.get_state(wait_to_stabilize=False)
            print(f"after refresh {i}: elements={len(list(s.ui_elements))}", flush=True)
            time.sleep(3)
    env.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
