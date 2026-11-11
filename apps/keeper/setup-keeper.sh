#!/usr/bin/env bash
# Install and arm the keeper on one phone, from a computer that already has adb access to it.
#
# Usage:  ./setup-keeper.sh <SERIAL> [HUB_URL]
#   SERIAL   the adb device id, exactly as `adb devices` prints it (USB serial, ip:port, or the
#            adb-XXXX-yyyy._adb-tls-connect._tcp name)
#   HUB_URL  optional. Where the keeper POSTs its heartbeat, e.g. http://192.0.2.10:8765/keeper/heartbeat
#            Leave it out and the keeper keeps adb_wifi_enabled on and sends nothing anywhere.
#
# Environment:
#   KEEPER_REFUSE_SERIALS  space-separated hardware serials (ro.serialno) this script must never touch,
#                          e.g. your daily-driver phone. Checked before any change is made.
#   KEEPER_APK             path to the APK (default: out/keeper.apk next to this script)
#
# Every adb line takes -s "$SERIAL"; there is never a bare `adb`, so it cannot touch another device.
set -euo pipefail

SERIAL="${1:?usage: setup-keeper.sh <SERIAL> [HUB_URL]}"
HUB_URL="${2:-}"
PKG="dev.agentroid.keeper"
APK="${KEEPER_APK:-$(cd "$(dirname "$0")" && pwd)/out/keeper.apk}"
# Git Bash paths (/c/...) are unreadable by a Windows adb.exe; use the C:/ spelling when cygpath exists.
if command -v cygpath >/dev/null 2>&1; then APK="$(cygpath -m "$APK")"; fi
[ -f "$APK" ] || { echo "no APK at $APK; run ./build.sh first" >&2; exit 2; }

adb -s "$SERIAL" get-state >/dev/null || { echo "adb cannot reach $SERIAL" >&2; exit 2; }

# Identity check before ANY change.
HW="$(adb -s "$SERIAL" shell getprop ro.serialno | tr -d '\r')"
for refused in ${KEEPER_REFUSE_SERIALS:-}; do
  if [ "$HW" = "$refused" ]; then echo "REFUSED: $SERIAL is on KEEPER_REFUSE_SERIALS"; exit 3; fi
done
echo "target: $(adb -s "$SERIAL" shell getprop ro.product.model | tr -d '\r') (Android $(adb -s "$SERIAL" shell getprop ro.build.version.release | tr -d '\r'))"

echo "== install (keeps data on re-run) =="
adb -s "$SERIAL" install -r "$APK"

echo "== lift Android 15 restricted-settings guard (sideloaded install) =="
adb -s "$SERIAL" shell appops set "$PKG" ACCESS_RESTRICTED_SETTINGS allow

echo "== grant WRITE_SECURE_SETTINGS (lets the keeper set adb_wifi_enabled=1) =="
adb -s "$SERIAL" shell pm grant "$PKG" android.permission.WRITE_SECURE_SETTINGS

echo "== battery allowlist (survives reboot; keeps network + wakelocks in Doze) =="
adb -s "$SERIAL" shell dumpsys deviceidle whitelist +"$PKG"

echo "== background run + HyperOS/MIUI Autostart app-ops (harmless no-ops on other builds) =="
adb -s "$SERIAL" shell appops set "$PKG" RUN_ANY_IN_BACKGROUND allow
adb -s "$SERIAL" shell appops set "$PKG" 10007 allow   # boot completed
adb -s "$SERIAL" shell appops set "$PKG" 10008 allow   # autostart
adb -s "$SERIAL" shell appops set "$PKG" 10021 allow   # background activity start
adb -s "$SERIAL" shell appops set "$PKG" 10023 allow   # foreground service
adb -s "$SERIAL" shell appops set "$PKG" 10053 allow || true  # autostart switch (some builds only)

echo "== exact alarms (the 15-minute backstop) =="
adb -s "$SERIAL" shell appops set "$PKG" SCHEDULE_EXACT_ALARM allow

echo "== persist the app-op table =="
adb -s "$SERIAL" shell appops write-settings

echo "== start the foreground service =="
# KeeperService is exported=false, so adb cannot start it directly ("Requires permission not exported");
# the launcher activity forwards these extras to it.
if [ -n "$HUB_URL" ]; then
  adb -s "$SERIAL" shell am start -n "$PKG/.MainActivity" --es src setup --es hub "$HUB_URL" --es serial "$HW"
else
  adb -s "$SERIAL" shell am start -n "$PKG/.MainActivity" --es src setup --es serial "$HW"
fi

echo "== done. verify: =="
echo "   adb -s \"$SERIAL\" shell dumpsys deviceidle whitelist | grep $PKG"
echo "   adb -s \"$SERIAL\" shell settings get global adb_wifi_enabled        # 1"
echo "   adb -s \"$SERIAL\" shell run-as $PKG cat files/keeper.log             # not available on release builds; use logcat -s Keeper"
echo "   (then, on HyperOS/MIUI, set the app's battery mode to 'No restrictions' once)"
