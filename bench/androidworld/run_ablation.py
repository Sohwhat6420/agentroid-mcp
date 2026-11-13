"""The policy v3 ablation, end to end, on the user's Claude subscription: one command once `claude auth login` is done.

    python -B run_ablation.py [--subset subset-20.json] [--model opus] [--then-full] [--dry-run]

Order:
  1. preflight: `claude auth status` says loggedIn, memory commit under 75%, C: over 30 GB free (warns under 100);
  2. the stream-json image probe (policy-v3/probe_stream_json.py) must pass;
  3. A0 (the v2 prompt alone, on the warm session), then each change added ON TOP OF WHAT WAS KEPT, in the ladder's
     order (screens, planner, verifier, notes, stall). A change is kept only when it clears its threshold over the
     best score so far; otherwise the next change is tried without it;
  4. with --then-full: one full 116-task run with the kept flags.

Each stage is one run_full.py call (chunks, a fresh emulator per chunk), so a stopped run resumes where it was. Create
results/v3-ablation/STOP to end after the current stage. Memory is rechecked before every stage (75%: wait, up to
--wait-min; 85%: stop). Writes results/v3-ablation/ABLATION.json and ABLATION.md, and prints one line per stage.
Every child process is hidden (CREATE_NO_WINDOW).
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Sequence

HERE = Path(__file__).resolve().parent
NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0  # creationflags is Windows-only
ORDER = ("screens", "planner", "verifier", "notes", "stall")
# gain over the best score so far needed to keep a change (one run each, so under 2 tasks is noise)
THRESHOLD = {"screens": 2, "planner": 2, "verifier": 1, "notes": 0, "stall": 0}
MEM_WAIT, MEM_STOP = 75.0, 85.0
DISK_FLOOR_GB = 30  # hard stop: the run writes under 1 GB, but the emulator must not fill C: mid-run
DISK_WARN_GB = 100  # the device rule; below it the run still starts and says so
# The honesty rule, carried into every report this writes
HONESTY = ("**How these numbers were chosen.** The v3 flags in the full run were picked by this ablation on subset-20, "
           "which is part of the 116 tasks, and the v2 and v3 designs were informed by the earlier v1 and v2 runs' failures. "
           "So the full-run score is not a held-out number. Leaderboard entries carry similar caveats (AGI-0 trained on the tasks themselves).")
MODEL_IDS = {"sonnet": "claude-sonnet-5-5", "opus": "claude-opus-5-5", "haiku": "claude-haiku-5-5", "fable": "claude-fable-5-1"}


@dataclass
class Stage:
    label: str
    flags: tuple[str, ...]
    added: str | None = None
    passed: int | None = None
    scored: int | None = None
    kept: bool | None = None
    note: str = ""


@dataclass
class Ladder:
    """The keep-or-drop walk. Pure: the runner feeds it scores."""

    kept: list[str] = field(default_factory=list)
    best: int | None = None
    stages: list[Stage] = field(default_factory=list)

    def baseline(self) -> Stage:
        s = Stage("A0", ())
        self.stages.append(s)
        return s

    def candidate(self, flag: str) -> Stage:
        s = Stage(f"+{flag}", tuple(self.kept) + (flag,), added=flag)
        self.stages.append(s)
        return s

    def record(self, s: Stage, passed: int, scored: int) -> None:
        s.passed, s.scored = passed, scored
        if s.added is None:
            self.best = passed
            s.kept = True
            return
        need = (self.best or 0) + THRESHOLD[s.added]
        s.kept = passed >= need
        s.note = f"needed >= {need}"
        if s.kept:
            self.kept.append(s.added)
            self.best = passed


def hidden(argv: list[str], timeout: int | None = None, **kw) -> subprocess.CompletedProcess:
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout, creationflags=NO_WINDOW, **kw)


def logged_in() -> bool:
    exe = shutil.which("claude")
    if not exe:
        return False
    try:
        r = hidden([exe, "auth", "status"], timeout=30)
        return bool(json.loads(r.stdout or "{}").get("loggedIn"))
    except (OSError, ValueError, subprocess.TimeoutExpired):
        return False


def memory_pct() -> float:
    ps = "$o=Get-CimInstance Win32_OperatingSystem; [math]::Round(100*($o.TotalVirtualMemorySize-$o.FreeVirtualMemory)/$o.TotalVirtualMemorySize,1)"
    r = hidden(["powershell", "-NoProfile", "-NonInteractive", "-Command", ps], timeout=60)
    return float(r.stdout.strip())


def disk_free_gb() -> float:
    return shutil.disk_usage("C:\\").free / 2**30


def wait_for_memory(wait_min: int, log) -> bool:
    deadline = time.monotonic() + wait_min * 60
    while True:
        m = memory_pct()
        if m < MEM_WAIT:
            return True
        if m >= MEM_STOP or time.monotonic() > deadline:
            log(f"memory {m}% committed: stopping (gate {MEM_WAIT}/{MEM_STOP}%)")
            return False
        log(f"memory {m}% committed: waiting (under {MEM_WAIT}% to start)")
        time.sleep(300)


def load_done(out_root: Path) -> dict[str, tuple[tuple[str, ...], int, int]]:
    """--resume: the stages an earlier run already scored, from its ABLATION.json (label -> flags, passed, scored)."""
    f = out_root / "ABLATION.json"
    if not f.exists():
        return {}
    rows = json.loads(f.read_text(encoding="utf-8"))["stages"]
    return {r["stage"]: (tuple(r["flags"]), int(r["passed"]), int(r["scored"])) for r in rows if r["passed"] is not None}


def run_stage(s: Stage, a, out_root: Path, extra: Sequence[str] = ()) -> tuple[int, int]:
    out = out_root / s.label.replace("+", "plus-")
    argv = [sys.executable, "-B", str(HERE / "run_full.py"), "--subset", str(a.subset), "--out", str(out),
            "--chunk", str(a.chunk), "--backend", "claude-p", "--policy", "v3", "--v3", ",".join(s.flags),
            "--model", a.model, "--fresh-emulator", *extra]
    if a.effort:
        argv += ["--effort", a.effort]
    r = hidden(argv)
    last = [ln for ln in (r.stdout or "").splitlines() if ln.startswith("{")]
    if r.returncode != 0 or not last:
        raise RuntimeError(f"{s.label}: run_full exit {r.returncode}: {(r.stderr or '')[-400:]}")
    summ = json.loads(last[-1])
    return int(summ["passed"]), int(summ["scored"])


def write_report(ladder: Ladder, out_root: Path, a) -> None:
    out_root.mkdir(parents=True, exist_ok=True)
    rows = [{"stage": s.label, "flags": list(s.flags), "passed": s.passed, "scored": s.scored, "kept": s.kept, "note": s.note}
            for s in ladder.stages]
    (out_root / "ABLATION.json").write_text(json.dumps({"model": a.model, "subset": str(a.subset), "kept": ladder.kept,
                                                         "best": ladder.best, "stages": rows}, indent=1) + "\n", encoding="utf-8")
    md = ["# Policy v3 ablation on " + Path(a.subset).name + f" ({a.model}, a Claude subscription via claude -p)", "",
          "| Stage | Flags | Passed | Kept | Note |", "| --- | --- | --- | --- | --- |"]
    for r in rows:
        flags = ", ".join(r["flags"]) or "(v2 prompt alone)"
        score = "-" if r["passed"] is None else f"{r['passed']} / {r['scored']}"
        md.append(f"| {r['stage']} | {flags} | {score} | {'yes' if r['kept'] else ('no' if r['kept'] is False else '-')} | {r['note']} |")
    md += ["", f"Kept: {', '.join(ladder.kept) or 'none'}. Best: {ladder.best}.", "", HONESTY]
    (out_root / "ABLATION.md").write_text("\n".join(md) + "\n", encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--subset", default=str(HERE / "subset-20.json"))
    ap.add_argument("--out", default=str(HERE / "results" / "v3-ablation"))
    ap.add_argument("--model", default="opus")
    ap.add_argument("--effort", default=None)
    ap.add_argument("--chunk", type=int, default=10)
    ap.add_argument("--wait-min", type=int, default=60, help="how long to wait for memory under 75%% before a stage")
    ap.add_argument("--then-full", action="store_true", help="then one full 116-task run with the kept flags")
    ap.add_argument("--resume", action="store_true", help="replay the stages already in --out/ABLATION.json and run only the rest")
    ap.add_argument("--bench-approve", action="store_true", help="the full 116 only: the emulator-only stand-in approver (his 11 Nov answer: report both)")
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args(argv)
    out_root = Path(a.out)

    def log(msg: str) -> None:
        print(time.strftime("%H:%M:%S ") + msg, flush=True)

    if a.dry_run:
        log("plan: preflight (login, memory < 75%, C: > 30 GB, warn < 100) -> probe -> A0 -> " + " -> ".join(f"+{f} (keep if >= best+{THRESHOLD[f]})" for f in ORDER)
            + (" -> full 116 with the kept flags" if a.then_full else ""))
        return 0
    if not logged_in():
        log("not signed in: run `claude auth login` (MAIN account) first")
        return 3
    free = disk_free_gb()
    if free < DISK_FLOOR_GB:
        log(f"C: has {free:.0f} GB free (hard floor {DISK_FLOOR_GB}): stopping")
        return 4
    if free < DISK_WARN_GB:
        log(f"warning: C: has {free:.0f} GB free, under the device's {DISK_WARN_GB} GB guide (this run adds under 1 GB)")
    if not wait_for_memory(a.wait_min, log):
        return 5
    probe = hidden([sys.executable, "-B", str(HERE / "policy-v3" / "probe_stream_json.py"), MODEL_IDS.get(a.model, a.model)], timeout=600)
    log(f"probe: exit {probe.returncode}")
    (out_root).mkdir(parents=True, exist_ok=True)
    (out_root / "probe.json").write_text(probe.stdout or "", encoding="utf-8")
    if probe.returncode != 0:
        log("probe failed: see probe.json; stopping before any emulator run")
        return 6

    ladder = Ladder()
    done = load_done(out_root) if a.resume else {}
    plan = [None, *ORDER]
    for flag in plan:
        label = "A0" if flag is None else f"+{flag}"
        if label in done:  # already scored by an earlier run: replay it into the ladder, don't run it again
            s = ladder.baseline() if flag is None else ladder.candidate(flag)
            flags, passed, scored = done[label]
            if flags != s.flags:
                log(f"--resume: {label} ran with {flags}, the ladder now gives {s.flags}: stopping")
                return 7
            ladder.record(s, passed, scored)
            log(f"{label}: {passed}/{scored} kept={s.kept} (from the earlier run)")
            continue
        if (out_root / "STOP").exists():
            log("STOP file: ending after the last stage")
            break
        if not wait_for_memory(a.wait_min, log):
            break
        s = ladder.baseline() if flag is None else ladder.candidate(flag)
        passed, scored = run_stage(s, a, out_root)
        ladder.record(s, passed, scored)
        write_report(ladder, out_root, a)
        log(f"{s.label} [{','.join(s.flags) or 'v2'}]: {passed}/{scored} kept={s.kept} {s.note}")

    if a.then_full and ladder.best is not None and not (out_root / "STOP").exists() and wait_for_memory(a.wait_min, log):
        full = Stage("full-116", tuple(ladder.kept))
        a.subset = str(HERE / "subset-all.json")
        a.chunk = 15
        passed, scored = run_stage(full, a, out_root.parent / "v3-full-1", ["--bench-approve"] if a.bench_approve else [])
        log(f"FULL 116 [{','.join(full.flags)}]: {passed}/{scored} = {100 * passed / max(scored, 1):.1f}%")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
