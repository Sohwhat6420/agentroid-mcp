"""Token receipts for the AndroidWorld bench (differentiator D1): one receipt per task, and an aggregator that turns
the receipts into a tokens-per-success table.

Two sources, never mixed without saying so:

* **Live (from the next run on):** `policy_claude.dispatch_call` and `OmniRouteCall` keep a per-call log (`call.log`)
  with the usage the API returned; `run_pilot.run_task` attaches each step's calls to its log entry and a receipt to
  the task (`receipt_from_live`). Basis `measured`.
* **Backfill (runs before 10 Nov 2026):** the result files hold no token counts. The API ledger
  (`$AGENTROID_LEDGER`) holds one row per model call with the cost the dispatcher computed
  from the API's own `usage` block. A chunk's `meta.started` plus the tasks' `wall_s` (back to back) give each task's
  time window, and the ledger rows inside a window are that task's calls (`backfill`). This gives, per task, a **billed**
  USD and the exact number `input + 5 x output` tokens (Sonnet 5.5 bills output at 5x input and these requests send no
  cache_control, so cache reads and writes are 0); it does not give input and output separately. Basis `ledger`.
  Where the per-call prompt files survive in %TEMP% (`aw-policy-*`, only full-2 as of 10 Nov 2026), `estimate_split`
  splits the two with a lower-envelope fit. Basis `estimate`, never reported as measured.

    python -B receipts.py --runs full-1 full-2 v2-1 osmand-marker-rerun --subset-view full-1:subset-20.json \
        --calls-dir %TEMP% --out results/receipts

Prices: `PRICES` below, from platform.claude.com/docs/en/about-claude/pricing, read 10 Nov 2026 (identical to the
figures the dispatcher uses, read 18 Oct and re-verified 19 Oct 2026).
"""

from __future__ import annotations

import argparse
import bisect
import datetime as dt
import glob
import json
import os
import re
import statistics
from pathlib import Path
from typing import Any, Iterable, Sequence

HERE = Path(__file__).resolve().parent
# The dispatcher's ledger (jsonl): set AGENTROID_LEDGER; unset means no backfill source (pass --ledger).
LEDGER = Path(os.environ["AGENTROID_LEDGER"]) if os.environ.get("AGENTROID_LEDGER") else Path("anthropic-ledger.jsonl")
SCHEMA = "agentroid.bench.task-receipt/1"

PRICING_SOURCE = {"url": "https://platform.claude.com/docs/en/about-claude/pricing", "read": "2026-11-10",
                  "note": "USD per million tokens, global routing, standard (not batch) rates; Haiku 5.5 row is the up-to-100k-token-prompt rate"}
PRICES: dict[str, dict[str, float]] = {
    "claude-sonnet-5-5": {"input": 2.0, "cache_write_5m": 2.5, "cache_write_1h": 4.0, "cache_read": 0.10, "output": 10.0},
    "claude-opus-5-5": {"input": 4.0, "cache_write_5m": 5.0, "cache_write_1h": 8.0, "cache_read": 0.20, "output": 20.0},
    "claude-haiku-5-5": {"input": 0.10, "cache_write_5m": 0.125, "cache_write_1h": 0.20, "cache_read": 0.01, "output": 0.50},
}
ALIASES = {"sonnet": "claude-sonnet-5-5", "opus": "claude-opus-5-5", "haiku": "claude-haiku-5-5"}
EXECUTOR_SCHEMA_NOTE = "executor mode: the model is sent a fixed system prompt and the element list, no tool schemas"


def model_id(name: str | None) -> str | None:
    if not name:
        return None
    return ALIASES.get(name, name)


def price_of(model: str | None) -> dict[str, float] | None:
    return PRICES.get(model_id(model) or "")


# ------------------------------------------------------------------------------------------------ usage and cost

