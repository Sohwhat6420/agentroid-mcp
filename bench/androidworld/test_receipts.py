"""receipts.py (D1 token receipts) and the per-call logs it reads: synthetic ledgers and result files, scripted
subprocess and network. No node, no emulator, no model call, no spend.

Run: python -B test_receipts.py
"""

from __future__ import annotations

import datetime as dt
import json
import os
import statistics
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

import policy_claude as pc
import receipts as R
import run_pilot as rp
from test_policy_claude import FakeResp
from test_run_pilot import FakeEnv, FakeExecutor, make_task

STARTED = "2026-10-27T09:21:10"  # local SGT, as run_pilot writes meta.started
T0 = dt.datetime(2026, 10, 27, 9, 21, 10, tzinfo=R.SGT).timestamp()


def row(t, usd, tag="l-agr", mode="api"):
    """One ledger row as anthropic-dispatch books an `api` call."""
    return {"ts": dt.datetime.fromtimestamp(t, dt.timezone.utc).isoformat().replace("+00:00", "Z"), "tag": tag, "mode": mode,
            "model": "claude-sonnet-5-5", "cost_usd": usd, "http": 200}


def task(name, success, steps, wall_s, ended="status"):
    return {"task": name, "goal": "", "success": success, "steps": steps, "ended": ended, "answer": None, "error": None, "wall_s": wall_s, "log": []}


def write_ledger(d, rows):
    p = Path(d) / "ledger.jsonl"
    p.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
    return p


