"""The AndroidWorld pilot loop (executor mode): subset in, one JSON result per task out.

For each task in a subset file: set the task up in an AndroidWorld environment, let a Policy choose
JSONActions from the screen's indexed elements, perform each through Agentroid (agentroid_mcp.Executor),
then score with the task's own `is_successful` and tear down. The loop (`run_task`) calls no model: the Policy is
injected (a scripted one in tests; `policy_claude.ClaudePolicy` for a real run, one dispatcher call per step).

    python -B run_pilot.py --subset subset-20.json --out results/pilot-1
        [--tasks A,B] [--max-steps 25] [--model sonnet] [--pool POOL] [--dry-run]

--dry-run lists what would run and exits, so it is safe with no emulator and no android_world installed.
The `android_world` import is deferred to `make_env`, so this file and its tests load without it.
A real run needs the emulator up: see U-A2-RESULTS.md.
"""

from __future__ import annotations

import argparse
import os
import json
import re
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Callable, Protocol, Sequence

HERE = Path(__file__).resolve().parent
MAX_STEPS_DEFAULT = 25
OMNIROUTE_MAX_TOKENS = 1024  # the anthropic policy uses 300; the free reasoning models spend part of max_tokens on thinking


class Policy(Protocol):
    def decide(self, goal: str, observation: str, history: Sequence[dict[str, Any]]) -> dict[str, Any]:
        """The next JSONAction as a dict (`{"action_type": "click", "index": 3}`)."""


@dataclass
class TaskResult:
    task: str
    goal: str = ""
    success: bool | None = None  # None = never scored (setup or scoring failed)
    steps: int = 0
    ended: str = ""  # status | max_steps | action_error | setup_error | score_error | no_a11y
    answer: str | None = None
    error: str | None = None
    wall_s: float = 0.0
    log: list[dict[str, Any]] = field(default_factory=list)
    receipt: dict[str, Any] | None = None  # D1 token receipt (receipts.receipt_from_live), set by real_run
    policy_events: list[dict[str, Any]] | None = None  # v3: plans, verdicts and notes, for reading a failure afterwards
    approvals: int | None = None  # --bench-approve: how many of the guard's asks the bench stand-in allowed (None = not a bench run)


def load_subset(path: str | Path) -> list[str]:
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    tasks = data["tasks"]
    if len(set(tasks)) != len(tasks):
        raise ValueError(f"{path}: a task is listed twice")
    return list(tasks)


def render_elements(elements: Sequence[Any]) -> str:
    """The indexed element list the Policy reads: one line each, the index first."""
    lines = []
    for i, e in enumerate(elements):
        label = getattr(e, "text", None) or getattr(e, "content_description", None) or getattr(e, "hint_text", None) or ""
        cls = getattr(e, "class_name", "") or ""
        flags = [n for n in ("is_clickable", "is_editable", "is_scrollable", "is_checked") if getattr(e, n, False)]
        lines.append(f"[{i}] {cls.rsplit('.', 1)[-1]} {label!r} {','.join(f.removeprefix('is_') for f in flags)}".rstrip())
    return "\n".join(lines)


def element_label(elements: Sequence[Any], action: dict[str, Any]) -> str | None:
    """What the action's `index` pointed at on the screen it was chosen from, for the Policy's history."""
    i = action.get("index")
    if not isinstance(i, int) or not 0 <= i < len(elements):
        return None
    e = elements[i]
    return (getattr(e, "text", None) or getattr(e, "content_description", None) or getattr(e, "hint_text", None) or getattr(e, "class_name", None) or None)


SETTLE_FIRST_S = 1.5  # measured on the emulator: a click's new screen reaches the a11y tree 1 to 1.5 s later
SETTLE_POLL_S = 0.7
SETTLE_POLLS = 5
BLIND_LIMIT = 3  # steps in a row with no UI elements at all, even after a reconnect, before the task is ended as "no_a11y"
# Measured 11 Nov: Agentroid's screen guard reads the screen with `uiautomator dump` before a tap, and UiAutomation
# suppresses accessibility services while it runs, so AndroidWorld's forwarder returns an EMPTY tree for up to ~3.5 s
# after the next screen change. An empty read is therefore "not yet", never a settled screen.
EMPTY_WAIT_S = 10.0


