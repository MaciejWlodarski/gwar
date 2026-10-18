plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

android {
    namespace = "app.gwar.android"
    compileSdk = 37

    defaultConfig {
        applicationId = "app.gwar.android"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }

    // Built by build-rust.sh: the core's native library and its Kotlin bindings.
    sourceSets.named("main") {
        jniLibs.directories.add("build/rust/jniLibs")
        kotlin.directories.add("build/rust/kotlin")
    }
}

// The Rust core is rebuilt (incrementally) before every build. `-Pgwar.skipRust`
// reuses the last one, e.g. while only Kotlin changes.
val buildRust = tasks.register<Exec>("buildRust") {
    description = "Builds crates/gwar-app for Android and generates its Kotlin bindings."
    workingDir = projectDir.parentFile
    commandLine("./build-rust.sh")
    enabled = !providers.gradleProperty("gwar.skipRust").isPresent
}
tasks.named("preBuild") { dependsOn(buildRust) }

dependencies {
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.ui.tooling.preview)
    implementation(libs.compose.material3)
    implementation(libs.compose.material.icons)
    implementation(libs.activity.compose)
    implementation(libs.lifecycle.viewmodel.compose)
    implementation(libs.lifecycle.runtime.compose)
    implementation(libs.coroutines.android)
    // UniFFI's Kotlin bindings call the native library through JNA.
    implementation(libs.jna) { artifact { type = "aar" } }
    debugImplementation(libs.compose.ui.tooling)
    testImplementation(libs.junit)
}
