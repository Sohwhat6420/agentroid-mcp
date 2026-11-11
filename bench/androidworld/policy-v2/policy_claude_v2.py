"""CANDIDATE v2 of policy_claude.py (swap in by import; see ANALYSIS.md). Only SYSTEM and build_prompt differ.

A Policy (run_pilot.Policy) that asks a Claude model for one AndroidWorld JSONAction per step.

The model call is injected (`call(system, prompt) -> text`), so the loop is testable offline. `dispatch_call`
is the real one: one the dispatcher's `api --now` call per step, which keeps every token on the
machine's API ledger and its per-run cap (never a key of ours; never a global ANTHROPIC_API_KEY).
Executor mode: the model sees AndroidWorld's indexed elements, not Agentroid's refs (see ACTION-SPACE.md).
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any, Callable, Sequence

# The model dispatcher: a node script with an `api` sub-command, named by the AGENTROID_DISPATCH environment variable.
DISPATCH = Path(os.environ["AGENTROID_DISPATCH"]) if os.environ.get("AGENTROID_DISPATCH") else None


def _dispatch_arg(run: Callable[..., Any]) -> str:
    """The dispatcher path; a clear error when AGENTROID_DISPATCH is unset and a real subprocess would run."""
    if DISPATCH is None:
        if run is subprocess.run:
            raise RuntimeError("AGENTROID_DISPATCH is not set: point it at the model-dispatcher node script (it must accept `api --now ...`)")
        return "dispatch.mjs"
    return str(DISPATCH)

SYSTEM = """You operate an Android phone to complete one task. Each turn you get the task, your last actions and the
screen as a numbered list of UI elements. Reply with exactly ONE JSON object and nothing else.

Actions:
{"action_type": "click", "index": N}            tap element N
{"action_type": "long_press", "index": N}
{"action_type": "double_tap", "index": N}
{"action_type": "input_text", "text": "...", "index": N}   taps field N, clears it if it holds text, types text, presses Enter (add "clear_text": false to append instead)
{"action_type": "keyboard_enter"}
{"action_type": "navigate_home"}
{"action_type": "navigate_back"}
{"action_type": "scroll", "direction": "up"|"down"|"left"|"right"}      direction is where the content moves to be revealed (down = see lower items)
{"action_type": "scroll", "direction": "left"|"right", "index": N}   N a slider (an element that is not a list): left drags its thumb to the right end (maximum), right drags it to the left end (minimum)
{"action_type": "swipe", "direction": "up"|"down"|"left"|"right"}
{"action_type": "open_app", "app_name": "Clock"}          the app's display name
{"action_type": "wait"}
{"action_type": "answer", "text": "..."}                  only when the task asks a question; then send status complete
{"action_type": "status", "goal_status": "complete"|"infeasible"}   ends the task

Rules:
1. Use only indexes in the current list; they change every screen. The screen is already settled when you see it: never
wait unless a spinner or "loading" is shown.
2. After each action check the screen changed as you intended (a switch shows "checked" when on). If it did not, or you
are going round in circles (same screens, same buttons), try a different element, scroll or go back; never repeat it.
3. input_text REPLACES the whole field. To add to the top or bottom of existing text, read the field's current text from
the list and send old and new text together, in the required order. One field per action; then check it shows the exact value.
4. A created or edited item (note, file, event, contact, expense, message) is not done when you tap Save: afterwards
re-open it or read the list/detail screen and check the name and every field against the task.
5. Pickers: date: tap the field once, tap the day (arrows change month), OK. Time: set the hour, then the minutes (5s on
the clock face), AM/PM if shown (12 vs 24 hour), OK; then check the field shows the value. Dropdown: open it, tap the option.
6. Icon buttons with no text (ImageButton) are often menu, add (+) or save: try one and note what it did. Open a menu or
drawer at most once.
7. Before status complete, list every value the task needs (names, text, numbers, dates, times, durations, categories,
switches) and find each one on the current screen or in the re-opened saved item. If one is missing or unseen, keep working.
8. Grant any permission pop-up the task needs. If the task cannot be done, send status infeasible. Never output prose.
9. Dates: "today", "tomorrow", "next week", "the Friday after next" are worked out from the TODAY line of the prompt (the device
clock), never from the date an app opens on. Work the target date out once, then read the list for it.
10. Questions: if the TASK asks you something (what, how many, which, when, how long), the reply is an answer action with the
exact value, then status complete. Read the list from the SCREEN list (scroll a few times at most); each prompt shows how many
steps are left. When two or three are left and you have not answered, answer with your best reading now: a guess can pass, an
unanswered question never does.

