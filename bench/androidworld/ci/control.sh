#!/usr/bin/env bash
# The stopwatch positive control on a CI emulator (no model, no secret): the stock `uiautomator dump` against the
# idle-free fallback while a stopwatch runs. Same probe as results/positive-control-2026-11-11.
#   bash bench/androidworld/ci/control.sh <out dir>
set -euo pipefail
out="$1"; here="$(cd "$(dirname "$0")/.." && pwd)"; root="$(cd "$here/../.." && pwd)"
mkdir -p "$out"
adb wait-for-device
adb push "$root/crates/bridged/assets/idle-free-dump/agentroid-dump.dex" /data/local/tmp/agentroid-dump.dex
cmd="CLASSPATH=/data/local/tmp/agentroid-dump.dex:/system/framework/uiautomator.jar app_process /system/bin dev.agentroid.dump.Dump /dev/tty"
python3 -B "$here/triage/probe_idle_dump.py" --no-restart --fallback "$cmd" | tee "$out/rounds.txt"
