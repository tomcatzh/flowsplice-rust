package io.zxf.flowsplice.travel

import java.io.ByteArrayInputStream
import org.junit.Test

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail

class TravelUnitTest {
    @Test
    fun trafficValuesUseCompactUnits() {
        assertEquals("999 B", TravelService.formatBytes(999))
        assertEquals("1.5 KB", TravelService.formatBytes(1_500))
        assertEquals("2.0 MB", TravelService.formatBytes(2_000_000))
    }

    @Test
    fun deviceNamesBecomeValidTravelIds() {
        assertEquals("HONOR-Magic-V5", DeviceIdentity.normalizeTravelId("HONOR Magic V5"))
        assertEquals("android-travel", DeviceIdentity.normalizeTravelId("android-travel"))
    }

    @Test
    fun relayAddressesRequireAHostAndValidPort() {
        assertEquals(true, RelayPreference.isValid("relay.example:8443"))
        assertEquals(true, RelayPreference.isValid("192.0.2.1:443"))
        assertEquals(true, RelayPreference.isValid("[2001:db8::1]:8443"))
        assertFalse(RelayPreference.isValid("relay.example"))
        assertFalse(RelayPreference.isValid("relay.example:0"))
        assertFalse(RelayPreference.isValid("2001:db8::1:8443"))
    }

    @Test
    fun relayHandshakeClosureBecomesAnActionableEnrollmentError() {
        val raw = "peer closed connection without sending TLS close_notify: upstream details"
        assertEquals(
            "Could not establish a secure enrollment connection. Check that this is the " +
                "Relay management port and that the Relay is running the same 0.3 build as this app.",
            EnrollmentSnapshot.friendlyEnrollmentError(raw),
        )
    }

    @Test
    fun deploymentRootAssetRejectsOversizedFakeInput() {
        assertEquals(
            "trusted-root",
            DeploymentRootAsset.read(ByteArrayInputStream("trusted-root".toByteArray())),
        )
        val oversized = ByteArray(DeploymentRootAsset.MAX_BYTES + 1) { 'a'.code.toByte() }
        try {
            DeploymentRootAsset.read(ByteArrayInputStream(oversized))
            fail("Expected an oversized deployment root to fail")
        } catch (error: IllegalStateException) {
            assertEquals(
                "Required deployment root asset exceeds ${DeploymentRootAsset.MAX_BYTES} bytes",
                error.message,
            )
        }
    }

    @Test
    fun idleStatusRefreshBacksOffButActiveFlowsStayFast() {
        val policy = StatusRefreshPolicy(nowMillis = 0)
        val onlineIdle = TravelSnapshot(phase = TravelPhase.RUNNING, online = true)
        val offline = onlineIdle.copy(online = false)
        val active = onlineIdle.copy(activeFlows = 1)

        assertEquals(15_000, policy.nextTimeoutMillis(onlineIdle, 0))
        assertEquals(2_000, policy.nextTimeoutMillis(offline, 10_000))
        assertEquals(15_000, policy.nextTimeoutMillis(offline, 30_000))
        assertEquals(30_000, policy.nextTimeoutMillis(onlineIdle, 120_000))
        assertEquals(2_000, policy.nextTimeoutMillis(active, 120_000))
        assertEquals(30_000, policy.nextTimeoutMillis(active, 180_000))
        policy.recordBusinessActivity(180_000)
        assertEquals(2_000, policy.nextTimeoutMillis(active, 359_999))
        assertEquals(30_000, policy.nextTimeoutMillis(active, 360_000))
    }

    @Test
    fun statusObserverIgnoresUptimeOnlyChanges() {
        val first = TravelSnapshot(
            phase = TravelPhase.RUNNING,
            online = true,
            uptimeSeconds = 10,
        )
        assertEquals(first.observableState(), first.copy(uptimeSeconds = 20).observableState())
        assertFalse(
            first.observableState() == first.copy(downloadedBytes = 1).observableState(),
        )
    }

    @Test
    fun duplicateNotificationContentIsSuppressed() {
        val gate = DistinctNotificationGate()

        gate.recordForeground("online-zero")
        assertFalse(gate.accept("online-zero"))
        assertTrue(gate.accept("online-one"))
        assertFalse(gate.accept("online-one"))
        gate.reset()
        assertTrue(gate.accept("online-one"))
    }
}
