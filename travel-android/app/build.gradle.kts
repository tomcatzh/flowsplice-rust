plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

val releaseAbi = providers.gradleProperty("flowspliceReleaseAbi")
    .orElse("arm64-v8a")
    .get()
require(releaseAbi in setOf("arm64-v8a", "x86_64")) {
    "flowspliceReleaseAbi must be arm64-v8a or x86_64"
}

android {
    namespace = "io.zxf.flowsplice.travel"
    compileSdk {
        version = release(37)
    }

    defaultConfig {
        applicationId = "io.zxf.flowsplice.travel"
        minSdk = 34
        targetSdk = 37
        versionCode = 4
        versionName = "0.3.0"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    ndkVersion = "29.0.14206865"

    buildTypes {
        release {
            optimization {
                enable = true
            }
            ndk {
                abiFilters += releaseAbi
            }
            // The 0.3 artifact is for local sideloading. A publishing build must replace this
            // with a private, durable release signing configuration.
            signingConfig = signingConfigs.getByName("debug")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
    }
    buildFeatures {
        compose = true
    }
    sourceSets.named("main") {
        jniLibs.directories.add("build/generated/rustJniLibs")
    }
}

androidComponents {
    onVariants(selector().all()) { variant ->
        variant.outputs.forEach { output ->
            output.outputFileName.set("flowsplice-travel.apk")
        }
    }
}

val repositoryRoot = rootProject.projectDir.parentFile
val rustJniOutput = layout.buildDirectory.dir("generated/rustJniLibs")
val buildRustAndroid = tasks.register<Exec>("buildRustAndroid") {
    group = "build"
    description = "Builds the Rust Travel Core JNI libraries for Android"
    workingDir(repositoryRoot)
    commandLine(
        "bash",
        rootProject.projectDir.resolve("scripts/build-rust-android.sh").absolutePath,
        rustJniOutput.get().asFile.absolutePath,
    )
    inputs.files(
        repositoryRoot.resolve("Cargo.toml"),
        repositoryRoot.resolve("Cargo.lock"),
        repositoryRoot.resolve("rust-toolchain.toml"),
    )
    inputs.dir(repositoryRoot.resolve("crates"))
    inputs.dir(repositoryRoot.resolve("travelagent/web"))
    outputs.dir(rustJniOutput)
}

tasks.named("preBuild").configure {
    dependsOn(buildRustAndroid)
}

dependencies {
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.graphics)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    testImplementation(libs.junit)
    androidTestImplementation(platform(libs.androidx.compose.bom))
    androidTestImplementation(libs.androidx.compose.ui.test.junit4)
    androidTestImplementation(libs.androidx.espresso.core)
    androidTestImplementation(libs.androidx.junit)
    debugImplementation(libs.androidx.compose.ui.test.manifest)
    debugImplementation(libs.androidx.compose.ui.tooling)
}
