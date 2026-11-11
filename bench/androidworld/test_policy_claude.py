"""policy_claude.py with a scripted model call: no network, no spend.

Run: python -B test_policy_claude.py
"""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

import policy_claude as pc


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


class FakeResp:
    def __init__(self, body):
        self.body = body if isinstance(body, bytes) else body.encode("utf-8")

    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False

    def read(self):
        return self.body


def ok_body(text='{"action_type": "wait"}', model="gemini-3-flash-preview"):
    return json.dumps({"model": model, "choices": [{"message": {"role": "assistant", "content": text}}]})


def http_error(code, text=""):
    import io
    import urllib.error

    return urllib.error.HTTPError("http://x", code, "err", {}, io.BytesIO(text.encode("utf-8")))


class OmniRoute(unittest.TestCase):
    """omniroute_call with a scripted opener, sleep and clock: no network, no spend."""

    class Net:
        """Each outcome per request: a body str, ('http', code, text), or an Exception to raise. Records the payloads."""

        def __init__(self, outcomes):
            self.outcomes, self.sent = list(outcomes), []

        def __call__(self, req, timeout=None):
            self.sent.append((json.loads(req.data.decode("utf-8")), req.full_url, req.headers.get("Content-type"), timeout))
            o = self.outcomes.pop(0)
            if isinstance(o, tuple):
                raise http_error(o[1], o[2] if len(o) > 2 else "")
            if isinstance(o, Exception):
                raise o
            return FakeResp(o)

    @staticmethod
    def clock():
        t = [0.0]

        def now():
            return t[0]

        def sleep(s):
            t[0] += s

        return now, sleep, t

    def make(self, outcomes, models="gemini/gemini-3-flash-preview", **kw):
        now, sleep, t = self.clock()
        net = self.Net(outcomes)
        waits = []
        kw.setdefault("waits", (5, 15, 40))
        call = pc.omniroute_call(models, opener=net, sleep=lambda s: (waits.append(s), sleep(s)), clock=now, **kw)
        return call, net, waits, t

    def test_it_posts_a_non_streaming_chat_completion_and_records_the_model(self):
        call, net, waits, _ = self.make([ok_body()], max_tokens=300)
        self.assertEqual(call("SYS", "PROMPT"), '{"action_type": "wait"}')
        payload, url, ctype, timeout = net.sent[0]
        self.assertEqual(url, pc.OMNIROUTE_URL + "/v1/chat/completions")
        self.assertEqual((payload["model"], payload["stream"], payload["max_tokens"]), ("gemini/gemini-3-flash-preview", False, 300))
        self.assertEqual([m["role"] for m in payload["messages"]], ["system", "user"])
        self.assertEqual((payload["messages"][0]["content"], payload["messages"][1]["content"]), ("SYS", "PROMPT"))
        self.assertNotIn("temperature", payload, "unset by default: the model's own default is used")
        self.assertEqual((call.last_model, call.last_served, call.counts, call.served), ("gemini/gemini-3-flash-preview", "gemini-3-flash-preview", {"gemini/gemini-3-flash-preview": 1}, {"gemini-3-flash-preview": 1}))
        self.assertEqual((ctype, waits), ("application/json", []))

    def test_a_429_is_retried_with_backoff_and_then_succeeds_on_the_same_model(self):
        call, net, waits, _ = self.make([("http", 429, "slow down"), ("http", 503, ""), ok_body()])
        self.assertEqual(call("s", "p"), '{"action_type": "wait"}')
        self.assertEqual((len(net.sent), waits, call.retries), (3, [5, 15], 2))
        self.assertEqual({p["model"] for p, *_ in net.sent}, {"gemini/gemini-3-flash-preview"})

    def test_a_timeout_or_dropped_connection_is_retried(self):
        import urllib.error

        call, net, waits, _ = self.make([TimeoutError("read timed out"), urllib.error.URLError("refused"), ok_body()])
        call("s", "p")
        self.assertEqual(waits, [5, 15])

    def test_an_empty_reply_is_retried_not_returned(self):
        call, net, waits, _ = self.make([ok_body(""), ok_body('{"action_type": "navigate_home"}')])
        self.assertEqual(call("s", "p"), '{"action_type": "navigate_home"}')
        self.assertEqual(waits, [5])

    def test_a_streamed_reply_is_joined(self):
        sse = 'data: {"model":"m","choices":[{"delta":{"content":"{\\"action_type\\":"}}]}\n\ndata: {"model":"m","choices":[{"delta":{"content":" \\"wait\\"}"}}]}\n\ndata: [DONE]\n\n'
        call, *_ = self.make([sse])
        self.assertEqual(call("s", "p"), '{"action_type": "wait"}')
        self.assertEqual(call.last_served, "m")

    def test_the_routers_own_reset_time_benches_the_model_and_the_next_one_answers(self):
        call, net, waits, _ = self.make([("http", 429, '{"error":{"message":"quota (reset after 2m)"}}'), ok_body(model="gpt-oss")], models="gemini/gemini-3-flash-preview,groq/openai/gpt-oss-120b".split(","))
        call("s", "p")
        self.assertEqual([p["model"] for p, *_ in net.sent], ["gemini/gemini-3-flash-preview", "groq/openai/gpt-oss-120b"])
        self.assertEqual((waits, call.last_model), ([], "groq/openai/gpt-oss-120b"), "no in-place retries when the router said when it is back")
        self.assertEqual(call.counts, {"groq/openai/gpt-oss-120b": 1})

    def test_a_benched_model_is_not_tried_again_until_its_time_is_up_and_then_comes_back_first(self):
        call, net, waits, t = self.make([("http", 429, "reset after 2m"), ok_body(), ok_body(), ok_body()], models=["a/one", "b/two"])
        call("s", "p")
        call("s", "p")
        self.assertEqual([p["model"] for p, *_ in net.sent], ["a/one", "b/two", "b/two"], "still benched: straight to the fallback")
        t[0] += 121
        call("s", "p")
        self.assertEqual(net.sent[-1][0]["model"], "a/one", "the preferred model is back after its reset")

    def test_exhausted_retries_bench_the_model_for_bench_s(self):
        call, net, waits, t = self.make([("http", 500, "boom")] * 4 + [ok_body()], models=["a/one", "b/two"], bench_s=60)
        call("s", "p")
        self.assertEqual((waits, call.last_model), ([5, 15, 40], "b/two"))
        self.assertEqual(len(net.sent), 5)

    def test_a_model_that_is_gone_is_benched_at_once_and_for_long(self):
        call, net, waits, t = self.make([("http", 404, "no longer available"), ok_body(), ok_body()], models=["a/gone", "b/two"])
        call("s", "p")
        t[0] += 1800
        call("s", "p")
        self.assertEqual(([p["model"] for p, *_ in net.sent], waits), (["a/gone", "b/two", "b/two"], []))

    def test_with_one_model_a_short_outage_is_waited_out_and_a_long_one_raises(self):
        call, net, waits, t = self.make([("http", 429, "reset after 2m"), ok_body()], patience_s=300)
        self.assertEqual(call("s", "p"), '{"action_type": "wait"}')
        self.assertEqual(waits, [120.0], "slept until the reset, then answered")
        call2, net2, waits2, _ = self.make([("http", 429, "reset after 1h")], patience_s=300)
        with self.assertRaises(RuntimeError) as cm:
            call2("s", "p")
        self.assertIn("every model is out", str(cm.exception))
        self.assertIn("reset after 1h", str(cm.exception))

    def test_the_auto_combos_are_refused_because_they_switch_model_per_call(self):
        with self.assertRaises(ValueError):
            pc.omniroute_call("auto/best-fast")
        with self.assertRaises(ValueError):
            pc.omniroute_call(["gemini/gemini-3-flash-preview", "auto/best-coding"])
        with self.assertRaises(ValueError):
            pc.omniroute_call("")

    def test_it_plugs_into_the_policy_as_the_call(self):
        call, *_ = self.make([ok_body('```json\n{"action_type": "click", "index": 2}\n```')])
        self.assertEqual(pc.ClaudePolicy(call).decide("g", "[0] x", []), {"action_type": "click", "index": 2})

    def test_reset_seconds_parsing(self):
        self.assertEqual(pc._reset_seconds("blah (reset after 2m)"), 120.0)
        self.assertEqual(pc._reset_seconds("reset after 45s"), 45.0)
        self.assertEqual(pc._reset_seconds("reset after 1h"), 3600.0)
        self.assertIsNone(pc._reset_seconds("no hint"))
        self.assertEqual(pc._reset_seconds('{"error":{"code":"model_cooldown","reset_seconds":60713,"message":"cooling down"}}'), 60713.0)

    def test_a_model_cooldown_error_benches_the_model_for_the_stated_seconds(self):
        cool = '{"error":{"message":"All credentials for model m are cooling down","code":"model_cooldown","reset_seconds":600}}'
        call, net, waits, t = self.make([("http", 429, cool), ok_body(), ok_body()], models=["a/one", "b/two"])
        call("s", "p")
        self.assertEqual((waits, call.last_model), ([], "b/two"))
        t[0] += 601
        call("s", "p")
        self.assertEqual(net.sent[-1][0]["model"], "a/one")


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1], verbosity=2)
