import Foundation
import WebKit
import Security
import Combine
#if os(macOS)
import AppKit
#endif

private enum HostError: Error { case invalidResponse, native(String), keychain(OSStatus) }


final class LocalAssets: NSObject, WKURLSchemeHandler {
    func webView(_ webView: WKWebView, start task: WKURLSchemeTask) {
        guard task.request.httpMethod == "GET", let url = task.request.url,
              let root = Bundle.main.resourceURL?.appendingPathComponent("pty-ui"),
              let file = LocalOrigin.resource(url, root: root),
              let data = try? Data(contentsOf: file) else {
            task.didFailWithError(URLError(.fileDoesNotExist)); return
        }
        let mime = ["html":"text/html", "js":"text/javascript", "css":"text/css", "woff2":"font/woff2", "svg":"image/svg+xml"][file.pathExtension] ?? "application/octet-stream"
        task.didReceive(URLResponse(url: url, mimeType: mime, expectedContentLength: data.count, textEncodingName: "utf-8"))
        task.didReceive(data); task.didFinish()
    }
    func webView(_ webView: WKWebView, stop task: WKURLSchemeTask) {}
}

private enum InstallationScope {
    static var namespace: String? {
#if DEBUG
        guard let value = ProcessInfo.processInfo.environment["FLOWSPLICE_PTY_TEST_NAMESPACE"],
              let id = UUID(uuidString: value) else { return nil }
        return id.uuidString.lowercased()
#else
        return nil
#endif
    }
    static var account: String { "terminal-installation" + (namespace.map { "." + $0 } ?? "") }
}

