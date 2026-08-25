package io.zxf.flowsplice.travel

import org.json.JSONObject

internal data class NativeTravelStatusUpdate(
    val generation: Long,
    val snapshot: TravelSnapshot,
) {
    companion object {
        fun fromNative(json: String, enrolled: Boolean): NativeTravelStatusUpdate {
            val envelope = JSONObject(json)
            check(envelope.optBoolean("ok")) {
                envelope.optString("error", "Travel Core status wait failed")
            }
            val data = envelope.getJSONObject("data")
            val snapshotEnvelope = JSONObject()
                .put("ok", true)
                .put("data", data.getJSONObject("status"))
            return NativeTravelStatusUpdate(
                generation = data.optLong("generation"),
                snapshot = TravelSnapshot.fromNative(snapshotEnvelope.toString(), enrolled),
            )
        }
    }
}

internal data class TravelObservableState(
    val phase: TravelPhase,
    val online: Boolean,
    val enrolled: Boolean,
    val travelId: String,
    val activeFlows: Int,
    val uploadedBytes: Long,
    val downloadedBytes: Long,
    val relayCount: Int,
    val catalogGeneration: Long,
    val mappings: List<TravelMapping>,
    val error: String?,
)

internal fun TravelSnapshot.observableState(): TravelObservableState = TravelObservableState(
    phase = phase,
    online = online,
    enrolled = enrolled,
    travelId = travelId,
    activeFlows = activeFlows,
    uploadedBytes = uploadedBytes,
    downloadedBytes = downloadedBytes,
    relayCount = relayCount,
    catalogGeneration = catalogGeneration,
    mappings = mappings,
    error = error,
)

internal class StatusRefreshPolicy(
    nowMillis: Long,
    private val activeRefreshMillis: Long = 2_000,
    private val idleRefreshMillis: Long = 15_000,
    private val deepIdleRefreshMillis: Long = 30_000,
    private val offlineFastWindowMillis: Long = 30_000,
    private val deepIdleAfterMillis: Long = 120_000,
    private val businessActivityFastWindowMillis: Long = ScreenOffWakeLock.DEFAULT_TIMEOUT_MILLIS,
) {
    private var lastChangeMillis = nowMillis
    private var lastBusinessActivityMillis = nowMillis

    fun recordChange(nowMillis: Long) {
        lastChangeMillis = nowMillis
    }

    fun recordBusinessActivity(nowMillis: Long) {
        lastBusinessActivityMillis = nowMillis
        lastChangeMillis = nowMillis
    }

    fun nextTimeoutMillis(snapshot: TravelSnapshot, nowMillis: Long): Long {
        val unchangedFor = (nowMillis - lastChangeMillis).coerceAtLeast(0)
        val businessIdleFor = (nowMillis - lastBusinessActivityMillis).coerceAtLeast(0)
        if (
            snapshot.phase != TravelPhase.RUNNING ||
            snapshot.activeFlows > 0 && businessIdleFor < businessActivityFastWindowMillis
        ) {
            return activeRefreshMillis
        }
        if (!snapshot.online && unchangedFor < offlineFastWindowMillis) {
            return activeRefreshMillis
        }
        return if (unchangedFor < deepIdleAfterMillis) {
            idleRefreshMillis
        } else {
            deepIdleRefreshMillis
        }
    }
}

internal class DistinctNotificationGate {
    private var lastContentKey: String? = null

    @Synchronized
    fun recordForeground(contentKey: String) {
        lastContentKey = contentKey
    }

    @Synchronized
    fun accept(contentKey: String): Boolean {
        if (lastContentKey == contentKey) return false
        lastContentKey = contentKey
        return true
    }

    @Synchronized
    fun reset() {
        lastContentKey = null
    }
}
