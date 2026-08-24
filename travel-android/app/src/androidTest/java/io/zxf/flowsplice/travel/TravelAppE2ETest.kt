package io.zxf.flowsplice.travel

import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class TravelAppE2ETest {
    @get:Rule
    val composeTestRule = createAndroidComposeRule<MainActivity>()

    @Test
    fun launchDisplaysNativeTravelDashboard() {
        composeTestRule
            .onNodeWithText("FlowSplice Travel")
            .assertIsDisplayed()
        composeTestRule
            .onNodeWithText("Profile required")
            .assertIsDisplayed()
        composeTestRule
            .onNodeWithText("Local mappings")
            .assertIsDisplayed()
    }
}
