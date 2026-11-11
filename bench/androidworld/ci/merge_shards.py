"""Merges the CI shards' results.json into one summary and prints it as Markdown (for $GITHUB_STEP_SUMMARY).

    python -B ci/merge_shards.py <dir with results-*/results.json> --config run3 --tasks all

It also writes <dir>/ci-merged.json (never merged.json, which is run_full's). Both scores are given on a bench-approve run (guarded = passed with no approval),
with the count of asks, the unscored tasks (usage limit, setup) and any shard that stopped at its cost cap. CI is its
own hardware configuration: these numbers are never added to the laptop's.
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import run_pilot  # noqa: E402
from ci.plan_shards import load_subset, plan  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("dir")
    ap.add_argument("--config", required=True)
    ap.add_argument("--tasks", default="all", help="the same value plan_shards was given; the headline is out of these")
    a = ap.parse_args()
    root = Path(a.dir)
    results, capped, shas, stopped = [], [], set(), []
    for f in sorted(root.glob("*/results.json")):
        d = json.loads(f.read_text(encoding="utf-8"))
        results += [run_pilot.TaskResult(**r) for r in d["results"]]
        meta = d.get("meta") or {}
        if meta.get("usd_cap_hit") is not None:
            capped.append(f"{f.parent.name} (USD {meta['usd_cap_hit']})")
        if meta.get("stopped_before"):
            stopped.append(f"{f.parent.name} (before {meta['stopped_before']})")
        if meta.get("bridged_sha256"):
            shas.add(meta["bridged_sha256"][:16])
    planned = [n for shard in plan(a.tasks, 1, "run2", load_subset()) for n in shard.split(",")]
    ran = {r.task for r in results}
    not_run = [n for n in planned if n not in ran]  # BENCH_STOP, a usage limit, a cap or a shard that died
    s = run_pilot.summarise(results)
    s.update({"config": a.config, "hardware": "GitHub Actions ubuntu-latest, KVM", "bridged_sha256": sorted(shas), "capped_shards": capped,
              "planned": len(planned), "not_run": not_run, "stopped_shards": stopped})
    (root / "ci-merged.json").write_text(json.dumps(s, indent=2), encoding="utf-8")
    print(f"## AndroidWorld, {a.config}, CI (ubuntu-latest, KVM)\n")
    print(f"- passed **{s['passed']} of {len(planned)}** planned (every task not run or not scored counts against it); "
          f"run {s['tasks']}, scored {s['scored']}")
    if "passed_guarded" in s:
        print(f"- guarded (no approval): **{s['passed_guarded']}**; asks approved by the bench stand-in: {s['approvals_total']} in {s['tasks_with_approvals']} tasks")
    print(f"- unscored: {', '.join(s['unscored']) or 'none'}")
    print(f"- not run: {len(not_run)}{(': ' + ', '.join(not_run)) if not_run else ''}")
    print(f"- shards stopped by BENCH_STOP: {', '.join(stopped) or 'none'}")
    print(f"- shards stopped at the cost cap: {', '.join(capped) or 'none'}")
    print(f"- bridged sha256: {', '.join(sorted(shas)) or 'unknown'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
