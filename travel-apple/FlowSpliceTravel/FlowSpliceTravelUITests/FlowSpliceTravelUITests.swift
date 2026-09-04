import Foundation
import Darwin
import UIKit
import XCTest

final class FlowSpliceTravelUITests: XCTestCase {
    override func setUpWithError() throws {
        continueAfterFailure = false
    }

    @MainActor
    func testEnrollmentUsesNativeAdaptiveForm() {
        let app = resetApplication()
        app.launch()

        XCTAssertTrue(app.staticTexts["Connect to your Home"].waitForExistence(timeout: 10))
        XCTAssertTrue(app.textFields["enrollment-travel-id"].exists)
        XCTAssertTrue(app.textFields["enrollment-home-id"].exists)
        XCTAssertTrue(app.textFields["enrollment-relay"].exists)
        XCTAssertFalse(app.buttons["enrollment-submit"].isEnabled)
    }

    @MainActor
    func testEnrollmentValidationEnablesSubmission() {
        let app = resetApplication()
        app.launch()

        replaceText(in: app.textFields["enrollment-travel-id"], with: "apple-ui-test")
        replaceText(in: app.textFields["enrollment-home-id"], with: "home-1")
        replaceText(in: app.textFields["enrollment-relay"], with: "127.0.0.1:18446")
        app.secureTextFields["enrollment-password"].tap()
        app.secureTextFields["enrollment-password"].typeText("correct-horse-battery")
        app.secureTextFields["enrollment-confirmation"].tap()
        app.secureTextFields["enrollment-confirmation"].typeText("correct-horse-battery")

        XCTAssertTrue(app.buttons["enrollment-submit"].isEnabled)
    }

