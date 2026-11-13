"""A warm Claude Code session on the user's Claude subscription (`claude -p` in stream-json mode), for AndroidWorld policy v3.

Why a session and not one `claude -p` per step: each new `claude -p` process pays its start-up and an uncached prompt.
One long-lived process per task keeps the conversation and the prompt cache warm; each step is one user turn.

    s = ClaudeSession(model="claude-opus-5-5", system=SYSTEM)
    reply = s.ask("TASK: ...", images=[png_bytes])
    s.close()

- Tools are switched off (`--tools ""`), settings, MCP servers and session files are not loaded, so the model only
  ever answers in text and nothing on disk changes (OWASP LLM06: no agency beyond the reply).
- Never sets or reads an API key: it uses whatever login `claude` already has (the user's own Claude subscription).
- Windows: the child gets CREATE_NO_WINDOW (no console windows on his screen).
- `log` collects one usage record per turn (the `result` event's usage and cost-equivalent), like dispatch_call's log.
"""

from __future__ import annotations

import sys
import base64
import json
import os
import queue
import subprocess
import threading
import time
from typing import Any, Callable, Sequence

CREATE_NO_WINDOW = 0x08000000 if sys.platform == "win32" else 0  # creationflags is Windows-only

# The CI sign-in token. It is for `claude` children only: hold_token() takes it out of this process's environment
# (so bridged, the MCP server, android_world and anything else started from here never inherit it), and
# child_env() hands it back to each `claude` child alone.
TOKEN_VAR = "CLAUDE_CODE_OAUTH_TOKEN"
_held_token: str | None = None


def hold_token() -> bool:
    """Move the sign-in token from os.environ into this module. Idempotent; True when a token is held."""
    global _held_token
    t = os.environ.pop(TOKEN_VAR, None)
    if t:
        _held_token = t
    return _held_token is not None


def child_env() -> dict[str, str] | None:
    """The environment for a `claude` child: this process's, plus the held token. None (inherit) when none is held."""
    if _held_token is None:
        return None
    return {**os.environ, TOKEN_VAR: _held_token}
CLAUDE = os.environ.get("AGENTROID_CLAUDE", "claude")


def user_message(text: str, images: Sequence[bytes] = ()) -> dict[str, Any]:
    """One stream-json user turn: images first (as the vision docs recommend), then the text."""
    content: list[dict[str, Any]] = [
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": base64.b64encode(b).decode("ascii")}}
        for b in images
    ]
    content.append({"type": "text", "text": text})
    return {"type": "user", "message": {"role": "user", "content": content}}


def session_argv(model: str, system: str, effort: str | None = None) -> list[str]:
    argv = [CLAUDE, "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
            "--model", model, "--system-prompt", system, "--tools", "", "--setting-sources", "",
            "--strict-mcp-config", "--no-session-persistence"]
    if effort:
        argv += ["--effort", effort]
    return argv


class SessionError(RuntimeError):
    pass


class ClaudeSession:
    def __init__(self, model: str, system: str, effort: str | None = None, timeout_s: float = 180.0,
                 popen: Callable[..., Any] = subprocess.Popen):
        self.model, self.timeout_s = model, timeout_s
        self.log: list[dict[str, Any]] = []
        self.last_model = model
        self._proc = popen(session_argv(model, system, effort), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                           stderr=subprocess.DEVNULL, text=True, encoding="utf-8", bufsize=1, env=child_env(),
                           creationflags=CREATE_NO_WINDOW if os.name == "nt" else 0)
        self._lines: "queue.Queue[str | None]" = queue.Queue()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self) -> None:
        for line in self._proc.stdout:
            self._lines.put(line)
        self._lines.put(None)

    def ask(self, text: str, images: Sequence[bytes] = ()) -> str:
        """Send one turn and return the assistant's final text for it."""
        if self._proc.poll() is not None:
            raise SessionError(f"claude exited with {self._proc.returncode}")
        self._proc.stdin.write(json.dumps(user_message(text, images)) + "\n")
        self._proc.stdin.flush()
        deadline = time.monotonic() + self.timeout_s
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise SessionError(f"no result within {self.timeout_s:.0f} s")
            try:
                line = self._lines.get(timeout=left)
            except queue.Empty:
                raise SessionError(f"no result within {self.timeout_s:.0f} s") from None
            if line is None:
                raise SessionError("claude closed its output before a result")
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") != "result":
                continue
            self.log.append({"usage": ev.get("usage"), "cost_usd": ev.get("total_cost_usd"), "is_error": ev.get("is_error"),
                             "duration_ms": ev.get("duration_ms"), "model": self.model, "subtype": ev.get("subtype")})
            if ev.get("is_error"):
                raise SessionError(str(ev.get("result") or ev.get("subtype"))[:300])
            return str(ev.get("result") or "")

    def close(self) -> None:
        try:
            if self._proc.poll() is None:
                self._proc.stdin.close()
                self._proc.wait(timeout=10)
        except Exception:  # noqa: BLE001 - closing must never fail a task
            self._proc.kill()


def one_shot(model: str, system: str, effort: str | None = None, timeout_s: float = 180.0) -> Callable[[str, Sequence[bytes]], str]:
    """A fresh session per call (the planner and the verifier: their own context, no executor history)."""
    log: list[dict[str, Any]] = []

    def call(text: str, images: Sequence[bytes] = ()) -> str:
        s = ClaudeSession(model, system, effort, timeout_s)
        try:
            return s.ask(text, images)
        finally:
            log.extend(s.log)
            s.close()

    call.log = log  # type: ignore[attr-defined]
    return call
