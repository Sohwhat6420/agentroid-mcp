"""run_pilot.py with a fake environment, task, policy and executor: no emulator, no android_world.

Run: python -B test_run_pilot.py
"""

from __future__ import annotations

import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import run_pilot as rp

HERE = Path(__file__).resolve().parent


class FakeEnv:
    def __init__(self):
        self.interaction_cache = ""
        self.logical_screen_size = (1080, 2400)
        self.elements = [SimpleNamespace(text="Save", class_name="android.widget.Button", is_clickable=True)]

    def get_state(self):
        return SimpleNamespace(ui_elements=self.elements)


def make_task(score=1.0, setup_error=None, score_error=None):
    class T:
        goal = "do the thing"
        torn_down = False
        inits = 0

        @staticmethod
        def generate_random_params():
            return {}

        def __init__(self, params):
            pass

        def initialize_task(self, env):
            if setup_error:
                raise RuntimeError(setup_error)
            T.inits += 1

        def is_successful(self, env):
            if score_error:
                raise RuntimeError(score_error)
            return score

        def tear_down(self, env):
            T.torn_down = True

    return T


class Script:
    def __init__(self, *actions):
        self.actions = list(actions)
        self.seen = []

    def decide(self, goal, observation, history):
        self.seen.append((goal, observation, len(history)))
        return self.actions.pop(0) if self.actions else {"action_type": "wait"}


class FakeExecutor:
    def __init__(self, fail_on=None):
        self.fail_on = fail_on
        self.calls = []

    def execute(self, action, elements, screen):
        self.calls.append((action, screen))
        err = "tap: boom" if self.fail_on == action.get("action_type") else None
        return SimpleNamespace(
            error=err,
            answer=action.get("text") if action["action_type"] == "answer" else None,
            status=action.get("goal_status") if action["action_type"] == "status" else None,
        )


def run(task_cls, policy, executor=None, max_steps=5, env=None):
    return rp.run_task(env or FakeEnv(), "T", task_cls, policy, executor or FakeExecutor(), max_steps, sleep=lambda s: None)


class RunTask(unittest.TestCase):
    def test_a_finished_task_is_scored_and_torn_down(self):
        t = make_task(1.0)
        pol = Script({"action_type": "click", "index": 0}, {"action_type": "status", "goal_status": "complete"})
        r = run(t, pol)
        self.assertEqual((r.success, r.ended, r.steps, r.error), (True, "status", 2, None))
        self.assertTrue(t.torn_down)
        self.assertEqual(r.goal, "do the thing")
        self.assertIn("[0] Button 'Save' clickable", pol.seen[0][1], "the policy sees the indexed elements")
        self.assertEqual(pol.seen[1][2], 1, "and its own history")

    def test_the_history_names_what_each_index_pointed_at(self):
        pol = Script({"action_type": "click", "index": 0}, {"action_type": "click", "index": 9}, {"action_type": "status", "goal_status": "complete"})
        seen = []
        orig = pol.decide

        def spy(goal, obs, history):
            seen.append([h.get("target") for h in history])
            return orig(goal, obs, history)

        pol.decide = spy
        run(make_task(1.0), pol)
        self.assertEqual(seen[-1], ["Save", None], "an index outside the screen has no label")

    def test_the_task_decides_success_not_the_policy(self):
        r = run(make_task(0.0), Script({"action_type": "status", "goal_status": "complete"}))
        self.assertEqual((r.success, r.ended), (False, "status"))

    def test_running_out_of_steps_still_scores(self):
        r = run(make_task(0.0), Script(), max_steps=3)
        self.assertEqual((r.ended, r.steps, r.success), ("max_steps", 3, False))

    def test_an_executor_error_ends_the_task_but_it_is_still_scored(self):
        t = make_task(0.0)
        r = run(t, Script({"action_type": "click", "index": 0}, {"action_type": "wait"}), FakeExecutor(fail_on="click"))
        self.assertEqual((r.ended, r.steps, r.success, r.error), ("action_error", 1, False, "tap: boom"))
        self.assertTrue(t.torn_down)

    def test_an_answer_reaches_the_environment_cache(self):
        env = FakeEnv()
        r = run(make_task(1.0), Script({"action_type": "answer", "text": "42"}, {"action_type": "status", "goal_status": "complete"}), env=env)
        self.assertEqual((env.interaction_cache, r.answer), ("42", "42"))

    def test_the_screen_size_is_passed_to_the_executor(self):
        ex = FakeExecutor()
        run(make_task(1.0), Script({"action_type": "status", "goal_status": "complete"}), ex)
        self.assertEqual(ex.calls[0][1], (1080, 2400))

    def test_a_setup_failure_is_a_result_and_nothing_is_torn_down(self):
        t = make_task(setup_error="no such app")
        r = run(t, Script())
        self.assertEqual((r.ended, r.success, r.steps), ("setup_error", None, 0))
        self.assertIn("no such app", r.error)
        self.assertFalse(t.torn_down)

    def test_a_scoring_failure_is_unscored_and_still_tears_down(self):
        t = make_task(score_error="adb gone")
        r = run(t, Script({"action_type": "status", "goal_status": "complete"}))
        self.assertEqual((r.success, r.ended), (None, "score_error"))
        self.assertTrue(t.torn_down)


