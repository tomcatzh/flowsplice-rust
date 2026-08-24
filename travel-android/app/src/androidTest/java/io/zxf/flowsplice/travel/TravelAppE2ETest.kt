package io.zxf.flowsplice.travel

import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.assertTextContains
import androidx.compose.ui.test.assert
import androidx.compose.ui.test.junit4.v2.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.performTextReplacement
import androidx.compose.ui.text.AnnotatedString
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
            .onNodeWithText("Enroll this device")
            .assertIsDisplayed()
        composeTestRule
            .onNodeWithText("Travel ID")
            .assertIsDisplayed()
        composeTestRule
            .onNodeWithText("Home ID")
            .assertIsDisplayed()
        composeTestRule
            .onNodeWithText("Relay address")
            .assertIsDisplayed()
    }

    @Test
    fun homeIdKeepsTrailingSeparatorWhileTyping() {
        val homeId = composeTestRule.onNodeWithTag("enrollment-home-id")

        homeId.performTextReplacement("home-")
        homeId.assertTextContains("home-")
        homeId.performTextInput("2")
        homeId.assertTextContains("home-2")
    }

    @Test
    fun passwordsSurviveActivityRecreation() {
        val password = "rotation-test-password"
        composeTestRule
            .onNodeWithTag("enrollment-password")
            .performTextReplacement(password)
        composeTestRule
            .onNodeWithTag("enrollment-confirm-password")
            .performTextReplacement(password)

        composeTestRule.activityRule.scenario.recreate()

        composeTestRule
            .onNodeWithTag("enrollment-password")
            .assert(
                SemanticsMatcher.expectValue(
                    SemanticsProperties.InputText,
                    AnnotatedString(password),
                ),
            )
        composeTestRule
            .onNodeWithTag("enrollment-confirm-password")
            .assert(
                SemanticsMatcher.expectValue(
                    SemanticsProperties.InputText,
                    AnnotatedString(password),
                ),
            )
    }
}
