package io.zxf.flowsplice.travel

import androidx.compose.ui.test.assertIsEnabled
import androidx.compose.ui.test.assertTextContains
import androidx.compose.ui.test.junit4.v2.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextReplacement
import io.zxf.flowsplice.travel.ui.theme.FlowSpliceTravelAgentTheme
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Rule
import org.junit.Test

class MappingDialogTest {
    @get:Rule
    val composeTestRule = createComposeRule()

    @Test
    fun catalogChoicesDetermineHomeServiceAndProtocol() {
        var saved: TravelMapping? = null
        val catalog = TravelCatalog.fromNative(
            """{
                "ok": true,
                "data": {
                    "generation": 3,
                    "homes": [
                        {
                            "home_id": "home-2",
                            "home_alias": "Second Home",
                            "services": [{
                                "id": "echo",
                                "alias": "Echo",
                                "protocol": "udp",
                                "target": "127.0.0.1:9000"
                            }]
                        },
                        {
                            "home_id": "home-1",
                            "home_alias": "Primary Home",
                            "services": [{
                                "id": "dsh",
                                "alias": "Desktop Shell",
                                "protocol": "tcp",
                                "target": "127.0.0.1:1080"
                            }]
                        }
                    ]
                }
            }""".trimIndent(),
        )
        composeTestRule.setContent {
            FlowSpliceTravelAgentTheme(dynamicColor = false) {
                MappingDialog(catalog = catalog, onDismiss = {}, onSave = { saved = it })
            }
        }
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithTag("mapping-home-selector").assertTextContains("Primary Home (home-1)")
        composeTestRule.onNodeWithTag("mapping-service-selector").assertTextContains("Desktop Shell (dsh)")
        composeTestRule.onNodeWithTag("mapping-protocol").assertTextContains("TCP")

        composeTestRule.onNodeWithTag("mapping-home-selector").performClick()
        composeTestRule.onNodeWithTag("mapping-home-selector-option-home-2").performClick()
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithTag("mapping-service-selector").assertTextContains("Echo (echo)")
        composeTestRule.onNodeWithTag("mapping-protocol").assertTextContains("UDP")
        composeTestRule.onNodeWithTag("mapping-local-port").performTextReplacement("1080")
        composeTestRule.onNodeWithText("Save").assertIsEnabled().performClick()

        composeTestRule.runOnIdle {
            assertNotNull(saved)
            assertEquals(
                TravelMapping("home-2", "echo", "udp", "127.0.0.1:1080"),
                saved,
            )
        }
    }
}
