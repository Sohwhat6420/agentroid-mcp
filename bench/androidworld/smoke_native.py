"""Native-mode smoke: a headless Claude (the dispatcher agent) uses the Agentroid MCP tools itself,
against `bridged --fake` (no phone, no emulator). One task, one pool, a small cap.

    python -B smoke_native.py [--pool POOL] [--budget 1] [--model sonnet] [--print-args]

--print-args shows the claude.exe command and spends nothing. Writes the transcript to --out.
The MCP config carries no credential-looking key or value (the dispatcher refuses one).
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCAFFOLD = HERE.parents[1]
BRIDGED = SCAFFOLD / "target" / "debug" / "bridged.exe"
SERVER = SCAFFOLD / "apps" / "mcp" / "src" / "sdk-wiring.ts"
# The model dispatcher script (node), named by the AGENTROID_DISPATCH environment variable.
DISPATCH = Path(os.environ["AGENTROID_DISPATCH"]) if os.environ.get("AGENTROID_DISPATCH") else None
NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0  # creationflags is Windows-only

PROMPT = (
    "You have the Agentroid tools (mcp__agentroid__*). Using only those tools: "
    "1) list the devices; 2) on the one device that is not read-only, read the screen and say in one line what the top "
    "of the screen shows. Do not act on a read-only device. Finish with one line starting 'SMOKE-DONE' naming the device you read."
)


def build_config(pipe: str) -> dict:
    return {"mcpServers": {"agentroid": {
        "command": "node",
        "args": ["--experimental-strip-types", "--no-warnings", str(SERVER)],
        "env": {"AGENTROID_PIPE": pipe, "AGENTROID_SESSION": "aw-native-smoke", "AGENTROID_PROFILE": "full", "AGENTROID_NO_PUSH": "1"},
    }}}


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pool", default="own", help="which API key pool the dispatcher should use (optional)")
    ap.add_argument("--budget", default="1")
    ap.add_argument("--model", default="sonnet")
    ap.add_argument("--print-args", action="store_true")
    ap.add_argument("--out", default=str(HERE / "results" / "smoke-native.txt"))
    args = ap.parse_args(argv)

    tmp = Path(tempfile.mkdtemp(prefix="aw-native-"))
    pipe = rf"\\.\pipe\agentroid-aw-native-{os.getpid()}"
    cfg = tmp / "agentroid-mcp.json"
    cfg.write_text(json.dumps(build_config(pipe), indent=2), encoding="utf-8")
    if DISPATCH is None:
        raise SystemExit("AGENTROID_DISPATCH is not set: point it at the model-dispatcher node script")
    cmd = ["node", str(DISPATCH), "agent", PROMPT, "--model", args.model, "--pool", args.pool, "--budget", str(args.budget),
           "--tools", "Read,mcp__agentroid__*", "--mcp-config", str(cfg), "--tag", "l-agr", "--for", "AndroidWorld native-mode smoke (native mode)", "--timeout-min", "10"]
    if args.print_args:
        r = subprocess.run(cmd + ["--print-args"], capture_output=True, text=True, encoding="utf-8", errors="replace", creationflags=NO_WINDOW)
        print(r.stdout, r.stderr)
        return r.returncode

    state = tmp / "state"
    proc = subprocess.Popen([str(BRIDGED), "--fake", "--pipe", pipe, "--state-dir", str(state)], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, creationflags=NO_WINDOW)
    try:
        log = state / "bridged.log"
        for _ in range(100):
            if log.exists() and "listening" in log.read_text(errors="replace"):
                break
            time.sleep(0.1)
        r = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", errors="replace", creationflags=NO_WINDOW)
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(f"exit {r.returncode}\n--- stdout ---\n{r.stdout}\n--- stderr ---\n{r.stderr}\n", encoding="utf-8")
        print(f"exit {r.returncode}; transcript in {out}")
        print(r.stdout[-1500:])
        return 0 if r.returncode == 0 and "SMOKE-DONE" in r.stdout else 1
    finally:
        subprocess.run([str(BRIDGED), "--stop", "--state-dir", str(state)], creationflags=NO_WINDOW, timeout=30)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


if __name__ == "__main__":
    sys.exit(main())
