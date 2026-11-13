#!/usr/bin/env bash
# One CI shard of the AndroidWorld run, inside android-emulator-runner (the emulator is up on emulator-5554, grpc 8554).
#   bash bench/androidworld/ci/run_shard.sh <run2|run3> <comma list of tasks>
# Writes out/results.json (+ audit*.jsonl from the bench approver). The flags match the laptop's run_stage, except
# that the emulator is the runner's (no --fresh-emulator; one emulator per shard instead).
set -euo pipefail
config="$1"; tasks="$2"
here="$(cd "$(dirname "$0")/.." && pwd)"
case "$config" in
  run2) v3=verifier ;;
  run3) v3=verifier,screens ;;
  *) echo "unknown config $config" >&2; exit 2 ;;
esac
# The sign-in token arrives as a 0600 file (CLAUDE_TOKEN_FILE), never in this step's environment. Drop any copy of
# it from the environment anyway, so nothing started here can inherit one.
unset CLAUDE_CODE_OAUTH_TOKEN
test -s "${CLAUDE_TOKEN_FILE:-}" || { echo "CLAUDE_TOKEN_FILE is not set or empty" >&2; exit 3; }
mkdir -p out

# The usage stop (repo variable BENCH_STOP=1, set by the orchestrator). Never cancels a task mid-way:
#  - set before this shard started: run nothing, and record that, so the merge lists the tasks as not run;
#  - set while it runs: a poller writes ./STOP, which run_pilot reads before each task (beside --out).
record_stop() { python3 -B -c 'import json,sys; json.dump({"results": [], "meta": {"stopped_before": sys.argv[1]}}, open("out/results.json", "w"))' "${tasks%%,*}"; }
if [ "${BENCH_STOP_AT_START:-}" = "1" ]; then
  echo "BENCH_STOP=1 when this shard started: running nothing"
  record_stop; exit 0
fi
read_stop() { [ -n "${GH_TOKEN:-}" ] && gh api "repos/${GITHUB_REPOSITORY}/actions/variables/BENCH_STOP" --jq .value 2>/dev/null || true; }
if [ "$(read_stop)" = "1" ]; then echo "BENCH_STOP=1: running nothing"; record_stop; exit 0; fi
( while sleep 60; do   # the poller needs only GH_TOKEN
    if [ "$(read_stop)" = "1" ]; then echo "BENCH_STOP=1 seen at $(date -u +%H:%MZ): ending after the current task"; : > STOP; exit 0; fi
  done ) &
poller=$!
trap 'kill "$poller" 2>/dev/null || true' EXIT

adb wait-for-device
# AndroidWorld's apps (the laptop installed these once with install_apps.py; a CI AVD starts empty).
python3 -B "$here/install_apps.py"   # the installer never needs the sign-in token
mkdir -p out
# read the token into a shell variable (not exported, so no child inherits it), delete the file, and set it for
# run_pilot alone, which takes it out of its own environment at once (claude_session.hold_token)
token="$(cat "$CLAUDE_TOKEN_FILE")"; rm -f "$CLAUDE_TOKEN_FILE"
CLAUDE_CODE_OAUTH_TOKEN="$token" python3 -B -u "$here/run_pilot.py" --subset "$here/subset-all.json" --tasks "$tasks" --out out \
  --max-steps 0 --backend claude-p --policy v3 --v3 "$v3" --model opus --bench-approve
