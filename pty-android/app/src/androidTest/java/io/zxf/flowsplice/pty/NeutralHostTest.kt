package io.zxf.flowsplice.pty

import android.content.Context
import android.content.pm.PackageManager
import android.widget.TextView
import androidx.lifecycle.Lifecycle
import androidx.test.core.app.ActivityScenario
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.security.KeyStore
import org.json.JSONObject
import org.json.JSONArray

/** Tests deliberately require the neutral package, never a private installation. */
@RunWith(AndroidJUnit4::class)
class NeutralHostTest {
    private val context: Context get() = InstrumentationRegistry.getInstrumentation().targetContext

    @Before fun requireNeutralPackage() {
        assertFalse("Do not run neutral tests against a private package", context.assets.list("bootstrap").orEmpty().any { it == "business.json" || it == "homes.json" || it == "service-class.json" })
        assertFalse(context.filesDir.resolve("installation/travelagent.toml").exists())
    }

    @Test fun unconfiguredStartupSurvivesForegroundLifecycle() {
        ActivityScenario.launch(MainActivity::class.java).use { scenario ->
            scenario.onActivity { activity ->
                val content = activity.findViewById<android.view.ViewGroup>(android.R.id.content)
                assertTrue((content.getChildAt(0) as TextView).text.contains("尚未配置"))
            }
            scenario.moveToState(Lifecycle.State.CREATED)
            scenario.moveToState(Lifecycle.State.RESUMED)
            scenario.recreate()
            scenario.onActivity { activity ->
                val content = activity.findViewById<android.view.ViewGroup>(android.R.id.content)
                assertTrue((content.getChildAt(0) as TextView).text.contains("尚未配置"))
            }
        }
    }

    @Test fun passwordRoundtripUsesSeparateKeystoreAliasAndCiphertext() {
        val preferences = context.getSharedPreferences("pty-credentials", Context.MODE_PRIVATE)
        assertNull("Neutral package must have no saved password", preferences.getString("password", null))
        val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        val travelAliasExisted = store.containsAlias("flowsplice-travel-private-key")
        val password = "fixture-only-秘密-\u0000-password"
        try {
            PasswordStore.save(context, password)
            assertEquals(password, PasswordStore.load(context))
            assertNotEquals(password, preferences.getString("password", null))
            assertTrue(store.containsAlias("flowsplice-pty-private-key"))
            assertEquals(travelAliasExisted, store.containsAlias("flowsplice-travel-private-key"))
        } finally {
            assertTrue(preferences.edit().clear().commit())
        }
    }

    @Test fun deviceLabelSanitizesAndBoundsSystemName() {
        assertEquals("My Pixel · PTY", MainActivity.deviceLabel(" My Pixel ", "Model"))
        assertEquals("Model · PTY", MainActivity.deviceLabel("\u202e\n", "Model"))
        assertEquals("Android · PTY", MainActivity.deviceLabel(null, null))
        assertEquals("abc · PTY", MainActivity.deviceLabel("a\u2066b\u2069c\t", null))
        val bounded = MainActivity.deviceLabel("🖥".repeat(50), null)
        assertTrue(bounded.toByteArray(Charsets.UTF_8).size <= 64)
        assertTrue(bounded.endsWith(" · PTY"))
        assertFalse(bounded.contains('\ufffd'))
    }

    @Test fun homeCatalogRejectsDuplicateAndUnsafeIds() {
        fun entry(id: String) = JSONObject().put("id", id).put("name", "Home").put("platform", "linux").put("descriptor", JSONObject())
        fun catalog(vararg ids: String) = JSONObject().put("version", 1).put("homes", JSONArray(ids.map { entry(it) }))
        assertEquals(listOf("default", "vps-1"), MainActivity.parseHomes(catalog("default", "vps-1")).map { it.getString("id") })
        for (invalid in listOf(catalog("default", "default"), catalog("../escape"), catalog(), catalog(*Array(9) { "home-$it" }))) {
            assertTrue(runCatching { MainActivity.parseHomes(invalid) }.isFailure)
        }
    }

    @Test fun homeCredentialsAreIsolatedAndDefaultUsesLegacyPreference() {
        val legacy = context.getSharedPreferences("pty-credentials", Context.MODE_PRIVATE)
        val other = context.getSharedPreferences("pty-credentials-home-test-home", Context.MODE_PRIVATE)
        try {
            PasswordStore.save(context, "legacy-fixture")
            PasswordStore.save(context, "other-fixture", "test-home")
            assertEquals("legacy-fixture", PasswordStore.load(context, "default"))
            assertEquals("other-fixture", PasswordStore.load(context, "test-home"))
            assertNotNull(legacy.getString("password", null))
            assertNotEquals(legacy.getString("password", null), other.getString("password", null))
        } finally { legacy.edit().clear().commit(); other.edit().clear().commit() }
    }

    @Test fun serviceClassPasswordDoesNotReuseLegacyOrSameNamedHomeAccount() {
        val names = listOf("pty-credentials", "pty-credentials-home-service-class", "pty-credentials-service-class")
        try {
            PasswordStore.save(context, "old-default")
            PasswordStore.save(context, "old-home", "service-class")
            assertNull(PasswordStore.load(context, "service-class", true))
            PasswordStore.save(context, "new-class", "service-class", true)
            assertEquals("old-default", PasswordStore.load(context))
            assertEquals("old-home", PasswordStore.load(context, "service-class"))
            assertEquals("new-class", PasswordStore.load(context, "service-class", true))
        } finally { names.forEach { context.getSharedPreferences(it, Context.MODE_PRIVATE).edit().clear().commit() } }
    }

    @Test fun manifestHasNoBackgroundComponentsOrPrivilegedPermissions() {
        val flags = PackageManager.GET_SERVICES or PackageManager.GET_RECEIVERS or PackageManager.GET_PERMISSIONS
        val info = context.packageManager.getPackageInfo(context.packageName, PackageManager.PackageInfoFlags.of(flags.toLong()))
        assertTrue(info.services.isNullOrEmpty())
        assertTrue(info.receivers.isNullOrEmpty())
        val permissions = info.requestedPermissions.orEmpty().filterNot { it.endsWith(".DYNAMIC_RECEIVER_NOT_EXPORTED_PERMISSION") }.toSet()
        assertEquals(setOf("android.permission.INTERNET"), permissions)
        assertEquals(0, info.applicationInfo!!.flags and android.content.pm.ApplicationInfo.FLAG_ALLOW_BACKUP)
    }
}
