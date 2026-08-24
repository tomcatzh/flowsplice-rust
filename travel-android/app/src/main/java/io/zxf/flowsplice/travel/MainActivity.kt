package io.zxf.flowsplice.travel

import android.Manifest
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import io.zxf.flowsplice.travel.ui.theme.FlowSpliceTravelAgentTheme

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        TravelRepository.initialize(this)
        enableEdgeToEdge()
        setContent {
            FlowSpliceTravelAgentTheme {
                TravelApp()
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun TravelApp() {
    val context = LocalContext.current
    val snapshot by TravelRepository.state.collectAsStateWithLifecycle()
    val enrollment by TravelRepository.enrollment.collectAsStateWithLifecycle()
    var showMappingDialog by rememberSaveable { mutableStateOf(false) }
    var notificationAllowed by remember {
        mutableStateOf(
            ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) ==
                PackageManager.PERMISSION_GRANTED,
        )
    }
    val notificationPermission = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { notificationAllowed = it }

    LaunchedEffect(Unit) {
        TravelRepository.initialize(context)
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text("FlowSplice Travel", fontWeight = FontWeight.SemiBold)
                        Text("Android", style = MaterialTheme.typography.labelMedium)
                    }
                },
            )
        },
    ) { padding ->
        if (!snapshot.enrolled) {
            LazyColumn(
                modifier = Modifier.fillMaxSize().padding(padding),
                contentPadding = PaddingValues(16.dp, 12.dp, 16.dp, 32.dp),
                verticalArrangement = Arrangement.spacedBy(14.dp),
            ) {
                item {
                    EnrollmentPane(
                        enrollment = enrollment,
                        notificationAllowed = notificationAllowed,
                        onPermission = { notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS) },
                        onEnroll = { travelId, homeId, relay, password ->
                            TravelRepository.enroll(context, travelId, homeId, relay, password)
                        },
                        onCancel = { TravelRepository.cancelEnrollment(context) },
                    )
                }
            }
        } else {
            BoxWithConstraints(
                modifier = Modifier.fillMaxSize().padding(padding),
            ) {
                val expanded = maxWidth >= 720.dp
                if (expanded) {
                    Row(
                        modifier = Modifier.fillMaxSize().padding(horizontal = 24.dp),
                        horizontalArrangement = Arrangement.spacedBy(24.dp),
                    ) {
                        LazyColumn(
                            modifier = Modifier.weight(1f),
                            contentPadding = PaddingValues(vertical = 20.dp),
                            verticalArrangement = Arrangement.spacedBy(16.dp),
                        ) {
                            item {
                                OverviewPane(
                                    snapshot = snapshot,
                                    notificationAllowed = notificationAllowed,
                                    onPermission = {
                                        notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
                                    },
                                    onStart = { TravelRepository.start(context) },
                                    onStop = { TravelRepository.stop(context) },
                                )
                            }
                        }
                        LazyColumn(
                            modifier = Modifier.weight(1f),
                            contentPadding = PaddingValues(vertical = 20.dp),
                            verticalArrangement = Arrangement.spacedBy(12.dp),
                        ) {
                            item {
                                MappingHeader(
                                    enabled = snapshot.phase == TravelPhase.RUNNING,
                                    onAdd = { showMappingDialog = true },
                                )
                            }
                            items(snapshot.mappings, key = { "${it.homeId}/${it.serviceId}/${it.protocol}" }) { mapping ->
                                MappingCard(mapping = mapping, onDelete = { TravelRepository.delete(context, mapping) })
                            }
                            if (snapshot.mappings.isEmpty()) item { EmptyMappings() }
                        }
                    }
                } else {
                    LazyColumn(
                        modifier = Modifier.fillMaxSize(),
                        contentPadding = PaddingValues(16.dp, 12.dp, 16.dp, 32.dp),
                        verticalArrangement = Arrangement.spacedBy(14.dp),
                    ) {
                        item {
                            OverviewPane(
                                snapshot = snapshot,
                                notificationAllowed = notificationAllowed,
                                onPermission = {
                                    notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
                                },
                                onStart = { TravelRepository.start(context) },
                                onStop = { TravelRepository.stop(context) },
                            )
                        }
                        item {
                            MappingHeader(
                                enabled = snapshot.phase == TravelPhase.RUNNING,
                                onAdd = { showMappingDialog = true },
                            )
                        }
                        items(snapshot.mappings, key = { "${it.homeId}/${it.serviceId}/${it.protocol}" }) { mapping ->
                            MappingCard(mapping = mapping, onDelete = { TravelRepository.delete(context, mapping) })
                        }
                        if (snapshot.mappings.isEmpty()) item { EmptyMappings() }
                    }
                }
            }
        }
    }

    if (showMappingDialog) {
        MappingDialog(
            onDismiss = { showMappingDialog = false },
            onSave = { mapping ->
                TravelRepository.upsert(context, mapping)
                showMappingDialog = false
            },
        )
    }
}

