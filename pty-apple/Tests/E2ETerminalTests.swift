import XCTest
import Vision
#if os(macOS)
import Carbon
#endif

/// Requires an isolated installation and external approval orchestration; never self-approves.
final class E2ETerminalTests: XCTestCase {
    private var app: XCUIApplication!
#if os(macOS)
    private var originalInputSource: TISInputSource?
#endif

    override func setUpWithError() throws {
        continueAfterFailure = false
        let environment = ProcessInfo.processInfo.environment
        guard environment["FLOWSPLICE_PTY_E2E_ISOLATED"] == "1" else {
            throw XCTSkip("Requires an explicitly isolated simulator or macOS user installation")
        }
        app = XCUIApplication()
        app.launchEnvironment["FLOWSPLICE_PTY_TEST_NAMESPACE"] = UUID().uuidString
#if os(macOS)
        originalInputSource = TISCopyCurrentKeyboardInputSource().takeRetainedValue()
        let asciiInputSource = TISCopyCurrentASCIICapableKeyboardInputSource().takeRetainedValue()
        XCTAssertEqual(TISSelectInputSource(asciiInputSource), noErr)
        // A failed older local test build can leave this specific Gatekeeper alert
        // on screen after its process exits. Cancel it; never override the policy.
        let system = XCUIApplication(bundleIdentifier: "com.apple.coreservices.uiagent")
        for dialog in system.dialogs.allElementsBoundByIndex {
            let runner = dialog.staticTexts.matching(NSPredicate(
                format: "value CONTAINS %@ OR label CONTAINS %@",
                "FlowSplicePTY-macOSUITests-Runner", "FlowSplicePTY-macOSUITests-Runner")).firstMatch
            if runner.exists && dialog.buttons["取消"].exists { dialog.buttons["取消"].click() }
        }
#else
        // A previous screen-off acceptance run may leave this dedicated device asleep.
        XCUIDevice.shared.press(.home)
#endif
    }

    override func tearDown() {
        app?.terminate()
#if os(macOS)
        if let originalInputSource { XCTAssertEqual(TISSelectInputSource(originalInputSource), noErr) }
#endif
        super.tearDown()
    }

    private func tap(_ element: XCUIElement, timeout: TimeInterval = 20) {
        XCTAssertTrue(element.waitForExistence(timeout: timeout), "Required PTY control is missing")
        allowOwnLocalNetworkPrompt()
        wait(NSPredicate(format: "enabled == true"), object: element, timeout: timeout)
#if os(macOS)
        element.click()
#else
        element.tap()
#endif
    }

    private func allowOwnLocalNetworkPrompt() {
#if os(macOS)
        let system = XCUIApplication(bundleIdentifier: "com.apple.UserNotificationCenter")
        guard system.state != .notRunning else { return }
        let dialogs = system.dialogs.allElementsBoundByIndex + system.alerts.allElementsBoundByIndex
#else
        let dialogs = XCUIApplication(bundleIdentifier: "com.apple.springboard").alerts.allElementsBoundByIndex
#endif
        for dialog in dialogs {
            let message = dialog.staticTexts.allElementsBoundByIndex
                .map { $0.label + " " + ($0.value as? String ?? "") }.joined(separator: " ")
            guard message.contains("FlowSplice"),
                  message.contains("本地网络") || message.lowercased().contains("local network") else { continue }
            for title in ["允许", "Allow", "好", "OK"] where dialog.buttons[title].exists {
#if os(macOS)
                dialog.buttons[title].click()
#else
                dialog.buttons[title].tap()
#endif
                return
            }
        }
    }

    private func text(_ value: String) -> XCUIElement {
        app.webViews.staticTexts.matching(NSPredicate(format: "label == %@ OR value == %@", value, value)).firstMatch
    }

    private func waitForText(containing fragment: String, timeout: TimeInterval) -> String {
        var found: String?
        // WebKit exposes text as label on iOS and value on macOS. Heading
        // values can be numbers, so string predicates cannot safely query all values.
        let expectation = XCTNSPredicateExpectation(predicate: NSPredicate { [unowned self] _, _ in
            for element in self.app.webViews.staticTexts.allElementsBoundByIndex {
                for value in [element.label, element.value as? String ?? ""] where value.contains(fragment) {
                    found = value
                    return true
                }
            }
            return false
        }, object: app)
        XCTAssertEqual(XCTWaiter.wait(for: [expectation], timeout: timeout), .completed,
            "Required visible text is missing: \(fragment)")
        return found ?? ""
    }

    private func wait(_ predicate: NSPredicate, object: Any, timeout: TimeInterval = 30) {
        let expectation = XCTNSPredicateExpectation(predicate: predicate, object: object)
        XCTAssertEqual(XCTWaiter.wait(for: [expectation], timeout: timeout), .completed)
    }

    private func waitForRenderedOutput(_ marker: String) {
        // Inspect actual pixels without changing xterm's input/accessibility mode
        // or exposing a product test hook. The submitted command contains octal
        // escapes, so only output from the remote shell can contain this marker.
        let expected = marker.filter { $0.isLetter || $0.isNumber }.uppercased()
        let expectation = XCTNSPredicateExpectation(predicate: NSPredicate { [unowned self] _, _ in
            let request = VNRecognizeTextRequest()
            request.recognitionLevel = .accurate
            request.recognitionLanguages = ["en-US"]
            request.usesLanguageCorrection = false
            let screenshot = self.app.windows.firstMatch.screenshot()
            do {
                try VNImageRequestHandler(data: screenshot.pngRepresentation, options: [:]).perform([request])
                // Vision may split one terminal line into adjacent observations.
                let rendered = (request.results ?? []).compactMap { $0.topCandidates(1).first?.string }
                    .joined().filter { $0.isLetter || $0.isNumber }.uppercased()
                return rendered.contains(expected)
            } catch { return false }
        }, object: app)
        let result = XCTWaiter.wait(for: [expectation], timeout: 20)
        let screenshot = XCTAttachment(screenshot: app.windows.firstMatch.screenshot())
        screenshot.name = "terminal"
        screenshot.lifetime = .keepAlways
        add(screenshot)
        XCTAssertEqual(result, .completed, "Remote output marker is absent from the rendered terminal")
    }

