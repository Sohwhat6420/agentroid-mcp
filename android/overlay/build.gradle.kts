plugins {
    id("com.android.library")
}

// Task 2.15, Detector 2 (phone side): the alpha-0 accessibility overlay that records human touches with adb down,
// and draws the reclaim warning dot. The logic is in overlay/logic (plain Java, tested by run-overlay-tests.sh on the
// JVM); the Android glue is HumanTouchOverlay and AndroidTouchscreens.
android {
    namespace = "dev.agentroid.helper.overlay"
    compileSdk = 34
    defaultConfig { minSdk = 29 }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
