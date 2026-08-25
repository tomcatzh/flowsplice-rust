package io.zxf.flowsplice.travel

import android.content.Context
import android.os.PowerManager

internal class ScreenOffWakeLock(
    context: Context,
    private val timeoutMillis: Long = DEFAULT_TIMEOUT_MILLIS,
) {
    private val wakeLock = context.getSystemService(PowerManager::class.java)
        .newWakeLock(
            PowerManager.PARTIAL_WAKE_LOCK,
            "${context.packageName}:screen-off-grace",
        )
        .apply { setReferenceCounted(false) }

    @Synchronized
    fun beginGracePeriod(sessionActive: Boolean) {
        if (sessionActive && !wakeLock.isHeld) {
            wakeLock.acquire(timeoutMillis)
        }
    }

    @Synchronized
    fun noteBusinessTraffic(sessionActive: Boolean, screenOff: Boolean) {
        if (!sessionActive || !screenOff) return
        if (wakeLock.isHeld) wakeLock.release()
        wakeLock.acquire(timeoutMillis)
    }

    @Synchronized
    fun release() {
        if (wakeLock.isHeld) wakeLock.release()
    }

    internal val isHeld: Boolean
        get() = wakeLock.isHeld

    companion object {
        const val DEFAULT_TIMEOUT_MILLIS = 3 * 60 * 1_000L
    }
}
