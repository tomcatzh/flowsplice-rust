package io.zxf.flowsplice.travel

import org.junit.Test

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse

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
}