class Settle(unittest.TestCase):
    class Changing:
        """An env whose screen shows `old` for the first `lag` reads after an action, then `new`."""

        def __init__(self, lag, new_reads=()):
            self.lag, self.reads, self.fixed = lag, 0, None
            self.old = [SimpleNamespace(text="Old", class_name="android.widget.Button", is_clickable=True)]
            self.new = [SimpleNamespace(text="New", class_name="android.widget.Button", is_clickable=True)]
            self.logical_screen_size = (1080, 2400)
            self.interaction_cache = ""

        def get_state(self):
            self.reads += 1
            return SimpleNamespace(ui_elements=self.old if self.reads <= self.lag else self.new)

    def test_it_waits_out_a_slow_transition_instead_of_returning_the_old_screen(self):
        env, clock = self.Changing(lag=0), [0.0]
        env.get_state = lambda: SimpleNamespace(ui_elements=env.old if clock[0] < 1.2 else env.new)  # the new screen lands 1.2 s after the action
        els = rp.settled_elements(env, lambda s: clock.__setitem__(0, clock[0] + s))
        self.assertEqual(els[0].text, "New")

    def test_it_returns_once_two_reads_agree(self):
        env = self.Changing(lag=0)
        rp.settled_elements(env, lambda s: None)
        self.assertEqual(env.reads, 2)

    def test_a_screen_that_never_settles_still_returns(self):
        class Flicker(self.Changing):
            def get_state(self):
                self.reads += 1
                return SimpleNamespace(ui_elements=[SimpleNamespace(text=str(self.reads), class_name="x")])

        env = Flicker(lag=0)
        rp.settled_elements(env, lambda s: None)
        self.assertEqual(env.reads, rp.SETTLE_POLLS)

    def test_the_policy_sees_the_screen_after_its_last_action(self):
        env = self.Changing(lag=0)
        env.get_state = lambda: SimpleNamespace(ui_elements=env.old if not ex_calls else env.new)
        ex_calls = []

        class Ex(FakeExecutor):
            def execute(self, action, elements, screen):
                ex_calls.append(action)
                return super().execute(action, elements, screen)

        pol = Script({"action_type": "click", "index": 0}, {"action_type": "status", "goal_status": "complete"})
        rp.run_task(env, "T", make_task(1.0), pol, Ex(), 5, sleep=lambda s: None)
        self.assertIn("'Old'", pol.seen[0][1])
        self.assertIn("'New'", pol.seen[1][1])


