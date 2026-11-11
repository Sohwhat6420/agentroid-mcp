#!/usr/bin/env bash
# Plain-JVM tests for :runner (task 2.14). javac + java only: no Gradle, no JUnit, no phone, no adb, no download.
# Compiles :core main, the three Android-free :link files the runner reads (TrustStore, LinkCrypto, Identity), and
# :runner main + tests. The bundled safe list (scaffold/runner/safe-classes.json) is on the classpath as the app has it.
# Exit code: 0 when every test passed, 1 when any failed, anything else means the build of the tests broke.
# Usage: bash android/runner/run-runner-tests.sh [ClassNameFilter...]
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"; command -v cygpath >/dev/null && HERE="$(cygpath -m "$HERE")"
ANDROID="$(cd "$HERE/.." && pwd)"; command -v cygpath >/dev/null && ANDROID="$(cygpath -m "$ANDROID")"
SCAFFOLD="$(cd "$HERE/../.." && pwd)"; command -v cygpath >/dev/null && SCAFFOLD="$(cygpath -m "$SCAFFOLD")"
OUT="$HERE/out"
rm -rf "$OUT/classes"
mkdir -p "$OUT/classes"
NET="$ANDROID/link/src/main/java/dev/agentroid/helper/link/net"

javac --release 8 -encoding UTF-8 -nowarn -d "$OUT/classes" \
    $(find "$ANDROID/core/src/main/java" "$HERE/src/main/java" "$HERE/src/test/java" -name '*.java') \
    "$NET/TrustStore.java" "$NET/LinkCrypto.java" "$NET/Identity.java"

SEP=":"; case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) SEP=";";; esac
java -Drunner.fixtures="$SCAFFOLD/schemas/fixtures/segment" -Drunner.out="$OUT" \
    -cp "$OUT/classes$SEP$SCAFFOLD/runner" dev.agentroid.helper.runner.RunnerTestRunner "$@"