@Composable
private fun EnrollmentPane(
    enrollment: EnrollmentSnapshot,
    notificationAllowed: Boolean,
    onPermission: () -> Unit,
    onEnroll: (String, String, String, String) -> Unit,
    onCancel: () -> Unit,
) {
    val context = LocalContext.current
    var travelId by rememberSaveable { mutableStateOf(DeviceIdentity.defaultTravelId(context)) }
    var homeId by rememberSaveable { mutableStateOf("home-1") }
    var relay by rememberSaveable { mutableStateOf(RelayPreference.load(context)) }
    var password by remember { mutableStateOf("") }
    var confirmPassword by remember { mutableStateOf("") }

    Column(verticalArrangement = Arrangement.spacedBy(14.dp)) {
        Card(
            modifier = Modifier.fillMaxWidth(),
            colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.primaryContainer),
            shape = RoundedCornerShape(24.dp),
        ) {
            Column(
                modifier = Modifier.padding(20.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Text("Enroll this device", style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.SemiBold)
                Text("Keys are generated on this Android device. Home approval completes enrollment automatically.")
            }
        }

        if (!notificationAllowed) PermissionCard(onPermission)

        Card(modifier = Modifier.fillMaxWidth()) {
            Column(
                modifier = Modifier.padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                Text("Device identity", style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
                OutlinedTextField(
                    value = travelId,
                    onValueChange = { travelId = DeviceIdentity.normalizeTravelId(it) },
                    modifier = Modifier.fillMaxWidth(),
                    label = { Text("Travel ID") },
                    supportingText = { Text("Defaults to this Android device name and can be changed.") },
                    enabled = !enrollment.active,
                    singleLine = true,
                )
                OutlinedTextField(
                    value = homeId,
                    onValueChange = { homeId = DeviceIdentity.normalizeTravelId(it) },
                    modifier = Modifier.fillMaxWidth(),
                    label = { Text("Home ID") },
                    supportingText = { Text("Enter the Home that will approve this device.") },
                    enabled = !enrollment.active,
                    singleLine = true,
                )
                OutlinedTextField(
                    value = relay,
                    onValueChange = {
                        relay = it
                        RelayPreference.save(context, it)
                    },
                    modifier = Modifier.fillMaxWidth(),
                    label = { Text("Relay address") },
                    supportingText = { Text("Enter host:port or IP:port. The APK does not contain this address.") },
                    enabled = !enrollment.active,
                    singleLine = true,
                )
                OutlinedTextField(
                    value = password,
                    onValueChange = { password = it },
                    modifier = Modifier.fillMaxWidth(),
                    label = { Text("Private-key password") },
                    supportingText = { Text("Use at least 12 characters.") },
                    visualTransformation = PasswordVisualTransformation(),
                    enabled = !enrollment.active,
                    singleLine = true,
                )
                OutlinedTextField(
                    value = confirmPassword,
                    onValueChange = { confirmPassword = it },
                    modifier = Modifier.fillMaxWidth(),
                    label = { Text("Confirm password") },
                    visualTransformation = PasswordVisualTransformation(),
                    enabled = !enrollment.active,
                    singleLine = true,
                )
                Button(
                    onClick = { onEnroll(travelId, homeId, relay, password) },
                    enabled = notificationAllowed &&
                        !enrollment.active &&
                        travelId.isNotBlank() &&
                        homeId.isNotBlank() &&
                        RelayPreference.isValid(relay) &&
                        password.length >= 12 &&
                        password == confirmPassword,
                ) {
                    Text(if (enrollment.phase == EnrollmentPhase.ERROR) "Retry enrollment" else "Enroll")
                }
            }
        }

        if (enrollment.active || enrollment.phase == EnrollmentPhase.ERROR) {
            EnrollmentStatusCard(enrollment, onCancel)
        }
    }
}

@Composable
private fun EnrollmentStatusCard(snapshot: EnrollmentSnapshot, onCancel: () -> Unit) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(
            containerColor = if (snapshot.phase == EnrollmentPhase.ERROR) {
                MaterialTheme.colorScheme.errorContainer
            } else {
                MaterialTheme.colorScheme.secondaryContainer
            },
        ),
    ) {
        Column(
            modifier = Modifier.padding(18.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Text(
                when (snapshot.phase) {
                    EnrollmentPhase.PREPARING -> "Generating device keys"
                    EnrollmentPhase.WAITING_FOR_APPROVAL -> "Waiting for Home approval"
                    EnrollmentPhase.ERROR -> "Enrollment needs attention"
                    else -> "Remote enrollment"
                },
                style = MaterialTheme.typography.titleMedium,
                fontWeight = FontWeight.SemiBold,
            )
            snapshot.verificationCode?.let { code ->
                Text("Compare this code on Home:", style = MaterialTheme.typography.bodyMedium)
                Text(
                    code,
                    style = MaterialTheme.typography.headlineMedium,
                    fontFamily = FontFamily.Monospace,
                    fontWeight = FontWeight.Bold,
                )
            }
            snapshot.error?.let { Text(it) }
            Text(
                if (snapshot.active) "You may leave the app while approval is pending."
                else "Start over to change the Travel ID, Home, Relay, or password.",
            )
            OutlinedButton(onClick = onCancel) {
                Text(if (snapshot.active) "Cancel enrollment" else "Start over")
            }
        }
    }
}

