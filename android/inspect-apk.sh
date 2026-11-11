#!/usr/bin/env bash
# Read-only look inside out/helper.apk: the packaged manifest's components and the helper classes in the dex.
# Never touches a device.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; command -v cygpath >/dev/null && HERE="$(cygpath -m "$HERE")"
SDK="${ANDROID_HOME:-${LOCALAPPDATA:-$HOME/AppData/Local}/Android/Sdk}"
BT="$SDK/build-tools/36.0.0"
APK="$HERE/out/helper.apk"

echo "--- packaged manifest: application and components"
"$BT/aapt2" dump xmltree --file AndroidManifest.xml "$APK" | grep -E "E: (application|activity|service|receiver|provider|uses-sdk)|A: [^ ]*:(name|exported|permission|allowBackup|debuggable|usesCleartextTraffic|foregroundServiceType|minSdkVersion|targetSdkVersion)\(" | grep -v -E "intent-filter|android.intent|android.accessibilityservice|android.view" || true
echo "--- helper classes in classes.dex"
unzip -p "$APK" classes.dex | grep -a -o -E "Ldev/agentroid/helper/[A-Za-z0-9/]+;" | sort -u
echo "--- apk entries"
unzip -l "$APK" | grep -E "classes|AndroidManifest|resources.arsc|res/xml"
