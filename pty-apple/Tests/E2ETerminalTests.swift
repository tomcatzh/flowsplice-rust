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
        dismissStaleMacPrompts()
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

#if os(macOS)
    private func dismissStaleMacPrompts() {
        func candidates() -> [(XCUIElement, String, Bool)] {
            var result: [(XCUIElement, String, Bool)] = []
            for identifier in ["com.apple.UserNotificationCenter", "com.apple.coreservices.uiagent"] {
                let system = XCUIApplication(bundleIdentifier: identifier)
                guard system.state != .notRunning else { continue }
                for dialog in system.dialogs.allElementsBoundByIndex + system.alerts.allElementsBoundByIndex {
                    let message = dialog.staticTexts.allElementsBoundByIndex
                        .map { $0.label + " " + ($0.value as? String ?? "") }.joined(separator: " ")
                    let network = message.contains("FlowSplicePTY") &&
                        (message.contains("本地网络") || message.lowercased().contains("local network"))
                    if network || message.contains("FlowSplicePTY-macOSUITests-Runner") {
                        result.append((dialog, message, network))
                    }
                }
            }
            return result.sorted { $0.2 && !$1.2 }
        }
        for _ in 0..<8 {
            let pending = candidates()
            if pending.isEmpty { return }
            var clicked = false
            for (dialog, _, network) in pending {
                let titles: Set<String> = network
                    ? ["不允许", "Don't Allow", "Don’t Allow", "Do Not Allow", "取消", "Cancel"]
                    : ["不打开", "Don't Open", "Do Not Open", "取消", "Cancel"]
                guard let button = dialog.buttons.allElementsBoundByIndex.first(where: {
                    titles.contains($0.title) && $0.isEnabled && $0.isHittable
                }) else { continue }
                button.click()
                clicked = true
                break // Every subsequent action starts with a fresh hierarchy query.
            }
            if !clicked {
                XCTFail("Cannot dismiss stale PTY prompts: " + pending.map { $0.1 }.joined(separator: " | "))
                return
            }
        }
        let remaining = candidates()
        XCTAssertTrue(remaining.isEmpty, "Stale PTY prompts remain: " + remaining.map { $0.1 }.joined(separator: " | "))
    }
#endif

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
#if os(macOS)
            guard app.state != .notRunning, message.contains("FlowSplicePTY"),
                  message.contains("本地网络") || message.lowercased().contains("local network") else { continue }
            let titles: Set<String> = ["允许", "Allow", "好", "OK"]
            if let button = dialog.buttons.allElementsBoundByIndex.first(where: {
                titles.contains($0.title) && $0.isEnabled && $0.isHittable
            }) {
                button.click()
                return
            }
#else
            guard message.contains("FlowSplice"),
                  message.contains("本地网络") || message.lowercased().contains("local network") else { continue }
            for title in ["允许", "Allow", "好", "OK"] where dialog.buttons[title].exists {
                dialog.buttons[title].tap()
                return
            }