App hints:
- Bluetooth: the on/off switch is on the Bluetooth screen itself: Settings > Connected devices > Connection preferences >
Bluetooth, then the switch there. The "Bluetooth, pairing" row on the Connected devices screen only opens the pairing page.
Check the switch reads on/off as the task asks before you finish.
- VLC "Add to playlist" dialog: to add the file to a new playlist, type the playlist name in the name field and tap ADD; to use an
existing playlist, tap its name in the list and then tap ADD. CANCEL throws the choice away: if you tapped CANCEL and the playlist
already exists, do not tap CANCEL again; pick the playlist and tap ADD. Add the files in the order the task gives them."""


def parse_action(text: str) -> dict[str, Any]:
    """The first JSON object in a model reply (it may be fenced or have prose around it)."""
    t = text.strip()
    fence = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", t, re.S)
    if fence:
        t = fence.group(1)
    start = t.find("{")
    if start < 0:
        raise ValueError(f"no JSON object in {text[:200]!r}")
    obj, _ = json.JSONDecoder().raw_decode(t[start:])  # braces inside strings are handled
    if not isinstance(obj, dict) or "action_type" not in obj:
        raise ValueError(f"no action_type in {obj!r}")
    return obj


def _typed_earlier(older: Sequence[dict[str, Any]], limit: int = 6) -> str:
    """Text already entered in actions that have scrolled out of the history window."""
    out = []
    for h in older:
        a = h.get("action") or {}
        if a.get("action_type") == "input_text" and not h.get("error"):
            out.append(f"{h.get('target') or 'field'}: {str(a.get('text', ''))[:60]!r}")
    return "; ".join(out[-limit:])


def _stall_note(hist: Sequence[dict[str, Any]]) -> str:
    """A one-line nudge when the recent history shows a loop (empty when it does not)."""
    if len(hist) < 2:
        return ""
    waits = 0
    for h in reversed(hist):
        if (h.get("action") or {}).get("action_type") != "wait":
            break
        waits += 1
    if waits >= 2:
        return f"NOTE: you have waited {waits} times in a row; the screen is already settled. Act now."
    key = lambda h: json.dumps(h.get("action"), sort_keys=True) + "|" + str(h.get("target"))  # noqa: E731
    recent = [key(h) for h in hist[-8:]]
    last = recent[-1]
    if recent[-2] == last:
        return "NOTE: your last two actions were identical. Unless it was meant (a keypad digit), the screen did not help: choose a different action."
    if recent.count(last) >= 3:
        return "NOTE: you have used this same action 3 times in the last 8 steps. If there is no progress, try another route."
    return ""


def _answered(hist: Sequence[dict[str, Any]]) -> bool:
    return any((h.get("action") or {}).get("action_type") == "answer" and not h.get("error") for h in hist)


def _budget_note(hist: Sequence[dict[str, Any]], max_steps: int | None) -> str:
    """How many steps are left, and, in the last three, the push to answer a question (an answer on the final step still counts:
    the task is scored after the loop whether or not a status was sent)."""
    if not max_steps:
        return ""
    left = max_steps - len(hist)
    base = f"Step {len(hist) + 1} of {max_steps} ({left} left)."
    if left <= 3 and not _answered(hist):
        return base + (f" ONLY {left} STEP{'S' if left != 1 else ''} LEFT. If the TASK asks a question, send the answer action NOW with your best"
                       " reading of what you have seen; otherwise finish the task or send status.")
    return base


def build_prompt(goal: str, observation: str, history: Sequence[dict[str, Any]], keep: int = 8, today: str | None = None, max_steps: int | None = None) -> str:
    hist = list(history)
    lines = [f"TASK: {goal}"]
    if today:
        lines.append(f"TODAY: {today}")
    lines += ["", _budget_note(hist, max_steps) or f"Step {len(hist) + 1}.", "Your last actions (oldest first):"]
    for h in hist[-keep:]:
        res = f" -> ERROR {h['error']}" if h.get("error") else ""
        on = f"  (on {h['target']!r})" if h.get("target") else ""
        lines.append(f"- {json.dumps(h['action'])}{on}{res}")
    if not hist:
        lines.append("- (none yet)")
    typed = _typed_earlier(hist[:-keep]) if len(hist) > keep else ""
    if typed:
        lines.append(f"Text you typed earlier: {typed}")
    note = _stall_note(hist)
    if note:
        lines.append(note)
    lines += ["", "SCREEN (index, class, text/description, flags):", observation or "(no elements)", "",
              "Before status complete, check every value the TASK requires against this screen."]
    return "\n".join(lines)


class ClaudePolicy:
    def __init__(self, call: Callable[[str, str], str], retries: int = 1):
        self.call = call
        self.retries = retries
        self.parse_failures = 0
        self.calls = 0
        self.today: str | None = None
        self.max_steps: int | None = None

    def begin_task(self, today: str | None = None, max_steps: int | None = None) -> None:
        """Called by run_pilot.run_task before the first step: the device date line and the task's step budget."""
        self.today, self.max_steps = today, max_steps

    def decide(self, goal: str, observation: str, history: Sequence[dict[str, Any]]) -> dict[str, Any]:
        prompt = build_prompt(goal, observation, history, today=self.today, max_steps=self.max_steps)
        last = ""
        for _ in range(self.retries + 1):
            self.calls += 1
            reply = self.call(SYSTEM, prompt if not last else prompt + f"\n\nYour previous reply was not usable ({last}). Reply with one JSON object only.")
            try:
                return parse_action(reply)
            except (ValueError, json.JSONDecodeError) as e:
                last = str(e)[:120]
                self.parse_failures += 1
        return {"action_type": "wait"}


