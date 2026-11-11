plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    // Task 2.14: the on-phone step runner. Plain Java, Android-free logic (tested on the JVM by run-runner-tests.sh).
    namespace = "dev.agentroid.helper.runner"
    compileSdk = 34
    defaultConfig { minSdk = 29 }
    // The hand-written safe list ships inside the library as a Java resource (/safe-classes.json), byte-identical to
    // the file the hub embeds (crates/segments, include_str!). One file, two readers.
    sourceSets.getByName("main").resources.srcDir("../../runner")
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {
    // core: Json and Clock. link: TrustStore (the pinned hub key, the serial, the epoch; read only) and LinkCrypto.
    implementation(project(":core"))
    implementation(project(":link"))
}
