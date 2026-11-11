"""A Policy (run_pilot.Policy) that asks a Claude model for one AndroidWorld JSONAction per step.

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
{"action_type": "input_text", "text": "...", "index": N}   taps field N, clears it, types text, presses Enter
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

Rules: use only indexes shown in the current list. Indexes change on every screen: always read the CURRENT list. After
each action, look at whether the screen changed as you intended (a switch shows "checked" when on); if it did not,
do something different rather than repeating it. Check your last action worked before moving on. Grant any
permission pop-up the task needs. When the goal is reached send status complete; do not keep acting. If the task
cannot be done, send status infeasible. Never output prose."""


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


def build_prompt(goal: str, observation: str, history: Sequence[dict[str, Any]], keep: int = 8) -> str:
    lines = [f"TASK: {goal}", "", "Your last actions (oldest first):"]
    for h in list(history)[-keep:]:
        res = f" -> ERROR {h['error']}" if h.get("error") else ""
        on = f"  (on {h['target']!r})" if h.get("target") else ""
        lines.append(f"- {json.dumps(h['action'])}{on}{res}")
    if not history:
        lines.append("- (none yet)")
    lines += ["", "SCREEN (index, class, text/description, flags):", observation or "(no elements)"]
    return "\n".join(lines)


class ClaudePolicy:
    def __init__(self, call: Callable[[str, str], str], retries: int = 1):
        self.call = call
        self.retries = retries
        self.parse_failures = 0
        self.calls = 0

    def decide(self, goal: str, observation: str, history: Sequence[dict[str, Any]]) -> dict[str, Any]:
        prompt = build_prompt(goal, observation, history)
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
    A failed or timed-out call is retried after RETRY_WAITS_S; the last failure is raised.

    D1 receipts: every answered call appends to `call.log` one record {t, wall_s, usage, prompt_chars, system_chars,
    reply_chars, attempts}, `usage` being the `(N in / M out)` the dispatcher prints (receipts.parse_dispatch_stdout;
    no cache fields, the request sends no cache_control) or None when the line is missing. run_pilot.run_task moves each
    step's records into its log entry."""
    from receipts import parse_dispatch_stdout  # local: receipts has no dependency back on this module

    log: list[dict[str, Any]] = []

    def call(system: str, prompt: str) -> str:
        d = Path(tempfile.mkdtemp(prefix="aw-policy-"))
        sysf, promptf, outf = d / "system.txt", d / "prompt.txt", d / "out.txt"
        sysf.write_text(system, encoding="utf-8")
        promptf.write_text(prompt, encoding="utf-8")
        cmd = ["node", _dispatch_arg(run), "api", "--now", "--model", model, "--max-tokens", str(max_tokens), "--system-file", str(sysf), "--prompt-file", str(promptf), "--out", str(outf), "--tag", tag, "--for", why]
        if pool:
            cmd += ["--pool", pool]
        failure = ""
        attempts = 0
        for wait in (*RETRY_WAITS_S, None):
            attempts += 1
            t0 = time.monotonic()
            try:
                r = run(cmd, capture_output=True, text=True, creationflags=0x08000000 if os.name == "nt" else 0, timeout=180)
                if r.returncode == 0 and outf.exists():
                    reply = outf.read_text(encoding="utf-8")
                    log.append({"t": time.time(), "wall_s": round(time.monotonic() - t0, 3), "usage": parse_dispatch_stdout(r.stdout or ""),
                                "prompt_chars": len(prompt), "system_chars": len(system), "reply_chars": len(reply), "attempts": attempts})
                    return reply
                failure = f"dispatcher api failed ({r.returncode}): {(r.stderr or r.stdout).strip()[-300:]}"
            except subprocess.TimeoutExpired:
                failure = "dispatcher api timed out after 180 s"
            if wait is not None:
                sleep(wait)
        raise RuntimeError(failure)

    call.log = log  # type: ignore[attr-defined]
    return call


# ---------------------------------------------------------------------------------------------------------------------
# OmniRoute backend (7 Nov 2026): the same `call(system, prompt) -> str` shape, served by the local free-provider router.
# ---------------------------------------------------------------------------------------------------------------------

OMNIROUTE_URL = os.environ.get("OMNIROUTE_URL", "http://localhost:20128")
OMNIROUTE_WAITS_S = (5, 15, 40)  # in-place retries on one model for 429 / 5xx / timeouts before it is benched
OMNIROUTE_PATIENCE_S = 900.0  # most a single call waits when every model is benched, before it raises


