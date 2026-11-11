"""A small MCP stdio client for Agentroid's server (apps/mcp/src/sdk-wiring.ts), and the executor
that runs action_map plans through it.

Standard library only. The child gets CREATE_NO_WINDOW on Windows and its stderr goes to a file,
so nothing can appear on the user's screen.
"""

from __future__ import annotations

import json
import os
import queue
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Sequence

import action_map

CREATE_NO_WINDOW = 0x08000000
PROTOCOL_VERSION = "2025-06-18"
HERE = Path(__file__).resolve().parent
MCP_DIR = HERE.parent.parent / "apps" / "mcp"


class McpError(RuntimeError):
    pass


@dataclass
class ToolResult:
    name: str
    text: str
    is_error: bool
    structured: Any = None
    seconds: float = 0.0


class McpClient:
    """One MCP session over a child process's stdin/stdout (newline-delimited JSON-RPC)."""

    def __init__(self, argv: Sequence[str], env: dict[str, str] | None = None, cwd: str | os.PathLike | None = None, stderr_path: str | os.PathLike | None = None, timeout_s: float = 120.0):
        self._argv = list(argv)
        self._env = {**os.environ, **(env or {})}
        self._cwd = str(cwd) if cwd else None
        self._stderr_path = stderr_path
        self._timeout = timeout_s
        self._proc: subprocess.Popen | None = None
        self._lines: "queue.Queue[str | None]" = queue.Queue()
        self._next_id = 1
        self._stderr_f = None

    # ---- lifecycle ----
    def start(self) -> dict[str, Any]:
        kwargs: dict[str, Any] = {}
        if os.name == "nt":
            kwargs["creationflags"] = CREATE_NO_WINDOW
        self._stderr_f = open(self._stderr_path, "ab") if self._stderr_path else subprocess.DEVNULL
        self._proc = subprocess.Popen(
            self._argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self._stderr_f, env=self._env, cwd=self._cwd, **kwargs
        )
        threading.Thread(target=self._pump, daemon=True).start()
        init = self._request(
            "initialize",
            {"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "agentroid-androidworld", "version": "0.1.0"}},
        )
        self._notify("notifications/initialized")
        return init

    def close(self) -> None:
        p, self._proc = self._proc, None
        if p is not None:
            try:
                p.stdin.close()  # the server shuts down when stdin ends
            except OSError:
                pass
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()  # our own recorded child, never a by-name kill
                p.wait()
        if self._stderr_f not in (None, subprocess.DEVNULL):
            self._stderr_f.close()

    def __enter__(self) -> "McpClient":
        self.start()
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    @property
    def pid(self) -> int | None:
        return self._proc.pid if self._proc else None

    # ---- protocol ----
    def _pump(self) -> None:
        assert self._proc and self._proc.stdout
        for raw in self._proc.stdout:
            self._lines.put(raw.decode("utf-8", "replace"))
        self._lines.put(None)

    def _send(self, msg: dict[str, Any]) -> None:
        if not self._proc or not self._proc.stdin:
            raise McpError("the MCP server is not running")
        self._proc.stdin.write((json.dumps(msg, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8"))
        self._proc.stdin.flush()

    def _notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        self._send({"jsonrpc": "2.0", "method": method, **({"params": params} if params is not None else {})})

    def _request(self, method: str, params: dict[str, Any] | None = None, timeout_s: float | None = None) -> dict[str, Any]:
        rid = self._next_id
        self._next_id += 1
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, **({"params": params} if params is not None else {})})
        deadline = time.monotonic() + (timeout_s or self._timeout)
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise McpError(f"{method}: no answer in {timeout_s or self._timeout:.0f} s")
            try:
                line = self._lines.get(timeout=left)
            except queue.Empty:
                continue
            if line is None:
                raise McpError(f"{method}: the MCP server exited")
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue  # not protocol traffic
            if msg.get("id") != rid:
                continue  # a notification or a server request: nothing here uses them
            if "error" in msg:
                raise McpError(f"{method}: {msg['error']}")
            return msg.get("result", {})

    # ---- tools ----
    def list_tools(self) -> list[dict[str, Any]]:
        return self._request("tools/list").get("tools", [])

    def call(self, name: str, arguments: dict[str, Any] | None = None, timeout_s: float | None = None) -> ToolResult:
        t0 = time.monotonic()
        res = self._request("tools/call", {"name": name, "arguments": arguments or {}}, timeout_s)
        text = "\n".join(c.get("text", "") for c in res.get("content", []) if c.get("type") == "text")
        return ToolResult(name, text, bool(res.get("isError")), res.get("structuredContent"), time.monotonic() - t0)


def default_server_argv() -> list[str]:
    """How to start the real server: node on the TypeScript source (the app's own `npm start`)."""
    return ["node", "--experimental-strip-types", "--no-warnings", "src/sdk-wiring.ts"]


def start_agentroid(pipe: str | None = None, session: str = "androidworld", profile: str = "full", stderr_path: str | os.PathLike | None = None) -> McpClient:
    env = {"AGENTROID_SESSION": session, "AGENTROID_PROFILE": profile, "AGENTROID_NO_PUSH": "1"}
    if pipe:
        env["AGENTROID_PIPE"] = pipe
    c = McpClient(default_server_argv(), env=env, cwd=MCP_DIR, stderr_path=stderr_path)
    c.start()
    return c


@dataclass
class StepLog:
    """What one android_world action did: for the run log and the receipt-per-task numbers."""

    action: dict[str, Any]
    results: list[ToolResult]
    notes: list[str]
    answer: str | None = None
    status: str | None = None
    error: str | None = None


class Executor:
    """Runs android_world actions through Agentroid tools (the adapter's device half)."""

    def __init__(self, client: McpClient, device: str | None = None, sleep: Callable[[float], None] = time.sleep):
        self.client = client
        self.device = device
        self._sleep = sleep

    def execute(self, action: Any, elements: Sequence[Any] | None = None, screen: tuple[int, int] | None = None) -> StepLog:
        raw = dict(action) if isinstance(action, dict) else {k: getattr(action, k, None) for k in ("action_type", "index", "x", "y", "text", "direction", "goal_status", "app_name", "keycode", "clear_text")}
        try:
            p = action_map.plan(action, elements, screen)
        except action_map.ActionError as e:
            return StepLog(raw, [], [], error=str(e))
        results: list[ToolResult] = []
        for call in p.calls:
            args = dict(call.args)
            if self.device:
                args["device"] = self.device
            r = self.client.call(call.name, args)
            results.append(r)
            if r.is_error:
                return StepLog(raw, results, p.notes, error=f"{call.name}: {r.text.strip().splitlines()[-1] if r.text.strip() else 'error'}")
            if call.sleep_after_s:
                self._sleep(call.sleep_after_s)
        return StepLog(raw, results, p.notes, answer=p.answer, status=p.status)
