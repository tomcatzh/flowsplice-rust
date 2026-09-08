import Foundation
import WebKit
import Security
import Combine
#if os(macOS)
import AppKit
#else
import UIKit
#endif

enum DeviceLabel {
    static func make(_ name: String?, fallback: String) -> String {
        let cleaned = String(String.UnicodeScalarView((name ?? "").unicodeScalars.filter {
            !CharacterSet.controlCharacters.contains($0) && !(0x202A...0x202E).contains($0.value) && !(0x2066...0x2069).contains($0.value)
        })).trimmingCharacters(in:.whitespacesAndNewlines)
        let base = cleaned.isEmpty ? fallback : cleaned
        let suffix = " · PTY"
        var result = ""
        for scalar in base.unicodeScalars {
            guard result.utf8.count + scalar.utf8.count + suffix.utf8.count <= 64 else { break }
            result.unicodeScalars.append(scalar)
        }
        return result.trimmingCharacters(in:.whitespacesAndNewlines) + suffix
    }
}

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
    var homeID = "default"
    var query: [String: Any] { [kSecClass as String:kSecClassGenericPassword,
        kSecAttrService as String:(Bundle.main.bundleIdentifier ?? "io.zxf.flowsplice.pty") + ".private-key",
        kSecAttrAccount as String:InstallationScope.account + (homeID == "default" ? "" : ".home." + homeID)] }
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
    private struct Home {
        let id: String
        let info: [String:Any]
        var handle: UInt64 = 0
        var error: String?
        var pendingPassword: String?
    }
    private var homes: [Home] = []
    private var timer: Timer?
    private var ready = false
    private var rendering = false
    private var visible = true
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
                  let root = try? String(contentsOf:resources.appendingPathComponent("bootstrap/deployment-root.pub"), encoding:.utf8) else {
                throw HostError.native("Private configuration missing. Install a privately configured FlowSplice PTY build.")
            }
            let catalogURL = resources.appendingPathComponent("bootstrap/homes.json")
            let entries: [[String:Any]]
            if FileManager.default.fileExists(atPath:catalogURL.path) {
                guard let catalog = try JSONSerialization.jsonObject(with:Data(contentsOf:catalogURL)) as? [String:Any],
                      catalog["version"] as? Int == 1, let values = catalog["homes"] as? [[String:Any]],
                      !values.isEmpty, values.count <= 8 else { throw HostError.invalidResponse }
                entries = values
            } else {
                let descriptor = try JSONSerialization.jsonObject(with:Data(contentsOf:resources.appendingPathComponent("bootstrap/business.json")))
                entries = [["id":"default", "name":"我的 Mac", "platform":"macos", "descriptor":descriptor]]
            }
            var ids = Set<String>()
            for entry in entries {
                guard let id = entry["id"] as? String, id.range(of:"^[a-z0-9][a-z0-9_-]{0,47}$", options:.regularExpression) != nil,
                      ids.insert(id).inserted, let name = entry["name"] as? String,
                      !name.trimmingCharacters(in:.whitespacesAndNewlines).isEmpty, name.utf8.count <= 128,
                      !name.unicodeScalars.contains(where:{ CharacterSet.controlCharacters.contains($0) }),
                      let platform = entry["platform"] as? String, ["linux","macos"].contains(platform),
                      entry["descriptor"] is [String:Any],
                      entry["relay"] == nil || entry["relay"] is String,
                      (entry["relay"] as? String ?? "").utf8.count <= 512 else { throw HostError.invalidResponse }
            }
            var support = try FileManager.default.url(for:.applicationSupportDirectory, in:.userDomainMask, appropriateFor:nil, create:true)
                .appendingPathComponent(Bundle.main.bundleIdentifier!, isDirectory:true)
            if let namespace = InstallationScope.namespace { support = support.appendingPathComponent("tests").appendingPathComponent(namespace) }
            for entry in entries {
                let id = entry["id"] as! String
                var home = Home(id:id, info:["id":id,"name":entry["name"]!,"platform":entry["platform"]!,"relay":entry["relay"] as? String ?? ""])
                do {
                    let directory = id == "default" ? support : support.appendingPathComponent("homes").appendingPathComponent(id)
                    try FileManager.default.createDirectory(at:directory, withIntermediateDirectories:true, attributes:[.posixPermissions:0o700])
                    let identityFile = directory.appendingPathComponent("travel-id")
                    let identity: String
                    if let existing = try? String(contentsOf:identityFile, encoding:.utf8) { identity = existing }
                    else {
                        identity = "pty-" + UUID().uuidString.lowercased()
                        try Data(identity.utf8).write(to:identityFile, options:.atomic)
                    }
#if os(macOS)
                    let label = DeviceLabel.make(Host.current().localizedName ?? Host.current().name, fallback:"Mac")
#else
                    let label = DeviceLabel.make(UIDevice.current.name, fallback:UIDevice.current.userInterfaceIdiom == .pad ? "iPad" : "iPhone")
#endif
                    let options: [String:Any] = ["install_dir":directory.path,"root_public_key":root.trimmingCharacters(in:.whitespacesAndNewlines),"descriptor":entry["descriptor"]!,"travel_id":identity,"label":label]
                    let json = String(data:try JSONSerialization.data(withJSONObject:options), encoding:.utf8)!
                    let data = try json.withCString { try response(flowsplice_pty_open($0)) }
                    guard let opened = (data as? [String:Any])?["handle"] as? NSNumber else { throw HostError.invalidResponse }
                    home.handle = opened.uint64Value
                } catch { home.error = describe(error) }
                homes.append(home)
            }
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
    private func send(_ action: [String:Any], handle:UInt64) throws {
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
#if os(macOS)
                let platform = "macos"
#else
                let platform = "ios"
#endif
                deliver(["type":"homes","platform":platform,"homes":homes.map { $0.info }])
                if let error = configurationError { deliver(["type":"error","message":error]) }
                for home in homes { if let error = home.error { deliver(["type":"error","home_id":home.id,"message":error]) } }
                startPolling()
                return
            }
            guard ready, visible, let id = action.removeValue(forKey:"home_id") as? String,
                  let index = homes.firstIndex(where:{ $0.id == id }), homes[index].handle != 0 else { return }
            do {
                if let op = action["op"] as? String, op == "enroll" || op == "connect" {
                    let supplied = action["password"] as? String ?? ""
                    let password = supplied.isEmpty ? try PasswordStore(homeID:id).load() ?? "" : supplied
                    guard !password.isEmpty else {
                        deliver(["type":"error","home_id":id,"code":"credential_required","message":"此 Home 缺少已保存的私钥密码，请输入密码。"]); return
                    }
                    action["password"] = password
                    if op == "enroll" { try PasswordStore(homeID:id).save(password) }
                    else if !supplied.isEmpty { homes[index].pendingPassword = supplied }
                }
                try send(action, handle:homes[index].handle)
                if action["op"] as? String == "disconnect" { homes[index].pendingPassword = nil }
            } catch {
                homes[index].pendingPassword = nil
                deliver(["type":"error","home_id":id,"message":describe(error)])
            }
        } catch { deliver(["type":"error","message":describe(error)]) }
    }
    func setVisible(_ value: Bool) {
        visible = value
        if !value {
            timer?.invalidate(); timer = nil
            for index in homes.indices {
                homes[index].pendingPassword = nil
                if homes[index].handle != 0 { try? send(["op":"disconnect"], handle:homes[index].handle) }
            }
        } else { startPolling() }
    }
    private func startPolling() {
        guard ready, visible, timer == nil, homes.contains(where:{ $0.handle != 0 }) else { return }
        timer = Timer.scheduledTimer(withTimeInterval:0.025, repeats:true) { [weak self] _ in self?.poll() }
    }
    private func poll() {
        guard !rendering else { return }
        var batch: [[String:Any]] = []
        // Native queues are bounded at 128 events per Home; at most eight are drained.
        for index in homes.indices where homes[index].handle != 0 {
            let id = homes[index].id
            do {
                guard let events = try response(flowsplice_pty_poll(homes[index].handle)) as? [[String:Any]] else { throw HostError.invalidResponse }
                for var event in events {
                    if event["type"] as? String == "state", event["connected"] as? Bool == true, let password = homes[index].pendingPassword {
                        try PasswordStore(homeID:id).save(password); homes[index].pendingPassword = nil
                    }
                    if event["type"] as? String == "error" { homes[index].pendingPassword = nil }
                    event["home_id"] = id; batch.append(event)
                }
            } catch {
                homes[index].pendingPassword = nil
                batch.append(["type":"error","home_id":id,"message":describe(error)])
            }
        }
        if !batch.isEmpty {
            rendering = true
            web.callAsyncJavaScript("await window.flowsplice.receiveBatch(events)", arguments:["events":batch], in:nil, in:.page) { [weak self] _ in self?.rendering = false }
        }
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
    deinit { timer?.invalidate(); for home in homes where home.handle != 0 { flowsplice_pty_string_free(flowsplice_pty_close(home.handle)) }; NotificationCenter.default.removeObserver(self) }
}
