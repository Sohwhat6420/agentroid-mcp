# AndroidWorld action space → Agentroid tools

Read from the source of google-research/android_world on 26 Oct 2026 (`env/json_action.py`, `env/actuation.py`, `env/adb_utils.py`, `env/interface.py`, `episode_runner.py`, `agents/base_agent.py`). The earlier research note (`research/remote-and-benchmarks.md` §B.1.2) listed 8 actions with different names; **the real space has 14**, and it has no "complete task" tool: the agent ends an episode with a `status` action. Code: `action_map.py` (pure), tested by `test_action_map.py` (27 tests) and, on the real daemon with the fake device, `test_integration_fake.py`.

## The 14 actions

| AndroidWorld `action_type` | What android_world does | Agentroid calls | Status |
| --- | --- | --- | --- |
| `click` (`index` or `x`,`y`) | tap the centre of element `index`'s pixel box, or the point | `tap {x, y, space:"device"}` | fake-stack test |
| `double_tap` | double tap | `tap` twice | **unverified**: no double-tap tool, the gap is one MCP round trip against Android's 300 ms window; measure at U-A2 |
| `long_press` | long press, no duration set | `long_press {x, y, space:"device"}` (default hold) | fake-stack test |
| `input_text` (`text`, `index`/`x,y`, `clear_text`) | click target, wait 1 s; if `clear_text`: select-all (113+29) + delete, wait 1 s; type; **always press Enter** | `tap` (pause 1 s) · `key KEYCODE_MOVE_END` + `key delete` × (len(field text)+2, max 60) (pause 1 s) · `type_text {text, submit:true}` | tap/type fake-stack test; **clear is unverified** (no select-all combo tool; text over 60 characters is only part-cleared) |
| `keyboard_enter` | Enter | `key enter` | fake-stack test |
| `navigate_back` | Back | `key back` | fake-stack test |
| `navigate_home` | Home | `key home` | |
| `scroll` (`direction`, optional `index`) | swipe from the centre of the element (or screen) to its edge: `down` ends at the top, `up` at the bottom, `right` at the left, `left` at the right. in the source `index` 0 counts as "no index" (`if action.index:`); **we deliberately honour element 0** (the policy names it on purpose; a slider was element 0 in the 26 Oct pilot), so scroll-with-index-0 differs from upstream | no index: `scroll {direction}` (same convention: `down` = finger moves up). With index: `swipe` centre→edge inside the element's box clamped to the screen | no-index fake-stack test; swipe geometry unit-tested. Agentroid's page scroll is 0.75h→0.3h in 350 ms, not centre→edge, so page distances differ slightly |
| `swipe` (`direction`) | whole screen, midpoints, **500 ms**, the inverse of scroll (`down` = top→bottom) | `swipe {x1,y1,x2,y2, ms:500, space:"device"}` from edge to edge | unit-tested; fake-stack test |
| `open_app` (`app_name`) | name → `_PATTERN_TO_ACTIVITY` (first `re.match` in dict order, lower-cased), start that activity; unknown names are tried as a package with `monkey -p` | `open_app {package}` with the package half of the table (57 patterns copied); unknown name that looks like a package passes through | table unit-tested. Agentroid launches the package's launcher activity, not AndroidWorld's named one. `calendar`, `browser`, `email`, `gallery` are VIEW-intent shortcuts in android_world and are refused here |
| `wait` | sleep 1 s | `wait {seconds:1}` | fake-stack test |
| `answer` (`text`) | stores the text in `env.interaction_cache` and shows it; no device action | none: the harness calls `env.execute_action` itself | harness |
| `status` (`goal_status` complete/infeasible) | no device action; the agent's way to end the episode | none: the harness ends the episode | harness |
| `unknown` | invalid | refused | |

`JSONAction` rules kept: an `index` and an `x`/`y` together are refused; `direction` must be left/right/up/down; `keycode` must start `KEYCODE_` (the agent loop here does not use it).

## Observation side

AndroidWorld's `AsyncEnv.get_state()` returns `pixels`, the raw `forest` and `ui_elements` (each with `bbox_pixels`). Two ways to use Agentroid:

1. **Native mode (the headline).** A Claude agent with the Agentroid MCP tools does the whole task itself, reading `ui_tree` (stable refs, `since` diffs) and using `tap {ref}` etc. The harness only prepares the task (`initialize_task`), hands over the goal, reads the final `ANSWER:`/`STATUS:` line, runs `is_successful`, `tear_down`. No action map is involved; the numbers are tokens per success, steps and wall time from real API usage.
2. **Executor mode (same brain, different hands).** AndroidWorld's own agents (M3A, T3A, or any `EnvironmentInteractingAgent`) choose `JSONAction`s from AndroidWorld's observation; `agentroid_mcp.Executor` performs them through Agentroid with this map. Used as a baseline pairing and to test the map; it shows what the execution layer adds, not what Agentroid's observation adds.

## Things the emulator run had to confirm

- AndroidWorld's accessibility forwarder (gRPC on 8554, `-grpc 8554`) alongside `uiautomator dump` (the Artemis u2 backend did not coexist with it).
- Agentroid needed a live adb transport; on 26 Oct `bridged` ran only `--fake`. It now lives in `crates/bridged/src/live.rs`.
- The daemon does not return image bytes over IPC, so native mode is text-only (`ui_tree`); AndroidWorld agents that need `pixels` get them from AndroidWorld's own env in executor mode.
- `double_tap`, `clear_text` and the scroll distance, as above.
- Whether `-no-window` emulator + AndroidWorld's setup works on Windows with WHPX (AndroidWorld documents Linux/macOS); fall back to WSL2 with KVM only if it does not.