class Pricing(unittest.TestCase):
    def test_published_sonnet_5_5_prices_and_a_plain_call(self):
        self.assertEqual(R.PRICES["claude-sonnet-5-5"], {"input": 2.0, "cache_write_5m": 2.5, "cache_write_1h": 4.0, "cache_read": 0.10, "output": 10.0})
        u = R.normalise_usage({"input_tokens": 1000, "output_tokens": 100})
        self.assertAlmostEqual(R.cost_usd(u, "sonnet"), 0.003)
        self.assertIsNone(R.cost_usd(u, "gemini/flash"), "an unpriced model has no cost, not a guessed one")

    def test_cache_writes_are_split_into_5m_and_1h_and_priced_apart(self):
        u = R.normalise_usage({"input_tokens": 10, "output_tokens": 0, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 300,
                               "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 200}, "output_tokens_details": {"thinking_tokens": 7}})
        self.assertEqual((u["cache_write_5m"], u["cache_write_1h"], u["cache_read"], u["thinking"]), (100, 200, 1000, 7))
        self.assertAlmostEqual(R.cost_usd(u, "claude-sonnet-5-5"), (10 * 2 + 1000 * 0.1 + 100 * 2.5 + 200 * 4) / 1e6)

    def test_an_openai_shaped_usage_block_is_normalised(self):
        u = R.normalise_usage({"prompt_tokens": 500, "completion_tokens": 40, "prompt_tokens_details": {"cached_tokens": 100}, "completion_tokens_details": {"reasoning_tokens": 30}})
        self.assertEqual((u["input"], u["output"], u["cache_read"], u["thinking"]), (400, 40, 100, 30))
        self.assertIsNone(R.normalise_usage(None))

    def test_billed_units_are_input_plus_five_times_output_and_exact(self):
        self.assertEqual(R.billed_units(0.00207, "sonnet"), 1035)
        u = {"input": 700, "output": 67, "cache_read": 0, "cache_write_5m": 0, "cache_write_1h": 0, "thinking": 0}
        self.assertEqual(R.billed_units(R.cost_usd(u, "sonnet"), "sonnet"), 700 + 5 * 67)

    def test_the_dispatch_stdout_line_gives_input_and_output(self):
        line = "run 2026-11-11T01-00-00-000Z-abc (own pool, claude-sonnet-5-5): HTTP 200, cost USD0.00 (812 in / 133 out), text written to C:/x/out.txt"
        self.assertEqual(R.parse_dispatch_stdout(line)["input"], 812)
        self.assertEqual(R.parse_dispatch_stdout(line)["output"], 133)
        self.assertIsNone(R.parse_dispatch_stdout("anthropic-dispatch: REFUSED"))

    def test_nearest_rank_percentile(self):
        self.assertEqual(R.pct([5, 1, 4, 2, 3], 90), 5)
        self.assertEqual(R.pct(list(range(1, 11)), 90), 9)
        self.assertEqual(R.pct([7], 50), 7)
        self.assertIsNone(R.pct([], 90))


class Join(unittest.TestCase):
    def test_windows_run_back_to_back_from_started(self):
        w = R.task_windows(STARTED, [task("a", True, 1, 10.0), task("b", False, 2, 5.5)])
        self.assertEqual(w, [(T0, T0 + 10.0), (T0 + 10.0, T0 + 15.5)])

    def test_rows_go_to_their_window_and_the_rest_are_stray(self):
        windows = [(100.0, 110.0), (110.0, 120.0)]
        rows = [{"t": t} for t in (101, 109, 111, 125, 50)]
        per, stray = R.assign(rows, windows, slack=0.0)
        self.assertEqual([[r["t"] for r in p] for p in per], [[101, 109], [111]])
        self.assertEqual(stray, [], "rows outside the span are not this chunk's")
        per, stray = R.assign([{"t": 115}], [(100.0, 110.0), (120.0, 130.0)], slack=0.0)
        self.assertEqual(([len(p) for p in per], [r["t"] for r in stray]), ([0, 0], [115]))

    def test_best_offset_moves_a_call_stamped_after_a_short_task_ends(self):
        # a 1-step task whose call returns 0.4 s after the (second-truncated) window ends, then a 2-step task
        results = [task("Verify", True, 1, 3.0), task("Next", True, 2, 10.0)]
        windows = R.task_windows(STARTED, results)
        rows = [{"t": T0 + 3.4}, {"t": T0 + 6.0}, {"t": T0 + 9.0}]
        per, _ = R.assign(rows, windows, slack=0.0)
        self.assertEqual([len(p) for p in per], [0, 3], "without the shift the Verify call lands in the next task")
        self.assertEqual(R.violations(per, results), 1, "a step with no call is impossible")
        off = R.best_offset(rows, windows, results)
        self.assertGreater(off, 0.4)
        per, _ = R.assign(rows, [(a + off, b + off) for a, b in windows], slack=0.0)
        self.assertEqual(([len(p) for p in per], R.violations(per, results)), ([1, 2], 0))

    def test_violations_counts_impossible_call_counts(self):
        rs = [task("a", True, 2, 1), task("b", True, 1, 1), task("c", None, 0, 1, "setup_error")]
        self.assertEqual(R.violations([[{}], [{}, {}, {}], [{}]], rs), 3)
        self.assertEqual(R.violations([[{}, {}], [{}, {}], []], rs), 0)


class Backfill(unittest.TestCase):
    def make_run(self, d, chunks):
        run = Path(d) / "results" / "full-x"
        for name, meta, results in chunks:
            p = run / name if name else run
            p.mkdir(parents=True, exist_ok=True)
            (p / "results.json").write_text(json.dumps({"meta": meta, "summary": {}, "results": results}), encoding="utf-8")
        return run

    def test_a_run_is_joined_task_by_task_and_the_checks_add_up(self):
        results = [task("Pass", True, 2, 20.0), task("Fail", False, 3, 30.0), task("NoRun", None, 0, 1.0, "setup_error")]
        meta = {"mode": "executor", "model": "sonnet", "policy": "v1", "started": STARTED, "policy_calls": 6}
        rows =[row(T0 + 5, 0.002), row(T0 + 12, 0.003), row(T0 + 25, 0.004), row(T0 + 30, 0.001), row(T0 + 35, 0.002), row(T0 + 44, 0.002),
                row(T0 + 120, 0.5), row(T0 - 3600, 0.9, tag="other")]
        with tempfile.TemporaryDirectory() as d:
            run = self.make_run(d, [("chunk-01", meta, results)])
            ledger = R.load_ledger(write_ledger(d, rows))
            self.assertEqual(len(ledger), 7, "other tags are not the bench's")
            b = R.backfill_run(run, ledger)
        rc = b["receipts"]
        self.assertEqual([r["model_calls"] for r in rc], [2, 4, 0])
        self.assertAlmostEqual(rc[0]["usd"], 0.005)
        self.assertEqual(rc[0]["tokens"]["billed_units"], 2500)
        self.assertEqual((rc[0]["tokens"]["input"], rc[0]["tokens"]["output"], rc[0]["tokens"]["cache_read"]), (None, None, 0), "the split is never invented")
        self.assertEqual(rc[1]["parse_retries"], 1)
        self.assertEqual(rc[0]["schema_overhead_tokens"], 0)
        self.assertFalse(rc[2]["in_analysis"])
        c = b["checks"]
        self.assertEqual((c["meta_calls"], c["ledger_calls"], c["tasks_impossible_call_count"], c["stray_calls"]), (6, 6, 0, 0))

    def test_rows_between_chunks_are_reported_as_stray_spend(self):
        r1 = [task("A", True, 1, 10.0)]
        r2 = [task("B", True, 1, 10.0)]
        later = "2026-10-27T09:31:10"  # 600 s after STARTED
        rows = [row(T0 + 5, 0.002), row(T0 + 300, 0.25), row(T0 + 605, 0.002)]  # the middle row: a failed chunk attempt
        with tempfile.TemporaryDirectory() as d:
            run = self.make_run(d, [("chunk-01", {"model": "sonnet", "started": STARTED, "policy_calls": 1}, r1),
                                    ("chunk-02", {"model": "sonnet", "started": later, "policy_calls": 1}, r2)])
            b = R.backfill_run(run, R.load_ledger(write_ledger(d, rows)))
        self.assertEqual((b["checks"]["stray_calls"], b["checks"]["stray_usd"]), (1, 0.25))
        self.assertEqual([r["model_calls"] for r in b["receipts"]], [1, 1])

    def test_a_free_backend_costs_nothing_and_claims_no_tokens(self):
        meta = {"backend": "omniroute", "model": "gemini/x", "started": STARTED, "policy_calls": 2}
        with tempfile.TemporaryDirectory() as d:
            run = self.make_run(d, [("chunk-01", meta, [task("A", True, 2, 10.0)])])
            b = R.backfill_run(run, R.load_ledger(write_ledger(d, [row(T0 + 5, 0.002)])))
        r = b["receipts"][0]
        self.assertEqual((r["usd"], r["tokens"]["basis"], r["model_calls"]), (0.0, "unknown", 0))

    def test_a_chunk_without_meta_is_flagged_not_guessed(self):
        with tempfile.TemporaryDirectory() as d:
            run = self.make_run(d, [(None, {}, [task("A", True, 2, 10.0)])])
            b = R.backfill_run(run, [])
        self.assertEqual(b["checks"]["chunks_without_meta"], ["full-x"])
        self.assertIsNone(b["receipts"][0]["usd"])


class Estimate(unittest.TestCase):
    def tearDown(self):
        R.ENVELOPE.update(k=None, c=None, n=0)

    def test_the_envelope_recovers_the_input_line_under_noisy_output(self):
        pairs = []
        for i in range(400):
            chars = 1000 + (i % 40) * 50
            out = 0 if i < 40 else (i * 37) % 200  # every prompt size has one call with no output, the rest 0..199
            pairs.append((chars, round(0.5 * chars + 1000) + 5 * out))
        k, c = R.fit_envelope(pairs)
        self.assertAlmostEqual(k, 0.5, delta=0.02)
        self.assertAlmostEqual(c, 1000, delta=40)
        with self.assertRaises(ValueError):
            R.fit_envelope([(1000, 1500)] * 3)

    def test_a_task_split_uses_the_matched_prompt_files_and_clips_output(self):
        R.ENVELOPE.update(k=0.5, c=1000.0, n=99)
        calls = [{"t": 100.0, "cost_usd": (2 * 1500 + 10 * 40) / 1e6}, {"t": 200.0, "cost_usd": (2 * 1500 + 10 * 900) / 1e6}]
        texts = [{"t": 100.002, "prompt_chars": 1000, "system_chars": 3000, "out_chars": 30, "out_empty": False},
                 {"t": 200.003, "prompt_chars": 1000, "system_chars": 3000, "out_chars": 0, "out_empty": True}]
        e = R.estimate_for_calls(calls, texts, "sonnet", max_tokens=300)
        self.assertEqual((e["output"], e["calls_with_empty_reply"]), (40 + 300, 1))
        self.assertEqual(e["input"], (1500 + 5 * 40 - 5 * 40) + (1500 + 5 * 900 - 5 * 300))
        self.assertIsNone(R.estimate_for_calls(calls, texts[:1], "sonnet"), "a call with no surviving prompt file gives no estimate")

    def test_call_files_are_read_as_sizes(self):
        with tempfile.TemporaryDirectory() as d:
            for n, (pr, out) in enumerate((("abc", '{"action_type": "wait"}'), ("abcdef", "  "))):
                c = Path(d) / f"aw-policy-{n}"
                c.mkdir()
                (c / "system.txt").write_text("S" * 10, encoding="utf-8")
                (c / "prompt.txt").write_text(pr, encoding="utf-8")
                (c / "out.txt").write_text(out, encoding="utf-8")
            (Path(d) / "aw-policy-empty").mkdir()  # a cleaned-out folder (as Storage Sense leaves them) is skipped
            rows = R.load_call_texts(d)
        self.assertEqual(sorted((r["prompt_chars"], r["out_empty"]) for r in rows), [(3, False), (6, True)])


class Summary(unittest.TestCase):
    def receipt(self, success, steps, usd, units, est=None):
        r = R.make_receipt(run="r", chunk=None, task=task("t", success, steps, 10.0 * steps), meta={"model": "sonnet"}, calls=[{}] * steps,
                           tokens={"billed_units": units}, usd=usd, usd_basis="ledger", estimate=est)
        return r

    def test_tokens_per_success_and_failure_cost(self):
        rs = [self.receipt(True, 2, 0.01, 5000), self.receipt(True, 4, 0.03, 15000), self.receipt(True, 6, 0.05, 25000),
              self.receipt(False, 10, 0.10, 50000), self.receipt(None, 0, 0.0, 0)]
        s = R.summarise(rs, "x")
        self.assertEqual((s["tasks"], s["passed"], s["excluded"]), (4, 3, 1))
        self.assertAlmostEqual(s["usd_total"], 0.19)
        self.assertAlmostEqual(s["usd_per_success_all_in"], round(0.19 / 3, 4))
        self.assertEqual((s["units_success_median"], s["units_success_p90"], s["units_per_success_all_in"]), (15000, 25000, round(95000 / 3)))
        self.assertAlmostEqual(s["usd_failures_total"], 0.10)
        self.assertEqual((s["steps_success_median"], s["retry_share"]), (4, 0.0))
        self.assertIsNone(s["est_output_success_median"], "no estimate unless every passed task has one")

    def test_a_task_stopped_by_the_pool_refusing_is_not_an_agent_failure(self):
        t = task("Cut", False, 10, 100.0, "action_error")
        t["error"] = "RuntimeError: anthropic-dispatch api failed (3): anthropic-dispatch: REFUSED - only USD0.05 is available"
        r = R.make_receipt(run="r", chunk=None, task=t, meta={}, calls=[{}] * 10, tokens={"billed_units": 1}, usd=0.1, usd_basis="ledger")
        self.assertFalse(r["in_analysis"])
        self.assertEqual(R.summarise([r], "x")["tasks"], 0)

    def test_the_markdown_has_a_row_per_run_and_says_n_a_for_unknowns(self):
        rows = [R.summarise([self.receipt(True, 2, 0.01, 5000)], "full-1"), R.summarise([], "empty")]
        md = R.render_markdown(rows, {"full-1": {"chunks": 1, "meta_calls": 2, "ledger_calls": 2, "stray_calls": 0, "stray_usd": 0.0,
                                                  "tasks_fewer_calls_than_steps": [], "chunks_without_meta": []}}, None)
        self.assertIn("| full-1 | 1 | 1 (100.0%) | $0.0100 |", md)
        self.assertIn("| empty | 0 | 0 (n/a%) | n/a |", md)
        self.assertIn(R.PRICING_SOURCE["url"], md)


class LiveEmission(unittest.TestCase):
    """The next run records usage per call and per step: dispatch_call, OmniRouteCall, run_task, receipt_from_live."""

    def test_dispatch_call_logs_the_usage_dispatch_prints(self):
        def run(cmd, **kw):
            Path(cmd[cmd.index("--out") + 1]).write_text('{"action_type": "wait"}', encoding="utf-8")
            return SimpleNamespace(returncode=0, stdout="run x (own pool, claude-sonnet-5-5): HTTP 200, cost USD0.00 (900 in / 120 out), text written to y", stderr="")

        call = pc.dispatch_call(run=run, sleep=lambda s: None)
        call("SYS", "PROMPT!")
        self.assertEqual(len(call.log), 1)
        rec = call.log[0]
        self.assertEqual((rec["usage"]["input"], rec["usage"]["output"], rec["prompt_chars"], rec["system_chars"], rec["reply_chars"], rec["attempts"]),
                         (900, 120, 7, 3, 23, 1))

    def test_omniroute_logs_the_usage_block_of_the_reply(self):
        body = json.dumps({"model": "m", "choices": [{"message": {"content": '{"action_type": "wait"}'}}], "usage": {"prompt_tokens": 50, "completion_tokens": 9}})
        call = pc.omniroute_call("gemini/x", opener=lambda req, timeout=None: FakeResp(body), sleep=lambda s: None, clock=lambda: 0.0)
        call("S", "P")
        self.assertEqual((call.log[0]["usage"], call.log[0]["model"]), ({"prompt_tokens": 50, "completion_tokens": 9}, "gemini/x"))
        self.assertIsNone(pc._usage_of("not json"))
        self.assertEqual(pc._usage_of('data: {"choices": []}\ndata: {"usage": {"prompt_tokens": 3}}\ndata: [DONE]'), {"prompt_tokens": 3})

    def test_run_task_attaches_each_steps_calls_and_the_receipt_adds_them_up(self):
        class Call:
            def __init__(self):
                self.log = []

        class Pol:
            def __init__(self, plan):
                self.call, self.plan = Call(), list(plan)

            def decide(self, goal, observation, history):
                n_calls, action = self.plan.pop(0)
                for _ in range(n_calls):
                    self.call.log.append({"usage": {"input": 1000, "output": 100, "cache_read": 0, "cache_write_5m": 0, "cache_write_1h": 0, "thinking": 0}})
                return action

        pol = Pol([(2, {"action_type": "click", "index": 0}), (1, {"action_type": "status", "goal_status": "complete"})])
        res = rp.run_task(FakeEnv(), "T", make_task(1.0), pol, FakeExecutor(), 5, sleep=lambda s: None)
        self.assertEqual([len(s["calls"]) for s in res.log], [2, 1])
        from dataclasses import asdict

        rc = R.receipt_from_live(asdict(res), {"model": "sonnet", "mode": "executor", "backend": "anthropic"})
        self.assertEqual((rc["model_calls"], rc["parse_retries"], rc["tokens"]["input"], rc["tokens"]["output"], rc["tokens"]["basis"]), (3, 1, 3000, 300, "measured"))
        self.assertAlmostEqual(rc["usd"], 3 * 0.003)
        self.assertEqual(rc["tokens"]["billed_units"], 3 * 1500)
        self.assertEqual(rc["pricing"], R.PRICING_SOURCE)
        summary = rp.summarise([rp.TaskResult("T", success=True, steps=2, receipt=rc)])
        self.assertEqual((summary["tokens_in_total"], summary["tokens_out_total"], summary["model_calls_total"]), (3000, 300, 3))

    def test_a_policy_without_a_call_log_adds_no_calls_key_and_older_summaries_are_unchanged(self):
        class Pol:
            def decide(self, goal, observation, history):
                return {"action_type": "status", "goal_status": "complete"}

        res = rp.run_task(FakeEnv(), "T", make_task(1.0), Pol(), FakeExecutor(), 3, sleep=lambda s: None)
        self.assertNotIn("calls", res.log[0])
        self.assertNotIn("usd_total", rp.summarise([res]))

    def test_partial_usage_is_marked_and_a_free_backend_is_zero_usd(self):
        res = task("T", True, 2, 5.0)
        res["log"] = [{"calls": [{"usage": {"prompt_tokens": 10, "completion_tokens": 2}}]}, {"calls": [{"usage": None}]}]
        rc = R.receipt_from_live(res, {"backend": "omniroute", "model": "gemini/x"})
        self.assertEqual((rc["usd"], rc["tokens"]["input"], rc["tokens"]["basis"]), (0.0, 10, "measured (partial)"))
        self.assertTrue(any("returned no usage" in n for n in rc["notes"]))


if __name__ == "__main__":
    unittest.main()
