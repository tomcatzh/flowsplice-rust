package io.zxf.flowsplice.travel

import org.json.JSONObject

enum class TravelPhase {
    STOPPED,
    STARTING,
    RUNNING,
    STOPPING,
    ERROR,
}

data class TravelMapping(
    val homeId: String,
    val serviceId: String,
    val protocol: String,
    val bind: String,
)

data class TravelSnapshot(
    val phase: TravelPhase = TravelPhase.STOPPED,
    val online: Boolean = false,
    val profileInstalled: Boolean = false,
    val travelId: String = "Travel",
    val uptimeSeconds: Long = 0,
    val activeFlows: Int = 0,
    val uploadedBytes: Long = 0,
    val downloadedBytes: Long = 0,
    val relayCount: Int = 0,
    val mappings: List<TravelMapping> = emptyList(),
    val error: String? = null,
) {
    companion object {
        fun fromNative(json: String, profileInstalled: Boolean): TravelSnapshot {
            val envelope = JSONObject(json)
            if (!envelope.optBoolean("ok")) {
                return TravelSnapshot(
                    phase = TravelPhase.ERROR,
                    profileInstalled = profileInstalled,
                    error = envelope.optString("error", "Travel Core failed"),
                )
            }
            val data = envelope.getJSONObject("data")
            val mappingsJson = data.optJSONArray("mappings")
            val mappings = buildList {
                if (mappingsJson != null) {
                    for (index in 0 until mappingsJson.length()) {
                        val mapping = mappingsJson.getJSONObject(index)
                        add(
                            TravelMapping(
                                homeId = mapping.getString("home_id"),
                                serviceId = mapping.getString("service_id"),
                                protocol = mapping.getString("protocol"),
                                bind = mapping.getString("bind"),
                            ),
                        )
                    }
                }
            }
            return TravelSnapshot(
                phase = TravelPhase.RUNNING,
                online = data.optBoolean("online"),
                profileInstalled = profileInstalled,
                travelId = data.optString("travel_id", "Travel"),
                uptimeSeconds = data.optLong("uptime_secs"),
                activeFlows = data.optInt("active_flows"),
                uploadedBytes = data.optLong("session_uploaded_bytes"),
                downloadedBytes = data.optLong("session_downloaded_bytes"),
                relayCount = data.optJSONArray("active_relays")?.length() ?: 0,
                mappings = mappings,
            )
        }
    }
}

fun TravelMapping.toNativeJson(): String = JSONObject()
    .put("home_id", homeId)
    .put("service_id", serviceId)
    .put("protocol", protocol)
    .put("bind", bind)
    .toString()
