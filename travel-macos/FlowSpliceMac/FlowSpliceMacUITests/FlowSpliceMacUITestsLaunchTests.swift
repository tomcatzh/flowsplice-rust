import XCTest

final class FlowSpliceMacUITestsLaunchTests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    @MainActor
    func testLaunchVisual() throws {
        let app = XCUIApplication()
        app.launchEnvironment["FLOWSPLICE_UI_TESTING"] = "1"
        app.launchArguments = ["--reset-state", "--mock-enrolled", "--ui-test-allow-termination"]
        addTeardownBlock { app.terminate() }
        app.launch()
        if !app.windows.firstMatch.exists {
            let openButton = app.buttons["Open FlowSplice"]
            openStatusMenu(app.statusItems.firstMatch, showing: openButton)
            openButton.click()
        }
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 6))

        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = "FlowSplice macOS Overview"
        attachment.lifetime = .keepAlways
        add(attachment)
    }
}
