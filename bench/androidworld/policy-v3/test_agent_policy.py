"""Offline tests for policy v3 (no emulator, no model): python -I -m pytest policy-v3 -q, or python policy-v3/test_agent_policy.py"""

from __future__ import annotations

import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import agent_policy as ap  # noqa: E402
import claude_session as cs  # noqa: E402


class FakeSession:
    def __init__(self, replies):
        self.replies, self.asked, self.closed, self.log = list(replies), [], False, []

    def ask(self, text, images=()):
        self.asked.append((text, list(images)))
        self.log.append({"usage": {"output_tokens": 5}})
        return self.replies.pop(0)

    def close(self):
        self.closed = True


def factory(replies, made):
    def f(system):
        s = FakeSession(replies)
        s.system = system
        made.append(s)
        return s
    return f


def call(replies, seen):
    def c(text, images=()):
        seen.append((text, list(images)))
        return replies.pop(0)
    c.log = []
    return c


def J(**kw):
    return json.dumps(kw)


def test_flags_off_is_v2_with_a_session():
    made = []
    p = ap.AgentPolicy(factory([J(action_type="click", index=3)], made), flags=())
    p.begin_task(today="Mon", max_steps=10)
    assert p.decide("Turn wifi on", "0 Switch Wi-Fi", []) == {"action_type": "click", "index": 3}
    assert "Scratchpad" not in made[0].system and "screenshot" not in made[0].system
    assert made[0].asked[0][1] == []  # no screenshot without the flag


def test_screens_attach_a_png_and_survive_a_failing_grabber():
    made = []
    p = ap.AgentPolicy(factory([J(action_type="wait"), J(action_type="wait")], made), screenshot=lambda: b"PNG", flags=("screens",))
    p.begin_task()
    p.decide("g", "obs", [])
    assert made[0].asked[0][1] == [b"PNG"]
    p.screenshot = lambda: (_ for _ in ()).throw(RuntimeError("no frame"))
    p.decide("g", "obs2", [{"action": {"action_type": "wait"}}])
    assert made[0].asked[1][1] == []


def test_planner_runs_once_and_its_plan_is_in_every_prompt():
    made, seen = [], []
    planner = call(["1. open Settings\n2. tap Wi-Fi"], seen)
    p = ap.AgentPolicy(factory([J(action_type="click", index=1), J(action_type="click", index=2)], made), planner=planner, flags=("planner",))
    p.begin_task()
    p.decide("Turn wifi on", "a", [])
    p.decide("Turn wifi on", "b", [{"action": {"action_type": "click", "index": 1}}])
    assert len(seen) == 1
    assert all("PLAN:\n1. open Settings" in t for t, _ in made[0].asked)


def test_notes_are_kept_shown_and_do_not_reach_the_phone():
    made = []
    p = ap.AgentPolicy(factory([J(action_type="note", text="Run 3 km"), J(action_type="scroll", direction="down"), J(action_type="answer", text="3 km")], made), flags=("notes",))
    p.begin_task(max_steps=10)
    a = p.decide("What was my longest run?", "list", [])
    assert a == {"action_type": "scroll", "direction": "down"}
    assert p.notes == ["Run 3 km"]
    p.decide("What was my longest run?", "list2", [{"action": a}])
    assert "NOTES: Run 3 km" in made[0].asked[-1][0]


def test_endless_notes_end_in_a_wait():
    made = []
    p = ap.AgentPolicy(factory([J(action_type="note", text=str(i)) for i in range(10)], made), flags=("notes",))
    p.begin_task()
    assert p.decide("g", "o", []) == {"action_type": "wait"}
    assert len(p.notes) == ap.MAX_NOTES_PER_STEP


