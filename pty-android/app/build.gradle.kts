import java.io.File

plugins { id("com.android.application") }
val privateBuild = providers.gradleProperty("flowsplicePrivateBuild").orNull == "true"
val repo = rootProject.projectDir.parentFile
fun outside(path: File) {
    val process = ProcessBuilder("python3", repo.resolve("scripts/private-travel-trust.py").path, "check-path", "--path", path.absolutePath).redirectErrorStream(true).start()
    process.inputStream.use { it.readBytes() }
    check(process.waitFor() == 0) { "Private build paths must be outside Git" }
}
val hasPrivateInputs = file("src/main/assets/bootstrap").listFiles()?.isNotEmpty() == true
check(!hasPrivateInputs || privateBuild) { "Private inputs require external private build mode" }
if (privateBuild) {
    listOf(repo, rootProject.projectDir, gradle.gradleUserHomeDir, layout.buildDirectory.get().asFile).forEach(::outside)
    outside(File(System.getenv("CARGO_TARGET_DIR") ?: error("CARGO_TARGET_DIR required")))
    check(!gradle.startParameter.isBuildScan && !gradle.startParameter.isConfigurationCacheRequested) { "Private build must disable scans/configuration cache" }
    gradle.startParameter.setBuildCacheEnabled(false)
    listOf("deployment-root.pub", "business.json").forEach { check(file("src/main/assets/bootstrap/$it").isFile) { "Missing private bootstrap input" } }
}
android {
    namespace = "io.zxf.flowsplice.pty"
    compileSdk { version = release(37) }
    defaultConfig { testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"; applicationId = "io.zxf.flowsplice.pty"; minSdk = 34; targetSdk = 37; versionCode = 1; versionName = "0.4.0" }
    ndkVersion = "29.0.14206865"
    compileOptions { sourceCompatibility = JavaVersion.VERSION_11; targetCompatibility = JavaVersion.VERSION_11 }
    if (privateBuild && System.getenv("FLOWSPLICE_PTY_DEBUG_KEYSTORE") != null) {
        val debugKey = File(System.getenv("FLOWSPLICE_PTY_DEBUG_KEYSTORE"))
        outside(debugKey)
        signingConfigs.getByName("debug").storeFile = debugKey
    }
    val signingPath = System.getenv("FLOWSPLICE_PTY_KEYSTORE")
    if (signingPath != null) {
        outside(File(signingPath))
        signingConfigs.create("privateRelease") {
            storeFile = File(signingPath)
            keyAlias = System.getenv("FLOWSPLICE_PTY_KEY_ALIAS") ?: error("Missing signing alias")
            storePassword = System.getenv("FLOWSPLICE_PTY_STORE_PASSWORD") ?: error("Missing store password")
            keyPassword = System.getenv("FLOWSPLICE_PTY_KEY_PASSWORD") ?: error("Missing key password")
        }
        buildTypes.getByName("release").signingConfig = signingConfigs.getByName("privateRelease")
    }
}
tasks.matching { it.name.contains("Release") }.configureEach {
    doFirst { check(System.getenv("FLOWSPLICE_PTY_KEYSTORE") != null) { "Release requires durable external signing" } }
}

dependencies {
    androidTestImplementation("androidx.test:core:1.7.0")
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
}
