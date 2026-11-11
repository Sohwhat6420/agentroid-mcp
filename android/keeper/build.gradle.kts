plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    // Was "com.agentroid.keeper"; 2.12 moves it under dev.agentroid.helper like the other modules (HF-25).
    namespace = "dev.agentroid.helper.keeper"
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
    // LinkExtension (the hook LinkService loads by class name) and LinkProto (the untrusted_bssid frame).
    implementation(project(":link"))
}