class DateAndBudget(unittest.TestCase):
    class Aware(Script):
        begun = None

        def begin_task(self, today=None, max_steps=None):
            self.begun = (today, max_steps)

    def test_a_policy_that_wants_the_date_and_budget_gets_both(self):
        pol = self.Aware({"action_type": "status", "goal_status": "complete"})
        rp.run_task(FakeEnv(), "T", make_task(1.0), pol, FakeExecutor(), 7, sleep=lambda s: None, today_fn=lambda: "Sunday 15 October 2023, 15:34 (device clock)")
        self.assertEqual(pol.begun, ("Sunday 15 October 2023, 15:34 (device clock)", 7))

    def test_a_failing_date_lookup_does_not_end_the_task(self):
        def boom():
            raise OSError("adb gone")

        pol = self.Aware({"action_type": "status", "goal_status": "complete"})
        res = rp.run_task(FakeEnv(), "T", make_task(1.0), pol, FakeExecutor(), 3, sleep=lambda s: None, today_fn=boom)
        self.assertEqual((pol.begun, res.ended), ((None, 3), "status"))

    def test_a_policy_without_begin_task_is_untouched(self):
        res = rp.run_task(FakeEnv(), "T", make_task(1.0), Script({"action_type": "status", "goal_status": "complete"}), FakeExecutor(), 3, sleep=lambda s: None, today_fn=lambda: "x")
        self.assertEqual(res.ended, "status")

    def test_the_budget_is_the_final_one_when_the_caller_asked_for_the_tasks_own(self):
        pol = self.Aware({"action_type": "status", "goal_status": "complete"})
        t = make_task(1.0)
        t.complexity = 1.4
        rp.run_task(FakeEnv(), "T", t, pol, FakeExecutor(), 0, sleep=lambda s: None)
        self.assertEqual(pol.begun[1], 14)

    def test_device_today_reads_the_adb_clock(self):
        out = SimpleNamespace(stdout="2023-10-15_15:34 ", returncode=0)
        self.assertEqual(rp.device_today("emulator-5554", run=lambda *a, **k: out), "Sunday 15 October 2023, 15:34 (device clock)")
        self.assertIsNone(rp.device_today("emulator-5554", run=lambda *a, **k: SimpleNamespace(stdout="error: no devices", returncode=1)))


class Blind(unittest.TestCase):
    """An env whose accessibility tree is empty (the forwarder was lost). refresh_env, when it exists, may bring it back."""

    class Env(FakeEnv):
        def __init__(self, heals):
            super().__init__()
            self.heals, self.refreshes, self.blind = heals, 0, True

        def refresh_env(self):
            self.refreshes += 1
            if self.heals:
                self.blind = False

        def get_state(self):
            return SimpleNamespace(ui_elements=[] if self.blind else self.elements)

    def test_a_reconnect_that_heals_the_tree_lets_the_task_continue(self):
        env = self.Env(heals=True)
        pol = Script({"action_type": "status", "goal_status": "complete"})
        res = rp.run_task(env, "T", make_task(1.0), pol, FakeExecutor(), 5, sleep=lambda s: None)
        self.assertEqual((env.refreshes, res.ended, res.success), (1, "status", True))
        self.assertIn("Save", pol.seen[0][1])

    def test_a_tree_that_stays_empty_ends_the_task_as_no_a11y_and_still_scores_it(self):
        env = self.Env(heals=False)
        pol = Script(*[{"action_type": "wait"}] * 10)
        res = rp.run_task(env, "T", make_task(0.0), pol, FakeExecutor(), 10, sleep=lambda s: None)
        self.assertEqual(res.ended, "no_a11y")
        self.assertEqual(res.steps, rp.BLIND_LIMIT - 1)  # the policy was asked on the first steps only
        self.assertIs(res.success, False)
        self.assertIn("no UI elements", res.error)

    def test_empty_reads_are_waited_out_not_taken_as_a_settled_screen(self):
        # the 11 Nov bug: after the screen guard's uiautomator dump the forwarder is empty for ~3.5 s, and two empty
        # reads used to "agree", so the policy was shown nothing
        env, clock = FakeEnv(), [0.0]
        env.get_state = lambda: SimpleNamespace(ui_elements=[] if clock[0] < 4.0 else env.elements)
        els = rp.settled_elements(env, lambda s: clock.__setitem__(0, clock[0] + s))
        self.assertTrue(els)

    def test_an_empty_read_between_two_full_ones_is_skipped(self):
        env, reads = FakeEnv(), [0]

        def gs():
            reads[0] += 1
            return SimpleNamespace(ui_elements=[] if reads[0] == 2 else env.elements)

        env.get_state = gs
        self.assertEqual(rp.settled_elements(env, lambda s: None), env.elements)

    def test_the_reconnect_on_the_controller_is_found(self):
        # AndroidWorld's AsyncAndroidEnv has no refresh_env; its controller does
        env = FakeEnv()
        env.elements, calls = [], []
        env.controller = SimpleNamespace(refresh_env=lambda: calls.append(1))
        rp.read_elements(env, lambda s: None)
        self.assertEqual(calls, [1])

    def test_an_env_without_refresh_env_is_not_broken_by_an_empty_read(self):
        env = FakeEnv()
        env.elements = []
        self.assertEqual(rp.read_elements(env, lambda s: None), [])


