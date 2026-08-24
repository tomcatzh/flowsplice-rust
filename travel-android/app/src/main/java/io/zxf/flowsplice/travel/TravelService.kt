package io.zxf.flowsplice.travel

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import androidx.core.content.ContextCompat
import androidx.core.content.edit
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.json.JSONObject
import java.util.Locale

object TravelRepository {
    private val mutableState = MutableStateFlow(TravelSnapshot())
    val state: StateFlow<TravelSnapshot> = mutableState.asStateFlow()

    fun initialize(context: Context) {
        if (mutableState.value.phase == TravelPhase.STOPPED) {
            mutableState.value = mutableState.value.copy(
                profileInstalled = TravelProfile.isInstalled(context),
            )
        }
    }

    fun publish(snapshot: TravelSnapshot) {
        mutableState.value = snapshot
    }

    fun start(context: Context) {
        mutableState.value = mutableState.value.copy(
            phase = TravelPhase.STARTING,
            profileInstalled = TravelProfile.isInstalled(context),
            error = null,
        )
        ContextCompat.startForegroundService(
            context,
            Intent(context, TravelService::class.java).setAction(TravelService.ACTION_START),
        )
    }

    fun stop(context: Context) {
        mutableState.value = mutableState.value.copy(phase = TravelPhase.STOPPING)
        context.startService(
            Intent(context, TravelService::class.java).setAction(TravelService.ACTION_STOP),
        )
    }

    fun upsert(context: Context, mapping: TravelMapping) {
        context.startService(
            Intent(context, TravelService::class.java)
                .setAction(TravelService.ACTION_UPSERT)
                .putExtra(TravelService.EXTRA_MAPPING, mapping.toNativeJson()),
        )
    }

    fun delete(context: Context, mapping: TravelMapping) {
        context.startService(
            Intent(context, TravelService::class.java)
                .setAction(TravelService.ACTION_DELETE)
                .putExtra(TravelService.EXTRA_HOME_ID, mapping.homeId)
                .putExtra(TravelService.EXTRA_SERVICE_ID, mapping.serviceId)
                .putExtra(TravelService.EXTRA_PROTOCOL, mapping.protocol),
        )
    }

    fun profileChanged(context: Context) {
        mutableState.value = TravelSnapshot(profileInstalled = TravelProfile.isInstalled(context))
    }
}

