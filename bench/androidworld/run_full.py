"""The full AndroidWorld run in chunks, one run_pilot.py process per chunk (a fresh daemon and environment each time).

    python -B run_full.py --out results/full-1 [--chunk 15] [--pool POOL] [--model sonnet] [--fresh-emulator]

A chunk is skipped when its results.json already holds every task of it, so a stopped run resumes. Create a file
named STOP in --out to end the run after the current chunk (pace guard). Prints one line per chunk.
Task budget: AndroidWorld's own (complexity x 10 steps), the same budget AndroidWorld uses. The score is NOT directly comparable with leaderboard entries that use a different agent loop or observation.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0  # creationflags is Windows-only


def chunks(names: list[str], size: int) -> list[list[str]]:
    return [names[i:i + size] for i in range(0, len(names), size)]


def chunk_done(out: Path, names: list[str]) -> bool:
    f = out / "results.json"
    if not f.exists():
        return False
    try:
        got = {r["task"] for r in json.loads(f.read_text(encoding="utf-8"))["results"]}
    except (OSError, ValueError, KeyError):
        return False
    return set(names) <= got


def merge(out: Path) -> dict:
    results = []
    for f in sorted(out.glob("chunk-*/results.json")):
        results += json.loads(f.read_text(encoding="utf-8"))["results"]
    scored = [r for r in results if r["success"] is not None]
    passed = sum(1 for r in scored if r["success"])
    summary = {"tasks": len(results), "scored": len(scored), "passed": passed, "pass_rate": round(passed / len(scored), 4) if scored else None,
               "unscored": [r["task"] for r in results if r["success"] is None], "steps_total": sum(r["steps"] for r in results)}
    import run_pilot
    summary.update(run_pilot.guarded_totals(results))
    (out / "merged.json").write_text(json.dumps({"summary": summary, "results": results}, indent=1) + "\n", encoding="utf-8")
    return summary


def pilot_argv(args, cdir: Path, part: list[str]) -> list[str]:
    """The run_pilot.py command line for one chunk. The default (anthropic) backend keeps its original arguments exactly."""
    argv = [sys.executable, "-B", str(HERE / "run_pilot.py"), "--subset", args.subset, "--out", str(cdir), "--max-steps", "0",
            "--pool", args.pool, "--model", args.model, "--policy", args.policy, "--tasks", ",".join(part)]
    if args.backend != "anthropic":
        argv += ["--backend", args.backend]
        if args.max_tokens:
            argv += ["--max-tokens", str(args.max_tokens)]
    if args.policy == "v3":  # agent mode on a Claude subscription
        argv += ["--v3", args.v3]
        if args.planner_model:
            argv += ["--planner-model", args.planner_model]
        if args.effort:
            argv += ["--effort", args.effort]
    if getattr(args, "bench_approve", False):
        argv += ["--bench-approve"]
    return argv


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--subset", default=str(HERE / "subset-all.json"))
    ap.add_argument("--out", required=True)
    ap.add_argument("--chunk", type=int, default=15)
    ap.add_argument("--pool", default="own", help="which API key pool the dispatcher should use (optional)")
    ap.add_argument("--backend", default="anthropic", choices=["anthropic", "omniroute", "claude-p"], help="claude-p = a Claude subscription via a warm `claude -p` session (policy v3 only); omniroute = the local free-provider router (--model is then a model id, or a comma-separated fixed fallback order)")
    ap.add_argument("--model", default="sonnet")
    ap.add_argument("--max-tokens", type=int, default=None, help="omniroute: max_tokens per model call (default: run_pilot's 1024)")
    ap.add_argument("--policy", default="v1", choices=["v1", "v2", "v3"])
    ap.add_argument("--v3", default="A5", help="v3: ladder rung A1..A5 or a flag list ('' = the v2 prompt alone, the A0 baseline)")
    ap.add_argument("--planner-model", default=None)
    ap.add_argument("--effort", default=None)
    ap.add_argument("--fresh-emulator", action="store_true", help="restart the emulator before every chunk (emulator_ctl.py) and stop it at the end")
    ap.add_argument("--bench-approve", action="store_true", help="pass --bench-approve to every chunk (emulator-only stand-in approver)")
    args = ap.parse_args(argv)
    # a run folder can ask for it too (run-config.json {"bench_approve": true}), for a run whose command line is already fixed
    cfg = Path(args.out) / "run-config.json"
    if cfg.exists() and json.loads(cfg.read_text(encoding="utf-8")).get("bench_approve"):
        args.bench_approve = True

    names = json.loads(Path(args.subset).read_text(encoding="utf-8"))["tasks"]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    plan = chunks(names, args.chunk)
    for i, part in enumerate(plan, 1):
        cdir = out / f"chunk-{i:02d}"
        if chunk_done(cdir, part):
            print(f"chunk {i}/{len(plan)} already done", flush=True)
            continue
        if (out / "STOP").exists():
            print(f"STOP file found: ending before chunk {i}", flush=True)
            break
        if args.fresh_emulator:
            import emulator_ctl
            print(f"chunk {i}/{len(plan)}: fresh emulator {emulator_ctl.restart()}", flush=True)
        t0 = time.monotonic()
        r = subprocess.run(pilot_argv(args, cdir, part), cwd=HERE, creationflags=NO_WINDOW if sys.platform == "win32" else 0, capture_output=True, text=True, encoding="utf-8", errors="replace")
        (out / f"chunk-{i:02d}.log").write_text(r.stdout + "\n--- stderr ---\n" + r.stderr, encoding="utf-8")
        print(f"chunk {i}/{len(plan)} exit {r.returncode} in {time.monotonic() - t0:.0f} s; so far {merge(out)}", flush=True)
    if args.fresh_emulator:
        import emulator_ctl
        print("; ".join(emulator_ctl.stop()), flush=True)
    print(json.dumps(merge(out)), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