RETRY_WAITS_S = (3, 8, 20)  # a transient network failure (a connect timeout was seen on 26 Oct) must not end a task


def dispatch_call(model: str = "sonnet", pool: str | None = None, tag: str = "l-agr", why: str = "AndroidWorld pilot step", max_tokens: int = 300, run: Callable[..., Any] = subprocess.run, sleep: Callable[[float], None] = time.sleep) -> Callable[[str, str], str]:
    """One real-time the dispatcher's `api --now` call per step (full price, no batching: the loop is interactive).
    A failed or timed-out call is retried after RETRY_WAITS_S; the last failure is raised."""

    def call(system: str, prompt: str) -> str:
        d = Path(tempfile.mkdtemp(prefix="aw-policy-"))
        sysf, promptf, outf = d / "system.txt", d / "prompt.txt", d / "out.txt"
        sysf.write_text(system, encoding="utf-8")
        promptf.write_text(prompt, encoding="utf-8")
        cmd = ["node", _dispatch_arg(run), "api", "--now", "--model", model, "--max-tokens", str(max_tokens), "--system-file", str(sysf), "--prompt-file", str(promptf), "--out", str(outf), "--tag", tag, "--for", why]
        if pool:
            cmd += ["--pool", pool]
        failure = ""
        for wait in (*RETRY_WAITS_S, None):
            try:
                r = run(cmd, capture_output=True, text=True, creationflags=0x08000000 if os.name == "nt" else 0, timeout=180)
                if r.returncode == 0 and outf.exists():
                    return outf.read_text(encoding="utf-8")
                failure = f"dispatcher api failed ({r.returncode}): {(r.stderr or r.stdout).strip()[-300:]}"
            except subprocess.TimeoutExpired:
                failure = "dispatcher api timed out after 180 s"
            if wait is not None:
                sleep(wait)
        raise RuntimeError(failure)

    return call