def normalise_usage(u: dict[str, Any] | None) -> dict[str, int] | None:
    """An Anthropic `usage` block or an OpenAI-style one (OmniRoute) as
    {input, output, cache_read, cache_write_5m, cache_write_1h, thinking}; None when there is none."""
    if not u:
        return None
    if "prompt_tokens" in u or "completion_tokens" in u:  # chat-completions shape
        details = u.get("completion_tokens_details") or {}
        cached = (u.get("prompt_tokens_details") or {}).get("cached_tokens") or 0
        return {"input": int(u.get("prompt_tokens") or 0) - int(cached), "output": int(u.get("completion_tokens") or 0),
                "cache_read": int(cached), "cache_write_5m": 0, "cache_write_1h": 0, "thinking": int(details.get("reasoning_tokens") or 0)}
    total_w = int(u.get("cache_creation_input_tokens") or 0)
    cc = u.get("cache_creation") or {}
    w1 = int(cc.get("ephemeral_1h_input_tokens") or 0)
    w5 = total_w - w1
    thinking = int(((u.get("output_tokens_details") or {}).get("thinking_tokens")) or 0)
    return {"input": int(u.get("input_tokens") or 0), "output": int(u.get("output_tokens") or 0),
            "cache_read": int(u.get("cache_read_input_tokens") or 0), "cache_write_5m": w5, "cache_write_1h": w1, "thinking": thinking}


def cost_usd(usage: dict[str, int] | None, model: str | None) -> float | None:
    """USD for one normalised usage block at PRICES; None for an unknown model or no usage (free backends: 0 by the caller)."""
    p = price_of(model)
    if p is None or usage is None:
        return None
    return (usage["input"] * p["input"] + usage["output"] * p["output"] + usage["cache_read"] * p["cache_read"]
            + usage["cache_write_5m"] * p["cache_write_5m"] + usage["cache_write_1h"] * p["cache_write_1h"]) / 1e6


def billed_units(usd: float, model: str | None) -> int | None:
    """Input-token equivalents of a billed amount: for an uncached call, input + (output price / input price) x output.
    On Sonnet 5.5 that is input + 5 x output, exact (the ledger costs are whole multiples of USD 2e-6)."""
    p = price_of(model)
    if p is None:
        return None
    return round(usd * 1e6 / p["input"])


DISPATCH_USAGE = re.compile(r"cost USD[\d.]+ \((\d+) in / (\d+) out\)")


def parse_dispatch_stdout(text: str) -> dict[str, int] | None:
    """The `(N in / M out)` the dispatcher prints for an `api` call (the dispatcher's `api` mode). It carries no cache
    fields; the request sends no cache_control, so they are 0."""
    m = DISPATCH_USAGE.search(text or "")
    if not m:
        return None
    return {"input": int(m.group(1)), "output": int(m.group(2)), "cache_read": 0, "cache_write_5m": 0, "cache_write_1h": 0, "thinking": 0}


# ------------------------------------------------------------------------------------------------ stats helpers