@Composable
private fun OverviewPane(
    snapshot: TravelSnapshot,
    notificationAllowed: Boolean,
    onPermission: () -> Unit,
    onStart: () -> Unit,
    onStop: () -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(14.dp)) {
        StatusCard(snapshot, onStart, onStop)
        if (snapshot.error != null) ErrorCard(snapshot.error)
        MetricRow(snapshot)
        if (!notificationAllowed) PermissionCard(onPermission)
    }
}

@Composable
private fun StatusCard(snapshot: TravelSnapshot, onStart: () -> Unit, onStop: () -> Unit) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceContainerHigh),
        shape = RoundedCornerShape(24.dp),
    ) {
        Column(
            modifier = Modifier.padding(20.dp),
            verticalArrangement = Arrangement.spacedBy(18.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Surface(
                    modifier = Modifier.width(12.dp).height(12.dp),
                    shape = CircleShape,
                    color = statusColor(snapshot),
                    content = {},
                )
                Spacer(Modifier.width(10.dp))
                Column(modifier = Modifier.weight(1f)) {
                    Text(snapshot.travelId, style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.SemiBold)
                    Text(statusLabel(snapshot), style = MaterialTheme.typography.bodyMedium)
                }
                when (snapshot.phase) {
                    TravelPhase.RUNNING, TravelPhase.STARTING -> OutlinedButton(onClick = onStop) { Text("Stop") }
                    TravelPhase.STOPPING -> Button(onClick = {}, enabled = false) { Text("Stopping") }
                    else -> Button(onClick = onStart, enabled = snapshot.enrolled) { Text("Start") }
                }
            }
            if (snapshot.phase == TravelPhase.RUNNING) {
                HorizontalDivider()
                Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween) {
                    Text("Uptime", style = MaterialTheme.typography.labelLarge)
                    Text(formatDuration(snapshot.uptimeSeconds), fontWeight = FontWeight.Medium)
                }
            }
        }
    }
}

@Composable
private fun MetricRow(snapshot: TravelSnapshot) {
    Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
        MetricCard("Upload", TravelService.formatBytes(snapshot.uploadedBytes), Modifier.weight(1f))
        MetricCard("Download", TravelService.formatBytes(snapshot.downloadedBytes), Modifier.weight(1f))
        MetricCard("Flows", snapshot.activeFlows.toString(), Modifier.weight(1f))
    }
}

@Composable
private fun MetricCard(label: String, value: String, modifier: Modifier) {
    Card(modifier = modifier, shape = RoundedCornerShape(18.dp)) {
        Column(modifier = Modifier.padding(14.dp)) {
            Text(label, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Spacer(Modifier.height(6.dp))
            Text(value, style = MaterialTheme.typography.titleMedium, fontWeight = FontWeight.SemiBold)
        }
    }
}

@Composable
private fun PermissionCard(onPermission: () -> Unit) {
    Card(
        modifier = Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.secondaryContainer),
    ) {
        Row(
            modifier = Modifier.padding(16.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text("Keep status visible", fontWeight = FontWeight.SemiBold)
                Text("Allow notifications for enrollment, connection state, and traffic.")
            }
            FilledTonalButton(onClick = onPermission) { Text("Allow") }
        }
    }
}

@Composable
private fun ErrorCard(message: String) {
    Surface(
        modifier = Modifier.fillMaxWidth(),
        color = MaterialTheme.colorScheme.errorContainer,
        shape = RoundedCornerShape(16.dp),
    ) {
        Column(modifier = Modifier.padding(16.dp)) {
            Text("Needs attention", fontWeight = FontWeight.SemiBold, color = MaterialTheme.colorScheme.onErrorContainer)
            Text(message, color = MaterialTheme.colorScheme.onErrorContainer)
        }
    }
}