private struct PasswordStore {
    var query: [String: Any] { [kSecClass as String:kSecClassGenericPassword,
        kSecAttrService as String:(Bundle.main.bundleIdentifier ?? "io.zxf.flowsplice.pty") + ".private-key",
        kSecAttrAccount as String:InstallationScope.account] }
    func load() throws -> String? {
        var q = query; q[kSecReturnData as String] = true; q[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?; let status = SecItemCopyMatching(q as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess else { throw HostError.keychain(status) }
        return (result as? Data).flatMap { String(data:$0, encoding:.utf8) }
    }
    func save(_ password: String) throws {
        let values: [String: Any] = [kSecValueData as String:Data(password.utf8),
            kSecAttrAccessible as String:kSecAttrAccessibleWhenUnlockedThisDeviceOnly]
        var status = SecItemUpdate(query as CFDictionary, values as CFDictionary)
        if status == errSecItemNotFound {
            status = SecItemAdd(query.merging(values) { _, new in new } as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw HostError.keychain(status) }
    }
}

private final class ScriptHandler: NSObject, WKScriptMessageHandler {
    weak var host: TerminalHost?
    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        host?.userContentController(controller, didReceive: message)
    }
}

final class TerminalHost: NSObject, ObservableObject, WKScriptMessageHandler, WKNavigationDelegate, WKUIDelegate {
    let web: WKWebView
    private var handle: UInt64 = 0
    private var timer: Timer?
    private var ready = false
    private var rendering = false
    private var visible = true
    private var pendingPassword: String?
    private let passwords = PasswordStore()
    private var configurationError: String?

    override init() {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
#if os(macOS)
        configuration.userContentController.addUserScript(WKUserScript(
            source:"window.flowsplicePlatform = 'macos';", injectionTime:.atDocumentStart, forMainFrameOnly:true))
#endif
        configuration.preferences.javaScriptCanOpenWindowsAutomatically = false
        configuration.setURLSchemeHandler(LocalAssets(), forURLScheme:"flowsplice-pty")
        web = WKWebView(frame:.zero, configuration:configuration)
        super.init()
        web.navigationDelegate = self; web.uiDelegate = self
        let handler = ScriptHandler(); handler.host = self
        configuration.userContentController.add(handler, name:"pty")
        do {
            guard let resources = Bundle.main.resourceURL,
                  let root = try? String(contentsOf:resources.appendingPathComponent("bootstrap/deployment-root.pub"), encoding:.utf8),
                  let descriptorData = try? Data(contentsOf:resources.appendingPathComponent("bootstrap/business.json")) else {
                throw HostError.native("Private configuration missing. Install a privately configured FlowSplice PTY build.")
            }
            let descriptor = try JSONSerialization.jsonObject(with:descriptorData)
            var support = try FileManager.default.url(for:.applicationSupportDirectory, in:.userDomainMask, appropriateFor:nil, create:true)
                .appendingPathComponent(Bundle.main.bundleIdentifier!, isDirectory:true)
            if let namespace = InstallationScope.namespace { support = support.appendingPathComponent("tests").appendingPathComponent(namespace) }
            try FileManager.default.createDirectory(at:support, withIntermediateDirectories:true, attributes:[.posixPermissions:0o700])
            let identityFile = support.appendingPathComponent("travel-id")
            let identity: String
            if let existing = try? String(contentsOf:identityFile, encoding:.utf8) { identity = existing }
            else {
                identity = "pty-" + UUID().uuidString.lowercased()
                try Data(identity.utf8).write(to:identityFile, options:.atomic)
            }
#if os(macOS)
            let label = "macOS"
#else
            let label = "iOS"
#endif
            let options: [String:Any] = ["install_dir":support.path, "root_public_key":root.trimmingCharacters(in:.whitespacesAndNewlines), "descriptor":descriptor, "travel_id":identity, "label":label]
            let json = String(data:try JSONSerialization.data(withJSONObject:options), encoding:.utf8)!
            let data = try json.withCString { try response(flowsplice_pty_open($0)) }
            guard let opened = (data as? [String:Any])?["handle"] as? NSNumber else { throw HostError.invalidResponse }
            handle = opened.uint64Value
        } catch { configurationError = describe(error) }
#if os(macOS)
        NotificationCenter.default.addObserver(self, selector:#selector(windowChanged(_:)), name:NSWindow.didChangeOcclusionStateNotification, object:nil)
        NotificationCenter.default.addObserver(self, selector:#selector(windowClosed(_:)), name:NSWindow.willCloseNotification, object:nil)
        NotificationCenter.default.addObserver(self, selector:#selector(applicationHidden), name:NSApplication.didHideNotification, object:nil)
        NotificationCenter.default.addObserver(self, selector:#selector(applicationShown), name:NSApplication.didUnhideNotification, object:nil)
#endif
        web.load(URLRequest(url:URL(string:"flowsplice-pty://app/index.html")!))
    }
    private func response(_ pointer: UnsafeMutablePointer<CChar>?) throws -> Any {
        guard let pointer else { throw HostError.invalidResponse }
        defer { flowsplice_pty_string_free(pointer) }
        guard let value = try JSONSerialization.jsonObject(with:Data(String(cString:pointer).utf8)) as? [String:Any] else { throw HostError.invalidResponse }
        guard value["ok"] as? Bool == true else { throw HostError.native(value["error"] as? String ?? "Native operation failed") }
        return value["data"] ?? NSNull()
    }
    private func describe(_ error: Error) -> String {
        if case HostError.native(let message) = error { return message }
        return "Private terminal operation failed. Check installation and Keychain access."
    }
    private func deliver(_ event: [String:Any]) {
        // JSON is an argument, never executable source or interpolated text.
        web.callAsyncJavaScript("window.flowsplice.receive(event)", arguments:["event":event], in:nil, in:.page, completionHandler: { _ in })
    }
    private func send(_ action: [String:Any]) throws {
        let text = String(data:try JSONSerialization.data(withJSONObject:action), encoding:.utf8)!
        _ = try text.withCString { try response(flowsplice_pty_send(handle,$0)) }
    }
    func userContentController(_ userContentController: WKUserContentController, didReceive message: WKScriptMessage) {
        guard message.frameInfo.isMainFrame, LocalOrigin.accepts(message.frameInfo.request.url),
              let text = message.body as? String,
              text.utf8.count <= 128 * 1024 else { return }
        do {
            guard var action = try JSONSerialization.jsonObject(with:Data(text.utf8)) as? [String:Any] else { throw HostError.invalidResponse }
            // Module scripts may initialize after WKNavigationDelegate.didFinish.
            // The bundled main frame announces readiness after installing its bridge.
            if action["op"] as? String == "ready", action.count == 1 {
                ready = true
                if let error = configurationError { deliver(["type":"error","message":error]) }
                startPolling()
                return
            }
            guard ready, visible, handle != 0 else { return }
            if let op = action["op"] as? String, op == "enroll" || op == "connect" {
                let supplied = action["password"] as? String ?? ""
                if supplied.isEmpty { action["password"] = try passwords.load() ?? "" }
                if op == "enroll" { pendingPassword = action["password"] as? String }
            }
            try send(action)
            if action["op"] as? String == "disconnect" { pendingPassword = nil }
        } catch { pendingPassword = nil; deliver(["type":"error","message":describe(error)]) }
    }
    func setVisible(_ value: Bool) {
        visible = value
        if !value {
            timer?.invalidate(); timer = nil; pendingPassword = nil
            if handle != 0 { try? send(["op":"disconnect"]) }
        } else { startPolling() }
    }
    private func startPolling() {
        guard ready, visible, timer == nil, handle != 0 else { return }
        timer = Timer.scheduledTimer(withTimeInterval:0.025, repeats:true) { [weak self] _ in self?.poll() }
    }
    private func poll() {
        guard !rendering else { return }
        do {
            guard let events = try response(flowsplice_pty_poll(handle)) as? [[String:Any]] else { throw HostError.invalidResponse }
            for event in events {
                if event["type"] as? String == "progress", (event["progress"] as? [String:Any])?["phase"] as? String == "installed", let password = pendingPassword {
                    try passwords.save(password); pendingPassword = nil
                }
                if event["type"] as? String == "error" { pendingPassword = nil }
            }
            if !events.isEmpty {
                rendering = true
                web.callAsyncJavaScript("await window.flowsplice.receiveBatch(events)", arguments:["events":events], in:nil, in:.page) { [weak self] _ in self?.rendering = false }
            }
        } catch { pendingPassword = nil; deliver(["type":"error","message":describe(error)]) }
    }
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        decisionHandler(LocalOrigin.accepts(action.request.url) && action.targetFrame?.isMainFrame == true && !action.shouldPerformDownload ? .allow : .cancel)
    }
    func webView(_ webView: WKWebView, decidePolicyFor response: WKNavigationResponse, decisionHandler: @escaping (WKNavigationResponsePolicy) -> Void) {
        decisionHandler(LocalOrigin.accepts(response.response.url) && response.canShowMIMEType ? .allow : .cancel)
    }
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration, for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? { nil }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) { ready = false; setVisible(false) }
#if os(macOS)
    @objc private func windowChanged(_ note: Notification) {
        guard let window = note.object as? NSWindow, window === web.window else { return }
        setVisible(window.occlusionState.contains(.visible) && !NSApp.isHidden)
    }
    @objc private func windowClosed(_ note: Notification) {
        guard let window = note.object as? NSWindow, window === web.window else { return }; setVisible(false)
    }
    @objc private func applicationHidden() { setVisible(false) }
    @objc private func applicationShown() { setVisible(web.window?.occlusionState.contains(.visible) == true) }
#endif
    deinit { timer?.invalidate(); if handle != 0 { flowsplice_pty_string_free(flowsplice_pty_close(handle)) }; NotificationCenter.default.removeObserver(self) }
}
