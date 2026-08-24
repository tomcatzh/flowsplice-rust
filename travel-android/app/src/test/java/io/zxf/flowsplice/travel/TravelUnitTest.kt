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
    fun profileConfigurationReplacementIsExact() {
        val source = "id = \"travel-1\"\nstate_store = \"/old/state.redb\"\n"
        val updated = TravelProfile.replaceTomlValue(source, "state_store", "/new/state.redb")
        assertEquals("id = \"travel-1\"\nstate_store = \"/new/state.redb\"\n", updated)
        assertFalse(updated.contains("/old/"))
    }
}