class Batch(unittest.TestCase):
    def test_the_real_subset_is_twenty_distinct_tasks(self):
        names = rp.load_subset(HERE / "subset-20.json")
        self.assertEqual((len(names), len(set(names))), (20, 20))

    def test_the_full_set_is_116_distinct_tasks_and_contains_the_pilot(self):
        full = rp.load_subset(HERE / "subset-all.json")
        self.assertEqual((len(full), len(set(full))), (116, 116))
        self.assertLessEqual(set(rp.load_subset(HERE / "subset-20.json")), set(full))

    def test_a_duplicate_in_a_subset_is_refused(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "s.json"
            p.write_text(json.dumps({"tasks": ["A", "A"]}))
            with self.assertRaises(ValueError):
                rp.load_subset(p)

    def test_summary_counts_unscored_apart_from_failed(self):
        rs = [rp.TaskResult("a", success=True, steps=2), rp.TaskResult("b", success=False, steps=3), rp.TaskResult("c", success=None, ended="setup_error")]
        s = rp.summarise(rs)
        self.assertEqual((s["tasks"], s["scored"], s["passed"], s["pass_rate"], s["unscored"], s["steps_total"]), (3, 2, 1, 0.5, ["c"], 5))

    def test_results_are_written_as_json(self):
        with tempfile.TemporaryDirectory() as d:
            p = rp.write_results(Path(d) / "out", [rp.TaskResult("a", success=True)], {"mode": "executor"})
            data = json.loads(p.read_text(encoding="utf-8"))
            self.assertEqual((data["meta"]["mode"], data["summary"]["passed"], data["results"][0]["task"]), ("executor", 1, "a"))

    def test_dry_run_lists_and_needs_nothing(self):
        buf = io.StringIO()
        with redirect_stdout(buf):
            code = rp.main(["--out", "unused", "--dry-run", "--tasks", "SystemWifiTurnOn"])
        self.assertEqual(code, 0)
        self.assertIn("would run 1 tasks", buf.getvalue())
        self.assertIn("SystemWifiTurnOn", buf.getvalue())

    def test_zero_max_steps_means_androidworlds_complexity_budget(self):
        t = make_task(0.0)
        t.complexity = 1.4
        r = run(t, Script(), max_steps=0)
        self.assertEqual((r.ended, r.steps), ("max_steps", 14))

    def test_a_task_outside_the_subset_is_refused(self):
        self.assertEqual(rp.main(["--out", "unused", "--dry-run", "--tasks", "NoSuchTask"]), 2)


class Backend(unittest.TestCase):
    """The --backend option: the default is unchanged; omniroute builds the free-router call; the log says which model answered."""

    def parse(self, *argv):
        import argparse

        captured = {}
        orig = rp.real_run
        rp.real_run = lambda args, names: captured.setdefault("args", args) and 0
        try:
            rp.main(["--out", "unused", *argv])
        finally:
            rp.real_run = orig
        return captured["args"]

    def test_the_default_backend_is_anthropic_with_the_old_arguments(self):
        a = self.parse()
        self.assertEqual((a.backend, a.model, a.pool, a.model_max_tokens), ("anthropic", "sonnet", None, None))

    def test_omniroute_builds_a_fixed_model_call_and_a_comma_list_is_the_fallback_order(self):
        import policy_claude as pc

        a = self.parse("--backend", "omniroute", "--model", "gemini/gemini-3-flash-preview, groq/openai/gpt-oss-120b", "--max-tokens", "512")
        call = rp.make_call(a)
        self.assertIsInstance(call, pc.OmniRouteCall)
        self.assertEqual((call.models, call.max_tokens), (["gemini/gemini-3-flash-preview", "groq/openai/gpt-oss-120b"], 512))
        self.assertEqual(rp.make_call(self.parse("--backend", "omniroute", "--model", "gemini/gemini-3.5-flash-lite")).max_tokens, 1024)

    def test_the_anthropic_backend_still_goes_through_dispatch_call(self):
        seen = {}
        fake = SimpleNamespace(dispatch_call=lambda **kw: seen.update(kw) or "CALL", omniroute_call=lambda *a, **k: self.fail("not the free router"))
        self.assertEqual(rp.make_call(self.parse("--pool", "own", "--model", "opus"), fake), "CALL")
        self.assertEqual((seen["model"], seen["pool"]), ("opus", "own"))

    def test_the_auto_combos_are_refused_for_a_run(self):
        with self.assertRaises(ValueError):
            rp.make_call(self.parse("--backend", "omniroute", "--model", "auto/best-fast"))

    def test_each_step_records_the_model_that_answered_it(self):
        class Called:
            def __init__(self):
                self.last_model = None

        class Pol(Script):
            def __init__(self, *a):
                super().__init__(*a)
                self.call = Called()
                self.models = ["m1", "m2"]

            def decide(self, goal, observation, history):
                self.call.last_model = self.models.pop(0)
                return super().decide(goal, observation, history)

        r = run(make_task(1.0), Pol({"action_type": "click", "index": 0}, {"action_type": "status", "goal_status": "complete"}))
        self.assertEqual([s["model"] for s in r.log], ["m1", "m2"])

    def test_run_full_passes_the_backend_on_and_keeps_the_original_command_line_otherwise(self):
        import argparse

        import run_full as rf

        base = dict(subset="s.json", pool="own", model="sonnet", policy="v2", backend="anthropic", max_tokens=None)
        old = rf.pilot_argv(argparse.Namespace(**base), Path("out/chunk-01"), ["A", "B"])
        self.assertNotIn("--backend", old)
        self.assertEqual(old[old.index("--model") + 1], "sonnet")
        dflt = rf.pilot_argv(argparse.Namespace(**{**base, "backend": "omniroute", "model": "m/x"}), Path("out/chunk-01"), ["A"])
        self.assertNotIn("--max-tokens", dflt)
        free = rf.pilot_argv(argparse.Namespace(**{**base, "backend": "omniroute", "model": "gemini/gemini-3-flash-preview", "max_tokens": 600}), Path("out/chunk-01"), ["A", "B"])
        self.assertEqual((free[free.index("--backend") + 1], free[free.index("--model") + 1], free[free.index("--max-tokens") + 1], free[free.index("--tasks") + 1]), ("omniroute", "gemini/gemini-3-flash-preview", "600", "A,B"))

    def test_a_policy_without_a_model_recorder_adds_no_model_key(self):
        r = run(make_task(1.0), Script({"action_type": "status", "goal_status": "complete"}))
        self.assertNotIn("model", r.log[0])


    def test_bench_approvals_are_counted_per_task_from_the_audit_chain(self):
        with tempfile.TemporaryDirectory() as d:
            st = Path(d)
            # the shape bridged writes (seen in the 11 Nov full run): asked, then allowed with rule approved:bench
            lines = [{"seq": 1, "decision": "asked", "rule": "screen:destructive"}, {"seq": 2, "decision": "allowed", "rule": "approved:bench"},
                     {"seq": 3, "decision": "allowed", "rule": "approved:bench"}]
            (st / "audit.jsonl").write_text("\n".join(json.dumps(e) for e in lines) + "\nnot json\n", encoding="utf-8")
            self.assertEqual(rp.count_bench_approvals(st, 0), (2, 3))
            self.assertEqual(rp.count_bench_approvals(st, 2), (1, 3))
            with open(st / "audit.jsonl", "a", encoding="utf-8") as f:
                f.write(json.dumps({"seq": 4, "decision": "allowed", "rule": "approved:ntfy"}) + "\n")
            self.assertEqual(rp.count_bench_approvals(st, 3), (0, 4))

    def test_guarded_score_is_passes_with_no_approval(self):
        rs = [rp.TaskResult("a", success=True, approvals=0), rp.TaskResult("b", success=True, approvals=2), rp.TaskResult("c", success=False, approvals=1)]
        s = rp.summarise(rs)
        self.assertEqual((s["passed"], s["passed_guarded"], s["approvals_total"], s["tasks_with_approvals"]), (2, 1, 3, 2))

    def test_a_stopped_chunk_resumes_from_its_results(self):
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "chunk-02"
            self.assertEqual(rp.load_prior(out), {})
            rp.write_results(out, [rp.TaskResult("a", success=True, steps=3, approvals=1), rp.TaskResult("b", success=False)], {"mode": "executor"})
            prior = rp.load_prior(out)
            self.assertEqual(sorted(prior), ["a", "b"])
            self.assertEqual((prior["a"].success, prior["a"].steps, prior["a"].approvals), (True, 3, 1))

    def test_a_usage_limit_is_told_apart_from_an_agent_failure(self):
        for e in ["Claude AI usage limit reached|1731340800", "You've hit your limit · resets 3am", "API Error: 429 rate_limit_error", "overloaded_error"]:
            self.assertTrue(rp.is_limit_error(e), e)
        for e in [None, "", "no result within 300 s", "the element index 7 is not on the screen", "action_error: tap refused"]:
            self.assertFalse(rp.is_limit_error(e), e)

    def test_the_usd_cap_defaults_off_and_reads_the_environment(self):
        old = os.environ.pop("AGENTROID_USD_CAP", None)
        try:
            with mock.patch.object(rp, "real_run", return_value=0) as live:
                rp.main(["--out", "x", "--subset", str(Path(rp.__file__).parent / "subset-all.json"), "--tasks", "AudioRecorderRecordAudio"])
            self.assertEqual(live.call_args[0][0].usd_cap, 0)
            os.environ["AGENTROID_USD_CAP"] = "20"
            with mock.patch.object(rp, "real_run", return_value=0) as live:
                rp.main(["--out", "x", "--subset", str(Path(rp.__file__).parent / "subset-all.json"), "--tasks", "AudioRecorderRecordAudio"])
            self.assertEqual(live.call_args[0][0].usd_cap, 20.0)
        finally:
            os.environ.pop("AGENTROID_USD_CAP", None)
            if old is not None:
                os.environ["AGENTROID_USD_CAP"] = old

    def test_a_run_without_the_bench_adds_no_guarded_keys(self):
        self.assertNotIn("passed_guarded", rp.summarise([rp.TaskResult("a", success=True)]))

    def test_run_full_passes_the_flag_and_merges_both_scores(self):
        import run_full
        with tempfile.TemporaryDirectory() as d:
            out = Path(d)
            for i, rs in enumerate([[("a", True, 0), ("b", True, 1)], [("c", False, 0)]], 1):
                c = out / f"chunk-{i:02d}"
                c.mkdir()
                res = [{"task": t, "success": ok, "steps": 1, "approvals": n} for t, ok, n in rs]
                (c / "results.json").write_text(json.dumps({"results": res}), encoding="utf-8")
            s = run_full.merge(out)
            self.assertEqual((s["passed"], s["passed_guarded"], s["approvals_total"]), (2, 1, 1))
            args = SimpleNamespace(subset="s.json", pool="", model="opus", policy="v2", backend="anthropic", max_tokens=0, bench_approve=True)
            self.assertEqual(run_full.pilot_argv(args, out, ["a"])[-1], "--bench-approve")
            args.bench_approve = False
            self.assertNotIn("--bench-approve", run_full.pilot_argv(args, out, ["a"]))


if __name__ == "__main__":
    import sys
    unittest.main(argv=sys.argv[:1], verbosity=2)
