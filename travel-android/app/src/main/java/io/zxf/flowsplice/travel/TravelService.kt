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
    private val mutableEnrollment = MutableStateFlow(EnrollmentSnapshot())
    private val mutableCatalog = MutableStateFlow(TravelCatalog())
    val state: StateFlow<TravelSnapshot> = mutableState.asStateFlow()
    val enrollment: StateFlow<EnrollmentSnapshot> = mutableEnrollment.asStateFlow()
    val catalog: StateFlow<TravelCatalog> = mutableCatalog.asStateFlow()

    fun initialize(context: Context) {
        val enrolled = TravelInstallation.isInstalled(context)
        if (mutableState.value.phase == TravelPhase.STOPPED) {
            mutableState.value = mutableState.value.copy(enrolled = enrolled)
        }
        if (!enrolled) {
            mutableEnrollment.value = runCatching {
                EnrollmentSnapshot.fromNative(NativeTravel.enrollmentStatus())
            }.getOrDefault(EnrollmentSnapshot())
        }
    }

    fun publish(snapshot: TravelSnapshot) {
        mutableState.value = snapshot
    }

    fun publishEnrollment(snapshot: EnrollmentSnapshot) {
        mutableEnrollment.value = snapshot
    }

    fun publishCatalog(catalog: TravelCatalog) {
        mutableCatalog.value = catalog
    }

    fun enroll(context: Context, travelId: String, homeId: String, relay: String, password: String) {
        CredentialStore.save(context, password)
        EnrollmentStore.save(context, travelId, homeId, relay)
        mutableEnrollment.value = EnrollmentSnapshot(
            phase = EnrollmentPhase.PREPARING,
            travelId = travelId,
        )
        ContextCompat.startForegroundService(
            context,
            Intent(context, TravelService::class.java).setAction(TravelService.ACTION_ENROLL),
        )
    }

    fun cancelEnrollment(context: Context) {
        context.startService(
            Intent(context, TravelService::class.java).setAction(TravelService.ACTION_CANCEL_ENROLLMENT),
        )
    }

    fun start(context: Context) {
        mutableState.value = mutableState.value.copy(
            phase = TravelPhase.STARTING,
            enrolled = TravelInstallation.isInstalled(context),
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
}

private object EnrollmentStore {
    private const val PREFERENCES = "travel-enrollment"
    private const val PENDING = "pending"
    private const val TRAVEL_ID = "travel-id"
    private const val HOME_ID = "home-id"
    private const val RELAY = "relay"

    data class Inputs(val travelId: String, val homeId: String, val relay: String)

    fun save(context: Context, travelId: String, homeId: String, relay: String) {
        context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE).edit {
            putBoolean(PENDING, true)
            putString(TRAVEL_ID, travelId)
            putString(HOME_ID, homeId)
            putString(RELAY, relay)
        }
    }

    fun load(context: Context): Inputs? {
        val preferences = context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
        if (!preferences.getBoolean(PENDING, false)) return null
        val travelId = preferences.getString(TRAVEL_ID, null) ?: return null
        val homeId = preferences.getString(HOME_ID, null) ?: return null
        val relay = preferences.getString(RELAY, null) ?: return null
        return Inputs(travelId, homeId, relay)
    }

    fun clear(context: Context) {
        context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE).edit { clear() }
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
            ACTION_ENROLL -> startEnrollment()
            ACTION_CANCEL_ENROLLMENT -> cancelEnrollment()
            ACTION_STOP -> stopTravel()
            ACTION_UPSERT -> intent.getStringExtra(EXTRA_MAPPING)?.let(::upsertMapping)
            ACTION_DELETE -> deleteMapping(intent)
            ACTION_START -> startTravel()
            null -> when {
                TravelInstallation.isInstalled(this) -> startTravel()
                EnrollmentStore.load(this) != null -> startEnrollment()
                else -> stopSelf()
            }
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

    private fun startEnrollment() {
        val inputs = EnrollmentStore.load(this)
        if (inputs == null) {
            failEnrollment("Enrollment inputs are unavailable")
            return
        }
        val initial = EnrollmentSnapshot(
            phase = EnrollmentPhase.PREPARING,
            travelId = inputs.travelId,
        )
        startForegroundNow(enrollmentNotification(initial))
        serviceScope.launch {
            val password = CredentialStore.load(this@TravelService)
            if (password.isNullOrEmpty()) {
                failEnrollment("The private-key password is unavailable")
                return@launch
            }
            val first = runCatching {
                EnrollmentSnapshot.fromNative(
                    NativeTravel.beginEnrollment(
                        TravelInstallation.directory(this@TravelService).absolutePath,
                        inputs.travelId,
                        inputs.homeId,
                        inputs.relay,
                        password,
                    ),
                )
            }.getOrElse { error ->
                failEnrollment(error.message ?: "Could not start remote enrollment")
                return@launch
            }
            TravelRepository.publishEnrollment(first)
            updateNotification(enrollmentNotification(first))
            pollingJob?.cancel()
            pollingJob = launch {
                while (isActive) {
                    delay(1_000)
                    val next = runCatching {
                        EnrollmentSnapshot.fromNative(NativeTravel.enrollmentStatus())
                    }.getOrElse { error ->
                        EnrollmentSnapshot(
                            phase = EnrollmentPhase.ERROR,
                            travelId = inputs.travelId,
                            error = error.message ?: "Remote enrollment failed",
                        )
                    }
                    TravelRepository.publishEnrollment(next)
                    updateNotification(enrollmentNotification(next))
                    when (next.phase) {
                        EnrollmentPhase.INSTALLED -> {
                            EnrollmentStore.clear(this@TravelService)
                            TravelRepository.publish(TravelSnapshot(enrolled = true, travelId = inputs.travelId))
                            pollingJob = null
                            startTravel()
                            return@launch
                        }
                        EnrollmentPhase.ERROR, EnrollmentPhase.CANCELLED -> {
                            failEnrollment(next.error ?: "Remote enrollment stopped")
                            return@launch
                        }
                        else -> Unit
                    }
                }
            }
        }
    }

    private fun cancelEnrollment() {
        pollingJob?.cancel()
        serviceScope.launch {
            runCatching { NativeTravel.cancelEnrollment() }
            EnrollmentStore.clear(this@TravelService)
            TravelInstallation.discardPending(this@TravelService)
            CredentialStore.clear(this@TravelService)
            TravelRepository.publishEnrollment(EnrollmentSnapshot())
            TravelRepository.publish(TravelSnapshot())
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
        }
    }

    private fun startTravel() {
        startForegroundNow(
            travelNotification(TravelSnapshot(phase = TravelPhase.STARTING, enrolled = true)),
        )
        serviceScope.launch {
            if (!TravelInstallation.isInstalled(this@TravelService)) {
                failAndStop("Complete remote enrollment before starting")
                return@launch
            }
            val password = CredentialStore.load(this@TravelService)
            if (password.isNullOrEmpty()) {
                failAndStop("The private-key password is unavailable")
                return@launch
            }
            val snapshot = runCatching {
                TravelSnapshot.fromNative(
                    NativeTravel.start(TravelInstallation.config(this@TravelService).absolutePath, password),
                    enrolled = true,
                )
            }.getOrElse { error ->
                TravelSnapshot(
                    phase = TravelPhase.ERROR,
                    enrolled = true,
                    error = error.message ?: "Travel Core failed to start",
                )
            }
            TravelRepository.publish(snapshot)
            updateNotification(travelNotification(snapshot))
            if (snapshot.phase == TravelPhase.ERROR) {
                failAndStop(snapshot.error ?: "Travel Core failed to start")
                return@launch
            }
            publishCurrentCatalog()
            getSharedPreferences(PREFERENCES, MODE_PRIVATE).edit { putBoolean(AUTO_START, true) }
            pollingJob?.cancel()
            pollingJob = launch {
                while (isActive) {
                    delay(1_000)
                    val next = runCatching {
                        TravelSnapshot.fromNative(NativeTravel.status(), enrolled = true)
                    }.getOrElse { error ->
                        TravelRepository.state.value.copy(
                            phase = TravelPhase.ERROR,
                            online = false,
                            error = error.message,
                        )
                    }
                    TravelRepository.publish(next)
                    if (next.phase == TravelPhase.RUNNING) publishCurrentCatalog()
                    updateNotification(travelNotification(next))
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
                TravelSnapshot(enrolled = TravelInstallation.isInstalled(this@TravelService)),
            )
            TravelRepository.publishCatalog(TravelCatalog())
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
        val next = TravelSnapshot.fromNative(NativeTravel.status(), enrolled = true)
        TravelRepository.publish(next)
        updateNotification(travelNotification(next))
    }

    private fun publishCurrentCatalog() {
        runCatching { TravelCatalog.fromNative(NativeTravel.catalog()) }
            .onSuccess(TravelRepository::publishCatalog)
    }

    private fun failEnrollment(message: String) {
        val current = TravelRepository.enrollment.value
        TravelRepository.publishEnrollment(
            current.copy(phase = EnrollmentPhase.ERROR, error = message),
        )
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun failAndStop(message: String) {
        TravelRepository.publish(
            TravelSnapshot(
                phase = TravelPhase.ERROR,
                enrolled = TravelInstallation.isInstalled(this),
                error = message,
            ),
        )
        TravelRepository.publishCatalog(TravelCatalog())
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun startForegroundNow(notification: Notification) {
        startForeground(
            NOTIFICATION_ID,
            notification,
            ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
        )
    }

    private fun updateNotification(notification: Notification) {
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification)
    }

    private fun enrollmentNotification(snapshot: EnrollmentSnapshot): Notification {
        val status = when (snapshot.phase) {
            EnrollmentPhase.PREPARING -> "Preparing device keys"
            EnrollmentPhase.WAITING_FOR_APPROVAL -> "Waiting for Home approval"
            EnrollmentPhase.INSTALLED -> "Enrollment complete"
            EnrollmentPhase.ERROR -> "Enrollment needs attention"
            EnrollmentPhase.CANCELLED -> "Enrollment cancelled"
            EnrollmentPhase.IDLE -> "Ready to enroll"
        }
        val details = snapshot.verificationCode?.let { "Verification code: $it" } ?: status
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_travel_notification)
            .setContentTitle("${snapshot.travelId.ifEmpty { "FlowSplice Travel" }} · $status")
            .setContentText(details)
            .setStyle(Notification.BigTextStyle().bigText(details))
            .setContentIntent(openPendingIntent())
            .setOngoing(snapshot.active)
            .setOnlyAlertOnce(true)
            .setCategory(Notification.CATEGORY_SERVICE)
            .addAction(Notification.Action.Builder(null, "Cancel", cancelEnrollmentPendingIntent()).build())
            .build()
    }

    private fun travelNotification(snapshot: TravelSnapshot): Notification {
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
            .setContentIntent(openPendingIntent())
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setCategory(Notification.CATEGORY_SERVICE)
            .addAction(Notification.Action.Builder(null, "Stop", stopPendingIntent()).build())
        if (Build.VERSION.SDK_INT >= 37) {
            builder.setRequestPromotedOngoing(true)
            builder.setShortCriticalText(status)
        }
        return builder.build()
    }

    private fun openPendingIntent(): PendingIntent = PendingIntent.getActivity(
        this,
        0,
        Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )

    private fun stopPendingIntent(): PendingIntent = PendingIntent.getService(
        this,
        1,
        Intent(this, TravelService::class.java).setAction(ACTION_STOP),
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )

    private fun cancelEnrollmentPendingIntent(): PendingIntent = PendingIntent.getService(
        this,
        2,
        Intent(this, TravelService::class.java).setAction(ACTION_CANCEL_ENROLLMENT),
        PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )

    private fun createNotificationChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Travel status",
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = "Enrollment, connection, traffic, and active-flow status"
            setShowBadge(false)
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    companion object {
        const val ACTION_ENROLL = "io.zxf.flowsplice.travel.ENROLL"
        const val ACTION_CANCEL_ENROLLMENT = "io.zxf.flowsplice.travel.CANCEL_ENROLLMENT"
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