class _Retryable(Exception):
    def __init__(self, msg: str, reset_s: float | None = None):
        super().__init__(msg)
        self.reset_s = reset_s


class _Fatal(Exception):
    """A 4xx that retrying cannot fix for this model (unknown model, no longer available): bench it long."""


def _reset_seconds(text: str) -> float | None:
    """When the router says the model is back, in seconds: its JSON `reset_seconds` field (a model_cooldown error:
    {"error": {"code": "model_cooldown", "reset_seconds": 60713}}), else 'reset after 2m' / '90s' / '1h' in the message."""
    try:
        err = json.loads(text).get("error")
        if isinstance(err, dict) and isinstance(err.get("reset_seconds"), (int, float)):
            return float(err["reset_seconds"])
    except (ValueError, AttributeError):
        pass
    m = re.search(r"reset after\s+(\d+(?:\.\d+)?)\s*(ms|s|m|h)", text)
    if not m:
        return None
    return float(m.group(1)) * {"ms": 0.001, "s": 1, "m": 60, "h": 3600}[m.group(2)]


def _usage_of(body: str) -> dict[str, Any] | None:
    """The `usage` block of a chat-completions body (plain JSON, or the last SSE chunk that has one); None if absent."""
    b = body.lstrip()
    found = None
    try:
        if b.startswith("data:"):
            for line in b.splitlines():
                line = line.strip()
                if line.startswith("data:") and not line.endswith("[DONE]"):
                    try:
                        ch = json.loads(line[5:].strip())
                    except ValueError:
                        continue
                    if isinstance(ch, dict) and ch.get("usage"):
                        found = ch["usage"]
            return found
        obj = json.loads(b)
        return obj.get("usage") if isinstance(obj, dict) and isinstance(obj.get("usage"), dict) else None
    except ValueError:
        return None


def _content_of(body: str) -> tuple[str, str | None]:
    """(text, served model) from a chat-completions body: plain JSON, or SSE chunks if the router streamed anyway."""
    b = body.lstrip()
    if b.startswith("data:"):
        parts, served = [], None
        for line in b.splitlines():
            line = line.strip()
            if not line.startswith("data:") or line.endswith("[DONE]"):
                continue
            try:
                ch = json.loads(line[5:].strip())
            except ValueError:
                continue
            served = ch.get("model", served)
            for c in ch.get("choices") or []:
                parts.append(((c.get("delta") or {}).get("content")) or "")
        return "".join(parts), served
    obj = json.loads(b)
    if isinstance(obj, dict) and obj.get("error"):
        err = obj["error"]
        msg = str(err.get("message", err)) if isinstance(err, dict) else str(err)
        raise _Retryable(msg, _reset_seconds(b))
    msg = obj["choices"][0].get("message") or {}
    content = msg.get("content") or ""
    if isinstance(content, list):
        content = "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return content, obj.get("model")