    @MainActor
    func testDockerRemoteEnrollmentCatalogMappingAndRecovery() throws {
        let environment = ProcessInfo.processInfo.environment
        guard tcpPortIsReachable(host: "127.0.0.1", port: 18_446) else {
            throw XCTSkip("The local Docker Relay is not running.")
        }
        let relay = argumentValue("--flowsplice-e2e-relay")
            ?? environment["FLOWSPLICE_APPLE_E2E_RELAY"]
            ?? "127.0.0.1:18446"
        let password = environment["FLOWSPLICE_APPLE_E2E_PASSWORD"] ?? "flowsplice-e2e-private-key-password"
        let defaultTravelID = UIDevice.current.userInterfaceIdiom == .pad ? "apple-e2e-ipad-mini" : "apple-e2e-iphone"
        let travelID = argumentValue("--flowsplice-e2e-travel-id")
            ?? environment["FLOWSPLICE_APPLE_E2E_TRAVEL_ID"]
            ?? defaultTravelID
        let backgroundSeconds = TimeInterval(
            argumentValue("--flowsplice-e2e-background-seconds")
                ?? environment["FLOWSPLICE_APPLE_E2E_BACKGROUND_SECONDS"]
                ?? "120"
        ) ?? 120
        let sustainedBackgroundSeconds = TimeInterval(
            argumentValue("--flowsplice-e2e-sustained-seconds")
                ?? environment["FLOWSPLICE_APPLE_E2E_SUSTAINED_SECONDS"]
                ?? "900"
        ) ?? 900
        let app = XCUIApplication()
        app.launchEnvironment = [
            "FLOWSPLICE_UI_TEST_RESET": "1",
            "FLOWSPLICE_E2E": "1",
            "FLOWSPLICE_E2E_RELAY": relay,
            "FLOWSPLICE_E2E_TRAVEL_ID": travelID,
            "FLOWSPLICE_E2E_PASSWORD": password,
        ]
        app.launch()

        XCTAssertEqual(app.textFields["enrollment-travel-id"].value as? String, travelID)
        XCTAssertEqual(app.textFields["enrollment-home-id"].value as? String, "home-1")
        XCTAssertEqual(app.textFields["enrollment-relay"].value as? String, relay)
        XCTAssertTrue(app.buttons["enrollment-submit"].isEnabled)
        app.buttons["enrollment-submit"].tap()
        XCTAssertTrue(app.staticTexts["Compare this code on Home"].waitForExistence(timeout: 30))

        let status = app.descendants(matching: .any)["overview-status"]
        XCTAssertTrue(status.waitForExistence(timeout: 150))
        XCTAssertTrue(app.staticTexts["Online"].waitForExistence(timeout: 150))
        navigate(in: app, compactLabel: "Device", regularIdentifier: "nav-device")
        assertBackgroundAudioIsActive(in: app)
        assertLiveActivityIsVisible(in: app)
        navigate(in: app, compactLabel: "Mappings", regularIdentifier: "nav-mappings")
        XCTAssertTrue(app.buttons["mapping-add"].waitForExistence(timeout: 90))
        app.buttons["mapping-add"].tap()
        XCTAssertTrue(app.navigationBars["Add Local Mapping"].waitForExistence(timeout: 10))
        let servicePicker = app.descendants(matching: .any)["mapping-service-picker"]
        XCTAssertTrue(servicePicker.waitForExistence(timeout: 10))
        servicePicker.tap()
        let tcpEcho = app.descendants(matching: .any)["mapping-service-tcp-echo-tcp"]
        XCTAssertTrue(tcpEcho.waitForExistence(timeout: 10))
        tcpEcho.tap()
        let serviceSelection = app.navigationBars["Service"]
        if serviceSelection.waitForExistence(timeout: 2) {
            let back = serviceSelection.buttons["Add Local Mapping"]
            XCTAssertTrue(back.waitForExistence(timeout: 5))
            back.tap()
        }
        let port = app.textFields["mapping-port"]
        XCTAssertTrue(port.wait(for: \.isHittable, toEqual: true, timeout: 10))
        port.tap()
        port.typeText("10800")
        XCTAssertTrue(app.buttons["mapping-save"].isEnabled)
        app.buttons["mapping-save"].tap()
        XCTAssertTrue(app.descendants(matching: .any)["mapping-home-1-tcp-echo"].waitForExistence(timeout: 30))
        try assertEcho("apple-foreground-roundtrip")

        XCUIDevice.shared.press(.home)
        try assertEchoContinuously(
            "apple-sustained-background",
            duration: sustainedBackgroundSeconds
        )
        app.activate()
        XCTAssertTrue(app.navigationBars["Local Mappings"].waitForExistence(timeout: 30))
        XCTAssertTrue(app.descendants(matching: .any)["mapping-home-1-tcp-echo"].exists)
        try assertEcho("apple-background-recovery")

        app.terminate()
        app.launchEnvironment["FLOWSPLICE_UI_TEST_RESET"] = "0"
        app.launch()
        navigate(in: app, compactLabel: "Device", regularIdentifier: "nav-device")
        XCTAssertTrue(app.buttons["device-stop"].waitForExistence(timeout: 90))
        assertBackgroundAudioIsActive(in: app)
        assertLiveActivityIsVisible(in: app)
        navigate(in: app, compactLabel: "Mappings", regularIdentifier: "nav-mappings")
        XCTAssertTrue(app.descendants(matching: .any)["mapping-home-1-tcp-echo"].waitForExistence(timeout: 60))
        try assertEcho("apple-after-cold-launch-restart")

        navigate(in: app, compactLabel: "Diagnostics", regularIdentifier: "nav-diagnostics")
        let outage = app.buttons["diagnostics-prepare-network-outage"]
        XCTAssertTrue(outage.waitForExistence(timeout: 20))
        outage.tap()
        XCUIDevice.shared.press(.home)
        try assertEchoContinuously(
            "apple-after-relay-outage-while-backgrounded",
            duration: backgroundSeconds
        )
        app.activate()
        let networkChange = app.buttons["diagnostics-simulate-network-change"]
        XCTAssertTrue(networkChange.waitForExistence(timeout: 20))
        networkChange.tap()
        try assertEcho("apple-after-network-change")

        navigate(in: app, compactLabel: "Device", regularIdentifier: "nav-device")
        XCTAssertTrue(app.buttons["device-stop"].waitForExistence(timeout: 20))
        app.buttons["device-stop"].tap()
        XCTAssertTrue(app.buttons["device-start"].waitForExistence(timeout: 30))
        app.buttons["device-start"].tap()
        navigate(in: app, compactLabel: "Mappings", regularIdentifier: "nav-mappings")
        XCTAssertTrue(app.descendants(matching: .any)["mapping-home-1-tcp-echo"].waitForExistence(timeout: 60))
        try assertEcho("apple-after-runtime-restart")

        navigate(in: app, compactLabel: "Diagnostics", regularIdentifier: "nav-diagnostics")
        let screenOff = app.buttons["diagnostics-prepare-screen-off"]
        XCTAssertTrue(screenOff.waitForExistence(timeout: 20))
        screenOff.tap()
        XCUIDevice.shared.press(.home)
        try assertEchoContinuously(
            "apple-while-screen-off-backgrounded",
            duration: backgroundSeconds
        )
        app.activate()
        Thread.sleep(forTimeInterval: 2)
        try assertEcho("apple-after-screen-off")
        print("FLOWSPLICE_APPLE_E2E_COMPLETE")
    }

    @MainActor
    private func resetApplication() -> XCUIApplication {
        let app = XCUIApplication()
        app.launchEnvironment["FLOWSPLICE_UI_TEST_RESET"] = "1"
        return app
    }

