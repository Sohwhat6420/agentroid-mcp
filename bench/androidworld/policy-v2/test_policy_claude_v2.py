"""policy_claude_v2.py (the v2 candidate) with a scripted model call: no network, no spend.

Run: python -B test_policy_claude_v2.py
"""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.append(str(Path(__file__).resolve().parent.parent))  # action_map lives in the parent folder
import policy_claude_v2 as pc  # noqa: E402


class Parse(unittest.TestCase):
    def test_plain_fenced_and_surrounded_replies(self):
        want = {"action_type": "click", "index": 3}
        for text in ['{"action_type": "click", "index": 3}', '```json\n{"action_type": "click", "index": 3}\n```', 'Sure.\n{"action_type": "click", "index": 3}\nDone.']:
            self.assertEqual(pc.parse_action(text), want, text)

    def test_braces_inside_a_string_do_not_confuse_it(self):
        self.assertEqual(pc.parse_action('{"action_type": "input_text", "text": "a}b{c", "index": 1}')["text"], "a}b{c")

    def test_replies_without_an_action_are_refused(self):
        for text in ["I would tap the button.", '{"index": 3}', "[1, 2]", ""]:
            with self.assertRaises((ValueError, json.JSONDecodeError)):
                pc.parse_action(text)


class Prompt(unittest.TestCase):
    def test_it_carries_the_goal_the_screen_and_the_recent_history_only(self):
        hist = [{"action": {"action_type": "click", "index": i}, "error": None} for i in range(20)]
        hist[-1]["error"] = "tap: boom"
        p = pc.build_prompt("Turn on Wi-Fi", "[0] Switch 'Wi-Fi'", hist)
        self.assertIn("TASK: Turn on Wi-Fi", p)
        self.assertIn("[0] Switch 'Wi-Fi'", p)
        self.assertIn('"index": 19} -> ERROR tap: boom', p)
        self.assertNotIn('"index": 11}', p, "only the last 8 actions")
        self.assertIn("(none yet)", pc.build_prompt("g", "", []))
        named = pc.build_prompt("g", "", [{"action": {"action_type": "click", "index": 2}, "error": None, "target": "Bluetooth"}])
        self.assertIn("(on 'Bluetooth')", named)


class DateAndBudget(unittest.TestCase):
    def test_the_prompt_carries_the_device_date_and_the_step_count(self):
        p = pc.build_prompt("Which events are next week?", "[0] x", [], today="Sunday 15 October 2023, 15:34 (device clock)", max_steps=10)
        self.assertIn("TODAY: Sunday 15 October 2023, 15:34 (device clock)", p)
        self.assertIn("Step 1 of 10 (10 left).", p)
        self.assertNotIn("ONLY", p)

    def test_without_a_date_or_budget_the_prompt_is_the_old_one(self):
        p = pc.build_prompt("g", "[0] x", [])
        self.assertNotIn("TODAY", p)
        self.assertIn("Step 1.", p)

    def test_in_the_last_three_steps_an_unanswered_question_is_pushed_to_answer(self):
        hist = [{"action": {"action_type": "scroll", "direction": "down"}, "error": None} for _ in range(7)]
        p = pc.build_prompt("How many events?", "[0] x", hist, max_steps=10)
        self.assertIn("Step 8 of 10 (3 left).", p)
        self.assertIn("ONLY 3 STEPS LEFT", p)
        self.assertIn("send the answer action NOW", p)
        last = pc.build_prompt("How many events?", "[0] x", hist + hist[:2], max_steps=10)
        self.assertIn("ONLY 1 STEP LEFT", last)

    def test_no_push_once_it_has_answered_or_while_there_is_time(self):
        hist = [{"action": {"action_type": "scroll", "direction": "down"}, "error": None} for _ in range(8)]
        hist[3] = {"action": {"action_type": "answer", "text": "4"}, "error": None}
        self.assertNotIn("ONLY", pc.build_prompt("g", "[0] x", hist, max_steps=10))
        hist[3]["error"] = "boom"  # an answer that failed does not count
        self.assertIn("ONLY 2 STEPS LEFT", pc.build_prompt("g", "[0] x", hist, max_steps=10))

    def test_begin_task_feeds_the_next_prompt(self):
        seen = []
        pol = pc.ClaudePolicy(lambda system, prompt: seen.append(prompt) or '{"action_type": "wait"}')
        pol.begin_task(today="Monday 16 October 2023, 09:00 (device clock)", max_steps=20)
        pol.decide("g", "[0] x", [])
        self.assertIn("TODAY: Monday 16 October 2023", seen[0])
        self.assertIn("Step 1 of 20 (20 left).", seen[0])

    def test_the_system_prompt_has_the_date_question_and_app_rules(self):
        s = pc.SYSTEM
        for need in ("TODAY line", "answer action", "Connection preferences", "Bluetooth, pairing", "tap ADD", "clear_text"):
            self.assertIn(need, s)