def settled_elements(env: Any, sleep: Callable[[float], None]) -> list[Any]:
    """The elements once the screen has stopped changing: wait out the transition, keep reading while the tree is
    empty (up to EMPTY_WAIT_S), then read until two non-empty reads agree (at most SETTLE_POLLS reads, so a screen
    that never stops animating still returns). An empty read after a non-empty one is skipped, not taken."""
    sleep(SETTLE_FIRST_S)
    elements = list(env.get_state().ui_elements)
    waited = 0.0
    while not elements and waited < EMPTY_WAIT_S:
        sleep(SETTLE_POLL_S)
        waited += SETTLE_POLL_S
        elements = list(env.get_state().ui_elements)
    if not elements:
        return elements
    for _ in range(SETTLE_POLLS - 1):
        sleep(SETTLE_POLL_S)
        again = list(env.get_state().ui_elements)
        if not again:
            continue
        if render_elements(again) == render_elements(elements):
            return again
        elements = again
    return elements


def _refresher(env: Any) -> Callable[[], Any] | None:
    """AndroidWorld's reconnect lives on the controller (env.controller.refresh_env), not on the AsyncAndroidEnv: the
    old hasattr(env, "refresh_env") check never found it, so no reconnect ever ran."""
    fn = getattr(env, "refresh_env", None)
    if fn is None:
        fn = getattr(getattr(env, "controller", None), "refresh_env", None)
    return fn if callable(fn) else None


def read_elements(env: Any, sleep: Callable[[float], None]) -> list[Any]:
    """settled_elements, and when the screen still comes back empty one reconnect through the controller's own
    refresh_env (about 17 s, measured), then one more read."""
    elements = settled_elements(env, sleep)
    refresh = None if elements else _refresher(env)
    if refresh is not None:
        try:
            refresh()
        except Exception:  # noqa: BLE001 - a failed reconnect leaves the empty read, which the caller counts
            return elements
        sleep(3.0)
        elements = settled_elements(env, sleep)
    if os.environ.get("AW_DEBUG_READS"):  # triage: one line per read, to stderr
        print(f"[read] elements={len(elements)} {[getattr(e, 'text', None) for e in elements][:8]}", file=sys.stderr, flush=True)
    return elements


def device_today(serial: str, run: Callable[..., Any] = subprocess.run) -> str | None:
    """'Sunday 15 October 2023, 15:34' from the device clock (`adb shell date`), or None when adb does not answer. The policy
    needs it: Calendar and Tasks questions are about 'next week' and 'the Friday after next', and the emulator clock is not today."""
    import datetime as dt
    import emulator_ctl

    try:
        r = run([emulator_ctl.adb_path(), "-s", serial, "shell", "date", "+%Y-%m-%d_%H:%M"], capture_output=True, text=True, timeout=20,
                creationflags=0x08000000 if sys.platform == "win32" else 0)
        when = dt.datetime.strptime(r.stdout.strip(), "%Y-%m-%d_%H:%M")
    except (OSError, ValueError, subprocess.SubprocessError):
        return None
    return f"{when:%A} {when.day} {when:%B %Y}, {when:%H:%M} (device clock)"


