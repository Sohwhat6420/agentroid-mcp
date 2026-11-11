#!/usr/bin/env bash
# usage: gradle-offline.sh <tasks...>
# Runs the Gradle wrapper OFFLINE, without a daemon, with plain console output, through bash (gradlew, not gradlew.bat),
# so no console window opens and nothing is downloaded. ANDROID_HOME defaults to the usual Windows SDK folder; the SDK
# location is passed by environment, so no local.properties file is written into the tree.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
export ANDROID_HOME="${ANDROID_HOME:-${LOCALAPPDATA:-$HOME/AppData/Local}/Android/Sdk}"
exec bash "$HERE/gradlew" -p "$HERE" --offline --no-daemon --console=plain "$@"
