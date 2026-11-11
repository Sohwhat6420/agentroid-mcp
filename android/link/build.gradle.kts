plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.agentroid.helper.link"
    compileSdk = 34
    defaultConfig { minSdk = 29 }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {
    implementation(project(":core"))
    // The no-Gradle build dexes okhttp-jvm 5.5.0, okio-jvm 3.18.1 and kotlin-stdlib 2.1.21 from HELPER_LIBS (see build-helper.sh).
    // okhttp-jvm, not okhttp: on Android, okhttp 5.5.0 resolves to okhttp-android, which needs compileSdk 37 (AGP 8.5.2 tops out at 34).
    implementation("com.squareup.okhttp3:okhttp-jvm:5.5.0")
}
