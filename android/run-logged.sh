#!/usr/bin/env bash
# usage: run-logged.sh LOGFILE command [args...]
# Runs the command, copies its output to LOGFILE and prints "EXIT CODE: n" as the last line, so the exit code is on record.
log="$1"; shift
mkdir -p "$(dirname "$log")"
set +e
"$@" 2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
echo "EXIT CODE: $rc" | tee -a "$log"
exit "$rc"
