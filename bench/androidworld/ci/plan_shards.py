"""Splits the AndroidWorld task list into CI shards and prints `shards=<json>` for $GITHUB_OUTPUT.

    python -B ci/plan_shards.py --tasks all|smoke|A,B,C --shards 13 --config run3

Each shard is a comma list of task names in subset order (contiguous, so a shard's cost is close to the laptop's
chunk of the same tasks). `smoke` is two cheap tasks that pass on the laptop (AudioRecorderRecordAudio, CameraTakePhoto:
USD 0.49 and 0.65 there). `control` plans no shards.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
SMOKE = ["AudioRecorderRecordAudio", "CameraTakePhoto"]


def load_subset() -> list[str]:
    subset = json.loads((HERE / "subset-all.json").read_text(encoding="utf-8"))
    if isinstance(subset, dict):
        subset = subset.get("tasks") or list(subset)
    return list(subset)


def plan(tasks: str, shards: int, config: str, subset: list[str]) -> list[str]:
    if config == "control":
        return []
    if tasks == "smoke":
        return [",".join(SMOKE)]  # one shard: the smoke checks one whole job end to end
    elif tasks == "all":
        names = subset
    else:
        names = [t.strip() for t in tasks.split(",") if t.strip()]
        unknown = [t for t in names if t not in subset]
        if unknown:
            raise SystemExit(f"not in subset-all.json: {', '.join(unknown)}")
    n = max(1, min(shards, len(names)))
    size, extra = divmod(len(names), n)
    out, i = [], 0
    for k in range(n):
        take = size + (1 if k < extra else 0)
        out.append(",".join(names[i:i + take]))
        i += take
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tasks", default="smoke")
    ap.add_argument("--shards", type=int, default=13)
    ap.add_argument("--config", required=True, choices=["control", "run2", "run3"])
    a = ap.parse_args()
    print("shards=" + json.dumps(plan(a.tasks, a.shards, a.config, load_subset())))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
