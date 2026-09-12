package io.zxf.flowsplice.pty

import androidx.test.ext.junit.runners.AndroidJUnit4
import org.json.JSONArray
import org.json.JSONObject
import org.json.JSONTokener
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith

/** Uses Android's real JSON implementation, not the JVM android.jar stubs. */
@RunWith(AndroidJUnit4::class)
class BridgeAccountingTest {
    private fun bytes(value: String) = value.toByteArray(Charsets.UTF_8).size
    private fun event(value: String) = JSONObject().put("type", "history").put("data", value).toString()
    private fun fragment(value: String) = MainActivity.BridgeEvent(value)

    @Test fun escDenseHistoryAddsOneBytePerAlreadySerializedEscape() {
        val single = event("\u001b")
        val dense = event("\u001b".repeat(65536))
        // ESC is already six ASCII bytes; outer quoting adds one backslash.
        assertEquals(6 * 65535, bytes(dense) - bytes(single))
        assertEquals(7 * 65535, bytes(JSONObject.quote(dense)) - bytes(JSONObject.quote(single)))
        assertEquals(bytes(JSONObject.quote(dense)) - 1, fragment(dense).bytes)
    }

    @Test fun escapedFragmentsMatchWholeBatchQuotingAndRoundTripInOrder() {
        val values = listOf("\u001b[31mred\u001b[0m", "\"quoted\" \\path\\ </script>",
            "中文 😀 café \u2028 \u2029", "\n\r\t\b\u0000", "plain")
        val serialized = values.map(::event)
        val events = serialized.map(::fragment)
        val literal = MainActivity.bridgeLiteral(events)
        assertEquals(JSONObject.quote(serialized.joinToString(",", "[", "]")), literal)
        val decoded = JSONArray(JSONTokener(literal).nextValue() as String)
        assertEquals(values.size, decoded.length())
        values.forEachIndexed { index, value -> assertEquals(value, decoded.getJSONObject(index).getString("data")) }
        assertEquals(events.sumOf { it.bytes } + 3, bytes(literal))
    }

    @Test fun budgetBoundsActualUtf8ScriptsAcrossBatchBoundariesAndTokenWidths() {
        for (count in listOf(1, 63, 64, 65, 128, 129)) {
            val events = List(count) { fragment(event("中文\\\"\u001b😀:$it")) }
            val fragmentBytes = events.sumOf { it.bytes }.toLong()
            for (token in listOf(0, 1, Int.MAX_VALUE, Int.MIN_VALUE)) {
                val chunks = events.chunked(MainActivity.BRIDGE_BATCH)
                val actual = chunks.sumOf { bytes(MainActivity.bridgeScript(it, token)) }.toLong()
                val budget = MainActivity.bridgeBudget(fragmentBytes, count, 0)
                assertTrue("count=$count token=$token", actual <= budget)
                if (token == Int.MIN_VALUE) assertEquals(actual + chunks.size, budget)
                val first = chunks.first()
                val inFlight = bytes(MainActivity.bridgeScript(first, token))
                val remainingBytes = fragmentBytes - first.sumOf { it.bytes }
                val transitioned = MainActivity.bridgeBudget(remainingBytes, count - first.size, inFlight)
                assertTrue(transitioned <= budget)
                assertTrue(actual <= transitioned)
            }
        }
    }

    @Test fun exactLimitIncludesEscapingWrappersAndExistingFlight() {
        val inFlight = bytes(MainActivity.bridgeScript(listOf(fragment(event("\u001b\\\"😀"))), 7))
        val empty = fragment(event(""))
        val room = MainActivity.BRIDGE_LIMIT - inFlight - MainActivity.bridgeOverhead - empty.bytes
        val fits = fragment(event("x".repeat(room)))
        assertEquals(MainActivity.BRIDGE_LIMIT.toLong(), MainActivity.bridgeBudget(fits.bytes.toLong(), 1, inFlight))
        val over = fragment(event("x".repeat(room + 1)))
        assertTrue(MainActivity.bridgeBudget(over.bytes.toLong(), 1, inFlight) > MainActivity.BRIDGE_LIMIT)
        assertTrue(bytes(MainActivity.bridgeScript(listOf(fits), Int.MIN_VALUE)) + inFlight <= MainActivity.BRIDGE_LIMIT)
        assertEquals(inFlight.toLong(), MainActivity.bridgeBudget(0, 0, inFlight))
    }
}
