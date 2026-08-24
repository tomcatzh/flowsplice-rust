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

data class CatalogService(
    val id: String,
    val alias: String,
    val protocol: String,
) {
    val key: String
        get() = "$id/$protocol"

    val displayName: String
        get() = if (alias.isBlank() || alias == id) id else "$alias ($id)"
}

data class CatalogHome(
    val id: String,
    val alias: String,
    val services: List<CatalogService>,
) {
    val displayName: String
        get() = if (alias.isBlank() || alias == id) id else "$alias ($id)"
}

data class TravelCatalog(
    val generation: Long = 0,
    val homes: List<CatalogHome> = emptyList(),
) {
    companion object {
        fun fromNative(json: String): TravelCatalog {
            val envelope = JSONObject(json)
            check(envelope.optBoolean("ok")) {
                envelope.optString("error", "Could not load the Home service catalog")
            }
            val data = envelope.getJSONObject("data")
            val homesJson = data.optJSONArray("homes")
            val homes = buildList {
                if (homesJson != null) {
                    for (homeIndex in 0 until homesJson.length()) {
                        val home = homesJson.getJSONObject(homeIndex)
                        val servicesJson = home.optJSONArray("services")
                        val services = buildList {
                            if (servicesJson != null) {
                                for (serviceIndex in 0 until servicesJson.length()) {
                                    val service = servicesJson.getJSONObject(serviceIndex)
                                    add(
                                        CatalogService(
                                            id = service.getString("id"),
                                            alias = service.optString("alias"),
                                            protocol = service.getString("protocol"),
                                        ),
                                    )
                                }
                            }
                        }.sortedWith(compareBy(CatalogService::displayName, CatalogService::id, CatalogService::protocol))
                        add(
                            CatalogHome(
                                id = home.getString("home_id"),
                                alias = home.optString("home_alias"),
                                services = services,
                            ),
                        )
                    }
                }
            }.filter { it.services.isNotEmpty() }
                .sortedWith(compareBy(CatalogHome::displayName, CatalogHome::id))
            return TravelCatalog(
                generation = data.optLong("generation"),
                homes = homes,
            )
        }
    }
}

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
    val catalogGeneration: Long = 0,
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
                catalogGeneration = data.optLong("catalog_generation"),
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
