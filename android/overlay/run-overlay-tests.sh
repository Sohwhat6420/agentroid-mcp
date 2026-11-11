#!/usr/bin/env bash
# Plain-JVM tests for :overlay's logic (task 2.15, Detector 2). javac + java only: no Gradle, no JUnit, no phone, no adb.
# Exit code: 0 when every test passed, 1 when any failed, anything else means the build of the tests broke.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; command -v cygpath >/dev/null && HERE="$(cygpath -m "$HERE")"
OUT="$HERE/build/jvm-tests"
rm -rf "$OUT"
mkdir -p "$OUT"
javac --release 8 -encoding UTF-8 -d "$OUT" \
    $(find "$HERE/src/main/java/dev/agentroid/helper/overlay/logic" "$HERE/src/test/java" -name '*.java')
java -Doverlay.root="$HERE" -cp "$OUT" dev.agentroid.helper.overlay.logic.OverlayTestRunner "$@"