class Policy(unittest.TestCase):
    def test_a_good_reply_is_one_call(self):
        calls = []
        pol = pc.ClaudePolicy(lambda s, p: calls.append((s, p)) or '{"action_type": "navigate_home"}')
        self.assertEqual(pol.decide("g", "obs", []), {"action_type": "navigate_home"})
        self.assertEqual((len(calls), pol.parse_failures), (1, 0))
        self.assertIs(calls[0][0], pc.SYSTEM)

    def test_a_bad_reply_is_retried_once_with_the_reason(self):
        replies = iter(["no idea", '{"action_type": "wait"}'])
        seen = []
        pol = pc.ClaudePolicy(lambda s, p: seen.append(p) or next(replies))
        self.assertEqual(pol.decide("g", "obs", []), {"action_type": "wait"})
        self.assertEqual((pol.calls, pol.parse_failures), (2, 1))
        self.assertIn("previous reply was not usable", seen[1])
        self.assertNotIn("previous reply", seen[0])

    def test_two_bad_replies_fall_back_to_wait_and_are_counted(self):
        pol = pc.ClaudePolicy(lambda s, p: "nonsense")
        self.assertEqual(pol.decide("g", "obs", []), {"action_type": "wait"})
        self.assertEqual((pol.calls, pol.parse_failures), (2, 2))

    def test_a_failed_call_is_not_swallowed(self):
        def boom(s, p):
            raise RuntimeError("budget")

        with self.assertRaises(RuntimeError):
            pc.ClaudePolicy(boom).decide("g", "obs", [])

    def test_the_system_prompt_names_every_action_the_adapter_handles(self):
        import action_map

        for name in ("click", "long_press", "double_tap", "input_text", "keyboard_enter", "navigate_home", "navigate_back", "scroll", "swipe", "open_app", "wait", "answer", "status"):
            self.assertIn(f'"{name}"', pc.SYSTEM)
        self.assertTrue(hasattr(action_map, "plan"))

    def test_the_prompt_says_how_to_drive_a_slider_and_that_matches_the_action_map(self):
        import action_map
        from types import SimpleNamespace as NS

        self.assertIn("slider", pc.SYSTEM)
        els = [NS(text="", bbox_pixels=NS(x_min=0, x_max=0, y_min=0, y_max=0)), NS(text="Brightness", bbox_pixels=NS(x_min=100, x_max=900, y_min=500, y_max=560))]
        plan = action_map.plan({"action_type": "scroll", "direction": "left", "index": 1}, els, (1080, 2400))
        a = plan.calls[0].args
        self.assertGreater(a["x2"], a["x1"], "scroll left on a slider drags its thumb to the right end")


class Dispatch(unittest.TestCase):
    """dispatch_call with a scripted subprocess: no node, no network."""

    @staticmethod
    def runner(outcomes):
        """Each outcome is 'ok', ('fail', code), or 'timeout'; 'ok' writes the --out file."""
        seen = []

        def run(cmd, **kw):
            seen.append(cmd)
            o = outcomes.pop(0)
            if o == "timeout":
                raise subprocess.TimeoutExpired(cmd, 180)
            if o == "ok":
                Path(cmd[cmd.index("--out") + 1]).write_text('{"action_type": "wait"}', encoding="utf-8")
                return SimpleNamespace(returncode=0, stdout="", stderr="")
            return SimpleNamespace(returncode=o[1], stdout="", stderr="connect timeout")

        return run, seen

    def test_a_transient_failure_is_retried_and_the_call_succeeds(self):
        run, seen = self.runner([("fail", 1), "timeout", "ok"])
        waits = []
        call = pc.dispatch_call(run=run, sleep=waits.append)
        self.assertEqual(call("s", "p"), '{"action_type": "wait"}')
        self.assertEqual((len(seen), waits), (3, [3, 8]))

    def test_it_gives_up_after_the_last_retry_with_the_reason(self):
        run, seen = self.runner([("fail", 1)] * 4)
        waits = []
        with self.assertRaises(RuntimeError) as cm:
            pc.dispatch_call(run=run, sleep=waits.append)("s", "p")
        self.assertIn("connect timeout", str(cm.exception))
        self.assertEqual((len(seen), waits), (4, [3, 8, 20]))

    def test_the_pool_flag_is_passed_when_asked(self):
        run, seen = self.runner(["ok"])
        pc.dispatch_call(pool="own", run=run, sleep=lambda s: None)("s", "p")
        self.assertEqual(seen[0][seen[0].index("--pool") + 1], "own")


class V2Additions(unittest.TestCase):
    """What v2 adds on top of the v1 contract."""

    def test_system_prompt_stays_compact_and_carries_the_new_rules(self):
        self.assertLess(len(pc.SYSTEM) / 3.6, 1400, "about 1.4k tokens at most (v2 was 1k; the 7 Nov date, question and app-hint rules added about 0.2k)")
        for phrase in ("REPLACES", "re-open", "settled", "Pickers", "status complete"):
            self.assertIn(phrase, pc.SYSTEM)

    def test_older_typed_text_survives_the_history_window(self):
        hist = [{"action": {"action_type": "input_text", "text": "Meeting with HR", "index": 4}, "error": None, "target": "Title"}]
        hist += [{"action": {"action_type": "click", "index": i}, "error": None} for i in range(10)]
        p = pc.build_prompt("g", "obs", hist)
        self.assertIn("Text you typed earlier: Title: 'Meeting with HR'", p)
        self.assertIn("Step 12.", p)
        self.assertNotIn("Text you typed earlier", pc.build_prompt("g", "obs", hist[1:6]))

    def test_loop_nudges(self):
        wait = {"action": {"action_type": "wait"}, "error": None}
        click = {"action": {"action_type": "click", "index": 1}, "error": None, "target": "X"}
        self.assertIn("waited 2 times", pc.build_prompt("g", "obs", [click, wait, wait]))
        self.assertIn("last two actions were identical", pc.build_prompt("g", "obs", [click, click]))
        self.assertIn("same action 3 times", pc.build_prompt("g", "obs", [click, wait.copy() | {"action": {"action_type": "back"}}, click, {"action": {"action_type": "scroll", "direction": "down"}, "error": None}, click]))
        self.assertNotIn("NOTE", pc.build_prompt("g", "obs", [click, {"action": {"action_type": "click", "index": 2}, "error": None}]))
        self.assertNotIn("NOTE", pc.build_prompt("g", "obs", []))


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1], verbosity=2)