class OmniRouteCall:
    """`call(system, prompt) -> text` through OmniRoute's OpenAI-compatible endpoint (stdlib urllib, no key; in tests
    `opener`, `sleep` and `clock` are injected, so no network).

    `models` is a fixed fallback order: the first model that is not benched answers. A model is benched when a call to it
    exhausts its retries (for the router's own 'reset after N' when it says so, else `bench_s`) or answers with a
    non-retryable 4xx (for an hour); a benched model is not tried again until its time is up, so one run stays on
    one model unless that model is genuinely out. Every answer records which model gave it: `last_model` (the id we
    asked for), `counts`, and the `served` model OmniRoute reports. Never pass the `auto/*` combos: they switch model per call.
    """

    def __init__(self, models: Sequence[str] | str, base_url: str = OMNIROUTE_URL, max_tokens: int = 300, timeout: float = 120.0, temperature: float | None = None,
                 waits: Sequence[float] = OMNIROUTE_WAITS_S, bench_s: float = 120.0, patience_s: float = OMNIROUTE_PATIENCE_S,
                 opener: Callable[..., Any] | None = None, sleep: Callable[[float], None] = time.sleep, clock: Callable[[], float] = time.monotonic):
        import urllib.request

        self.models = [m.strip() for m in ([models] if isinstance(models, str) else models) if m and m.strip()]
        if not self.models:
            raise ValueError("omniroute: no model given")
        if any(m.startswith("auto/") for m in self.models):
            raise ValueError("omniroute: the auto/* combos switch model per call and ruin a benchmark; name a model")
        self.url = base_url.rstrip("/") + "/v1/chat/completions"
        self.max_tokens, self.timeout, self.temperature = max_tokens, timeout, temperature
        self.waits, self.bench_s, self.patience_s = tuple(waits), bench_s, patience_s
        self._open = opener or urllib.request.urlopen
        self._sleep, self._clock = sleep, clock
        self._benched: dict[str, float] = {}
        self.last_model: str | None = None
        self.last_served: str | None = None
        self.counts: dict[str, int] = {}
        self.served: dict[str, int] = {}
        self.retries = 0
        self.errors: list[str] = []
        self._last_usage: dict[str, Any] | None = None
        # D1 receipts: one record per answered call {t, wall_s, model, served, usage (raw chat-completions block or None),
        # prompt_chars, system_chars, reply_chars}; run_pilot.run_task moves each step's records into its log entry.
        self.log: list[dict[str, Any]] = []

    def _once(self, model: str, system: str, prompt: str) -> tuple[str, str | None]:
        import urllib.error
        import urllib.request

        payload: dict[str, Any] = {"model": model, "stream": False, "max_tokens": self.max_tokens,
                                   "messages": [{"role": "system", "content": system}, {"role": "user", "content": prompt}]}
        if self.temperature is not None:
            payload["temperature"] = self.temperature
        req = urllib.request.Request(self.url, data=json.dumps(payload).encode("utf-8"), headers={"Content-Type": "application/json"}, method="POST")
        try:
            with self._open(req, timeout=self.timeout) as resp:
                body = resp.read().decode("utf-8", errors="replace")
        except urllib.error.HTTPError as e:
            text = ""
            try:
                text = e.read().decode("utf-8", errors="replace")[:400]
            except Exception:  # noqa: BLE001
                pass
            msg = f"HTTP {e.code} {text}".strip()
            if e.code in (408, 429) or e.code >= 500:
                raise _Retryable(msg, _reset_seconds(text)) from e
            raise _Fatal(msg) from e
        except (urllib.error.URLError, TimeoutError, OSError) as e:
            raise _Retryable(f"{type(e).__name__}: {e}") from e
        try:
            text, served = _content_of(body)
        except _Retryable:
            raise
        except (ValueError, KeyError, IndexError, TypeError) as e:
            raise _Retryable(f"unreadable reply ({type(e).__name__}): {body[:200]!r}") from e
        if not text.strip():
            raise _Retryable(f"empty reply from {model}: {body[:200]!r}")
        self._last_usage = _usage_of(body)
        return text, served

    def __call__(self, system: str, prompt: str) -> str:
        start = self._clock()
        while True:
            now = self._clock()
            live = [m for m in self.models if self._benched.get(m, 0.0) <= now]
            if not live:
                soonest = min(self._benched[m] for m in self.models)
                if (soonest - start) > self.patience_s:
                    raise RuntimeError("omniroute: every model is out: " + " | ".join(self.errors[-len(self.models):]))
                self._sleep(max(1.0, soonest - now))
                continue
            model = live[0]
            for wait in (*self.waits, None):
                try:
                    text, served = self._once(model, system, prompt)
                except _Fatal as e:
                    self.errors.append(f"{model}: {e}")
                    self._benched[model] = self._clock() + 3600.0
                    break
                except _Retryable as e:
                    self.errors.append(f"{model}: {e}")
                    if e.reset_s and e.reset_s > 0:  # the router said when it is back: honour that rather than hammer it
                        self._benched[model] = self._clock() + min(e.reset_s, 3600.0)
                        break
                    if wait is None:
                        self._benched[model] = self._clock() + self.bench_s
                        break
                    self.retries += 1
                    self._sleep(wait)
                    continue
                self.last_model, self.last_served = model, served
                self.counts[model] = self.counts.get(model, 0) + 1
                if served:
                    self.served[served] = self.served.get(served, 0) + 1
                self.log.append({"t": time.time(), "wall_s": round(self._clock() - start, 3), "model": model, "served": served, "usage": self._last_usage,
                                 "prompt_chars": len(prompt), "system_chars": len(system), "reply_chars": len(text)})
                return text
            if len(self.errors) > 200:
                del self.errors[:100]


def omniroute_call(model: str | Sequence[str], **kw: Any) -> OmniRouteCall:
    """The OmniRoute counterpart of `dispatch_call`: `omniroute_call("gemini/gemini-3-flash-preview")`, or a fallback list."""
    return OmniRouteCall(model, **kw)