class TravelService : Service() {
    private val serviceScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private var pollingJob: Job? = null

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> stopTravel()
            ACTION_UPSERT -> intent.getStringExtra(EXTRA_MAPPING)?.let(::upsertMapping)
            ACTION_DELETE -> deleteMapping(intent)
            ACTION_START, null -> startTravel()
        }
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onDestroy() {
        pollingJob?.cancel()
        runCatching { NativeTravel.stop() }
        serviceScope.cancel()
        super.onDestroy()
    }

    private fun startTravel() {
        startForegroundNow(TravelSnapshot(phase = TravelPhase.STARTING, profileInstalled = true))
        serviceScope.launch {
            if (!TravelProfile.isInstalled(this@TravelService)) {
                failAndStop("Import an enrolled Travel profile before starting")
                return@launch
            }
            val password = CredentialStore.load(this@TravelService)
            if (password.isNullOrEmpty()) {
                failAndStop("The private-key password is unavailable; import the profile again")
                return@launch
            }
            val snapshot = runCatching {
                TravelSnapshot.fromNative(
                    NativeTravel.start(TravelProfile.config(this@TravelService).absolutePath, password),
                    profileInstalled = true,
                )
            }.getOrElse { error ->
                TravelSnapshot(
                    phase = TravelPhase.ERROR,
                    profileInstalled = true,
                    error = error.message ?: "Travel Core failed to start",
                )
            }
            TravelRepository.publish(snapshot)
            updateNotification(snapshot)
            if (snapshot.phase == TravelPhase.ERROR) {
                failAndStop(snapshot.error ?: "Travel Core failed to start")
                return@launch
            }
            getSharedPreferences(PREFERENCES, MODE_PRIVATE).edit { putBoolean(AUTO_START, true) }
            pollingJob?.cancel()
            pollingJob = launch {
                while (isActive) {
                    delay(1_000)
                    val next = runCatching {
                        TravelSnapshot.fromNative(NativeTravel.status(), profileInstalled = true)
                    }.getOrElse { error ->
                        TravelRepository.state.value.copy(
                            phase = TravelPhase.ERROR,
                            online = false,
                            error = error.message,
                        )
                    }
                    TravelRepository.publish(next)
                    updateNotification(next)
                }
            }
        }
    }

    private fun stopTravel() {
        pollingJob?.cancel()
        serviceScope.launch {
            runCatching { NativeTravel.stop() }
            getSharedPreferences(PREFERENCES, MODE_PRIVATE).edit { putBoolean(AUTO_START, false) }
            TravelRepository.publish(
                TravelSnapshot(profileInstalled = TravelProfile.isInstalled(this@TravelService)),
            )
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
    }

    private fun upsertMapping(mappingJson: String) {
        serviceScope.launch {
            val response = runCatching { NativeTravel.upsertMapping(mappingJson) }
                .getOrElse { error -> nativeError(error.message ?: "Could not save the mapping") }
            applyMutationResult(response)
        }
    }

    private fun deleteMapping(intent: Intent) {
        val homeId = intent.getStringExtra(EXTRA_HOME_ID) ?: return
        val serviceId = intent.getStringExtra(EXTRA_SERVICE_ID) ?: return
        val protocol = intent.getStringExtra(EXTRA_PROTOCOL) ?: return
        serviceScope.launch {
            val response = runCatching { NativeTravel.deleteMapping(homeId, serviceId, protocol) }
                .getOrElse { error -> nativeError(error.message ?: "Could not delete the mapping") }
            applyMutationResult(response)
        }
    }

    private fun applyMutationResult(response: String) {
        val envelope = JSONObject(response)
        if (!envelope.optBoolean("ok")) {
            TravelRepository.publish(
                TravelRepository.state.value.copy(error = envelope.optString("error")),
            )
            return
        }
        val next = TravelSnapshot.fromNative(NativeTravel.status(), profileInstalled = true)
        TravelRepository.publish(next)
        updateNotification(next)
    }

    private fun failAndStop(message: String) {
        TravelRepository.publish(
            TravelSnapshot(
                phase = TravelPhase.ERROR,
                profileInstalled = TravelProfile.isInstalled(this),
                error = message,
            ),
        )
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun startForegroundNow(snapshot: TravelSnapshot) {
        val notification = notification(snapshot)
        startForeground(
            NOTIFICATION_ID,
            notification,
            ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
        )
    }

    private fun updateNotification(snapshot: TravelSnapshot) {
        getSystemService(NotificationManager::class.java).notify(
            NOTIFICATION_ID,
            notification(snapshot),
        )
    }

    private fun notification(snapshot: TravelSnapshot): Notification {
        val openIntent = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val stopIntent = PendingIntent.getService(
            this,
            1,
            Intent(this, TravelService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val status = when (snapshot.phase) {
            TravelPhase.STARTING -> "Connecting"
            TravelPhase.RUNNING -> if (snapshot.online) "Online" else "Waiting for Relay"
            TravelPhase.STOPPING -> "Stopping"
            TravelPhase.ERROR -> "Needs attention"
            TravelPhase.STOPPED -> "Stopped"
        }
        val details = "↑ ${formatBytes(snapshot.uploadedBytes)}  ↓ ${formatBytes(snapshot.downloadedBytes)}  •  ${snapshot.activeFlows} active"
        val builder = Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_travel_notification)
            .setContentTitle("${snapshot.travelId} · $status")
            .setContentText(details)
            .setStyle(Notification.BigTextStyle().bigText(details))
            .setContentIntent(openIntent)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setCategory(Notification.CATEGORY_SERVICE)
            .addAction(Notification.Action.Builder(null, "Stop", stopIntent).build())
        if (Build.VERSION.SDK_INT >= 37) {
            builder.setRequestPromotedOngoing(true)
            builder.setShortCriticalText(status)
        }
        return builder.build()
    }

    private fun createNotificationChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Travel status",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "Persistent connection, traffic, and active-flow status"
            setShowBadge(false)
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    companion object {
        const val ACTION_START = "io.zxf.flowsplice.travel.START"
        const val ACTION_STOP = "io.zxf.flowsplice.travel.STOP"
        const val ACTION_UPSERT = "io.zxf.flowsplice.travel.UPSERT"
        const val ACTION_DELETE = "io.zxf.flowsplice.travel.DELETE"
        const val EXTRA_MAPPING = "mapping"
        const val EXTRA_HOME_ID = "home_id"
        const val EXTRA_SERVICE_ID = "service_id"
        const val EXTRA_PROTOCOL = "protocol"
        const val PREFERENCES = "travel-service"
        const val AUTO_START = "auto-start"
        private const val CHANNEL_ID = "travel-status"
        private const val NOTIFICATION_ID = 3103

        fun shouldAutoStart(context: Context): Boolean =
            context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
                .getBoolean(AUTO_START, false)

        fun formatBytes(bytes: Long): String {
            if (bytes < 1_000) return "$bytes B"
            val units = arrayOf("KB", "MB", "GB", "TB")
            var value = bytes.toDouble()
            var index = -1
            do {
                value /= 1_000.0
                index += 1
            } while (value >= 1_000 && index < units.lastIndex)
            return String.format(Locale.US, if (value >= 100) "%.0f %s" else "%.1f %s", value, units[index])
        }

        private fun nativeError(message: String): String = JSONObject()
            .put("ok", false)
            .put("data", JSONObject.NULL)
            .put("error", message)
            .toString()
    }
}
