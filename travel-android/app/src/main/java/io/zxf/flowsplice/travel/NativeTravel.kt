package io.zxf.flowsplice.travel

object NativeTravel {
    init {
        System.loadLibrary("flowsplice_travel_android")
    }

    external fun beginEnrollment(
        installDirectory: String,
        travelId: String,
        homeId: String,
        selectedRelay: String,
        privateKeyPassword: String,
        trustedDeploymentRootPublicKey: String,
    ): String
    external fun enrollmentStatus(): String
    external fun cancelEnrollment(): String
    external fun start(
        configPath: String,
        privateKeyPassword: String,
        trustedDeploymentRootPublicKey: String,
    ): String
    external fun stop(): String
    external fun networkChanged(): String
    external fun status(): String
    external fun waitForStatusChange(knownGeneration: Long, timeoutMillis: Long): String
    external fun wakeStatusWaiters(): String
    external fun catalog(): String
    external fun upsertMapping(mappingJson: String): String
    external fun deleteMapping(homeId: String, serviceId: String, protocol: String): String
}
