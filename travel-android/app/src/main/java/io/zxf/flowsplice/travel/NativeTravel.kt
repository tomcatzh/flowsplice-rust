package io.zxf.flowsplice.travel

object NativeTravel {
    init {
        System.loadLibrary("flowsplice_travel_android")
    }

    external fun start(configPath: String, privateKeyPassword: String): String
    external fun stop(): String
    external fun status(): String
    external fun catalog(): String
    external fun upsertMapping(mappingJson: String): String
    external fun deleteMapping(homeId: String, serviceId: String, protocol: String): String
}
