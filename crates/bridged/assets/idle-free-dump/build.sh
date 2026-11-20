#!/usr/bin/env bash
# Builds agentroid-dump.dex from Dump.java (javac against android.jar, then d8). The dex is committed beside this
# script because bridged embeds it with include_bytes!; rebuild it here after any change to Dump.java.
#   bash crates/bridged/assets/idle-free-dump/build.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
sdk="${ANDROID_SDK_ROOT:-${ANDROID_HOME:-$LOCALAPPDATA/Android/Sdk}}"
jar="$sdk/platforms/android-34/android.jar"
d8="$sdk/build-tools/34.0.0/lib/d8.jar"
tmp="$(mktemp -d)"
javac --release 8 -cp "$jar" -d "$tmp/classes" "$here/Dump.java" 2>&1 | grep -v "^warning: \[options\]" || true
java -cp "$d8" com.android.tools.r8.D8 --min-api 29 --lib "$jar" --output "$tmp" "$tmp/classes/dev/agentroid/dump/Dump.class"
cp "$tmp/classes.dex" "$here/agentroid-dump.dex"
python -c "import shutil,sys;shutil.rmtree(sys.argv[1])" "$tmp"
ls -l "$here/agentroid-dump.dex"
# crates/bridged/src/live.rs pins this hash (IDLE_FREE_DEX_SHA256); a test fails until the two agree
if command -v sha256sum >/dev/null 2>&1; then sha256sum "$here/agentroid-dump.dex"; else certutil -hashfile "$here/agentroid-dump.dex" SHA256; fi