#endif
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

    private func finishFormEditing() {
#if !os(macOS)
        // Dismiss WebKit's form keyboard before locating the submit button;
        // hiding it changes the visual viewport and the button's screen position.
        if app.keyboards.firstMatch.exists {
            let done = app.buttons.matching(NSPredicate(format:"label IN %@", ["Done", "完成", "Hide keyboard", "Dismiss keyboard", "隐藏键盘", "收起键盘"]))
                .allElementsBoundByIndex.first(where: { $0.isHittable })
            XCTAssertNotNil(done, "The native form keyboard must expose a dismissal control")
            if let done { tap(done) }
            wait(NSPredicate(format:"exists == false"), object:app.keyboards.firstMatch)
        }
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
#if os(macOS)
            self.allowOwnLocalNetworkPrompt()
#endif
            for value in self.visibleTextSnapshot() where value.contains(fragment) {
                found = value
                return true
            }
            return false
        }, object: app)
        let result = XCTWaiter.wait(for: [expectation], timeout: timeout)
        if result != .completed {
            let window = app.windows.firstMatch
            if window.exists {
                let screenshot = XCTAttachment(screenshot: window.screenshot())
                screenshot.name = "PTY app window at text timeout"
                screenshot.lifetime = .keepAlways
                add(screenshot)
            }
            let snapshot = XCTAttachment(string: app.webViews.debugDescription)
            snapshot.name = "PTY WebView snapshot at text timeout"
            snapshot.lifetime = .keepAlways
            add(snapshot)
        }
        XCTAssertEqual(result, .completed,
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
        let isClass = environment["FLOWSPLICE_PTY_E2E_SERVICE_CLASS"] == "1"
        let firstHome = isClass ? try XCTUnwrap(environment["FLOWSPLICE_PTY_E2E_FIRST_HOME"]) : "测试 Mac"
        let secondHome = isClass ? try XCTUnwrap(environment["FLOWSPLICE_PTY_E2E_SECOND_HOME"]) : "测试 VPS"
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
        func approveRenderedRequest() {
            _ = waitForText(containing:"等待超级 Home 批准", timeout:120)
            var code = ""
            let expectation = XCTNSPredicateExpectation(predicate:NSPredicate { _, _ in
    #if os(macOS)
            self.allowOwnLocalNetworkPrompt()
#endif
            for value in self.visibleTextSnapshot() {
                    if value.range(of:"^[0-9A-Fa-f]{4}([ -][0-9A-Fa-f]{4})+$", options:.regularExpression) != nil {
                        code = value; return true
                    }
                }
                return false
            }, object:app)
            XCTAssertEqual(XCTWaiter.wait(for:[expectation], timeout:120), .completed, "Rendered approval code unavailable")
            print("PTY_E2E_VERIFICATION 等待批准，校验码：" + code)
        }
        func waitForHome() {
            _ = waitForText(containing:isClass ? "PTY 服务 · 已连接" : "已连接", timeout:180)
        }
        func enrollClass() {
            _ = waitForText(containing:"所有 Home 的 PTY 服务", timeout:30)
            let label = waitForText(containing:" · PTY", timeout:30)
            XCTAssertTrue(label.hasSuffix(" · PTY"))
            print("PTY_E2E_IDENTITY " + label)
            let relayField = inputField(containing:"Relay")
            tap(relayField); relayField.typeText(relay)
            let passwordField = web.secureTextFields.firstMatch
            tap(passwordField); passwordField.typeText(password)
            finishFormEditing()
            tap(web.buttons["注册此设备"])
            approveRenderedRequest()
            _ = waitForText(containing:"服务目录已连接", timeout:180)
        }
        func enroll(_ name: String) {
            home(name)
            let relayField = inputField(containing:"Relay")
            tap(relayField)
            relayField.typeText(relay)
            let passwordField = web.secureTextFields.firstMatch
            tap(passwordField); passwordField.typeText(password)
            finishFormEditing()
            tap(web.buttons["注册"])
            approveRenderedRequest()
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
            finishFormEditing()
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
        if isClass { enrollClass(); home(firstHome); waitForHome() }
        else { enroll(firstHome) }
        create()
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
        if isClass { home(secondHome); waitForHome() }
        else { enroll(secondHome) }
        create(); output(marker + "SECOND")
        switchTerminal(firstHome); output(marker + "FIRST")
        switchTerminal(secondHome); output(marker + "SECONDAGAIN")
        management(); tap(web.buttons["断开连接"])
        _ = waitForText(containing:"未连接", timeout:20)
        switchTerminal(firstHome); output(marker + "SURVIVED")
        home(secondHome)
        waitForHome()
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
        if isClass {
            tap(web.buttons["连接服务目录"])
            _ = waitForText(containing:"服务目录已连接", timeout:180)
            XCTAssertFalse(web.buttons["关闭标签"].exists, "Shared connect must not join any Home")
            home(secondHome)
        } else { tap(web.buttons["连接"]) }
        waitForHome()
        XCTAssertEqual(web.buttons.matching(identifier:"打开").count, 1, "Reconnect must list the existing shell only")
        XCTAssertFalse(web.buttons["关闭标签"].exists, "Reconnect must not join automatically")
        tap(web.buttons["打开"])
        waitForRenderedOutput(marker + "SECONDAGAIN")
        output(marker + "RESUMED")
        management(); tap(web.buttons["断开连接"])
    }
}
