"""AndroidWorld policy v3 ("agent mode"): what the top AndroidWorld entries add (planner, verifier, screenshots, notes, re-plan on a stall), each one switchable for the ablation.

Flags (each adds one thing on top of v2's executor prompt; the ladder A1..A5 turns them on in order):
  screens   a screenshot with every executor turn (hybrid perception: the indexed tree stays the action space)
  planner   a separate planner writes numbered subgoals once, and again when the executor stalls
  verifier  a separate verifier must agree before `status complete` reaches the harness (2 refusals at most per task)
  notes     a scratchpad: the executor may reply {"action_type":"note","text":...}; notes are shown on every turn
  stall     three identical actions, or the same screen three times, triggers a re-plan

The executor is one warm session per task (`exec_factory(system) -> obj with ask(text, images) and close()`); the
planner and verifier are one-shot calls `(text, images) -> text` with their own context. All three are injected, so
the whole policy runs offline in tests.
"""

from __future__ import annotations

import hashlib
import importlib.util
import json
import re
from pathlib import Path
from typing import Any, Callable, Sequence

_V2 = Path(__file__).resolve().parents[1] / "policy-v2" / "policy_claude_v2.py"
_spec = importlib.util.spec_from_file_location("policy_claude_v2", _V2)
v2 = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(v2)

ALL_FLAGS = ("screens", "planner", "verifier", "notes", "stall")
LADDER = {"A1": ("screens",), "A2": ("screens", "planner"), "A3": ("screens", "planner", "verifier"),
          "A4": ("screens", "planner", "verifier", "notes"), "A5": ALL_FLAGS}
MAX_NOTES_PER_STEP = 2
MAX_VERIFIER_REFUSALS = 2

NOTES_RULE = """
Scratchpad: {"action_type": "note", "text": "..."} saves a fact you have read (list items, counts, values you must
report later) without touching the phone; notes appear under NOTES on every turn. Use it while reading a list that
needs scrolling, then answer from your notes instead of scrolling back and forth. At most two notes per step."""

SCREENS_RULE = """
You also get a screenshot of the same screen. Use it to understand icons, sliders, pickers and layout, but act only
through indexes from the SCREEN list."""

PLANNER_SYSTEM = """You plan Android phone tasks for an executor that taps elements by index. Given the task (and, when
re-planning, what has happened so far and the current screen), reply with 2-8 numbered subgoals, one line each, concrete
(which app, which screen, which field, which value), ending with how to check the result. If the task asks a question,
the last subgoal is "answer with <what>". No prose before or after the list."""

VERIFIER_SYSTEM = """You check whether an Android phone task is really finished. You get the task, the executor's
notes, its recent actions and the current screen (element list, maybe a screenshot). Check every value the task
requires (names, text, numbers, dates, times, order, switch states) against what is visible. If the task asks a
question, check the answer given matches what was read. Reply with ONE JSON object only:
{"verdict": "done"} or {"verdict": "not_done", "why": "<what is missing or wrong, one sentence>"}."""

QUESTION = re.compile(r"^\s*(what|which|how many|how much|how long|when|who|where|is |are |do |does |did )|\?\s*$", re.I)


def is_question(goal: str) -> bool:
    return bool(QUESTION.search(goal or ""))


def executor_system(flags: Sequence[str]) -> str:
    return v2.SYSTEM + (SCREENS_RULE if "screens" in flags else "") + (NOTES_RULE if "notes" in flags else "")


def _key(h: dict[str, Any]) -> str:
    return json.dumps(h.get("action"), sort_keys=True) + "|" + str(h.get("target"))


