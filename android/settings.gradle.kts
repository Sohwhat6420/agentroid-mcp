pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "agentroid-android"

// :app carries the product flavours (full, helper, mic); the rest are library modules.
include(":app", ":core", ":a11y", ":ime", ":link", ":keeper")
include(":overlay")
include(":runner")
