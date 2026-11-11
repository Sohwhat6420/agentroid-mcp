plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.agentroid.helper.app"
    compileSdk = 34

    defaultConfig {
        // One applicationId and one signing key for every flavour.
        // Was "com.agentroid" in the scaffold; HF-25 fixes the helper's package at dev.agentroid.helper.
        // compileSdk/targetSdk stay at the scaffold's 34 here: the no-Gradle build uses 36, and moving Gradle to 36
        // needs a newer AGP than 8.5.2. Not run, since there is no Gradle on this machine.
        applicationId = "dev.agentroid.helper"
        minSdk = 29
        targetSdk = 34
        versionCode = 1
        versionName = "0.0.0"
    }

    flavorDimensions += "edition"
    productFlavors {
        create("full") { dimension = "edition" }
        create("helper") { dimension = "edition" }
        create("mic") { dimension = "edition" }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

dependencies {
    // Shared base for every flavour.
    implementation(project(":core"))
    implementation(project(":link"))
    // The on-phone step runner (2.14): RunnerWiring builds its verifier over the link's HubStore.
    implementation(project(":runner"))

    // helper: the accessibility service and the IME.
    "fullImplementation"(project(":a11y"))
    "fullImplementation"(project(":ime"))
    "helperImplementation"(project(":a11y"))
    "helperImplementation"(project(":ime"))

    // full only: the keeper (WRITE_SECURE_SETTINGS), never on a read_only device.
    "fullImplementation"(project(":keeper"))
    // One app (his ruling, 11 Nov 2026): the human-touch overlay ships in full too.
    "fullImplementation"(project(":overlay"))
}
