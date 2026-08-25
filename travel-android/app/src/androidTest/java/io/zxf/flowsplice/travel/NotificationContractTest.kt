package io.zxf.flowsplice.travel

import android.Manifest
import android.app.Notification
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.Assume.assumeTrue
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class NotificationContractTest {
    @Test
    fun notificationUsesSystemStatusIcon() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val notification = flowSpliceNotificationBuilder(context, "test-channel").build()

        assertEquals(R.drawable.ic_stat_flowsplice, notification.smallIcon.resId)
        assertNull(notification.getLargeIcon())
        assertEquals(Notification.BADGE_ICON_SMALL, notification.badgeIconType)
        assertNotNull(context.getDrawable(R.drawable.ic_stat_flowsplice))
        assertNotNull(context.getDrawable(R.mipmap.ic_flowsplice_launcher))
        assertEquals(
            R.mipmap.ic_flowsplice_launcher,
            context.packageManager.getApplicationInfo(
                context.packageName,
                PackageManager.ApplicationInfoFlags.of(0),
            ).icon,
        )
        if (Build.VERSION.SDK_INT >= 37) {
            assertTrue(notification.extras.getBoolean(Notification.EXTRA_PREFER_SMALL_ICON))
        }
    }

    @Test
    fun appRequestsPermissionForBoundedScreenOffGracePeriod() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val packageInfo = context.packageManager.getPackageInfo(
            context.packageName,
            PackageManager.PackageInfoFlags.of(PackageManager.GET_PERMISSIONS.toLong()),
        )

        assertTrue(
            packageInfo.requestedPermissions.orEmpty().contains(Manifest.permission.WAKE_LOCK),
        )
    }

    @Test
    fun screenOffWakeLockExpiresAndCanBeReleasedEarly() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val lock = ScreenOffWakeLock(context, timeoutMillis = 500)

        lock.beginGracePeriod(sessionActive = false)
        assertFalse(lock.isHeld)
        lock.beginGracePeriod(sessionActive = true)
        assertTrue(lock.isHeld)
        Thread.sleep(300)
        lock.noteBusinessTraffic(sessionActive = true, screenOff = true)
        Thread.sleep(300)
        assertTrue("business traffic must restart the idle countdown", lock.isHeld)
        Thread.sleep(300)
        assertFalse(lock.isHeld)

        lock.beginGracePeriod(sessionActive = true)
        assertTrue(lock.isHeld)
        lock.noteBusinessTraffic(sessionActive = true, screenOff = false)
        assertTrue("screen-on traffic must not alter the existing lock", lock.isHeld)
        lock.release()
        assertFalse(lock.isHeld)
    }

    @Test
    fun productionScreenOffGraceExpiresAfterThreeIdleMinutes() {
        assumeTrue(
            "run explicitly because this acceptance check takes more than three minutes",
            InstrumentationRegistry.getArguments().getString("long_power_test") == "true",
        )
        val context = ApplicationProvider.getApplicationContext<Context>()
        val lock = ScreenOffWakeLock(context)

        lock.beginGracePeriod(sessionActive = true)
        assertTrue(lock.isHeld)
        Thread.sleep(ScreenOffWakeLock.DEFAULT_TIMEOUT_MILLIS + 5_000)
        assertFalse("the production grace lock exceeded its three-minute timeout", lock.isHeld)
    }
}
