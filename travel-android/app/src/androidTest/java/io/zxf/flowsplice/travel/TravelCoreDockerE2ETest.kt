package io.zxf.flowsplice.travel

import android.app.Notification
import android.app.NotificationManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.os.Build
import android.os.ParcelFileDescriptor
import android.system.Os
import android.system.OsConstants
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import kotlinx.coroutines.delay
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.assertTrue
import org.junit.Assert.assertFalse
import org.junit.Assert.assertEquals
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
        val connectivity = context.getSystemService(ConnectivityManager::class.java)
        val arguments = InstrumentationRegistry.getArguments()
        val relayAddress = arguments.getString("relay_address")
        val privateKeyPassword = arguments.getString("private_key_password")
        assumeTrue(
            "Docker enrollment arguments are required",
            relayAddress != null && privateKeyPassword != null,
        )
        TravelRepository.initialize(context)

        try {
            TravelRepository.enroll(
                context,
                travelId = "android-e2e-travel",
                homeId = "home-1",
                relay = requireNotNull(relayAddress),
                password = requireNotNull(privateKeyPassword),
            )
            val enrollment = awaitEnrollment("Android did not submit its enrollment request") {
                it.phase == EnrollmentPhase.WAITING_FOR_APPROVAL &&
                    !it.verificationCode.isNullOrEmpty()
            }
            println("FLOWSPLICE_ANDROID_VERIFICATION_CODE=${enrollment.verificationCode}")
            File(context.filesDir, "e2e-verification-code")
                .writeText(requireNotNull(enrollment.verificationCode))
            awaitSnapshot("Travel Core did not come online") { it.phase == TravelPhase.RUNNING && it.online }
            val service = awaitCatalog("Android did not receive the Home service catalog") { catalog ->
                catalog.homes
                    .firstOrNull { it.id == "home-1" }
                    ?.services
                    ?.firstOrNull { it.id == "tcp-echo" && it.protocol == "tcp" }
            }
            val mapping = TravelMapping(
                homeId = "home-1",
                serviceId = service.id,
                protocol = service.protocol,
                bind = "127.0.0.1:10080",
            )
            TravelRepository.upsert(context, mapping)
            awaitSnapshot("the local mapping was not activated") { snapshot ->
                snapshot.mappings.any { it == mapping }
            }

            assertEcho("android-foreground-roundtrip")
            val receiver = context.packageManager.getReceiverInfo(
                ComponentName(context, BootReceiver::class.java),
                0,
            )
            assertFalse("Boot receiver must not be exported", receiver.exported)
            val activity = Intent.makeMainActivity(ComponentName(context, MainActivity::class.java))
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            context.startActivity(activity)
            delay(1_000)
            InstrumentationRegistry.getInstrumentation().uiAutomation
                .executeShellCommand("input keyevent KEYCODE_HOME")
                .close()
            delay(2_000)
            val idleStarted = processSample()
            delay(IDLE_SAMPLE_MILLIS)
            val idleFinished = processSample()
            val elapsedSeconds = IDLE_SAMPLE_MILLIS / 1_000.0
            val clockTicks = Os.sysconf(OsConstants._SC_CLK_TCK).coerceAtLeast(1)
            val idleCpuPercent =
                (idleFinished.cpuTicks - idleStarted.cpuTicks).toDouble() /
                    clockTicks.toDouble() / elapsedSeconds * 100.0
            val idleWriteBytes =
                (idleFinished.writeBytes - idleStarted.writeBytes).coerceAtLeast(0)
            val screenOffGraceLockHeld =
                shell("dumpsys power").contains("$APP_ID:screen-off-grace")
            val performance = JSONObject()
                .put("sample_seconds", elapsedSeconds)
                .put("idle_cpu_percent_one_core", idleCpuPercent)
                .put("rss_kib", idleFinished.rssKiB)
                .put("threads", idleFinished.threads)
                .put("write_bytes", idleWriteBytes)
                .put("screen_off_grace_lock_held_while_screen_on", screenOffGraceLockHeld)
            File(context.filesDir, "e2e-performance.json").writeText(performance.toString())
            println("FLOWSPLICE_ANDROID_PERFORMANCE=$performance")
            assertTrue("idle CPU exceeded 10% of one core: $idleCpuPercent", idleCpuPercent < 10.0)
            assertTrue("resident memory exceeded 256 MiB", idleFinished.rssKiB < 256 * 1_024)
            assertTrue("app process used too many threads", idleFinished.threads < 64)
            assertTrue("idle persistence wrote more than 1 MiB", idleWriteBytes < 1_048_576)
            assertFalse(
                "screen-on idle must not retain the screen-off grace lock",
                screenOffGraceLockHeld,
            )

            assertTrue(TravelRepository.state.value.online)
            assertEcho("android-background-roundtrip")
            Socket().use { persistentFlow ->
                persistentFlow.connect(InetSocketAddress("127.0.0.1", 10080), 10_000)
                persistentFlow.soTimeout = 35_000
                assertEcho(persistentFlow, "android-persistent-before-switch")

                shell("svc data enable")
                shell("svc wifi disable")
                awaitTransport(connectivity, NetworkCapabilities.TRANSPORT_CELLULAR)
                assertEcho(persistentFlow, "android-persistent-cellular")

                shell("svc wifi enable")
                awaitTransport(connectivity, NetworkCapabilities.TRANSPORT_WIFI)
                assertEcho(persistentFlow, "android-persistent-wifi")

                shell("svc data disable")
                shell("svc wifi disable")
                awaitNoDefaultNetwork(connectivity)
                val delayedEcho = async(Dispatchers.IO) {
                    assertEcho(persistentFlow, "android-persistent-short-outage")
                }
                delay(6_000)
                shell("svc wifi enable")
                awaitTransport(connectivity, NetworkCapabilities.TRANSPORT_WIFI)
                delayedEcho.await()

                shell("dumpsys deviceidle whitelist +$APP_ID")
                shell("input keyevent KEYCODE_SLEEP")
                shell("dumpsys battery unplug")
                shell("dumpsys deviceidle force-idle")
                delay(20_000)
                shell("dumpsys deviceidle unforce")
                shell("dumpsys battery reset")
                shell("input keyevent KEYCODE_WAKEUP")
                awaitTransport(connectivity, NetworkCapabilities.TRANSPORT_WIFI)
                assertEcho(persistentFlow, "android-persistent-after-idle")
            }

            repeat(3) { cycle ->
                TravelRepository.stop(context)
                awaitSnapshot("Travel did not stop in cycle $cycle") {
                    it.phase == TravelPhase.STOPPED
                }
                assertTrue(
                    "mapping disappeared after Stop in cycle $cycle",
                    TravelRepository.state.value.mappings.contains(mapping),
                )
                TravelRepository.start(context)
                awaitSnapshot("Travel did not restart with its mapping in cycle $cycle") {
                    it.phase == TravelPhase.RUNNING && it.online && it.mappings.contains(mapping)
                }
                assertEcho("android-restart-$cycle")
            }

            TravelRepository.stop(context)
            delay(100)
            TravelRepository.start(context)
            awaitSnapshot("rapid Stop/Start did not restore Travel and its mapping") {
                it.phase == TravelPhase.RUNNING && it.online && it.mappings.contains(mapping)
            }
            assertEcho("android-rapid-restart")

            val traffic = awaitSnapshot("traffic counters were not updated") {
                it.uploadedBytes > 0 && it.downloadedBytes > 0
            }
            assertTrue(traffic.uploadedBytes >= "android-rapid-restart\n".length)
            assertTrue(traffic.downloadedBytes >= "android-rapid-restart\n".length)

            val notifications = context.getSystemService(NotificationManager::class.java)
                .activeNotifications
            assertTrue(notifications.isNotEmpty())
            val ongoingNotification = notifications.firstOrNull { notification ->
                    notification.notification.category == Notification.CATEGORY_SERVICE &&
                        notification.notification.flags and Notification.FLAG_ONGOING_EVENT != 0
                }
            assertTrue(ongoingNotification != null)
            val systemNotification = requireNotNull(ongoingNotification).notification
            assertEquals(R.drawable.ic_stat_flowsplice, systemNotification.smallIcon.resId)
            assertEquals(Notification.BADGE_ICON_SMALL, systemNotification.badgeIconType)
            if (Build.VERSION.SDK_INT >= 37) {
                assertTrue(systemNotification.extras.getBoolean(Notification.EXTRA_PREFER_SMALL_ICON))
            }
        } finally {
            shell("dumpsys deviceidle unforce")
            shell("dumpsys battery reset")
            shell("dumpsys deviceidle whitelist -$APP_ID")
            shell("svc data enable")
            shell("svc wifi enable")
            shell("input keyevent KEYCODE_WAKEUP")
            TravelRepository.stop(context)
            delay(2_000)
        }
    }

    private suspend fun awaitEnrollment(
        message: String,
        predicate: (EnrollmentSnapshot) -> Boolean,
    ): EnrollmentSnapshot {
        repeat(120) {
            val snapshot = TravelRepository.enrollment.value
            if (snapshot.phase == EnrollmentPhase.ERROR) {
                error(snapshot.error ?: message)
            }
            if (predicate(snapshot)) return snapshot
            delay(1_000)
        }
        error("$message: ${TravelRepository.enrollment.value}")
    }

    private suspend fun awaitSnapshot(
        message: String,
        predicate: (TravelSnapshot) -> Boolean,
    ): TravelSnapshot {
        repeat(180) {
            val snapshot = TravelRepository.state.value
            if (snapshot.phase == TravelPhase.ERROR) {
                error(snapshot.error ?: message)
            }
            if (predicate(snapshot)) return snapshot
            delay(1_000)
        }
        error("$message: ${TravelRepository.state.value}")
    }

    private suspend fun awaitCatalog(
        message: String,
        select: (TravelCatalog) -> CatalogService?,
    ): CatalogService {
        repeat(180) {
            select(TravelRepository.catalog.value)?.let { return it }
            delay(1_000)
        }
        error("$message: ${TravelRepository.catalog.value}")
    }

    private fun assertEcho(payload: String) {
        Socket().use { socket ->
            socket.connect(InetSocketAddress("127.0.0.1", 10080), 10_000)
            socket.soTimeout = 10_000
            assertEcho(socket, payload)
        }
    }

    private fun assertEcho(socket: Socket, payload: String) {
        socket.getOutputStream().write("$payload\n".toByteArray())
        socket.getOutputStream().flush()
        val response = socket.getInputStream().bufferedReader().readLine()
        assertTrue("unexpected echo response: $response", response?.endsWith(":$payload") == true)
    }

    private suspend fun awaitTransport(connectivity: ConnectivityManager, transport: Int) {
        repeat(60) {
            val network = connectivity.activeNetwork
            val capabilities = network?.let(connectivity::getNetworkCapabilities)
            if (capabilities?.hasTransport(transport) == true) {
                delay(1_000)
                return
            }
            delay(500)
        }
        error("emulator did not switch to transport $transport")
    }

    private suspend fun awaitNoDefaultNetwork(connectivity: ConnectivityManager) {
        repeat(60) {
            if (connectivity.activeNetwork == null) return
            delay(500)
        }
        error("emulator kept a default network during the outage")
    }

    private fun processSample(): ProcessSample {
        val pid = shell("pidof $APP_ID")
            .trim()
            .split(Regex("\\s+"))
            .firstOrNull()
            ?.toIntOrNull()
            ?: error("could not locate the Travel app process")
        val stat = shell("run-as $APP_ID cat /proc/$pid/stat").trim()
        val fields = stat.substring(stat.lastIndexOf(')') + 2).split(' ')
        val status = shell("run-as $APP_ID cat /proc/$pid/status")
        val io = shell("run-as $APP_ID cat /proc/$pid/io")
        return ProcessSample(
            cpuTicks = fields[11].toLong() + fields[12].toLong(),
            rssKiB = status.lineValue("VmRSS"),
            threads = status.lineValue("Threads").toInt(),
            writeBytes = io.lineValue("write_bytes"),
        )
    }

    private fun String.lineValue(name: String): Long = lineSequence()
        .first { it.startsWith("$name:") }
        .substringAfter(':')
        .trim()
        .substringBefore(' ')
        .toLong()

    private fun shell(command: String): String {
        val descriptor = InstrumentationRegistry.getInstrumentation().uiAutomation
            .executeShellCommand(command)
        return ParcelFileDescriptor.AutoCloseInputStream(descriptor).use {
            it.readBytes().toString(Charsets.UTF_8)
        }
    }

    private data class ProcessSample(
        val cpuTicks: Long,
        val rssKiB: Long,
        val threads: Int,
        val writeBytes: Long,
    )

    companion object {
        private const val APP_ID = "io.zxf.flowsplice.travel"
        private const val IDLE_SAMPLE_MILLIS = 20_000L
    }
}