class AgentPolicy:
    def __init__(self, exec_factory: Callable[[str], Any], planner: Callable[..., str] | None = None,
                 verifier: Callable[..., str] | None = None, screenshot: Callable[[], bytes | None] | None = None,
                 flags: Sequence[str] = ALL_FLAGS):
        unknown = set(flags) - set(ALL_FLAGS)
        if unknown:
            raise ValueError(f"unknown flags {sorted(unknown)}")
        if "planner" in flags and planner is None or "verifier" in flags and verifier is None:
            raise ValueError("planner/verifier flag set without its call")
        self.flags = tuple(flags)
        self.exec_factory, self.planner, self.verifier, self.screenshot = exec_factory, planner, verifier, screenshot
        self.session: Any = None
        self.calls = self.parse_failures = 0
        self.today: str | None = None
        self.max_steps: int | None = None
        self.goal = ""
        self.plan = ""
        self.notes: list[str] = []
        self.refusals = 0
        self.replans = 0
        self.screens: list[str] = []
        self.events: list[dict[str, Any]] = []  # planner / verifier / note events, for the per-task log
        self.log: list[dict[str, Any]] = []     # usage records (executor + planner + verifier), read by run_task

    # --- life cycle ---------------------------------------------------------------------------------------------
    def begin_task(self, today: str | None = None, max_steps: int | None = None) -> None:
        self.close()
        self.today, self.max_steps = today, max_steps
        self.goal, self.plan, self.notes, self.refusals, self.replans, self.screens, self.events = "", "", [], 0, 0, [], []
        self.session = self.exec_factory(executor_system(self.flags))

    def close(self) -> None:
        if self.session is not None:
            self._harvest(self.session)
            self.session.close()
            self.session = None

    @property
    def call(self) -> "AgentPolicy":  # run_task reads policy.call.log for per-call usage records
        return self

    def _harvest(self, src: Any) -> None:
        got = getattr(src, "log", None)
        if isinstance(got, list):
            mark = getattr(src, "_harvested", 0)
            self.log.extend(got[mark:])
            try:
                src._harvested = len(got)
            except AttributeError:
                pass

    # --- helpers ------------------------------------------------------------------------------------------------
    def _shot(self) -> list[bytes]:
        if "screens" not in self.flags or self.screenshot is None:
            return []
        try:
            png = self.screenshot()
        except Exception:  # noqa: BLE001 - a missing screenshot must not end a step; the tree is still there
            png = None
        return [png] if png else []

    def _ask_planner(self, observation: str, history: Sequence[dict[str, Any]], why: str) -> None:
        recent = "\n".join(f"- {json.dumps(h['action'])}" + (f" (on {h['target']!r})" if h.get("target") else "") for h in history[-10:])
        text = f"TASK: {self.goal}\n" + (f"TODAY: {self.today}\n" if self.today else "") + (
            f"\nRE-PLAN because: {why}\nPrevious plan:\n{self.plan}\nRecent actions:\n{recent}\nNOTES: {'; '.join(self.notes) or '(none)'}\n" if why != "start" else "") + \
            f"\nCURRENT SCREEN:\n{observation or '(no elements)'}"
        self.calls += 1
        try:
            self.plan = self.planner(text, self._shot()).strip()[:1500]
        except Exception as e:  # noqa: BLE001 - no plan is better than no step
            self.events.append({"planner_error": str(e)[:200]})
        self._harvest(self.planner)
        self.events.append({"plan": self.plan, "why": why})

    def _stalled(self, observation: str, history: Sequence[dict[str, Any]]) -> str:
        if "stall" not in self.flags:
            return ""
        if len(history) >= 3 and len({_key(h) for h in history[-3:]}) == 1:
            return "the same action three times in a row"
        if len(self.screens) >= 3 and len(set(self.screens[-3:])) == 1 and history:
            return "the screen has not changed for three steps"
        return ""

    def _prompt(self, observation: str, history: Sequence[dict[str, Any]], extra: str = "") -> str:
        base = v2.build_prompt(self.goal, observation, history, today=self.today, max_steps=self.max_steps)
        parts = []
        if self.plan:
            parts.append("PLAN:\n" + self.plan)
        if "notes" in self.flags:
            parts.append("NOTES: " + ("; ".join(self.notes[-12:]) if self.notes else "(none)"))
        if extra:
            parts.append(extra)
        return (base + "\n\n" + "\n\n".join(parts)) if parts else base

    def _executor_turn(self, prompt: str) -> dict[str, Any]:
        last = ""
        for _ in range(2):
            self.calls += 1
            reply = self.session.ask(prompt if not last else f"Your previous reply was not usable ({last}). Reply with one JSON object only.", self._shot() if not last else [])
            try:
                return v2.parse_action(reply)
            except (ValueError, json.JSONDecodeError) as e:
                last = str(e)[:120]
                self.parse_failures += 1
        return {"action_type": "wait"}

    def _verify(self, action: dict[str, Any], observation: str, history: Sequence[dict[str, Any]]) -> str | None:
        """None when the verifier agrees (or is off / out of refusals); else its reason."""
        if "verifier" not in self.flags or self.refusals >= MAX_VERIFIER_REFUSALS:
            return None
        if action.get("action_type") != "status" or action.get("goal_status") != "complete":
            return None
        answers = [h["action"].get("text") for h in history if (h.get("action") or {}).get("action_type") == "answer"]
        recent = "\n".join(f"- {json.dumps(h['action'])}" for h in history[-8:])
        text = (f"TASK: {self.goal}\n" + (f"TODAY: {self.today}\n" if self.today else "") +
                f"ANSWER GIVEN: {answers[-1] if answers else '(none)'}\nNOTES: {'; '.join(self.notes) or '(none)'}\n"
                f"RECENT ACTIONS:\n{recent or '(none)'}\n\nCURRENT SCREEN:\n{observation or '(no elements)'}")
        self.calls += 1
        try:
            reply = self.verifier(text, self._shot())
            obj = json.loads(reply[reply.find("{"): reply.rfind("}") + 1])
        except Exception as e:  # noqa: BLE001 - a broken verifier must not block a finished task
            self.events.append({"verifier_error": str(e)[:200]})
            self._harvest(self.verifier)
            return None
        self._harvest(self.verifier)
        self.events.append({"verdict": obj})
        if obj.get("verdict") == "not_done":
            self.refusals += 1
            return str(obj.get("why") or "the verifier found something missing")[:300]
        return None

    # --- the step -----------------------------------------------------------------------------------------------
    def decide(self, goal: str, observation: str, history: Sequence[dict[str, Any]]) -> dict[str, Any]:
        if self.session is None:  # run without begin_task (tests, or an old runner)
            self.begin_task()
        if not self.goal:
            self.goal = goal
            if "planner" in self.flags:
                self._ask_planner(observation, history, "start")
        self.screens.append(hashlib.sha1((observation or "").encode()).hexdigest())
        why = self._stalled(observation, history)
        extra = ""
        if why and "planner" in self.flags and self.replans < 3:
            self.replans += 1
            self._ask_planner(observation, history, why)
            extra = f"NOTE: you were stuck ({why}); the PLAN above is new. Follow it from the step that fits this screen."
        elif why:
            extra = f"NOTE: you are stuck ({why}). Choose a different element, scroll, or go back."
        left = (self.max_steps - len(history)) if self.max_steps else None
        if is_question(self.goal) and left is not None and left <= 2 and "notes" in self.flags:
            extra += "\nLAST CHANCE: answer now from your NOTES and the screen."
        action = self._executor_turn(self._prompt(observation, history, extra))
        notes_taken = 0
        while action.get("action_type") == "note" and "notes" in self.flags:
            if notes_taken < MAX_NOTES_PER_STEP:
                self.notes.append(str(action.get("text", ""))[:300])
                self.events.append({"note": self.notes[-1]})
            notes_taken += 1
            if notes_taken > MAX_NOTES_PER_STEP + 1:
                action = {"action_type": "wait"}
                break
            action = self._executor_turn("Noted. Now the next phone action (one JSON object).")
        if action.get("action_type") == "note":  # notes flag off: not a phone action
            action = {"action_type": "wait"}
        refusal = self._verify(action, observation, history)
        if refusal:
            action = self._executor_turn(f"The task is NOT finished: {refusal}\nFix it, then finish. Reply with the next phone action.")
            if action.get("action_type") == "note":
                action = {"action_type": "wait"}
        self._harvest(self.session)
        return action
