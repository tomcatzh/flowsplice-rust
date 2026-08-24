package io.zxf.flowsplice.travel

import android.app.Notification
import android.app.NotificationManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.net.InetSocketAddress
import java.net.Socket

@RunWith(AndroidJUnit4::class)
class TravelCoreDockerE2ETest {
    @Test
    fun nativeCoreCompletesRoundTripsWhileAppIsInBackground() = runBlocking {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val arguments = InstrumentationRegistry.getArguments()
        val profilePath = arguments.getString("profile_path")
        val profilePassword = arguments.getString("profile_password")
        assumeTrue("Docker profile arguments are required", profilePath != null && profilePassword != null)
        File(requireNotNull(profilePath)).inputStream().use { input ->
            TravelProfile.install(context, input, requireNotNull(profilePassword))
        }
        TravelRepository.profileChanged(context)

        try {
            TravelRepository.start(context)
            awaitSnapshot("Travel Core did not come online") { it.phase == TravelPhase.RUNNING && it.online }
            val mapping = TravelMapping(
                homeId = "home-1",
                serviceId = "tcp-echo",
                protocol = "tcp",
                bind = "127.0.0.1:10080",
            )
            TravelRepository.upsert(context, mapping)
            awaitSnapshot("the local mapping was not activated") { snapshot ->
                snapshot.mappings.any { it == mapping }
            }

            assertEcho("android-foreground-roundtrip")
            val activity = Intent.makeMainActivity(ComponentName(context, MainActivity::class.java))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            context.startActivity(activity)
            delay(1_000)
            InstrumentationRegistry.getInstrumentation().uiAutomation
                .executeShellCommand("input keyevent KEYCODE_HOME")
                .close()
            delay(15_000)

            assertTrue(TravelRepository.state.value.online)
            assertEcho("android-background-roundtrip")
            val traffic = awaitSnapshot("traffic counters were not updated") {
                it.uploadedBytes > 0 && it.downloadedBytes > 0
            }
            assertTrue(traffic.uploadedBytes >= "android-background-roundtrip\n".length)
            assertTrue(traffic.downloadedBytes >= "android-background-roundtrip\n".length)

            val notifications = context.getSystemService(NotificationManager::class.java)
                .activeNotifications
            assertTrue(notifications.isNotEmpty())
            assertTrue(
                notifications.any { notification ->
                    notification.notification.category == Notification.CATEGORY_SERVICE &&
                        notification.notification.flags and Notification.FLAG_ONGOING_EVENT != 0
                },
            )
        } finally {
            TravelRepository.stop(context)
            delay(2_000)
        }
    }

    private suspend fun awaitSnapshot(
        message: String,
        predicate: (TravelSnapshot) -> Boolean,
    ): TravelSnapshot {
        repeat(90) {
            val snapshot = TravelRepository.state.value
            if (snapshot.phase == TravelPhase.ERROR) {
                error(snapshot.error ?: message)
            }
            if (predicate(snapshot)) return snapshot
            delay(1_000)
        }
        error("$message: ${TravelRepository.state.value}")
    }

    private fun assertEcho(payload: String) {
        Socket().use { socket ->
            socket.connect(InetSocketAddress("127.0.0.1", 10080), 10_000)
            socket.soTimeout = 10_000
            socket.getOutputStream().write("$payload\n".toByteArray())
            socket.getOutputStream().flush()
            val response = socket.getInputStream().bufferedReader().readLine()
            assertTrue("unexpected echo response: $response", response?.endsWith(":$payload") == true)
        }
    }
}