@Composable
private fun MappingHeader(enabled: Boolean, onAdd: () -> Unit) {
    Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Column(modifier = Modifier.weight(1f)) {
            Text("Local mappings", style = MaterialTheme.typography.titleLarge, fontWeight = FontWeight.SemiBold)
            Text("Open Home services on this device", style = MaterialTheme.typography.bodyMedium)
        }
        Button(onClick = onAdd, enabled = enabled) { Text("Add") }
    }
}

@Composable
private fun MappingCard(mapping: TravelMapping, onDelete: () -> Unit) {
    Card(modifier = Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier.padding(16.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text(
                mapping.protocol.uppercase(),
                modifier = Modifier
                    .background(MaterialTheme.colorScheme.primaryContainer, RoundedCornerShape(8.dp))
                    .padding(horizontal = 8.dp, vertical = 5.dp),
                style = MaterialTheme.typography.labelMedium,
                fontWeight = FontWeight.Bold,
            )
            Column(modifier = Modifier.weight(1f)) {
                Text(mapping.serviceId, fontWeight = FontWeight.SemiBold)
                Text("${mapping.homeId} · ${mapping.bind}", style = MaterialTheme.typography.bodySmall)
            }
            TextButton(onClick = onDelete) { Text("Remove") }
        }
    }
}

@Composable
private fun EmptyMappings() {
    Surface(
        modifier = Modifier.fillMaxWidth(),
        color = MaterialTheme.colorScheme.surfaceContainerLow,
        shape = RoundedCornerShape(18.dp),
    ) {
        Text(
            "No mappings yet. Add one after Travel is running.",
            modifier = Modifier.padding(20.dp),
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun MappingDialog(onDismiss: () -> Unit, onSave: (TravelMapping) -> Unit) {
    var homeId by rememberSaveable { mutableStateOf("") }
    var serviceId by rememberSaveable { mutableStateOf("") }
    var protocol by rememberSaveable { mutableStateOf("tcp") }
    var port by rememberSaveable { mutableStateOf("") }
    val validPort = port.toIntOrNull()?.let { it in 1..65535 } == true
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Add local mapping") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                OutlinedTextField(homeId, { homeId = it }, label = { Text("Home ID") }, singleLine = true)
                OutlinedTextField(serviceId, { serviceId = it }, label = { Text("Service ID") }, singleLine = true)
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (protocol == "tcp") Button({ protocol = "tcp" }) { Text("TCP") }
                    else OutlinedButton({ protocol = "tcp" }) { Text("TCP") }
                    if (protocol == "udp") Button({ protocol = "udp" }) { Text("UDP") }
                    else OutlinedButton({ protocol = "udp" }) { Text("UDP") }
                }
                OutlinedTextField(
                    value = port,
                    onValueChange = { port = it.filter(Char::isDigit).take(5) },
                    label = { Text("Local port") },
                    prefix = { Text("127.0.0.1:") },
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    singleLine = true,
                )
            }
        },
        confirmButton = {
            Button(
                onClick = {
                    onSave(TravelMapping(homeId.trim(), serviceId.trim(), protocol, "127.0.0.1:$port"))
                },
                enabled = homeId.isNotBlank() && serviceId.isNotBlank() && validPort,
            ) { Text("Save") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

@Composable
private fun statusColor(snapshot: TravelSnapshot): Color = when {
    snapshot.phase == TravelPhase.ERROR -> MaterialTheme.colorScheme.error
    snapshot.phase == TravelPhase.STARTING || snapshot.phase == TravelPhase.STOPPING -> MaterialTheme.colorScheme.tertiary
    snapshot.phase == TravelPhase.RUNNING && snapshot.online -> Color(0xFF16835B)
    snapshot.phase == TravelPhase.RUNNING -> MaterialTheme.colorScheme.secondary
    else -> MaterialTheme.colorScheme.outline
}

private fun statusLabel(snapshot: TravelSnapshot): String = when (snapshot.phase) {
    TravelPhase.STARTING -> "Connecting…"
    TravelPhase.RUNNING -> if (snapshot.online) "Online · ${snapshot.relayCount} Relay" else "Waiting for Relay"
    TravelPhase.STOPPING -> "Stopping…"
    TravelPhase.ERROR -> "Stopped"
    TravelPhase.STOPPED -> if (snapshot.enrolled) "Ready" else "Enrollment required"
}

private fun formatDuration(seconds: Long): String {
    val hours = seconds / 3_600
    val minutes = (seconds % 3_600) / 60
    return if (hours > 0) "${hours}h ${minutes}m" else "${minutes}m"
}
