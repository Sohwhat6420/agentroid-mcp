#!/usr/bin/env bash
# Builds out/helper.apk, the one Agentroid app (package dev.agentroid.helper, min SDK 29, target 36, DEBUG build), from
# :core :a11y :ime :link :runner :keeper :overlay :app, with no Gradle and no Kotlin: aapt2 + javac + d8 + zipalign + apksigner.
# Two safety choices:
#   - d8 and apksigner run as `java -cp/-jar` on the SDK jars, never the .bat shims (no console windows);
#   - OkHttp, Okio and kotlin-stdlib are put on the javac classpath and dexed in. They are READ from the folder in
#     HELPER_LIBS (default ./libs, git-ignored; nothing is committed): okhttp-jvm 5.5.0, okio-jvm 3.18.1 and
#     kotlin-stdlib 2.1.21 from Maven Central. The CI job android-helper fetches them and checks their SHA-256s;
#     copy that step to build locally.
# Differences from S4: the manifest is merged from the module manifests by tools/ManifestMerge (JDK only), and the
# resources come from each module's res/ folder. The APK is debug-signed with a throwaway key made on the first build in
# out/ (git-ignored, never committed). It never runs adb and never touches a device.
# Needs JDK 17 and the Android SDK (platforms;android-36, build-tools;36.0.0) under ANDROID_HOME or %LOCALAPPDATA%\Android\Sdk.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"; command -v cygpath >/dev/null && HERE="$(cygpath -m "$HERE")"
SDK="${ANDROID_HOME:-${LOCALAPPDATA:-$HOME/AppData/Local}/Android/Sdk}"
BT="$SDK/build-tools/36.0.0"
ANDROID_JAR="$SDK/platforms/android-36/android.jar"
OUT="$HERE/out"
LIBS_DIR="${HELPER_LIBS:-$HERE/libs}"
[ -d "$LIBS_DIR" ] && command -v cygpath >/dev/null && LIBS_DIR="$(cygpath -m "$LIBS_DIR")"

[ -f "$ANDROID_JAR" ] || { echo "missing $ANDROID_JAR"; exit 1; }
[ -d "$BT" ] || { echo "missing $BT"; exit 1; }
[ -d "$LIBS_DIR" ] || { echo "missing $LIBS_DIR (OkHttp, Okio, kotlin-stdlib jars)"; exit 1; }
LIBS=$(find "$LIBS_DIR" -name '*.jar' | tr '\n' ';')

rm -rf "$OUT/classes" "$OUT/dex" "$OUT/res" "$OUT/tools" "$OUT"/*.apk "$OUT/AndroidManifest.xml"
mkdir -p "$OUT/classes" "$OUT/dex" "$OUT/res" "$OUT/tools"

# 1. Merge the module manifests (stand-in for Gradle's merger).
javac --release 8 -encoding UTF-8 -d "$OUT/tools" $(find "$HERE/tools/src/main/java" -name '*.java')
java -cp "$OUT/tools" dev.agentroid.helper.tools.ManifestMerge "$HERE" "$OUT/AndroidManifest.xml"

# 2. Resources: one zip per module (flat file names collide across modules otherwise).
for m in a11y ime app; do
    "$BT/aapt2" compile --dir "$HERE/$m/src/main/res" -o "$OUT/res/$m.zip"
done
"$BT/aapt2" link -o "$OUT/base.apk" -I "$ANDROID_JAR" --manifest "$OUT/AndroidManifest.xml" \
    --min-sdk-version 29 --target-sdk-version 36 --debug-mode "$OUT"/res/*.zip

# 3. Java 8 source for every helper module, against android-36 and the three jars.
# :runner joined on 20 Oct (:app builds RunnerWiring). Its /safe-classes.json resource is not packed here;
# nothing loads it until a SegmentRunner is built, which is CP-D work.
javac -encoding UTF-8 -source 8 -target 8 -Xlint:-options -bootclasspath "$ANDROID_JAR" \
    -classpath "$BT/core-lambda-stubs.jar;$LIBS" -d "$OUT/classes" \
    $(find "$HERE/core/src/main/java" "$HERE/a11y/src/main/java" "$HERE/ime/src/main/java" "$HERE/link/src/main/java" "$HERE/runner/src/main/java" "$HERE/keeper/src/main/java" "$HERE/overlay/src/main/java" "$HERE/app/src/main/java" -name '*.java')

# 4. Dex (helper classes plus the three jars), add to the APK, align, sign.
java -cp "$BT/lib/d8.jar" com.android.tools.r8.D8 --release --min-api 29 --lib "$ANDROID_JAR" --output "$OUT/dex" \
    $(find "$OUT/classes" -name '*.class') $(find "$LIBS_DIR" -name '*.jar')
(cd "$OUT/dex" && for d in classes*.dex; do "$BT/aapt" add "$OUT/base.apk" "$d" >/dev/null; done)
"$BT/zipalign" -f -p 4 "$OUT/base.apk" "$OUT/aligned.apk"

# Throwaway debug key, made on first build and never committed (out/ is git-ignored).
KS="$OUT/debug.keystore"
[ -f "$KS" ] || keytool -genkeypair -keystore "$KS" -storepass helperdebug -keypass helperdebug -alias helperdebug \
    -keyalg RSA -keysize 2048 -validity 365 -dname "CN=Agentroid helper throwaway debug" >/dev/null 2>&1
java -jar "$BT/lib/apksigner.jar" sign --ks "$KS" --ks-pass pass:helperdebug --out "$OUT/helper.apk" "$OUT/aligned.apk"
rm -f "$OUT/base.apk" "$OUT/aligned.apk"

# 5. Read the result back: signature, package, SDK levels, permissions, components.
# (aapt2 for the read-back: the old aapt cannot resolve the framework icon reference and exits 1.)
java -jar "$BT/lib/apksigner.jar" verify --verbose "$OUT/helper.apk" | head -n 8
"$BT/aapt2" dump badging "$OUT/helper.apk" | grep -E "^(package|sdkVersion|minSdkVersion|targetSdkVersion|application-debuggable|launchable-activity)"
"$BT/aapt2" dump permissions "$OUT/helper.apk" | grep -E "^uses-permission"
echo "built $OUT/helper.apk ($(wc -c < "$OUT/helper.apk") bytes)"
