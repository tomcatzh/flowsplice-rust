import java.io.File

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
}

private fun requireOutsideGitTree(description: String, path: File) {
    // Share the packaging helper's lexical, symlink, worktree, and Git metadata checks.
    val helper = rootProject.projectDir.parentFile.resolve("scripts/private-travel-trust.py")
    val process = ProcessBuilder(
        "python3", helper.absolutePath, "check-path", "--path", path.absolutePath,
    ).redirectErrorStream(true).start()
    process.inputStream.use { it.readBytes() }
    check(process.waitFor() == 0) {
        "$description must be outside Git for a private deployment-root build"
    }
}

private data class ExternalReleaseSigning(
    val keystore: File,
    val keyAlias: String,
    val storePassword: String,
    val keyPassword: String,
)

val repositoryRoot = rootProject.projectDir.parentFile
val deploymentRootProperty = providers.gradleProperty("flowspliceDeploymentRootFile").orNull
val releaseSigningProperties = listOf(
    "flowspliceReleaseKeystoreFile",
    "flowspliceReleaseKeyAlias",
    "flowspliceReleaseStorePasswordEnv",
    "flowspliceReleaseKeyPasswordEnv",
)
val configuredReleaseSigningProperties = releaseSigningProperties.filter {
    !providers.gradleProperty(it).orNull.isNullOrBlank()
}

fun externalFileProperty(name: String): File {
    val configured = providers.gradleProperty(name).orNull
        ?.takeIf(String::isNotBlank)
        ?: error("$name is required")
    val file = File(configured)
    require(file.isAbsolute) { "$name must be an absolute path" }
    require(file.isFile) { "$name must name an existing regular file" }
    requireOutsideGitTree(name, file)
    return file.canonicalFile
}

fun passwordFromEnvironmentReference(name: String): String {
    val environmentName = providers.gradleProperty(name).orNull
        ?.takeIf(String::isNotBlank)
        ?: error("$name is required")
    require(environmentName.matches(Regex("[A-Za-z_][A-Za-z0-9_]*"))) {
        "$name must name an environment variable"
    }
    return System.getenv(environmentName)
        ?.takeIf(String::isNotEmpty)
        ?: error("$name refers to a missing or empty environment variable")
}

val deploymentRootFile = deploymentRootProperty?.let {
    val file = File(it)
    require(file.isAbsolute) { "flowspliceDeploymentRootFile must be an absolute path" }
    require(file.isFile) { "flowspliceDeploymentRootFile must name an existing regular file" }
    requireOutsideGitTree("flowspliceDeploymentRootFile", file)
    file.canonicalFile
}
private val externalReleaseSigning = when {
    configuredReleaseSigningProperties.isEmpty() -> null
    configuredReleaseSigningProperties.size != releaseSigningProperties.size -> error(
        "Release signing must provide all external signing properties or none",
    )
    else -> ExternalReleaseSigning(
        keystore = externalFileProperty("flowspliceReleaseKeystoreFile"),
        keyAlias = providers.gradleProperty("flowspliceReleaseKeyAlias").orNull
            ?.takeIf(String::isNotBlank)
            ?: error("flowspliceReleaseKeyAlias is required"),
        storePassword = passwordFromEnvironmentReference("flowspliceReleaseStorePasswordEnv"),
        keyPassword = passwordFromEnvironmentReference("flowspliceReleaseKeyPasswordEnv"),
    )
}

if (deploymentRootFile != null) {
    check(!gradle.startParameter.isBuildScan) {
        "A private deployment-root build must not run with a build scan"
    }
    check(!gradle.startParameter.isConfigurationCacheRequested) {
        "A private deployment-root build must not use the configuration cache"
    }
    gradle.startParameter.setNoBuildScan(true)
    gradle.startParameter.setBuildCacheEnabled(false)
    requireOutsideGitTree("Gradle project directory", rootProject.projectDir)
    requireOutsideGitTree("repository root", repositoryRoot)
    requireOutsideGitTree("Android build directory", layout.buildDirectory.get().asFile)
    requireOutsideGitTree("Gradle user home", gradle.gradleUserHomeDir)
    val cargoTargetDirectory = System.getenv("CARGO_TARGET_DIR")
        ?.takeIf(String::isNotBlank)
        ?: error("CARGO_TARGET_DIR must be set for a private deployment-root build")
    val cargoTarget = File(cargoTargetDirectory)
    require(cargoTarget.isAbsolute) { "CARGO_TARGET_DIR must be an absolute path" }
    requireOutsideGitTree("CARGO_TARGET_DIR", cargoTarget)
    check(externalReleaseSigning != null) {
        "A private deployment-root build requires durable external release signing"
    }
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
        versionCode = 14
        versionName = "0.4.0"

        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    ndkVersion = "29.0.14206865"

    if (externalReleaseSigning != null) {
        signingConfigs.create("externalRelease") {
            storeFile = externalReleaseSigning.keystore
            keyAlias = externalReleaseSigning.keyAlias
            storePassword = externalReleaseSigning.storePassword
            keyPassword = externalReleaseSigning.keyPassword
        }
    }
    buildTypes {
        release {
            optimization {
                enable = true
            }
            ndk {
                abiFilters += releaseAbi
            }
            signingConfig = if (externalReleaseSigning == null) {
                signingConfigs.getByName("debug")
            } else {
                signingConfigs.getByName("externalRelease")
            }
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
        if (deploymentRootFile != null) {
            assets.directories.add("build/generated/assets")
        }
    }
}

androidComponents {
    onVariants(selector().all()) { variant ->
        variant.outputs.forEach { output ->
            output.outputFileName.set("flowsplice-travel.apk")
        }
    }
}

val rustJniOutput = layout.buildDirectory.dir("generated/rustJniLibs")
val generatedDeploymentRootAssets = layout.buildDirectory.dir("generated/assets")
val stagedDeploymentRootAsset = generatedDeploymentRootAssets.map {
    it.file("bootstrap/deployment-root.pub")
}
val stageDeploymentRootAsset = deploymentRootFile?.let { rootFile ->
    tasks.register<Copy>("stageDeploymentRootAsset") {
        group = "build"
        description = "Stages the external deployment root for private APK packaging"
        from(rootFile)
        into(generatedDeploymentRootAssets.map { it.dir("bootstrap") })
        rename { "deployment-root.pub" }
        inputs.file(rootFile)
        outputs.file(stagedDeploymentRootAsset)
        outputs.cacheIf("private deployment roots must not enter a build cache") { false }
        doFirst {
            requireOutsideGitTree("flowspliceDeploymentRootFile", rootFile.canonicalFile)
            requireOutsideGitTree("staged deployment root", stagedDeploymentRootAsset.get().asFile)
        }
    }
}
stageDeploymentRootAsset?.let { stageTask ->
    tasks.configureEach {
        if (name.startsWith("merge") && name.endsWith("Assets")) {
            dependsOn(stageTask)
        }
    }
}
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
    inputs.dir(repositoryRoot.resolve("internal"))
    inputs.dir(repositoryRoot.resolve("travel-android/rust"))
    inputs.dir(repositoryRoot.resolve("travelagent/web"))
    outputs.dir(rustJniOutput)
}

tasks.named("preBuild").configure {
    dependsOn(buildRustAndroid)
    stageDeploymentRootAsset?.let { stageTask -> dependsOn(stageTask) }
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
