package io.zxf.flowsplice.travel

import org.json.JSONObject

enum class TravelPhase {
    STOPPED,
    STARTING,
    RUNNING,
    STOPPING,
    ERROR,
}

enum class EnrollmentPhase {
    IDLE,
    PREPARING,
    WAITING_FOR_APPROVAL,
    INSTALLED,
    CANCELLED,
    ERROR,
}

data class EnrollmentSnapshot(
    val phase: EnrollmentPhase = EnrollmentPhase.IDLE,
    val travelId: String = "",
    val requestId: String? = null,
    val verificationCode: String? = null,
    val error: String? = null,
) {
    val active: Boolean
        get() = phase == EnrollmentPhase.PREPARING || phase == EnrollmentPhase.WAITING_FOR_APPROVAL

    companion object {
        fun fromNative(json: String): EnrollmentSnapshot {
            val envelope = JSONObject(json)
            if (!envelope.optBoolean("ok")) {
                return EnrollmentSnapshot(
                    phase = EnrollmentPhase.ERROR,
                    error = friendlyEnrollmentError(
                        envelope.optString("error", "Remote enrollment failed"),
                    ),
                )
            }
            val data = envelope.getJSONObject("data")
            fun optionalString(key: String): String? =
                if (data.isNull(key)) null else data.optString(key).takeIf(String::isNotEmpty)
            return EnrollmentSnapshot(
                phase = when (data.optString("phase")) {
                    "preparing" -> EnrollmentPhase.PREPARING
                    "waiting_for_approval" -> EnrollmentPhase.WAITING_FOR_APPROVAL
                    "installed" -> EnrollmentPhase.INSTALLED
                    "cancelled" -> EnrollmentPhase.CANCELLED
                    "error" -> EnrollmentPhase.ERROR
                    else -> EnrollmentPhase.IDLE
                },
                travelId = data.optString("travel_id"),
                requestId = optionalString("request_id"),
                verificationCode = optionalString("verification_code"),
                error = optionalString("error")?.let(::friendlyEnrollmentError),
            )
        }

        internal fun friendlyEnrollmentError(error: String): String {
            val lower = error.lowercase()
            return if (
                "relay discovery tls handshake failed" in lower ||
                "peer closed connection without sending tls close_notify" in lower
            ) {
                "Could not establish a secure enrollment connection. Check that this is the " +
                    "Relay management port and that the Relay is running the same 0.3 build as this app."
            } else {
                error
            }
        }
    }
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
    val enrolled: Boolean = false,
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
        fun fromNative(json: String, enrolled: Boolean): TravelSnapshot {
            val envelope = JSONObject(json)
            if (!envelope.optBoolean("ok")) {
                return TravelSnapshot(
                    phase = TravelPhase.ERROR,
                    enrolled = enrolled,
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
                enrolled = enrolled,
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