def run_task(env: Any, name: str, task_cls: Any, policy: Policy, executor: Any, max_steps: int, sleep: Callable[[float], None] = time.sleep, today_fn: Callable[[], str | None] | None = None) -> TaskResult:
    """One task. Never raises for a task-level failure: the result says how it ended."""
    t0 = time.monotonic()
    res = TaskResult(task=name)
    task = None
    try:
        task = task_cls(task_cls.generate_random_params())
        task.initialize_task(env)
        res.goal = task.goal
        if max_steps <= 0:  # AndroidWorld's own budget: complexity x 10 steps
            max_steps = max(1, int(task.complexity * 10))
    except Exception as e:  # noqa: BLE001 - a setup failure is a result, not a crash of the batch
        res.ended, res.error, res.wall_s = "setup_error", f"{type(e).__name__}: {e}", time.monotonic() - t0
        return res
    history: list[dict[str, Any]] = []
    blind = 0
    if hasattr(policy, "begin_task"):  # optional: the date and the step budget, for policies that use them
        try:
            today = today_fn() if today_fn else None
        except Exception:  # noqa: BLE001 - a missing date must not end the task
            today = None
        policy.begin_task(today=today, max_steps=max_steps)
    try:
        elements = read_elements(env, sleep)
        for step in range(max_steps):
            blind = blind + 1 if not elements else 0
            if blind >= BLIND_LIMIT:
                res.ended, res.error = "no_a11y", f"no UI elements on {blind} steps in a row, after a reconnect"
                break
            screen = tuple(env.logical_screen_size) if hasattr(env, "logical_screen_size") else None
            call_log = getattr(getattr(policy, "call", None), "log", None)  # D1: per-call usage records (dispatch_call, OmniRouteCall)
            seen = len(call_log) if isinstance(call_log, list) else 0
            action = policy.decide(res.goal, render_elements(elements), history)
            answered_by = getattr(getattr(policy, "call", None), "last_model", None)  # set by the OmniRoute backend only
            log = executor.execute(action, elements, screen)
            res.steps = step + 1
            history.append({"action": action, "error": log.error, "answer": log.answer, "status": log.status, "target": element_label(elements, action)})
            if answered_by:
                history[-1]["model"] = answered_by
            if isinstance(call_log, list) and len(call_log) > seen:
                history[-1]["calls"] = list(call_log[seen:])  # every model call this step made, retries included
            res.log.append(history[-1])
            if log.error:
                res.ended, res.error = "action_error", log.error
                break
            if log.answer is not None:
                res.answer = log.answer
                env.interaction_cache = log.answer
            if log.status:  # the policy said the task is complete or infeasible
                res.ended = "status"
                break
            elements = read_elements(env, sleep)
        else:
            res.ended = "max_steps"
    except Exception as e:  # noqa: BLE001
        res.ended, res.error = "action_error", f"{type(e).__name__}: {e}"
    try:
        res.success = bool(task.is_successful(env) == 1.0)
    except Exception as e:  # noqa: BLE001
        res.success, res.ended, res.error = None, "score_error", f"{type(e).__name__}: {e}"
    try:
        task.tear_down(env)
    except Exception as e:  # noqa: BLE001
        res.error = (res.error + "; " if res.error else "") + f"tear_down: {type(e).__name__}: {e}"
    res.wall_s = round(time.monotonic() - t0, 2)
    return res


LIMIT_RE = re.compile(r"usage limit|rate.?limit|hit your limit|limit reached|\b429\b|overloaded", re.I)


def is_limit_error(error: str | None) -> bool:
    """A `claude -p` failure caused by the plan's usage window (or an overloaded API), not by the agent."""
    return bool(error) and bool(LIMIT_RE.search(error))


def summarise(results: Sequence[TaskResult]) -> dict[str, Any]:
    scored = [r for r in results if r.success is not None]
    return {
        "tasks": len(results),
        "scored": len(scored),
        "passed": sum(1 for r in scored if r.success),
        "pass_rate": round(sum(1 for r in scored if r.success) / len(scored), 4) if scored else None,
        "unscored": [r.task for r in results if r.success is None],
        "steps_total": sum(r.steps for r in results),
        "wall_s_total": round(sum(r.wall_s for r in results), 1),
        **_receipt_totals(results),
        **guarded_totals([asdict(r) for r in results]),
    }


