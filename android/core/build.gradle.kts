plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.agentroid.helper.core"
    compileSdk = 34
    defaultConfig { minSdk = 29 }
    // The audit test (ChecklistAuditTest) uses the JDK-only manifest merger that build-helper.sh also uses.
    // The tests are a hand-rolled runner (TestRunner), run by android/run-tests.sh; there is no JUnit offline.
    sourceSets.getByName("test").java.srcDir("../tools/src/main/java")
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}
