#!/usr/bin/env bash
# Builds out/keeper.apk. No Gradle: aapt2 + javac + d8 + zipalign + apksigner.
# No libs/ (the app uses only android.jar: HttpURLConnection + Settings + NsdManager), so nothing extra is dexed.
# Needs JDK 17 and the Android SDK (platforms;android-36, build-tools;36.0.0). Set ANDROID_HOME if the SDK is
# not in the default location.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"; command -v cygpath >/dev/null && HERE="$(cygpath -m "$HERE")"
SDK="${ANDROID_HOME:-${LOCALAPPDATA:-$HOME/AppData/Local}/Android/Sdk}"
BT="$SDK/build-tools/36.0.0"
ANDROID_JAR="$SDK/platforms/android-36/android.jar"
OUT="$HERE/out"
rm -rf "$OUT/classes" "$OUT/dex" "$OUT/res" "$OUT"/*.apk
mkdir -p "$OUT/classes" "$OUT/dex" "$OUT/res"

"$BT/aapt2" compile --dir "$HERE/res" -o "$OUT/res"
"$BT/aapt2" link -o "$OUT/base.apk" -I "$ANDROID_JAR" --manifest "$HERE/AndroidManifest.xml" \
    --min-sdk-version 29 --target-sdk-version 36 --debug-mode "$OUT"/res/*.flat

javac -encoding UTF-8 -source 8 -target 8 -Xlint:-options -bootclasspath "$ANDROID_JAR" \
    -classpath "$BT/core-lambda-stubs.jar" -d "$OUT/classes" $(find "$HERE/src" -name '*.java')

java -cp "$BT/lib/d8.jar" com.android.tools.r8.D8 --release --min-api 29 --lib "$ANDROID_JAR" --output "$OUT/dex" \
    $(find "$OUT/classes" -name '*.class')

(cd "$OUT/dex" && for d in classes*.dex; do "$BT/aapt" add "$OUT/base.apk" "$d" >/dev/null; done)
"$BT/zipalign" -f -p 4 "$OUT/base.apk" "$OUT/aligned.apk"

# Signing key: lives OUTSIDE the repo (default ~/.keys), is made on first build with a random password, and is
# never committed. Override with KEEPER_KEYSTORE / KEEPER_KS_PASS_FILE. Keep a backup: a different key means
# `adb install -r` over an older build is refused.
KEYS="${KEEPER_KEYS_DIR:-$HOME/.keys}"
KS="${KEEPER_KEYSTORE:-$KEYS/agentroid-keeper.keystore}"
PASSFILE="${KEEPER_KS_PASS_FILE:-$KEYS/agentroid-keeper.pass}"
if [ ! -f "$KS" ]; then
    mkdir -p "$(dirname "$KS")" "$(dirname "$PASSFILE")"
    ( umask 077; head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$PASSFILE" )
    keytool -genkeypair -keystore "$KS" -storepass "$(cat "$PASSFILE")" -keypass "$(cat "$PASSFILE")" -alias keeper \
        -keyalg RSA -keysize 2048 -validity 9125 -dname "CN=Agentroid keeper" >/dev/null 2>&1
    echo "made a new signing key at $KS (password in $PASSFILE); back both up"
fi
# Java on Windows cannot read Git Bash /c/... paths.
if command -v cygpath >/dev/null 2>&1; then KS="$(cygpath -m "$KS")"; PASSFILE="$(cygpath -m "$PASSFILE")"; fi
java -jar "$BT/lib/apksigner.jar" sign --ks "$KS" --ks-pass "file:$PASSFILE" --out "$OUT/keeper.apk" "$OUT/aligned.apk"
rm -f "$OUT/base.apk" "$OUT/aligned.apk"
echo "built $OUT/keeper.apk ($(wc -c < "$OUT/keeper.apk") bytes)"
