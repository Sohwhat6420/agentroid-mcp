#!/usr/bin/env bash
# Plain-JVM tests for :core plus the clean-room audit of the merged manifest and the sources.
# javac + java only: no Gradle, no JUnit (there is none offline), no phone, no adb.
# Exit code: 0 when every test passed, 1 when any failed, anything else means the build of the tests broke.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"; command -v cygpath >/dev/null && HERE="$(cygpath -m "$HERE")"
OUT="$HERE/out"
rm -rf "$OUT/test-classes"
mkdir -p "$OUT/test-classes"

# Task 2.12 adds the pure-Java parts of :link (link/net, which uses OkHttp) and :keeper (keeper/logic), with their tests.
# OkHttp, Okio and kotlin-stdlib are READ from HELPER_LIBS (default ../s4-apk/libs).
LIBS_DIR="${HELPER_LIBS:-$HERE/../s4-apk/libs}"
[ -d "$LIBS_DIR" ] && command -v cygpath >/dev/null && LIBS_DIR="$(cygpath -m "$LIBS_DIR")"
[ -d "$LIBS_DIR" ] || { echo "missing OkHttp/Okio/kotlin-stdlib jars: set HELPER_LIBS"; exit 2; }
LIBS=$(find "$LIBS_DIR" -name '*.jar' | tr '\n' ';')

# core (main + tests), the JDK-only manifest merger that the audit and the APK build share, link/net and keeper/logic.
javac --release 8 -encoding UTF-8 -d "$OUT/test-classes" -cp "$LIBS" \
    $(find "$HERE/core/src/main/java" "$HERE/core/src/test/java" "$HERE/tools/src/main/java" \
           "$HERE/link/src/main/java/dev/agentroid/helper/link/net" "$HERE/link/src/test/java" \
           "$HERE/keeper/src/main/java/dev/agentroid/helper/keeper/logic" "$HERE/keeper/src/test/java" -name '*.java')

java -Dhelper.root="$HERE" -cp "$OUT/test-classes;$LIBS" dev.agentroid.helper.core.TestRunner "$@"
