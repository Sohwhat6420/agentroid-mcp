# Benchmark

> Moved here from the README on 14 Nov 2026, unchanged except for links, so the README can lead with the context budget.

## Benchmark in progress

There is no full 116-task score yet. Everything below was measured, and each row gives its date and configuration. Nothing here is projected. Per-task result files are published with the full run; until then, treat these as the author's report.

**1. A 20-task pilot: which additions help** (11 Nov 2026; `bench/androidworld/subset-20.json`; Claude Opus 5.5 through `claude -p`; one attempt per task; Android 13 emulator on a Windows laptop; no stand-in approver, so the guard's refusals count as fails). Each addition was kept only if it beat the best score so far by its threshold. One run per stage, so a 1-task difference is close to noise.

| Stage | Adds | Passed of 20 | Kept |
| --- | --- | --- | --- |
| A0 | the base loop (policy v3, agent mode) | 15 | baseline |
| +screens | a screenshot with each turn | 16 | no (needed 17) |
| +planner | a separate planner | 14 | no |
| +verifier | a separate verifier must agree before "complete" | **16** | **yes** |
| +notes | a scratchpad, on top of the verifier | 15 | no |
| +stall | a re-plan after a stall, on top of the verifier | 15 | no |

**2. Run 2, the full task list with the verifier: partial** (11 Nov 2026; `subset-all.json` in order; the same model and laptop; the bench stand-in approver on). It was stopped for laptop memory after 16 of the 116 tasks.

| Tasks run | Passed (with approvals) | Passed (guarded: no approval needed) | Asks answered by the stand-in |
| --- | --- | --- | --- |
| 16 of 116 | 6 | 5 | 1 |

"Guarded" counts only tasks that passed without the guard asking for a human yes. On an emulator nobody can tap Allow, so the stand-in approver (`--bench-approve`, emulator only, audited as `approved:bench`) answers instead. Both numbers are reported, along with the count of asks. The approval counter in `run_pilot.py` was fixed at 20:15, before the second chunk, and the first chunk was recounted from its saved audit log.

**What comes next.** The rest of the benchmark runs on GitHub Actions emulators. Those runs are their own hardware configuration and are never added to the laptop numbers above. There will be two configurations: run 2 (verifier, the guard as it was) and run 3 (verifier and screens, with the stopwatch fix below), each on the full 116.

**A known limit the runs found.** The guard reads the screen with `uiautomator dump`, which waits for the screen to be still. On a screen that never stops changing, such as a running stopwatch, it gives up with "could not get idle state". The guard then cannot read the screen and refuses the tap: safe, but a lost task. On 11 Nov the stock dump read 0 of 3 times on the emulator with a stopwatch running. A small read-only fallback reader read 3 of 3. bridged now uses the fallback for that one error, and run 3 measures the change. Details are in [docs/SAFETY.md](SAFETY.md).

**How it is run.** This is the *executor* setup. AndroidWorld's own harness sets each task up and scores it. Agentroid's agent loop (`bench/androidworld/run_pilot.py`) plays the task, with a Claude model as the policy.

- Policy v3 (`bench/androidworld/policy-v3/`): one warm `claude -p` session per task acts as the executor, with tools, settings and MCP servers switched off, so the model only answers. The switchable additions in the table above (verifier, screens and the others) are separate one-shot calls.
- At each step the model is given the task, its recent actions and AndroidWorld's numbered list of UI elements, and returns one action in AndroidWorld's `JSONAction` vocabulary (14 action types). Agentroid carries the action out through its MCP tools.
- The step budget per task is AndroidWorld's own: the task's complexity times 10.
- Success is AndroidWorld's own `is_successful` for the task, checked after the last step.
- The runs are on an Android emulator with a headless x86_64 Google APIs image (Android 13, API 33).
- The task list is `bench/androidworld/subset-all.json` (116 tasks, the complete AndroidWorld registry as of 28 Oct 2026: the names match exactly). Many tasks use AndroidWorld's own apps, which must be installed on the emulator first. A task that cannot be set up or scored is recorded as unscored. The headline rate is passed tasks out of all 116, so an unscored task counts against the score; the rate over scored tasks only is shown beside it.
- Model, number of attempts (one per task), date and the commit of this repository go in the table when the run is done.
- The policy used for the release run was written after reading the failures of an earlier run, so the result is not a held-out score: it measures a loop that was tuned on this same task list.
- Policy v3 calls the model through the `claude` CLI (`--backend claude-p`; `AGENTROID_CLAUDE` names another binary). The older policies call it through a dispatcher script you supply (`AGENTROID_DISPATCH`). The scripts in `bench/androidworld/` read the SDK, emulator and APK locations from environment variables (`ANDROID_SDK_ROOT`, `ANDROID_AVD_HOME`, `AW_APKS`, `AW_PYTHON`). For the older policies no dispatcher is included, so a run on another machine needs one written first. The offline tests run anywhere.

**Compare only like with like.** These results are not comparable with leaderboard entries that use a different agent loop or observation. An entry that uses a different agent loop, prompt or observation (AndroidWorld's own reference agents, M3A and T3A, are examples of agents with their own loop) measures something different. Here the loop, the prompt and the action executor are Agentroid's, and the model sees AndroidWorld's element list rather than Agentroid's own tree, so the number measures this loop with this model and this executor, not the model alone. Known differences from AndroidWorld's own executor, such as how a double tap, a text field clear and a scroll on element 0 are performed, are listed in `bench/androidworld/ACTION-SPACE.md`. A second mode, where the model calls Agentroid's tools directly, has had a smoke test only and has no results.
