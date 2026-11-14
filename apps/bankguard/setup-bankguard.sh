#!/usr/bin/env bash
# Install and arm the bank guard on one phone, from a computer that already has adb access to it.
#
# Usage:  ./setup-bankguard.sh <SERIAL> [BANKS]
#   SERIAL  the adb device id, exactly as `adb devices` prints it
#   BANKS   optional comma-separated package list; replaces the built-in Singapore list. Leave it out to keep the
#           built-in list; the script prints the bank-like apps it finds so you can check.
#
# Environment:
#   BANKGUARD_REFUSE_SERIALS  space-separated hardware serials (ro.serialno) this script must never touch
#   BANKGUARD_APK             path to the APK (default: out/bankguard.apk next to this script)
#   ADB                       the adb to use (default: adb on PATH)
#
# Every adb line takes -s "$SERIAL"; there is never a bare `adb`, so it cannot touch another device.
# What it changes on the phone: installs the app; grants WRITE_SECURE_SETTINGS, Usage access and notifications to
# it; exempts it from battery optimisation (so the poll is not frozen); starts it. Nothing else.
set -euo pipefail
export MSYS_NO_PATHCONV=1

SERIAL="${1:?usage: setup-bankguard.sh <SERIAL> [BANKS]}"
BANKS="${2:-}"
PKG="dev.agentroid.bankguard"
ADB="${ADB:-adb}"
APK="${BANKGUARD_APK:-$(cd "$(dirname "$0")" && pwd)/out/bankguard.apk}"
if command -v cygpath >/dev/null 2>&1; then APK="$(cygpath -m "$APK")"; fi
[ -f "$APK" ] || { echo "no APK at $APK; run ./build.sh first" >&2; exit 2; }
a() { "$ADB" -s "$SERIAL" "$@"; }

a get-state >/dev/null || { echo "adb cannot reach $SERIAL" >&2; exit 2; }
HW="$(a shell getprop ro.serialno | tr -d '\r')"
for refused in ${BANKGUARD_REFUSE_SERIALS:-}; do
  if [ "$HW" = "$refused" ]; then echo "REFUSED: $SERIAL is on BANKGUARD_REFUSE_SERIALS"; exit 3; fi
done
echo "target: $(a shell getprop ro.product.model | tr -d '\r') (Android $(a shell getprop ro.build.version.release | tr -d '\r'), SDK $(a shell getprop ro.build.version.sdk | tr -d '\r'))"

echo "== install (keeps data on re-run) =="
a install -r "$APK"

echo "== grants =="
a shell pm grant "$PKG" android.permission.WRITE_SECURE_SETTINGS
a shell appops set "$PKG" ACCESS_RESTRICTED_SETTINGS allow 2>/dev/null || true   # Android 13+ sideload guard
a shell appops set "$PKG" GET_USAGE_STATS allow
a shell pm grant "$PKG" android.permission.POST_NOTIFICATIONS 2>/dev/null || true
a shell dumpsys deviceidle whitelist +"$PKG" >/dev/null

echo "== check the grants took =="
WSS="$(a shell dumpsys package "$PKG" | tr -d '\r' | grep -c 'android.permission.WRITE_SECURE_SETTINGS: granted=true' || true)"
USE="$(a shell appops get "$PKG" GET_USAGE_STATS | tr -d '\r')"
echo "  WRITE_SECURE_SETTINGS granted: $([ "$WSS" -ge 1 ] && echo yes || echo NO)"
echo "  usage access: $USE"
[ "$WSS" -ge 1 ] || { echo "WRITE_SECURE_SETTINGS did not take. On Xiaomi/HyperOS turn on 'USB debugging (Security settings)' first." >&2; exit 4; }

echo "== bank-like apps on this phone (check the list covers yours) =="
a shell pm list packages | tr -d '\r' | sed 's/^package://' \
  | grep -iE 'dbs|posb|paylah|ocbc|uob|sg\.ndi|singpass|trustbank|gxs|maribank|citi|hsbc|standardchartered|maybank|cimb|bank' \
  | sed 's/^/  /' || echo "  (none found)"

if [ -n "$BANKS" ]; then
  echo "== set the list =="
  a shell am broadcast -n "$PKG/.ConfigReceiver" -a dev.agentroid.bankguard.SET --es banks "$BANKS" | tr -d '\r' | tail -1
fi

echo "== start =="
a shell am start-foreground-service -n "$PKG/.GuardService" >/dev/null
sleep 2
a shell dumpsys activity service "$PKG/.GuardService" | tr -d '\r' | sed -n '/held=/,$p' | head -6
cat <<'EOF'

Done. On Xiaomi/HyperOS also set the app's battery mode to "No restrictions" and Autostart on.
Status any time:   adb -s <SERIAL> shell dumpsys activity service dev.agentroid.bankguard/.GuardService
Remove:            adb -s <SERIAL> uninstall dev.agentroid.bankguard   (put debugging back first if it is held)
EOF
