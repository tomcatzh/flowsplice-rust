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

/** Tests deliberately require the neutral package, never a private installation. */
@RunWith(AndroidJUnit4::class)
class NeutralHostTest {
    private val context: Context get() = InstrumentationRegistry.getInstrumentation().targetContext

    @Before fun requireNeutralPackage() {
        assertFalse("Do not run neutral tests against a private package", context.assets.list("bootstrap").orEmpty().contains("business.json"))
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
