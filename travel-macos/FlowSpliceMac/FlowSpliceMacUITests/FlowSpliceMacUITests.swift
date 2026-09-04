import AppKit
import Carbon
import Darwin
import Foundation
import XCTest

final class FlowSpliceMacUITests: XCTestCase {
    private static let flowSpliceBundleIdentifier = "io.zxf.flowsplice.travel.macos"
    private var originalInputSource: TISInputSource?

    override func setUpWithError() throws {
        continueAfterFailure = false
        originalInputSource = TISCopyCurrentKeyboardInputSource().takeRetainedValue()
        let asciiInputSource = TISCopyCurrentASCIICapableKeyboardInputSource().takeRetainedValue()
        XCTAssertEqual(TISSelectInputSource(asciiInputSource), noErr)
    }

    override func tearDownWithError() throws {
        if let originalInputSource {
            XCTAssertEqual(TISSelectInputSource(originalInputSource), noErr)
        }
    }

    @MainActor
    func testEnrollmentValidationAndCompletion() throws {
        let app = makeApp(enrolled: false, allowTermination: true)
        addTeardownBlock { app.terminate() }
        app.launch()
        showMainWindowIfNeeded(in: app)

        XCTAssertTrue(app.staticTexts["Enroll this Mac"].waitForExistence(timeout: 5))
        replaceText(in: app.textFields["enrollment-travel-id"], with: "mac-e2e")
        replaceText(in: app.textFields["enrollment-home-id"], with: "home-1")
        replaceText(in: app.textFields["enrollment-relay"], with: "invalid-relay")
        replaceText(in: app.secureTextFields["enrollment-password"], with: "correct-horse-battery")
        replaceText(in: app.secureTextFields["enrollment-password-confirmation"], with: "correct-horse-battery")
        app.buttons["enrollment-submit"].click()
        let warningSheet = app.sheets.firstMatch
        XCTAssertTrue(warningSheet.waitForExistence(timeout: 3))
        XCTAssertTrue(warningSheet.staticTexts["Enter a Relay as host:port or IP:port."].exists)
        warningSheet.buttons["OK"].click()

        replaceText(in: app.textFields["enrollment-relay"], with: "relay.example.com:8443")
        app.buttons["enrollment-submit"].click()

        XCTAssertTrue(element(in: app, identifier: "enrollment-verification-code").waitForExistence(timeout: 3))
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 6))
    }

    @MainActor
    func testControlCenterMappingsDiagnosticsAndLifecycle() throws {
        let app = makeApp(enrolled: true, allowTermination: true)
        addTeardownBlock { app.terminate() }
        app.launch()
        showMainWindowIfNeeded(in: app)
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 6))
        XCTAssertTrue(app.staticTexts["Online"].exists)

        element(in: app, identifier: "sidebar-mappings").click()
        XCTAssertTrue(app.staticTexts["Local service mappings"].waitForExistence(timeout: 3))
        XCTAssertTrue(app.staticTexts["127.0.0.1:10022"].exists)
        element(in: app, identifier: "add-mapping-button").click()
        XCTAssertTrue(app.textFields["mapping-port-field"].waitForExistence(timeout: 3))
        replaceText(in: app.textFields["mapping-port-field"], with: "12022")
        app.buttons["mapping-save-button"].click()
        XCTAssertTrue(app.staticTexts["127.0.0.1:12022"].waitForExistence(timeout: 4))

        element(in: app, identifier: "sidebar-diagnostics").click()
        XCTAssertTrue(app.staticTexts["Route diagnostics"].waitForExistence(timeout: 3))
        XCTAssertTrue(element(in: app, identifier: "route-path").exists)
        XCTAssertTrue(app.staticTexts["relay-shanghai"].exists)
        XCTAssertTrue(app.staticTexts["Eligible, unverified"].exists)
        XCTAssertTrue(app.staticTexts["Recent failure"].exists)
        XCTAssertTrue(app.staticTexts["Bootstrap only"].exists)

        element(in: app, identifier: "sidebar-settings").click()
        XCTAssertTrue(app.staticTexts["Settings"].waitForExistence(timeout: 3))
        XCTAssertTrue(app.staticTexts["macOS 26"].exists)
        XCTAssertTrue(app.buttons["export-diagnostics-button"].exists)

        let travelToggle = app.buttons["travel-toggle"]
        travelToggle.click()
        XCTAssertTrue(travelToggle.wait(for: \.label, toEqual: "Start Travel", timeout: 4))
        travelToggle.click()
        XCTAssertTrue(travelToggle.wait(for: \.label, toEqual: "Stop Travel", timeout: 4))
    }

    @MainActor
    func testMenuBarIconUsesNativeStatusItemFootprint() throws {
        let app = makeApp(enrolled: true, allowTermination: true)
        addTeardownBlock { app.terminate() }
        app.launch()

        let statusItem = app.statusItems.firstMatch
        XCTAssertTrue(statusItem.waitForExistence(timeout: 5))

        let screenshot = XCUIScreen.main.screenshot()
        let attachment = XCTAttachment(screenshot: screenshot)
        attachment.name = "FlowSplice-menu-bar-native-footprint"
        attachment.lifetime = .keepAlways
        add(attachment)

        // macOS 26 gives the selected status item a 42-point hit target around
        // the explicitly 18-point glyph. Guard against app-icon sizing without
        // mistaking the system-provided selection background for image bounds.
        XCTAssertLessThanOrEqual(statusItem.frame.width, 48, "The menu-bar item must not use app-icon dimensions.")
        XCTAssertLessThanOrEqual(statusItem.frame.height, 32, "The menu-bar item must fit the native menu-bar height.")
    }

    @MainActor
    func testCloseButtonHidesButDoesNotTerminate() throws {
        let app = makeApp(enrolled: true)
        addTeardownBlock { self.forceTerminateFlowSpliceApp() }
        app.launch()
        showMainWindowIfNeeded(in: app)
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 6))

        let closeButton = app.windows.firstMatch.buttons[XCUIIdentifierCloseWindow]
        XCTAssertTrue(closeButton.waitForExistence(timeout: 2))
        closeButton.click()

        verifyAppIsHiddenButRecoverable(app)
    }

    @MainActor
    func testCommandQHidesButDoesNotTerminate() throws {
        let app = makeApp(enrolled: true)
        addTeardownBlock { self.forceTerminateFlowSpliceApp() }
        app.launch()
        showMainWindowIfNeeded(in: app)
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 6))

        app.typeKey("q", modifierFlags: .command)
        verifyAppIsHiddenButRecoverable(app)
    }

    @MainActor
    func testDockQuitHidesButDoesNotTerminate() throws {
        let app = makeApp(enrolled: true)
        addTeardownBlock { self.forceTerminateFlowSpliceApp() }
        app.launch()
        showMainWindowIfNeeded(in: app)
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 6))

        app.typeKey(.F3, modifierFlags: .control)
        let dock = XCUIApplication(bundleIdentifier: "com.apple.dock")
        XCTAssertTrue(dock.waitForExistence(timeout: 2))
        let flowIcon = dockIconElement(in: dock)
        let flowIconExists = flowIcon.waitForExistence(timeout: 4)
        if !flowIconExists { attachDebugHierarchy(dock.debugDescription, name: "dock-hierarchy-before-quit") }
        XCTAssertTrue(flowIconExists)
        if !waitForHittable(flowIcon, timeout: 2) {
            // Apple documents both Control-F3 and Fn-Control-F3 because the
            // required form depends on the keyboard's function-key setting.
            app.typeKey(.F3, modifierFlags: [.control, .function])
        }
        let flowIconIsHittable = waitForHittable(flowIcon, timeout: 4)
        if !flowIconIsHittable { attachDebugHierarchy(dock.debugDescription, name: "dock-hierarchy-not-hittable") }
        XCTAssertTrue(flowIconIsHittable)
        flowIcon.rightClick()
        let dockQuitMenuItem = dock.descendants(matching: .menuItem).matching(
            NSPredicate(
                format: "identifier CONTAINS[c] %@ OR title BEGINSWITH[c] %@ OR title BEGINSWITH[c] %@",
                "quit",
                "Quit",
                "退出"
            )
        ).firstMatch
        XCTAssertTrue(dockQuitMenuItem.waitForExistence(timeout: 3))
        attachDebugHierarchy(dock.debugDescription, name: "dock-hierarchy-with-context-menu")
        dockQuitMenuItem.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).click()

        verifyAppIsHiddenButRecoverable(app)
    }

    @MainActor
    func testStatusBarRealQuitShowsCancelAndConfirm() throws {
        let app = makeApp(enrolled: true)
        addTeardownBlock { self.forceTerminateFlowSpliceApp() }
        app.launch()
        showMainWindowIfNeeded(in: app)
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 6))

        let statusItem = app.statusItems.firstMatch
        XCTAssertTrue(statusItem.waitForExistence(timeout: 3))
        statusItem.click()
        let trueQuitButton = element(in: app, identifier: "menu-true-quit")
        XCTAssertTrue(trueQuitButton.waitForExistence(timeout: 3))
        trueQuitButton.click()
        let quitDialog = app.dialogs.matching(
            NSPredicate(format: "label ==[c] %@ OR identifier == %@", "alert", "_NS:87")
        ).firstMatch
        XCTAssertTrue(quitDialog.waitForExistence(timeout: 3))
        attachDebugHierarchy(quitDialog.debugDescription, name: "statusbar-quit-dialog-before-cancel")
        let cancelButton = dialogButton(in: quitDialog, title: "Cancel", identifier: "action-button-2")
        XCTAssertTrue(cancelButton.waitForExistence(timeout: 3))
        cancelButton.click()
        XCTAssertNotEqual(app.state, .notRunning)
        XCTAssertTrue(app.windows.firstMatch.waitForExistence(timeout: 5))

        statusItem.click()
        XCTAssertTrue(trueQuitButton.waitForExistence(timeout: 3))
        trueQuitButton.click()
        XCTAssertTrue(quitDialog.waitForExistence(timeout: 3))
        attachDebugHierarchy(quitDialog.debugDescription, name: "statusbar-quit-dialog-before-confirm")
        let confirmButton = dialogButton(in: quitDialog, title: "Quit and Disconnect", identifier: "action-button-1")
        XCTAssertTrue(confirmButton.waitForExistence(timeout: 3))
        confirmButton.click()

        assertFlowSpliceAppTerminated(timeout: 12)
    }

    @MainActor
    func testStatusBarRealQuitConfirmsWithNoActiveFlows() throws {
        let app = makeApp(enrolled: false)
        addTeardownBlock { self.forceTerminateFlowSpliceApp() }
        app.launch()
        XCTAssertTrue(app.staticTexts["Enroll this Mac"].waitForExistence(timeout: 5))

        let statusItem = app.statusItems.firstMatch
        XCTAssertTrue(statusItem.waitForExistence(timeout: 3))
        statusItem.click()
        let trueQuitButton = element(in: app, identifier: "menu-true-quit")
        XCTAssertTrue(trueQuitButton.waitForExistence(timeout: 3))
        trueQuitButton.click()

        let quitDialog = app.dialogs.matching(
            NSPredicate(format: "label ==[c] %@ OR identifier == %@", "alert", "_NS:87")
        ).firstMatch
        XCTAssertTrue(quitDialog.waitForExistence(timeout: 3))
        XCTAssertTrue(quitDialog.staticTexts["This fully exits FlowSplice. Closing the window, pressing Command-Q, or using Dock Quit only hides it."].exists)
        let confirmButton = dialogButton(in: quitDialog, title: "Quit FlowSplice", identifier: "action-button-1")
        XCTAssertTrue(confirmButton.waitForExistence(timeout: 3))
        confirmButton.click()

        assertFlowSpliceAppTerminated(timeout: 12)
    }

    @MainActor
    func testDockerRemoteEnrollmentMappingDiagnosticsAndRecovery() throws {
        let environment = ProcessInfo.processInfo.environment
        guard environment["FLOWSPLICE_MACOS_E2E"] == "1" else {
            throw XCTSkip("The Docker-backed macOS E2E was not requested.")
        }
        guard tcpPortIsReachable(host: "127.0.0.1", port: 18_446) else {
            throw XCTSkip("The local Docker Relay is not running.")
        }

        let relay = environment["FLOWSPLICE_MACOS_E2E_RELAY"] ?? "127.0.0.1:18446"
        let password = environment["FLOWSPLICE_MACOS_E2E_PASSWORD"] ?? "flowsplice-e2e-private-key-password"
        let travelID = environment["FLOWSPLICE_MACOS_E2E_TRAVEL_ID"] ?? "macos-e2e-travel"
        let localPort = UInt16(environment["FLOWSPLICE_MACOS_E2E_LOCAL_PORT"] ?? "10800") ?? 10_800
        let markerToken = environment["FLOWSPLICE_MACOS_E2E_MARKER_TOKEN"] ?? UUID().uuidString
        let markerDirectory = try XCTUnwrap(
            FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
        )
        .appending(path: "FlowSpliceMacE2E", directoryHint: .isDirectory)
        .appending(path: markerToken, directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: markerDirectory, withIntermediateDirectories: true)

        let app = XCUIApplication()
        app.launchEnvironment["FLOWSPLICE_UI_TESTING"] = "1"
        applyE2EKeychainEnvironment(to: app)
        app.launchArguments = ["--reset-state", "--real-backend", "--ui-test-allow-termination"]
        addTeardownBlock {
            app.terminate()
            let cleanup = XCUIApplication()
            cleanup.launchEnvironment["FLOWSPLICE_UI_TESTING"] = "1"
            self.applyE2EKeychainEnvironment(to: cleanup)
            cleanup.launchArguments = ["--reset-state", "--ui-test-allow-termination"]
            cleanup.launch()
            cleanup.terminate()
        }
        app.launch()
        showMainWindowIfNeeded(in: app)

        XCTAssertTrue(app.staticTexts["Enroll this Mac"].waitForExistence(timeout: 10))
        replaceText(in: app.textFields["enrollment-travel-id"], with: travelID)
        replaceText(in: app.textFields["enrollment-home-id"], with: "home-1")
        replaceText(in: app.textFields["enrollment-relay"], with: relay)
        replaceText(in: app.secureTextFields["enrollment-password"], with: password)
        replaceText(in: app.secureTextFields["enrollment-password-confirmation"], with: password)
        app.buttons["enrollment-submit"].click()

        let verificationCodeElement = element(in: app, identifier: "enrollment-verification-code")
        guard verificationCodeElement.waitForExistence(timeout: 45) else {
            let detail = flowSpliceErrorDetail(in: app)
            XCTFail("Enrollment did not expose a verification code. \(detail)")
            throw URLError(.cannotConnectToHost)
        }
        let labelExpectation = XCTNSPredicateExpectation(
            predicate: NSPredicate { object, _ in
                guard let element = object as? XCUIElement else { return false }
                return !element.label.isEmpty
            },
            object: verificationCodeElement
        )
        guard XCTWaiter.wait(for: [labelExpectation], timeout: 5) == .completed else {
            XCTFail(
                "Enrollment verification-code accessibility label did not become readable. "
                    + verificationCodeElement.debugDescription
            )
            throw URLError(.cannotParseResponse)
        }
        let verificationCode = verificationCodeElement.label
        guard !verificationCode.isEmpty else {
            XCTFail("Enrollment verification-code accessibility element did not expose its label.")
            throw URLError(.cannotParseResponse)
        }
        try writeMarker("verification-code", value: verificationCode, in: markerDirectory)

        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 180))
        element(in: app, identifier: "sidebar-mappings").click()
        XCTAssertTrue(app.staticTexts["Local service mappings"].waitForExistence(timeout: 10))
        element(in: app, identifier: "add-mapping-button").click()
        XCTAssertTrue(app.textFields["mapping-port-field"].waitForExistence(timeout: 10))

        let servicePicker = element(in: app, identifier: "mapping-service-picker")
        XCTAssertTrue(servicePicker.waitForExistence(timeout: 10))
        servicePicker.click()
        let tcpEcho = element(in: app, identifier: "mapping-service-tcp-echo-tcp")
        XCTAssertTrue(tcpEcho.waitForExistence(timeout: 10))
        tcpEcho.click()

        replaceText(in: app.textFields["mapping-port-field"], with: String(localPort))
        app.buttons["mapping-save-button"].click()
        XCTAssertTrue(app.staticTexts["127.0.0.1:\(localPort)"].waitForExistence(timeout: 30))
        try assertEcho("macos-foreground", port: localPort)

        // The test deliberately keeps the selected Relay offline for five seconds.
        // Observe beyond Travel Core's 60-second recovery deadline so a socket
        // timeout cannot be mistaken for a failed same-Flow recovery.
        var persistentFlow: Int32? = try openEchoConnection(port: localPort, timeoutSeconds: 75)
        defer {
            if let persistentFlow { close(persistentFlow) }
        }
        try exchangeEcho("macos-persistent-before-outage", on: persistentFlow!)

        element(in: app, identifier: "sidebar-diagnostics").click()
        XCTAssertTrue(app.staticTexts["Route diagnostics"].waitForExistence(timeout: 10))
        let routePath = element(in: app, identifier: "route-path")
        XCTAssertTrue(routePath.waitForExistence(timeout: 30))
        let selectedRelay: String
        if routePath.label.contains("relay-1") {
            selectedRelay = "relay-1"
        } else if routePath.label.contains("relay-2") {
            selectedRelay = "relay-2"
        } else {
            XCTFail("The active Flow did not expose a concrete Relay: \(routePath.label)")
            throw URLError(.cannotParseResponse)
        }
        XCTAssertTrue(element(in: app, identifier: "relay-row-relay-1").waitForExistence(timeout: 20))
        XCTAssertTrue(element(in: app, identifier: "relay-row-relay-2").waitForExistence(timeout: 20))
        try writeMarker("selected-relay", value: selectedRelay, in: markerDirectory)
        try writeMarker("network-outage-ready", value: "ready", in: markerDirectory)
        try waitForMarker("network-outage-complete", in: markerDirectory, timeout: 180)
        try exchangeEcho("macos-persistent-after-outage", on: persistentFlow!)
        close(persistentFlow!)
        persistentFlow = nil

        try assertEcho("macos-new-flow-after-outage", port: localPort)

        let appMenu = app.menuBars.menuBarItems["FlowSpliceMac"]
        appMenu.click()
        let hideItem = app.menuItems["Hide FlowSplice to Menu Bar"]
        XCTAssertTrue(hideItem.waitForExistence(timeout: 3))
        hideItem.click()
        XCTAssertNotEqual(app.state, .notRunning)
        XCTAssertFalse(app.windows.firstMatch.exists)
        try assertEcho("macos-menu-bar-only", port: localPort)
        showMainWindowIfNeeded(in: app)
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 20))

        let travelToggle = app.buttons["travel-toggle"]
        travelToggle.click()
        XCTAssertTrue(travelToggle.wait(for: \.label, toEqual: "Start Travel", timeout: 30))
        travelToggle.click()
        XCTAssertTrue(travelToggle.wait(for: \.label, toEqual: "Stop Travel", timeout: 60))
        try assertEcho("macos-after-runtime-restart", port: localPort)

        app.terminate()
        applyE2EKeychainEnvironment(to: app)
        app.launchArguments = ["--real-backend", "--ui-test-allow-termination"]
        app.launch()
        showMainWindowIfNeeded(in: app)
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 120))
        try assertEcho("macos-after-cold-launch", port: localPort)
        try writeMarker("complete", value: "complete", in: markerDirectory)
    }

    private func makeApp(enrolled: Bool, allowTermination: Bool = false) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchEnvironment["FLOWSPLICE_UI_TESTING"] = "1"
        applyE2EKeychainEnvironment(to: app)
        app.launchArguments = ["--reset-state"]
        if allowTermination { app.launchArguments.append("--ui-test-allow-termination") }
        if enrolled { app.launchArguments.append("--mock-enrolled") }
        return app
    }

    private func applyE2EKeychainEnvironment(to app: XCUIApplication) {
        let environment = ProcessInfo.processInfo.environment
        if let path = environment["FLOWSPLICE_UI_TEST_KEYCHAIN_PATH"] {
            app.launchEnvironment["FLOWSPLICE_UI_TEST_KEYCHAIN_PATH"] = path
        }
        if let password = environment["FLOWSPLICE_UI_TEST_KEYCHAIN_PASSWORD"] {
            app.launchEnvironment["FLOWSPLICE_UI_TEST_KEYCHAIN_PASSWORD"] = password
        }
    }

    private func element(in app: XCUIApplication, identifier: String) -> XCUIElement {
        app.descendants(matching: .any)[identifier]
    }

    @MainActor
    private func showMainWindowIfNeeded(in app: XCUIApplication) {
        guard !app.windows.firstMatch.exists else { return }
        let statusItem = app.statusItems.firstMatch
        XCTAssertTrue(statusItem.waitForExistence(timeout: 3))
        statusItem.click()
        let openButton = app.buttons["Open FlowSplice"]
        if !openButton.waitForExistence(timeout: 1) {
            statusItem.click()
        }
        XCTAssertTrue(openButton.waitForExistence(timeout: 3))
        openButton.click()
        XCTAssertTrue(app.windows.firstMatch.waitForExistence(timeout: 3))
    }

    @MainActor
    private func replaceText(in field: XCUIElement, with value: String) {
        XCTAssertTrue(field.waitForExistence(timeout: 3))
        field.click()
        field.typeKey("a", modifierFlags: .command)
        field.typeText(value)
    }

    private func writeMarker(_ name: String, value: String, in directory: URL) throws {
        try Data(value.utf8).write(to: directory.appending(path: name), options: .atomic)
    }

    private func waitForMarker(_ name: String, in directory: URL, timeout: TimeInterval) throws {
        let marker = directory.appending(path: name)
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if FileManager.default.fileExists(atPath: marker.path) { return }
            Thread.sleep(forTimeInterval: 0.5)
        }
        XCTFail("Timed out waiting for E2E marker \(name).")
        throw URLError(.timedOut)
    }

    private func runningFlowSpliceApplications() -> [NSRunningApplication] {
        NSRunningApplication.runningApplications(withBundleIdentifier: Self.flowSpliceBundleIdentifier)
            .filter { !$0.isTerminated }
    }

    private func forceTerminateFlowSpliceApp() {
        let runningApps = runningFlowSpliceApplications()
        guard !runningApps.isEmpty else { return }
        for app in runningApps { _ = app.forceTerminate() }
    }

    private func assertFlowSpliceAppTerminated(
        timeout: TimeInterval = 5.0,
        file: StaticString = #file,
        line: UInt = #line
    ) {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if runningFlowSpliceApplications().isEmpty { return }
            Thread.sleep(forTimeInterval: 0.1)
        }
        XCTFail("FlowSplice app should terminate.", file: file, line: line)
    }

    private func assertFlowSpliceAppIsRunning(
        file: StaticString = #file,
        line: UInt = #line
    ) {
        XCTAssertFalse(runningFlowSpliceApplications().isEmpty, "FlowSplice app should keep running.", file: file, line: line)
    }

    @MainActor
    private func verifyAppIsHiddenButRecoverable(_ app: XCUIApplication) {
        XCTAssertNotEqual(app.state, .notRunning)
        assertFlowSpliceAppIsRunning()
        XCTAssertTrue(waitForMainWindowToDisappear(app: app))
        showMainWindowIfNeeded(in: app)
        XCTAssertTrue(app.windows.firstMatch.waitForExistence(timeout: 3))
    }

    private func waitForMainWindowToDisappear(
        app: XCUIApplication,
        timeout: TimeInterval = 5
    ) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if !app.windows.firstMatch.exists { return true }
            Thread.sleep(forTimeInterval: 0.1)
        }
        return false
    }

    private func waitForHittable(_ element: XCUIElement, timeout: TimeInterval) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if element.exists, element.isHittable { return true }
            Thread.sleep(forTimeInterval: 0.1)
        }
        return element.exists && element.isHittable
    }

    @MainActor
    private func dockIconElement(in dock: XCUIApplication) -> XCUIElement {
        let predicate = NSPredicate(format: "title CONTAINS[c] 'FlowSplice' OR identifier CONTAINS[c] 'FlowSplice'")
        return dock.descendants(matching: .dockItem).matching(predicate).firstMatch
    }

    private func dialogButton(in dialog: XCUIElement, title: String, identifier: String) -> XCUIElement {
        return dialog
            .descendants(matching: .button)
            .matching(
                NSPredicate(
                    format: "identifier ==[c] %@ OR label ==[c] %@ OR title ==[c] %@",
                    identifier,
                    title,
                    title
                )
            )
            .firstMatch
    }

    private func attachDebugHierarchy(_ value: String, name: String) {
        let attachment = XCTAttachment(string: value)
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    private func flowSpliceErrorDetail(in app: XCUIApplication) -> String {
        let containers: [(name: String, element: XCUIElement)] = [
            ("sheet", app.sheets.firstMatch),
            ("alert", app.alerts.firstMatch),
            ("dialog", app.dialogs.firstMatch),
        ]
        for container in containers where container.element.exists {
            attachDebugHierarchy(
                container.element.debugDescription,
                name: "enrollment-error-\(container.name)-hierarchy"
            )
            let labels = container.element.staticTexts.allElementsBoundByIndex
                .map(\.label)
                .filter { !$0.isEmpty }
            if !labels.isEmpty {
                return "FlowSplice presented a \(container.name): \(labels.joined(separator: " · "))"
            }
            return "FlowSplice presented a \(container.name), but it contained no readable detail text."
        }
        attachDebugHierarchy(app.debugDescription, name: "enrollment-failure-app-hierarchy")
        return "No FlowSplice error sheet, alert, or dialog was presented."
    }

    private func assertEcho(_ value: String, port: UInt16, timeout: TimeInterval = 90) throws {
        let deadline = Date().addingTimeInterval(timeout)
        var lastError: Error?
        repeat {
            do {
                let descriptor = try openEchoConnection(port: port)
                defer { close(descriptor) }
                try exchangeEcho(value, on: descriptor)
                return
            } catch {
                lastError = error
                Thread.sleep(forTimeInterval: 1)
            }
        } while Date() < deadline
        let error = lastError ?? URLError(.timedOut)
        XCTFail("TCP echo did not recover: \(error.localizedDescription)")
        throw error
    }

    private func openEchoConnection(port: UInt16, timeoutSeconds: Int = 3) throws -> Int32 {
        let descriptor = socket(AF_INET, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw URLError(.cannotCreateFile) }
        var timeout = timeval(tv_sec: timeoutSeconds, tv_usec: 0)
        setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
        setsockopt(descriptor, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        guard inet_pton(AF_INET, "127.0.0.1", &address.sin_addr) == 1 else {
            close(descriptor)
            throw URLError(.badURL)
        }
        let connected = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(descriptor, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard connected == 0 else {
            close(descriptor)
            throw URLError(.cannotConnectToHost)
        }
        return descriptor
    }

    private func exchangeEcho(_ value: String, on descriptor: Int32) throws {
        let payload = Array("\(value)\n".utf8)
        var sent = 0
        while sent < payload.count {
            let written = payload.withUnsafeBytes { buffer in
                send(descriptor, buffer.baseAddress!.advanced(by: sent), buffer.count - sent, 0)
            }
            guard written > 0 else { throw URLError(.networkConnectionLost) }
            sent += written
        }

        var response: [UInt8] = []
        var chunk = [UInt8](repeating: 0, count: 4_096)
        while response.last != UInt8(ascii: "\n") && response.count < 65_536 {
            let read = chunk.withUnsafeMutableBytes { buffer in
                recv(descriptor, buffer.baseAddress!, buffer.count, 0)
            }
            guard read > 0 else { throw URLError(.networkConnectionLost) }
            response.append(contentsOf: chunk.prefix(read))
        }
        guard response.firstIndex(of: UInt8(ascii: ":")) != nil,
              response.suffix(payload.count).elementsEqual(payload) else {
            throw URLError(.cannotParseResponse)
        }
    }

    private func tcpPortIsReachable(host: String, port: UInt16) -> Bool {
        let descriptor = socket(AF_INET, SOCK_STREAM, 0)
        guard descriptor >= 0 else { return false }
        defer { close(descriptor) }
        var timeout = timeval(tv_sec: 2, tv_usec: 0)
        setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = port.bigEndian
        guard inet_pton(AF_INET, host, &address.sin_addr) == 1 else { return false }
        return withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(descriptor, $0, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
            }
        }
    }

}