    func testEnrollmentSessionAndForegroundLifecycle() throws {
        let environment = ProcessInfo.processInfo.environment
        let relay = try XCTUnwrap(environment["FLOWSPLICE_PTY_E2E_RELAY"], "External fixture Relay is required")
        let passwordFile = try XCTUnwrap(environment["FLOWSPLICE_PTY_E2E_PASSWORD_FILE"], "External password file is required")
        XCTAssertTrue(passwordFile.hasPrefix("/"), "Password file must be absolute")
        let password = try String(contentsOfFile: passwordFile, encoding: .utf8)
            .trimmingCharacters(in: .newlines)
        XCTAssertFalse(password.isEmpty, "Fixture password cannot be empty")
        app.launch()
        let web = app.webViews.firstMatch
        XCTAssertTrue(web.waitForExistence(timeout: 20))
        let relayField = web.textFields.firstMatch
        tap(relayField); relayField.typeText(relay)
        XCTAssertEqual(relayField.value as? String, relay, "Relay input must reach the actual field unchanged")
        let passwordField = web.secureTextFields.firstMatch
        tap(passwordField); passwordField.typeText(password)
        tap(web.buttons["注册"])
        let pending = waitForText(containing: "等待批准", timeout: 60)
        print("PTY_E2E_VERIFICATION " + pending)
        // Parent observes the actual pending request and approves it outside this UI process.
        tap(web.buttons["连接"], timeout: 120)
        // Debug Rust builds execute both private-key derivations without release
        // optimizations. Keep the real cryptography and allow its measured latency.
        XCTAssertTrue(text("已连接").waitForExistence(timeout: 120))
        XCTAssertTrue(text("暂无会话").waitForExistence(timeout: 30), "Fixture must begin without sessions")
        tap(web.buttons["新建"])
        XCTAssertTrue(web.buttons["切换只读"].waitForExistence(timeout: 30))
        XCTAssertTrue(text("读写").exists)
        for label in ["Ctrl+C", "Tab", "Esc", "↑", "↓", "←", "→"] {
#if os(macOS)
            XCTAssertFalse(web.buttons[label].exists, "macOS must use the physical keyboard without virtual terminal keys")
#else
            XCTAssertTrue(web.buttons[label].exists, "iOS must retain its virtual terminal keys")
#endif
        }

        // xterm's real accessibility input is used, with no direct Rust or JS test hook.
        let terminal = web.textViews.firstMatch
        XCTAssertTrue(terminal.waitForExistence(timeout: 10), "xterm input is not exposed through WKWebView accessibility")
        tap(terminal)
        // Avoid slashed zero and lookalike I/O glyphs in pixel-only acceptance.
        let alphabet = Array("ABCDEFGHJKLMNPRS")
        let random = UUID().uuidString.replacingOccurrences(of: "-", with: "").prefix(8)
        let marker = "PTYCHECK" + String(random.map { alphabet[$0.hexDigitValue!] })
        let octal = marker.utf8.map { String(format: "\\%03o", Int($0)) }.joined()
        terminal.typeText("printf '\\n" + octal + "\\n'\n")
        waitForRenderedOutput(marker)
        tap(web.buttons["切换只读"])
        XCTAssertTrue(web.buttons["申请读写"].waitForExistence(timeout: 20))
        XCTAssertTrue(text("只读").exists)
        tap(web.buttons["申请读写"])
        XCTAssertTrue(web.buttons["切换只读"].waitForExistence(timeout: 20))

#if os(macOS)
        app.typeKey("h", modifierFlags: .command)
        wait(NSPredicate(format: "state == %d", XCUIApplication.State.runningBackground.rawValue), object: app)
#else
        XCUIDevice.shared.press(.home)
        wait(NSPredicate(format: "state == %d", XCUIApplication.State.runningBackground.rawValue), object: app)
#endif
        app.activate()
        XCTAssertTrue(text("未连接").waitForExistence(timeout: 20))
        XCTAssertFalse(web.buttons["关闭标签"].exists)
        let staysDisconnected = XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == true"), object: text("已连接"))
        staysDisconnected.isInverted = true
        XCTAssertEqual(XCTWaiter.wait(for: [staysDisconnected], timeout: 2), .completed)
        tap(web.buttons["连接"])
        XCTAssertTrue(text("已连接").waitForExistence(timeout: 120))
        let joins = web.buttons.matching(identifier: "读写加入")
        XCTAssertTrue(joins.firstMatch.waitForExistence(timeout: 20))
        XCTAssertEqual(joins.count, 1, "Reconnect must list the existing session without creating another")
        XCTAssertFalse(web.buttons["关闭标签"].exists, "Reconnect must not join automatically")
        tap(joins.firstMatch)
        XCTAssertTrue(web.buttons["切换只读"].waitForExistence(timeout: 20))
        tap(web.buttons["断开"])
        XCTAssertTrue(text("未连接").waitForExistence(timeout: 20))
    }
}