    @MainActor
    private func replaceText(in field: XCUIElement, with text: String) {
        if field.value as? String == text { return }
        field.tap()
        if let current = field.value as? String, !current.isEmpty {
            field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: current.count))
        }
        field.typeText(text)
    }

    @MainActor
    private func navigate(in app: XCUIApplication, compactLabel: String, regularIdentifier: String) {
        let tab = app.tabBars.buttons[compactLabel]
        if tab.waitForExistence(timeout: 10) {
            XCTAssertTrue(tab.wait(for: \.isHittable, toEqual: true, timeout: 20))
            tab.tap()
            return
        }
        let sidebarItem = app.descendants(matching: .any)[regularIdentifier]
        XCTAssertTrue(sidebarItem.waitForExistence(timeout: 10))
        if sidebarItem.isHittable {
            sidebarItem.tap()
            return
        }

        let regularTitle = compactLabel == "Mappings" ? "Local Mappings" : compactLabel
        let sidebarTitle = app.staticTexts[regularTitle]
        XCTAssertTrue(sidebarTitle.wait(for: \.isHittable, toEqual: true, timeout: 20))
        sidebarTitle.tap()
    }

    @MainActor
    private func assertBackgroundAudioIsActive(in app: XCUIApplication) {
        let status = app.staticTexts["device-background-audio-status"]
        XCTAssertTrue(status.waitForExistence(timeout: 30))
        let active = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "label CONTAINS[c] %@", "Active"),
            object: status
        )
        XCTAssertEqual(
            XCTWaiter.wait(for: [active], timeout: 60),
            .completed,
            "Expected active background audio, got \(status.label)."
        )
    }

    @MainActor
    private func assertLiveActivityIsVisible(in app: XCUIApplication) {
        let status = app.staticTexts["device-live-activity-status"]
        XCTAssertTrue(status.waitForExistence(timeout: 30))
        let visible = XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "label CONTAINS[c] %@", "Visible"),
            object: status
        )
        XCTAssertEqual(
            XCTWaiter.wait(for: [visible], timeout: 60),
            .completed,
            "Expected a visible Live Activity, got \(status.label)."
        )
    }

    private func assertEcho(_ value: String, timeout: TimeInterval = 60) throws {
        let deadline = Date().addingTimeInterval(timeout)
        var lastError: Error?
        repeat {
            do {
                try exchangeEcho(value)
                return
            } catch {
                lastError = error
                Thread.sleep(forTimeInterval: 1)
            }
        } while Date() < deadline
        let error = lastError ?? URLError(.timedOut)
        XCTFail("TCP echo did not recover within \(Int(timeout)) seconds: \(error.localizedDescription)")
        throw error
    }

    private func assertEchoContinuously(
        _ prefix: String,
        duration: TimeInterval,
        interval: TimeInterval = 5
    ) throws {
        let startedAt = Date()
        let deadline = startedAt.addingTimeInterval(duration)
        var probe = 0
        repeat {
            try exchangeEcho("\(prefix)-\(probe)")
            if probe.isMultiple(of: 6) {
                print(
                    "FLOWSPLICE_APPLE_E2E_HEARTBEAT phase=\(prefix) " +
                    "elapsed=\(Int(Date().timeIntervalSince(startedAt))) probe=\(probe)"
                )
            }
            probe += 1
            let remaining = deadline.timeIntervalSinceNow
            if remaining > 0 {
                Thread.sleep(forTimeInterval: min(interval, remaining))
            }
        } while Date() < deadline
    }

    private func exchangeEcho(_ value: String) throws {
        let descriptor = socket(AF_INET, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw URLError(.cannotCreateFile) }
        defer { close(descriptor) }
        var timeout = timeval(tv_sec: 2, tv_usec: 0)
        setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
        setsockopt(descriptor, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout.size(ofValue: timeout)))
        var address = sockaddr_in()
        address.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        address.sin_family = sa_family_t(AF_INET)
        address.sin_port = UInt16(10_800).bigEndian
        guard inet_pton(AF_INET, "127.0.0.1", &address.sin_addr) == 1 else {
            throw URLError(.badURL)
        }
        let connected = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(descriptor, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard connected == 0 else { throw URLError(.cannotConnectToHost) }

        let payload = Array("\(value)\n".utf8)
        var sent = 0
        while sent < payload.count {
            let written = payload.withUnsafeBytes { buffer in
                send(descriptor, buffer.baseAddress!.advanced(by: sent), buffer.count - sent, 0)
            }
            guard written > 0 else { throw URLError(.networkConnectionLost) }
            sent += written
        }
        guard sent == payload.count else { throw URLError(.cannotWriteToFile) }

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
            throw NSError(
                domain: "FlowSpliceTravelUITests.Echo",
                code: 1,
                userInfo: [NSLocalizedDescriptionKey: "Unexpected echo response: \(String(decoding: response, as: UTF8.self))"]
            )
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

    private func argumentValue(_ name: String) -> String? {
        let arguments = ProcessInfo.processInfo.arguments
        guard let index = arguments.firstIndex(of: name) else { return nil }
        let valueIndex = arguments.index(after: index)
        guard arguments.indices.contains(valueIndex) else { return nil }
        return arguments[valueIndex]
    }
}