def test_verifier_refusal_sends_the_reason_back_and_caps_at_two():
    made, seen = [], []
    verifier = call([J(verdict="not_done", why="title missing"), J(verdict="not_done", why="still missing")], seen)
    done = J(action_type="status", goal_status="complete")
    p = ap.AgentPolicy(factory([done, J(action_type="click", index=4), done, done, done], made), verifier=verifier, flags=("verifier",))
    p.begin_task()
    assert p.decide("Save note", "o", []) == {"action_type": "click", "index": 4}
    assert "NOT finished: title missing" in made[0].asked[1][0]
    assert p.decide("Save note", "o", [{"action": {"action_type": "click", "index": 4}}]) == json.loads(done)
    assert p.decide("Save note", "o", [{"action": {"action_type": "click", "index": 4}}]) == json.loads(done)  # 2 refusals used
    assert p.refusals == 2 and len(seen) == 2


def test_a_broken_verifier_does_not_block_completion():
    made = []
    bad = call(["no json here"], [])
    p = ap.AgentPolicy(factory([J(action_type="status", goal_status="complete")], made), verifier=bad, flags=("verifier",))
    p.begin_task()
    assert p.decide("g", "o", [])["action_type"] == "status"


def test_stall_triggers_a_replan_with_the_reason():
    made, seen = [], []
    planner = call(["1. first", "1. other route"], seen)
    same = {"action": {"action_type": "click", "index": 2}, "target": "Add"}
    p = ap.AgentPolicy(factory([J(action_type="click", index=2)] * 3, made), planner=planner, flags=("planner", "stall"))
    p.begin_task()
    p.decide("g", "o", [])
    p.decide("g", "o", [same])
    p.decide("g", "o", [same, same, same])
    assert len(seen) == 2 and "RE-PLAN because: the same action three times" in seen[1][0]
    assert "you were stuck" in made[0].asked[-1][0]


def test_unknown_flag_and_missing_call_are_refused():
    for kw in ({"flags": ("vision",)}, {"flags": ("planner",)}):
        try:
            ap.AgentPolicy(lambda s: FakeSession([]), **kw)
        except ValueError:
            continue
        raise AssertionError(kw)


def test_begin_task_closes_the_old_session_and_usage_is_collected():
    made = []
    p = ap.AgentPolicy(factory([J(action_type="wait")] * 2, made), flags=())
    p.begin_task()
    p.decide("g", "o", [])
    p.begin_task()
    assert made[0].closed and len(made) == 2
    assert len(p.call.log) == 1


def test_is_question():
    assert ap.is_question("What is the total distance of my runs this week?")
    assert ap.is_question("How many tasks are due on Friday?")
    assert not ap.is_question("Create a note named groceries")


def test_stream_json_turn_and_argv():
    m = cs.user_message("hi", [b"\x89PNG"])
    assert m["message"]["content"][0]["source"]["media_type"] == "image/png"
    assert m["message"]["content"][-1] == {"type": "text", "text": "hi"}
    argv = cs.session_argv("claude-opus-5-5", "SYS")
    assert argv[argv.index("--tools") + 1] == "" and "--strict-mcp-config" in argv and "--no-session-persistence" in argv


class FakeProc:
    def __init__(self, lines):
        import io
        self.stdout = io.StringIO("".join(lines))
        self.stdin = io.StringIO()
        self.returncode = None

    def poll(self):
        return None

    def wait(self, timeout=None):
        return 0

    def kill(self):
        pass


def test_session_reads_the_result_event_and_records_usage():
    lines = [json.dumps({"type": "system", "subtype": "init"}) + "\n",
             json.dumps({"type": "assistant", "message": {}}) + "\n",
             json.dumps({"type": "result", "result": '{"action_type":"wait"}', "usage": {"input_tokens": 9}, "total_cost_usd": 0.01, "is_error": False}) + "\n"]
    s = cs.ClaudeSession("m", "sys", popen=lambda *a, **k: FakeProc(lines))
    assert s.ask("x") == '{"action_type":"wait"}'
    assert s.log[0]["usage"] == {"input_tokens": 9}
    try:
        s.ask("again")  # the fake stream has ended
    except cs.SessionError:
        pass
    else:
        raise AssertionError("expected SessionError")


if __name__ == "__main__":
    n = 0
    for name, fn in list(globals().items()):
        if name.startswith("test_") and callable(fn):
            fn()
            n += 1
    print(f"{n} passed")