def guarded_totals(results: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """His 11 Nov answer ("Report both"): on a --bench-approve run, the guarded score is the tasks that passed with no
    approval at all, because with no approver the first ask is refused and ends the task. Nothing is added otherwise."""
    bench = [r for r in results if r.get("approvals") is not None]
    if not bench:
        return {}
    return {"approvals_total": sum(r["approvals"] for r in bench),
            "tasks_with_approvals": sum(1 for r in bench if r["approvals"]),
            "passed_guarded": sum(1 for r in bench if r.get("success") and not r["approvals"])}


def count_bench_approvals(state: Path, after_seq: int) -> tuple[int, int]:
    """(approvals with seq > after_seq, the highest seq seen) from the daemon's audit chain in `state`."""
    n, top = 0, after_seq
    for f in sorted(state.glob("audit*.jsonl")):
        for line in f.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                e = json.loads(line)
            except ValueError:
                continue
            seq = int(e.get("seq") or 0)
            top = max(top, seq)
            # an approval is recorded as decision "allowed" with rule "approved:bench" (core/src/transport.rs)
            if seq > after_seq and e.get("rule") == "approved:bench":
                n += 1
    return n, top


def _receipt_totals(results: Sequence[TaskResult]) -> dict[str, Any]:
    """D1: the chunk's token and USD totals, when the tasks carry receipts (nothing is added for older or scripted runs)."""
    rs = [r.receipt for r in results if r.receipt]
    if not rs:
        return {}
    add = lambda key: sum(x["tokens"][key] or 0 for x in rs)  # noqa: E731
    usd = [x["usd"] for x in rs]
    return {"tokens_in_total": add("input"), "tokens_out_total": add("output"), "model_calls_total": sum(x["model_calls"] for x in rs),
            "usd_total": round(sum(usd), 6) if all(u is not None for u in usd) else None}


def load_prior(out: Path) -> dict[str, TaskResult]:
    """The tasks an earlier run of this chunk already finished (its results.json), so a run stopped
    between tasks resumes where it ended instead of starting the chunk again."""
    f = out / "results.json"
    if not f.exists():
        return {}
    try:
        rows = json.loads(f.read_text(encoding="utf-8"))["results"]
    except (OSError, ValueError, KeyError):
        return {}
    return {r["task"]: TaskResult(**r) for r in rows}


def write_results(out: Path, results: Sequence[TaskResult], meta: dict[str, Any]) -> Path:
    out.mkdir(parents=True, exist_ok=True)
    path = out / "results.json"
    path.write_text(json.dumps({"meta": meta, "summary": summarise(results), "results": [asdict(r) for r in results]}, indent=2) + "\n", encoding="utf-8")
    return path


SCRCPY_ADB = os.environ.get("AGENTROID_ADB", "adb")  # the machine's one adb (for example the one bundled with scrcpy)


def make_env(console_port: int = 5554, grpc_port: int = 8554, adb_path: str = SCRCPY_ADB) -> tuple[Any, dict[str, Any]]:
    """The real AndroidWorld environment and its task registry. Needs the emulator (U-A2, see U-A2-RESULTS.md) and
    the venv the AndroidWorld venv (android_world installed, patch_android_env_windows.py applied). The adb is
    the machine's one (scrcpy's), so no second adb build restarts the server."""
    from android_world import registry  # type: ignore[import-not-found]
    from android_world.env import env_launcher  # type: ignore[import-not-found]

    env = env_launcher.load_and_setup_env(console_port=console_port, grpc_port=grpc_port, emulator_setup=False, adb_path=adb_path)
    return env, registry.TaskRegistry().get_registry(registry.TaskRegistry.ANDROID_WORLD_FAMILY)


def main(argv: Sequence[str] | None = None) -> int:
    # The CI sign-in token is for `claude` only. Take it out of this process's environment before anything is
    # started, so bridged, the MCP server and android_world never inherit it (claude_session.child_env).
    if str(HERE / "policy-v3") not in sys.path:
        sys.path.insert(0, str(HERE / "policy-v3"))
    import claude_session as _cs  # noqa: E402
    _cs.hold_token()
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--subset", default=str(HERE / "subset-20.json"))
    ap.add_argument("--out", required=True)
    ap.add_argument("--tasks", help="comma-separated names, to run fewer than the subset")
    ap.add_argument("--max-steps", type=int, default=MAX_STEPS_DEFAULT, help="steps per task; 0 = AndroidWorld's own budget (task complexity x 10)")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--backend", choices=["anthropic", "omniroute", "claude-p"], default="anthropic", help="anthropic = the dispatcher (default); omniroute = the local free-provider router (policy_claude.omniroute_call)")
    ap.add_argument("--model", default="sonnet", help="anthropic: sonnet | opus | haiku (dispatcher model names); omniroute: a model id such as gemini/gemini-3-flash-preview, or a comma-separated fixed fallback order (never auto/*)")
    ap.add_argument("--max-tokens", type=int, default=None, dest="model_max_tokens", help="omniroute: max_tokens per model call (default 1024: the free reasoning models count their thinking tokens in it)")
    ap.add_argument("--pool", help="which API key pool the dispatcher should use (optional; anthropic backend only)")
    ap.add_argument("--device", default="emulator-5554")
    ap.add_argument("--usd-cap", type=float, default=float(os.environ.get("AGENTROID_USD_CAP") or 0),
                    help="stop after the task that takes this run's API-equivalent cost counter to the cap (0 = no cap; CI shards use 20)")
    ap.add_argument("--bench-approve", action="store_true", help="emulator only: bridged answers the guard's asks with allow (audited as bench); the summary adds passed_guarded (his 11 Nov answer: report both)")
    ap.add_argument("--policy", choices=["v1", "v2", "v3"], default="v1", help="v1 = policy_claude.py (the scored full run); v2 = policy-v2/policy_claude_v2.py (candidate, evaluated on the v1 failures); v3 = policy-v3/agent_policy.py (agent mode; needs --backend claude-p)")
    ap.add_argument("--v3", default="A5", help="v3 only: a ladder rung A1..A5, or a comma-separated flag list from screens,planner,verifier,notes,stall ('' = none)")
    ap.add_argument("--planner-model", default=None, help="v3 only: model for the planner and the verifier (default: --model)")
    ap.add_argument("--effort", default=None, help="claude-p only: --effort for every session")
    args = ap.parse_args(argv)

    names = load_subset(args.subset)
    if args.tasks:
        want = [t.strip() for t in args.tasks.split(",") if t.strip()]
        unknown = [t for t in want if t not in names]
        if unknown:
            print(f"not in the subset: {unknown}", file=sys.stderr)
            return 2
        names = want
    if args.dry_run:
        print(f"would run {len(names)} tasks, max {args.max_steps} steps each:")
        for n in names:
            print("  ", n)
        return 0
    return real_run(args, names)


def make_call(args: argparse.Namespace, pc_module: Any = None) -> Callable[[str, str], str]:
    """The model call for the chosen backend. `anthropic` (default) = one dispatcher call per step on the API pools;
    `omniroute` = the free router (fixed model or fixed fallback order from a comma-separated --model; never auto/*)."""
    if pc_module is None:
        import policy_claude as pc_module  # the call factories live in the v1 module for both policies
    if getattr(args, "backend", "anthropic") == "omniroute":
        models = [m.strip() for m in str(args.model).split(",") if m.strip()]
        return pc_module.omniroute_call(models, max_tokens=getattr(args, "model_max_tokens", None) or OMNIROUTE_MAX_TOKENS)
    return pc_module.dispatch_call(model=args.model, pool=args.pool, why=f"AndroidWorld {Path(args.subset).stem}")


MODEL_IDS = {"sonnet": "claude-sonnet-5-5", "opus": "claude-opus-5-5", "haiku": "claude-haiku-5-5", "fable": "claude-fable-5-1"}


def v3_flags(spec: str) -> tuple[str, ...]:
    """'A3' -> the ladder rung's flags; 'screens,notes' -> those flags; '' -> none."""
    import importlib.util

    sp = importlib.util.spec_from_file_location("agent_policy", HERE / "policy-v3" / "agent_policy.py")
    m = importlib.util.module_from_spec(sp)
    sp.loader.exec_module(m)
    spec = (spec or "").strip()
    if spec in m.LADDER:
        return tuple(m.LADDER[spec])
    return tuple(f.strip() for f in spec.split(",") if f.strip())


def png_screenshot(env: Any, scale: int = 2) -> bytes | None:
    """The emulator's current frame as a PNG, downscaled by `scale` (1080x2400 -> 540x1200, about 0.9k image tokens)."""
    import io

    from PIL import Image  # type: ignore[import-not-found]

    px = env.get_state(wait_to_stabilize=False).pixels
    if px is None:
        return None
    im = Image.fromarray(px)
    if scale > 1:
        im = im.resize((im.width // scale, im.height // scale))
    buf = io.BytesIO()
    im.save(buf, format="PNG", optimize=True)
    return buf.getvalue()


def make_v3_policy(args: argparse.Namespace, env: Any) -> Any:
    """Policy v3 on a Claude subscription: a warm `claude -p` executor session per task, one-shot planner and verifier."""
    import importlib.util

    if args.backend != "claude-p":
        raise SystemExit("--policy v3 needs --backend claude-p (a Claude subscription)")
    sys.path.insert(0, str(HERE / "policy-v3"))
    import agent_policy as ap3  # noqa: E402
    import claude_session as cs  # noqa: E402

    model = MODEL_IDS.get(args.model, args.model)
    pmodel = MODEL_IDS.get(args.planner_model or args.model, args.planner_model or args.model)
    flags = v3_flags(args.v3)
    return ap3.AgentPolicy(
        exec_factory=lambda system: cs.ClaudeSession(model, system, effort=args.effort),
        planner=cs.one_shot(pmodel, ap3.PLANNER_SYSTEM, effort=args.effort) if "planner" in flags else None,
        verifier=cs.one_shot(pmodel, ap3.VERIFIER_SYSTEM, effort=args.effort) if "verifier" in flags else None,
        screenshot=(lambda: png_screenshot(env)) if "screens" in flags else None,
        flags=flags,
    )


def real_run(args: argparse.Namespace, names: Sequence[str]) -> int:
    """The emulator must be up (U-A2-RESULTS.md) and this must run under the AndroidWorld venv. One model call per step
    through the dispatcher (full price, on the pool's ledger and caps). Results are rewritten after every task."""
    import os
    import subprocess
    import tempfile

    import agentroid_mcp as am
    import receipts
    if args.policy == "v2":
        import importlib.util

        spec = importlib.util.spec_from_file_location("policy_claude_v2", HERE / "policy-v2" / "policy_claude_v2.py")
        pc = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(pc)
    else:
        import policy_claude as pc

    scaffold = HERE.parents[1]
    # AGENTROID_BRIDGED picks another build (run 3 uses target/run3, so run 2's binary stays as it was)
    bridged = Path(os.environ.get("AGENTROID_BRIDGED") or scaffold / "target" / "debug" / ("bridged.exe" if sys.platform == "win32" else "bridged"))
    import hashlib
    bridged_sha256 = hashlib.sha256(bridged.read_bytes()).hexdigest() if bridged.exists() else None
    tmp = Path(tempfile.mkdtemp(prefix="aw-pilot-"))
    state = tmp / "state"
    # A named pipe on Windows; elsewhere a Unix socket in this run's temp folder (bridged binds it 0600 and checks the peer).
    pipe = rf"\\.\pipe\agentroid-aw-pilot-{os.getpid()}" if sys.platform == "win32" else str(tmp / "bridged.sock")
    bargv = [str(bridged), "--adb", "auto", "--allow-emulators", "--pipe", pipe, "--state-dir", str(state)]
    if args.bench_approve:  # emulator-only stand-in for his Allow tap; bridged refuses it unless only emulators are allowed
        bargv.append("--bench-approve")
    proc = subprocess.Popen(bargv, creationflags=0x08000000 if sys.platform == "win32" else 0, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    audit_seq = 0
    client = env = None
    results: list[TaskResult] = []
    out = Path(args.out)
    try:
        log = state / "bridged.log"
        for _ in range(100):
            if log.exists() and "listening" in log.read_text(errors="replace"):
                break
            time.sleep(0.1)
        client = am.start_agentroid(pipe=pipe, session="aw-pilot", stderr_path=tmp / "mcp.err")
        env, registry = make_env()
        if args.policy == "v3":
            policy, call = make_v3_policy(args, env), None
        else:
            call = make_call(args)
            policy = pc.ClaudePolicy(call)
        executor = am.Executor(client, device=args.device)
        meta = {"bridged": str(bridged), "bridged_sha256": bridged_sha256, "bench_approve": bool(args.bench_approve), "mode": "executor", "backend": args.backend, "model": args.model, "pool": (args.pool or "default") if args.backend == "anthropic" else None, "subset": str(args.subset), "max_steps": args.max_steps, "device": args.device, "policy": args.policy, "started": time.strftime("%Y-%m-%dT%H:%M:%S")}
        prior = load_prior(out)
        for n in names:
            if n in prior:  # scored by an earlier, stopped run of this same chunk: keep it, don't run it again
                results.append(prior[n])
                continue
            if (out.parent / "STOP").exists():  # the pace or memory guard: end after the task that just finished
                print(f"STOP file found: ending before {n}", flush=True)
                meta["stopped_before"] = n  # the not-run tasks are absent from results, not failed
                write_results(out, results, meta)
                break
            if n not in registry:
                results.append(TaskResult(task=n, ended="setup_error", error="not in the AndroidWorld registry"))
            else:
                print(f"[{len(results) + 1}/{len(names)}] {n}", flush=True)
                results.append(run_task(env, n, registry[n], policy, executor, args.max_steps, today_fn=lambda: device_today(args.device)))
                if args.bench_approve:
                    results[-1].approvals, audit_seq = count_bench_approvals(state, audit_seq)
                if getattr(policy, "events", None) is not None:
                    results[-1].policy_events = list(policy.events)
                results[-1].receipt = receipts.receipt_from_live(asdict(results[-1]), meta, run=out.name)
                tok = results[-1].receipt["tokens"]
                print(f"    -> success={results[-1].success} ended={results[-1].ended} steps={results[-1].steps} "
                      f"tokens in={tok['input']} out={tok['output']} usd={results[-1].receipt['usd']} {results[-1].error or ''}", flush=True)
                if is_limit_error(results[-1].error):  # the subscription's usage limit, not the agent's failure: unscored, and stop
                    results[-1].success, results[-1].ended = None, "limit_error"
                    print("usage limit reached: task unscored, ending this run", flush=True)
                    write_results(out, results, meta)
                    break
                spent = sum(float((r.receipt or {}).get("usd") or 0) for r in results)
                if args.usd_cap and spent >= args.usd_cap:
                    print(f"usd cap {args.usd_cap} reached ({spent:.2f}): ending after {n}", flush=True)
                    meta["usd_cap_hit"] = round(spent, 2)
                    write_results(out, results, meta)
                    break
            meta["policy_calls"], meta["policy_parse_failures"] = policy.calls, policy.parse_failures
            if args.backend == "omniroute":
                meta["model_calls"], meta["model_served"], meta["model_retries"] = dict(call.counts), dict(call.served), call.retries
            write_results(out, results, meta)
        print(json.dumps(summarise(results)))
        return 0
    finally:
        if hasattr(locals().get("policy"), "close"):
            policy.close()
        if env is not None:
            env.close()
        if client:
            client.close()
        subprocess.run([str(bridged), "--stop", "--state-dir", str(state)], creationflags=0x08000000 if sys.platform == "win32" else 0, timeout=30)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        if args.bench_approve:  # keep the audit chain beside the results, so the approval counts can be re-read from it
            import shutil
            out.mkdir(parents=True, exist_ok=True)
            for f in state.glob("audit*"):
                shutil.copy2(f, out / f.name)


if __name__ == "__main__":
    raise SystemExit(main())
