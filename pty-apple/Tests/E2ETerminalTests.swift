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

    private func inputField(containing name: String) -> XCUIElement {
#if os(macOS)
        return app.webViews.textFields.matching(NSPredicate(format:"title CONTAINS %@", name)).firstMatch
#else
        return app.webViews.textFields.matching(NSPredicate(format:"label CONTAINS %@", name)).firstMatch
#endif
    }

    private func visibleTextSnapshot() -> [String] {
        // One immutable accessibility tree per poll: live query indices can
        // disappear while WebKit replaces the pending enrollment page.
        guard let root = try? app.webViews.firstMatch.snapshot() else { return [] }
        func collect(_ node: XCUIElementSnapshot) -> [String] {
            let own = node.elementType == .staticText ? [node.label, node.value as? String ?? ""] : []
            return own + node.children.flatMap { collect($0) }
        }
        return collect(root)
    }

    private func waitForText(containing fragment: String, timeout: TimeInterval) -> String {
        var found: String?
        // WebKit exposes text as label on iOS and value on macOS. Heading
        // values can be numbers, so string predicates cannot safely query all values.
        let expectation = XCTNSPredicateExpectation(predicate: NSPredicate { [unowned self] _, _ in
            for value in self.visibleTextSnapshot() where value.contains(fragment) {
                found = value
                return true
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
        func management() {
            if let control = web.buttons.matching(identifier:"Home 管理").allElementsBoundByIndex.first(where:{ $0.isHittable }) { tap(control) }
        }
        func home(_ name: String) {
            // Both desktop and mobile retain the real Home management action.
            management()
            if web.buttons["返回 Home"].isHittable { tap(web.buttons["返回 Home"]) }
            let buttons = web.buttons.matching(identifier:name)
            let visible = buttons.allElementsBoundByIndex.first(where:{ $0.isHittable }) ?? buttons.firstMatch
            tap(visible)
        }
        func enroll(_ name: String) {
            home(name)
            let relayField = inputField(containing:"Relay")
            tap(relayField)
            relayField.typeText(relay)
            let passwordField = web.secureTextFields.firstMatch
            tap(passwordField); passwordField.typeText(password)
            tap(web.buttons["注册"])
            _ = waitForText(containing:"等待超级 Home 批准", timeout:120)
            var code = ""
            let expectation = XCTNSPredicateExpectation(predicate:NSPredicate { _, _ in
                for value in self.visibleTextSnapshot() {
                    if value.range(of:"^[0-9A-Fa-f]{4}([ -][0-9A-Fa-f]{4})+$", options:.regularExpression) != nil {
                        code = value; return true
                    }
                }
                return false
            }, object:app)
            XCTAssertEqual(XCTWaiter.wait(for:[expectation], timeout:120), .completed, "Rendered approval code unavailable")
            print("PTY_E2E_VERIFICATION 等待批准，校验码：" + code)
            _ = waitForText(containing:"已连接", timeout:180)
            XCTAssertTrue(text("暂无会话，点击「新建会话」开始。").waitForExistence(timeout:30))
        }
        func create() {
            tap(web.buttons["＋ 新建会话"])
            tap(web.buttons["取消"])
            XCTAssertTrue(text("暂无会话，点击「新建会话」开始。").exists, "Cancel must not create a shell")
            tap(web.buttons["＋ 新建会话"])
            let field = inputField(containing:"会话名称")
            tap(field); field.typeText("E2E shell")
            tap(web.buttons["创建并打开"])
            XCTAssertTrue(web.buttons["切换只读"].waitForExistence(timeout:30))
            management()
            _ = waitForText(containing:"1 个连接", timeout:30)
            XCTAssertTrue(text("E2E shell").exists)
            XCTAssertFalse(text("尚未连接").exists, "Successful initial join must set latest connection time")
#if os(macOS)
            _ = waitForText(containing:"创建时间", timeout:20)
            _ = waitForText(containing:"最近连接", timeout:20)
#endif
            tap(web.buttons["继续"])
        }
        func switchTerminal(_ name: String) {
            if let control = web.buttons.matching(identifier:"已打开终端").allElementsBoundByIndex.first(where:{ $0.isHittable }) { tap(control) }
            else { tap(web.buttons["切换终端"]) }
            let matches = web.buttons.matching(identifier:name + " / E2E shell")
            tap(matches.allElementsBoundByIndex.first(where:{ $0.isHittable }) ?? matches.firstMatch)
        }
        func output(_ marker: String) {
            let terminal = web.textViews.allElementsBoundByIndex.first(where:{ $0.isHittable }) ?? web.textViews.firstMatch
            tap(terminal)
            let octal = marker.utf8.map { String(format:"\\%03o", Int($0)) }.joined()
            terminal.typeText("printf '\\n" + octal + "\\n'\n")
            waitForRenderedOutput(marker)
        }
        enroll("测试 Mac"); create()
        for label in ["Ctrl+C", "Tab", "Esc", "↑", "↓", "←", "→"] {
#if os(macOS)
            XCTAssertFalse(web.buttons[label].exists, "macOS must have no virtual terminal keys")
#else
            XCTAssertTrue(web.buttons[label].exists)
#endif
        }
        let alphabet = Array("ABCDEFGHJKLMNPRS")
        let random = UUID().uuidString.replacingOccurrences(of:"-", with:"").prefix(8)
        let marker = "PTYCHECK" + String(random.map { alphabet[$0.hexDigitValue!] })
        output(marker)
        tap(web.buttons["切换只读"])
        tap(web.buttons["申请读写"])
        XCTAssertTrue(web.buttons["切换只读"].waitForExistence(timeout:20))
        enroll("测试 VPS"); create(); output(marker + "SECOND")
        switchTerminal("测试 Mac"); output(marker + "FIRST")
        switchTerminal("测试 VPS"); output(marker + "SECONDAGAIN")
        management(); tap(web.buttons["断开连接"])
        _ = waitForText(containing:"未连接", timeout:20)
        switchTerminal("测试 Mac"); output(marker + "SURVIVED")
        home("测试 VPS")
        _ = waitForText(containing:"已连接", timeout:180)
        XCTAssertEqual(web.buttons.matching(identifier:"打开").count, 1)
        tap(web.buttons["打开"])
        XCTAssertTrue(web.buttons["切换只读"].waitForExistence(timeout:20))
#if os(macOS)
        app.typeKey("h", modifierFlags:.command)
#else
        XCUIDevice.shared.press(.home)
#endif
        wait(NSPredicate(format:"state == %d", XCUIApplication.State.runningBackground.rawValue), object:app!)
        app.activate()
        _ = waitForText(containing:"未连接", timeout:20)
        XCTAssertFalse(web.buttons["关闭标签"].exists)
        let staysDisconnected = XCTNSPredicateExpectation(predicate:NSPredicate { [unowned self] _, _ in
            self.visibleTextSnapshot().contains { $0.contains("已连接") }
        }, object:app)
        staysDisconnected.isInverted = true
        XCTAssertEqual(XCTWaiter.wait(for:[staysDisconnected], timeout:2), .completed)
        tap(web.buttons["连接"])
        _ = waitForText(containing:"已连接", timeout:180)
        XCTAssertEqual(web.buttons.matching(identifier:"打开").count, 1, "Reconnect must list the existing shell only")
        XCTAssertFalse(web.buttons["关闭标签"].exists, "Reconnect must not join automatically")
        tap(web.buttons["打开"])
        waitForRenderedOutput(marker + "SECONDAGAIN")
        output(marker + "RESUMED")
        management(); tap(web.buttons["断开连接"])
    }
}