def pct(values: Sequence[float], q: float) -> float | None:
    """Nearest-rank percentile (q in 0..100); None for no values."""
    v = sorted(values)
    if not v:
        return None
    k = max(1, -(-len(v) * q // 100))  # ceil(n q / 100), at least 1
    return v[int(k) - 1]


def median(values: Sequence[float]) -> float | None:
    return statistics.median(values) if values else None


# ------------------------------------------------------------------------------------------------ the receipt

def _tokens_block(**kw: Any) -> dict[str, Any]:
    keys = ("input", "output", "cache_read", "cache_write", "thinking", "billed_units", "basis")
    return {k: kw.get(k) for k in keys}


def make_receipt(*, run: str, chunk: str | None, task: dict[str, Any], meta: dict[str, Any], calls: Sequence[dict[str, Any]],
                 tokens: dict[str, Any], usd: float | None, usd_basis: str, estimate: dict[str, Any] | None = None,
                 notes: Sequence[str] = ()) -> dict[str, Any]:
    steps = int(task.get("steps") or 0)
    # The pool's credit ran out mid-task (full-2, 7 Nov: "anthropic-dispatch: REFUSED"): an infrastructure stop, not an
    # agent failure, so the task is kept out of the analysis like a task that never ran.
    refused = "anthropic-dispatch: REFUSED" in str(task.get("error") or "")
    in_analysis = task.get("success") is not None and steps > 0 and not refused
    return {
        "schema": SCHEMA,
        "run": run,
        "chunk": chunk,
        "task": task.get("task"),
        "policy": meta.get("policy") or "v1",
        "backend": meta.get("backend") or "anthropic",
        "model": model_id(meta.get("model")),
        "success": task.get("success"),
        "ended": task.get("ended"),
        "in_analysis": in_analysis,  # scored, at least one step (full-2 has 0-step 'passes': no run), not stopped by the pool
        "steps": steps,
        "wall_s": task.get("wall_s"),
        "model_calls": len(calls),
        "parse_retries": max(0, len(calls) - steps) if calls else None,
        "tokens": tokens,
        "tokens_estimate": estimate,
        "schema_overhead_tokens": 0 if (meta.get("mode") or "executor") == "executor" else None,
        "schema_overhead_basis": EXECUTOR_SCHEMA_NOTE if (meta.get("mode") or "executor") == "executor" else "native mode: see STATE.md token bodies",
        "usd": None if usd is None else round(usd, 6),
        "usd_basis": usd_basis,
        "pricing": PRICING_SOURCE if usd_basis.startswith("computed") else None,
        "notes": list(notes),
    }


def receipt_from_live(res: dict[str, Any], meta: dict[str, Any], run: str = "live", chunk: str | None = None) -> dict[str, Any]:
    """A receipt from a task result whose log entries carry `calls` (the per-call log the next run records)."""
    calls = [c for step in res.get("log") or [] for c in step.get("calls") or []]
    model = model_id(meta.get("model"))
    free = (meta.get("backend") or "anthropic") == "omniroute"
    usages = [normalise_usage(c.get("usage")) if "input" not in (c.get("usage") or {}) else c.get("usage") for c in calls]
    known = [u for u in usages if u is not None]
    notes = []
    if not calls:
        return make_receipt(run=run, chunk=chunk, task=res, meta=meta, calls=[], tokens=_tokens_block(basis="unknown"), usd=None,
                            usd_basis="unknown", notes=["no per-call log on this task"])
    if len(known) < len(calls):
        notes.append(f"{len(calls) - len(known)} of {len(calls)} calls returned no usage")
    tot = {k: sum(u[k] for u in known) for k in ("input", "output", "cache_read", "cache_write_5m", "cache_write_1h", "thinking")}
    if free:
        usd, basis = 0.0, "free backend (OmniRoute): no charge"
    else:
        costs = [cost_usd(u, model) for u in known]
        usd = sum(c for c in costs if c is not None) if all(c is not None for c in costs) else None
        basis = "computed from measured usage at PRICES" if usd is not None else "unknown model price"
    units = billed_units(usd, model) if (usd is not None and not free) else None
    tokens = _tokens_block(input=tot["input"], output=tot["output"], cache_read=tot["cache_read"],
                           cache_write=tot["cache_write_5m"] + tot["cache_write_1h"], thinking=tot["thinking"] or None,
                           billed_units=units, basis="measured" if len(known) == len(calls) else "measured (partial)")
    return make_receipt(run=run, chunk=chunk, task=res, meta=meta, calls=calls, tokens=tokens, usd=usd, usd_basis=basis, notes=notes)


# ------------------------------------------------------------------------------------------------ backfill from the ledger

SGT = dt.timezone(dt.timedelta(hours=8))


def parse_ts(s: str) -> float:
    return dt.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


def load_ledger(path: Path = LEDGER, tag: str = "l-agr") -> list[dict[str, Any]]:
    """The bench's `api` rows (one per model call), oldest first, each with a float epoch `t`."""
    rows = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        r = json.loads(line)
        if r.get("tag") == tag and r.get("mode") == "api":
            r["t"] = parse_ts(r["ts"])
            rows.append(r)
    rows.sort(key=lambda r: r["t"])
    return rows


def task_windows(started: str, results: Sequence[dict[str, Any]], tz: dt.tzinfo = SGT) -> list[tuple[float, float]]:
    """Each task's [start, end) in epoch seconds: run_pilot writes `meta.started` (local time, to the second) just before the
    first task and the tasks run back to back, each timed by `wall_s` (setup to tear-down)."""
    t = dt.datetime.strptime(started, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=tz).timestamp()
    out = []
    for r in results:
        w = float(r.get("wall_s") or 0.0)
        out.append((t, t + w))
        t += w
    return out


def assign(rows: Sequence[dict[str, Any]], windows: Sequence[tuple[float, float]], slack: float = 1.0) -> tuple[list[list[dict[str, Any]]], list[dict[str, Any]]]:
    """Ledger rows per window, plus the rows in the chunk's span (with `slack` seconds either side, for the second-truncated
    start) that fall in no window. A row on a boundary goes to the earlier window."""
    per: list[list[dict[str, Any]]] = [[] for _ in windows]
    stray: list[dict[str, Any]] = []
    if not windows:
        return per, stray
    lo, hi = windows[0][0] - slack, windows[-1][1] + slack
    starts = [w[0] for w in windows]
    for r in rows:
        t = r["t"]
        if not lo <= t <= hi:
            continue
        i = bisect.bisect_right(starts, t) - 1
        if 0 <= i < len(windows) and t <= windows[i][1] + slack:
            per[i].append(r)
        elif i < 0 and t >= lo:
            per[0].append(r)
        else:
            stray.append(r)
    return per, stray


def violations(per: Sequence[Sequence[dict[str, Any]]], results: Sequence[dict[str, Any]], max_calls_per_step: int = 2) -> int:
    """Tasks whose joined call count cannot be right: fewer calls than steps (every step makes at least one call), or more
    than `max_calls_per_step` per step (ClaudePolicy retries a bad reply once), or calls on a task that took no step."""
    bad = 0
    for calls, r in zip(per, results):
        s = int(r.get("steps") or 0)
        n = len(calls)
        if n < s or n > max_calls_per_step * s:
            bad += 1
    return bad


def best_offset(rows: Sequence[dict[str, Any]], windows: Sequence[tuple[float, float]], results: Sequence[dict[str, Any]],
                span: float = 2.0, step: float = 0.05) -> float:
    """`meta.started` is truncated to the second and a ledger row is stamped when its call returns, so a call that ends in the
    last instant of a short task can land in the next window. Shift all windows by the smallest offset in [0, span] that
    leaves the fewest impossible call counts (`violations`)."""
    best, best_bad = 0.0, None
    k = 0
    while k * step <= span + 1e-9:
        d = k * step
        per, _ = assign(rows, [(a + d, b + d) for a, b in windows], slack=0.0)
        bad = violations(per, results)
        if best_bad is None or bad < best_bad:
            best, best_bad = d, bad
        k += 1
    return round(best, 3)


def chunk_files(run_dir: Path) -> list[tuple[str | None, Path]]:
    chunks = sorted(run_dir.glob("chunk-*/results.json"))
    if chunks:
        return [(p.parent.name, p) for p in chunks]
    single = run_dir / "results.json"
    return [(None, single)] if single.exists() else []


def backfill_run(run_dir: Path, ledger: Sequence[dict[str, Any]], texts: Sequence[dict[str, Any]] | None = None) -> dict[str, Any]:
    """Receipts for every task of a past run, from its result files and the ledger. Returns
    {"receipts": [...], "checks": {...}} where checks says how well the join held."""
    run = run_dir.name
    receipts: list[dict[str, Any]] = []
    checks = {"chunks": 0, "meta_calls": 0, "ledger_calls": 0, "stray_calls": 0, "stray_usd": 0.0, "tasks_fewer_calls_than_steps": [],
              "tasks_impossible_call_count": 0, "chunks_without_meta": [], "offsets_s": []}
    spans: list[tuple[float, float] | None] = []
    joined: list[dict[str, Any]] = []
    for chunk, path in chunk_files(run_dir):
        doc = json.loads(path.read_text(encoding="utf-8"))
        meta, results = doc.get("meta") or {}, doc.get("results") or []
        checks["chunks"] += 1
        free = (meta.get("backend") or "anthropic") == "omniroute"
        if "started" not in meta:
            checks["chunks_without_meta"].append(chunk or run)
            for r in results:
                receipts.append(make_receipt(run=run, chunk=chunk, task=r, meta=meta, calls=[], tokens=_tokens_block(basis="unknown"),
                                             usd=None, usd_basis="unknown", notes=["no meta.started: cannot join to the ledger"]))
            continue
        checks["meta_calls"] += int(meta.get("policy_calls") or 0)
        windows = task_windows(meta["started"], results)
        if free:
            per = [[] for _ in results]
        else:
            near = [x for x in ledger if windows[0][0] - 5 <= x["t"] <= windows[-1][1] + 5] if windows else []
            off = best_offset(near, windows, results)
            windows = [(a + off, b + off) for a, b in windows]
            per, _ = assign(near, windows, slack=0.0)
            checks["offsets_s"].append(off)
            spans.append((windows[0][0], windows[-1][1]) if windows else None)
            joined.extend(c for calls in per for c in calls)
        model = model_id(meta.get("model"))
        for r, calls in zip(results, per):
            if free:
                receipts.append(make_receipt(run=run, chunk=chunk, task=r, meta=meta, calls=[], tokens=_tokens_block(basis="unknown"), usd=0.0,
                                             usd_basis="free backend (OmniRoute): no charge; tokens were not logged",
                                             notes=["per-call usage was not recorded before 10 Nov 2026"]))
                continue
            checks["ledger_calls"] += len(calls)
            if int(r.get("steps") or 0) > len(calls):
                checks["tasks_fewer_calls_than_steps"].append(f"{chunk or run}/{r.get('task')}")
            if violations([calls], [r]):
                checks["tasks_impossible_call_count"] += 1
            usd = sum(c["cost_usd"] for c in calls)
            units = billed_units(usd, model)
            tokens = _tokens_block(cache_read=0, cache_write=0, billed_units=units,
                                   basis="ledger: billed USD and input+5*output exact; split not logged")
            est = None
            if texts is not None:
                est = estimate_for_calls(calls, texts, model, max_tokens=300)
            receipts.append(make_receipt(run=run, chunk=chunk, task=r, meta=meta, calls=calls, tokens=tokens, usd=usd,
                                         usd_basis="ledger (the dispatcher cost from the API's usage block)", estimate=est,
                                         notes=[] if calls or not r.get("steps") else ["no ledger rows in this task's window"]))
    # Calls between the run's first task and its last that belong to no task: failed chunk attempts, crashed runs, or
    # another job on the same tag. They are real spend for the run but not for any task.
    live = [s for s in spans if s]
    if live:
        lo, hi = min(s[0] for s in live), max(s[1] for s in live)
        ids = {id(c) for c in joined}
        stray = [x for x in ledger if lo <= x["t"] <= hi and id(x) not in ids]
        checks["stray_calls"], checks["stray_usd"] = len(stray), round(sum(x["cost_usd"] for x in stray), 6)
    return {"run": run, "receipts": receipts, "checks": checks}


# ------------------------------------------------------------------------------------------------ estimate (full-2 texts)

def load_call_texts(temp_dir: str | Path) -> list[dict[str, Any]]:
    """Sizes of the per-call files dispatch_call left in %TEMP% (`aw-policy-*/{system,prompt,out}.txt`), keyed by the mtime
    of out.txt (written a few ms after the ledger row). Sizes only: the texts stay where they are."""
    rows = []
    for d in glob.glob(os.path.join(str(temp_dir), "aw-policy-*")):
        out, pr, sy = (os.path.join(d, n) for n in ("out.txt", "prompt.txt", "system.txt"))
        if not (os.path.exists(out) and os.path.exists(pr) and os.path.exists(sy)):
            continue
        reply = Path(out).read_text(encoding="utf-8", errors="replace")
        rows.append({"t": os.path.getmtime(out), "system_chars": len(Path(sy).read_text(encoding="utf-8", errors="replace")),
                     "prompt_chars": len(Path(pr).read_text(encoding="utf-8", errors="replace")), "out_chars": len(reply),
                     "out_empty": not reply.strip()})
    rows.sort(key=lambda r: r["t"])
    return rows


def match_texts(calls: Sequence[dict[str, Any]], texts: Sequence[dict[str, Any]], tol: float = 2.0) -> list[dict[str, Any] | None]:
    """For each ledger row, the call-file record written just after it (within `tol` seconds), else None."""
    ts = [x["t"] for x in texts]
    out = []
    for c in calls:
        i = bisect.bisect_left(ts, c["t"] - 0.05)
        cand = [j for j in (i, i + 1) if 0 <= j < len(texts) and -0.05 <= texts[j]["t"] - c["t"] <= tol]
        out.append(texts[cand[0]] if cand else None)
    return out


ENVELOPE = {"k": None, "c": None, "n": 0}  # set by fit_envelope; per-call input estimate = k * prompt_chars + c


def fit_envelope(pairs: Sequence[tuple[int, int]], bin_chars: int = 250, min_bin: int = 5) -> tuple[float, float]:
    """Line through the per-bin minimum of billed units against prompt size: the cheapest calls of a given prompt size are
    the ones with the least output, so the envelope approximates the input tokens (plus the shortest reply, about
    10 to 20 tokens, so the output estimate is biased low by that much). `pairs` are (prompt_chars, billed_units)."""
    bins: dict[int, list[tuple[int, int]]] = {}
    for chars, units in pairs:
        bins.setdefault(chars // bin_chars, []).append((chars, units))
    xs, ys = [], []
    for b in sorted(bins):
        pts = bins[b]
        if len(pts) >= min_bin:
            low = min(pts, key=lambda p: p[1])  # the cheapest call of the bin, at its own prompt size
            xs.append(low[0])
            ys.append(low[1])
    if len(xs) < 2:
        raise ValueError("not enough prompt-size bins to fit an envelope")
    k, c = statistics.linear_regression(xs, ys)
    return k, c


def estimate_for_calls(calls: Sequence[dict[str, Any]], texts: Sequence[dict[str, Any]], model: str | None, max_tokens: int = 300) -> dict[str, Any] | None:
    """Input/output split for a task's calls with the fitted envelope; None when the envelope is not fitted or a call has
    no surviving prompt file."""
    if ENVELOPE["k"] is None or not calls:
        return None
    p = price_of(model)
    if p is None:
        return None
    ratio = p["output"] / p["input"]
    matched = match_texts(calls, texts)
    if any(m is None for m in matched):
        return None
    tin = tout = empty = 0
    for c, m in zip(calls, matched):
        units = billed_units(c["cost_usd"], model)
        out = min(max(0.0, (units - (ENVELOPE["k"] * m["prompt_chars"] + ENVELOPE["c"])) / ratio), float(max_tokens))
        tout += round(out)
        tin += units - round(ratio * round(out))
        empty += 1 if m["out_empty"] else 0
    return {"input": tin, "output": tout, "calls_with_empty_reply": empty,
            "method": f"lower-envelope fit on {ENVELOPE['n']} calls (input ~ {ENVELOPE['k']:.3f} x prompt chars + {ENVELOPE['c']:.0f}); output = (billed units - input) / {ratio:g}, clipped to 0..{max_tokens}; biased low by the shortest reply"}


def prepare_estimate(ledger: Sequence[dict[str, Any]], texts: Sequence[dict[str, Any]], model: str = "claude-sonnet-5-5") -> dict[str, Any]:
    """Fit ENVELOPE on every ledger row that has a surviving prompt file."""
    matched = match_texts(ledger, texts)
    pairs = [(m["prompt_chars"], billed_units(c["cost_usd"], model)) for c, m in zip(ledger, matched) if m is not None and m["system_chars"] > 10]
    k, c = fit_envelope(pairs)
    ENVELOPE.update(k=k, c=c, n=len(pairs))
    return dict(ENVELOPE)


# ------------------------------------------------------------------------------------------------ aggregate and report

def summarise(receipts: Sequence[dict[str, Any]], label: str) -> dict[str, Any]:
    """The tokens-per-success row for a set of receipts (only tasks `in_analysis`)."""
    rs = [r for r in receipts if r["in_analysis"]]
    ok = [r for r in rs if r["success"]]
    bad = [r for r in rs if not r["success"]]
    have_usd = all(r["usd"] is not None for r in rs) and bool(rs)
    units = lambda xs: [r["tokens"]["billed_units"] for r in xs if r["tokens"]["billed_units"] is not None]  # noqa: E731
    usd_all = sum(r["usd"] for r in rs) if have_usd else None
    est_ok = [r["tokens_estimate"] for r in ok if r.get("tokens_estimate")]
    row = {
        "label": label,
        "tasks": len(rs), "passed": len(ok), "pass_rate": round(len(ok) / len(rs), 4) if rs else None,
        "excluded": len(receipts) - len(rs),
        "usd_total": round(usd_all, 4) if usd_all is not None else None,
        "usd_per_success_all_in": round(usd_all / len(ok), 4) if (usd_all is not None and ok) else None,
        "usd_success_median": median([r["usd"] for r in ok]) if have_usd else None,
        "usd_success_p90": pct([r["usd"] for r in ok], 90) if have_usd else None,
        "usd_failures_total": round(sum(r["usd"] for r in bad), 4) if have_usd else None,
        "usd_failure_mean": round(sum(r["usd"] for r in bad) / len(bad), 4) if (have_usd and bad) else None,
        "units_success_median": median(units(ok)), "units_success_p90": pct(units(ok), 90),
        "units_per_success_all_in": round(sum(units(rs)) / len(ok)) if (ok and len(units(rs)) == len(rs)) else None,
        "est_input_success_median": median([e["input"] for e in est_ok]) if est_ok and len(est_ok) == len(ok) else None,
        "est_output_success_median": median([e["output"] for e in est_ok]) if est_ok and len(est_ok) == len(ok) else None,
        "est_output_success_p90": pct([e["output"] for e in est_ok], 90) if est_ok and len(est_ok) == len(ok) else None,
        "steps_success_median": median([r["steps"] for r in ok]), "steps_success_p90": pct([r["steps"] for r in ok], 90),
        "wall_s_success_median": median([r["wall_s"] for r in ok]),
        "calls": sum(r["model_calls"] for r in rs), "steps": sum(r["steps"] for r in rs),
    }
    row["retry_share"] = round((row["calls"] - row["steps"]) / row["calls"], 4) if row["calls"] else None
    return row


def _f(v: Any, nd: int = 0, money: bool = False) -> str:
    if v is None:
        return "n/a"
    if money:
        return f"${v:.4f}" if v < 0.1 else f"${v:.3f}"
    return f"{v:,.{nd}f}"


def render_markdown(rows: Sequence[dict[str, Any]], checks: dict[str, dict[str, Any]], envelope: dict[str, Any] | None) -> str:
    lines = ["# Tokens per success (AndroidWorld, executor mode) - generated by receipts.py", "",
             f"Prices: {PRICING_SOURCE['url']} (read {PRICING_SOURCE['read']}). Billed units = input + 5 x output tokens on Sonnet 5.5, "
             "exact from the API ledger. USD is the billed amount. Cache tokens are 0 (no cache_control is sent). "
             "Tool-schema overhead is 0 in executor mode. Only tasks that were scored and took at least one step are counted.", "",
             "| Run | Tasks | Passed | USD total | USD per success (all-in) | USD per passed task, median / p90 | USD on failures | Billed units per passed task, median / p90 | Billed units per success (all-in) | Steps per pass, median / p90 | Wall s per pass, median | Calls that were retries |",
             "|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for r in rows:
        lines.append(f"| {r['label']} | {r['tasks']} | {r['passed']} ({_f(100 * r['pass_rate'] if r['pass_rate'] is not None else None, 1)}%) | {_f(r['usd_total'], money=True)} | "
                     f"{_f(r['usd_per_success_all_in'], money=True)} | {_f(r['usd_success_median'], money=True)} / {_f(r['usd_success_p90'], money=True)} | "
                     f"{_f(r['usd_failures_total'], money=True)} | {_f(r['units_success_median'])} / {_f(r['units_success_p90'])} | {_f(r['units_per_success_all_in'])} | "
                     f"{_f(r['steps_success_median'])} / {_f(r['steps_success_p90'])} | {_f(r['wall_s_success_median'], 1)} | "
                     + (f"{_f(100 * r['retry_share'], 1)}% |" if r["retry_share"] is not None else "n/a |"))
    est = [r for r in rows if r["est_output_success_median"] is not None]
    if est:
        lines += ["", "## ESTIMATE: input / output split (only where the per-call prompt files survive)", "",
                  f"Method: {envelope and 'lower-envelope fit on %d calls, input ~ %.3f x prompt chars + %.0f' % (envelope['n'], envelope['k'], envelope['c'])}. "
                  "Not measured; biased low on output by roughly the shortest reply.", "",
                  "| Run | Est. input tokens per passed task, median | Est. output tokens per passed task, median / p90 |", "|---|---|---|"]
        for r in est:
            lines.append(f"| {r['label']} | {_f(r['est_input_success_median'])} | {_f(r['est_output_success_median'])} / {_f(r['est_output_success_p90'])} |")
    lines += ["", "## Join checks", ""]
    for run, c in checks.items():
        lines.append(f"- {run}: {c['chunks']} chunk(s); calls by meta {c['meta_calls']}, ledger rows joined {c['ledger_calls']}; "
                     f"rows in the run's span outside any task {c['stray_calls']} (${c['stray_usd']:.4f}); "
                     f"tasks with fewer ledger rows than steps: {len(c['tasks_fewer_calls_than_steps'])}"
                     + (f"; chunks without meta: {c['chunks_without_meta']}" if c["chunks_without_meta"] else ""))
    return "\n".join(lines) + "\n"


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--runs", nargs="+", required=True, help="run folders under results/ (full-1, full-2, ...)")
    ap.add_argument("--results", default=str(HERE / "results"))
    ap.add_argument("--ledger", default=str(LEDGER))
    ap.add_argument("--calls-dir", help="where the aw-policy-* call folders are (usually %%TEMP%%); enables the ESTIMATE split")
    ap.add_argument("--subset-view", action="append", default=[], help="run:subset.json, an extra row for that run restricted to the subset's tasks")
    ap.add_argument("--out", default=str(HERE / "results" / "receipts"))
    args = ap.parse_args(argv)

    ledger = load_ledger(Path(args.ledger))
    texts = envelope = None
    if args.calls_dir:
        texts = load_call_texts(args.calls_dir)
        if texts:
            envelope = prepare_estimate(ledger, texts)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    rows, checks, by_run = [], {}, {}
    for run in args.runs:
        b = backfill_run(Path(args.results) / run, ledger, texts)
        by_run[run], checks[run] = b["receipts"], b["checks"]
        (out / f"{run}.receipts.jsonl").write_text("".join(json.dumps(r) + "\n" for r in b["receipts"]), encoding="utf-8")
        rows.append(summarise(b["receipts"], run))
    for view in args.subset_view:
        run, subset = view.split(":", 1)
        names = set(json.loads((HERE / subset).read_text(encoding="utf-8"))["tasks"])
        rows.append(summarise([r for r in by_run[run] if r["task"] in names], f"{run} on {Path(subset).stem}"))
    (out / "summary.json").write_text(json.dumps({"rows": rows, "checks": checks, "envelope": envelope, "pricing": PRICING_SOURCE, "prices": PRICES}, indent=2) + "\n", encoding="utf-8")
    (out / "TOKENS-PER-SUCCESS.md").write_text(render_markdown(rows, checks, envelope), encoding="utf-8")
    print(render_markdown(rows, checks, envelope))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
