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
            $0.value > 0x1F && !(0x7F...0x9F).contains($0.value) && ![0x061C, 0x200B, 0x200E, 0x200F, 0xFEFF].contains($0.value) && !(0x202A...0x202E).contains($0.value) && !(0x2060...0x2069).contains($0.value)
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

enum NativeLifecycle {
    static func shouldPoll(handle: UInt64, demand: Bool, draining: Bool, actionUntil: TimeInterval, now: TimeInterval) -> Bool {
        handle != 0 && (demand || draining || now < actionUntil)
    }
    static func shouldReopen(closed: Bool, recovered: Bool, hasOptions: Bool) -> Bool { closed && !recovered && hasOptions }
    static func drainExpired(started: TimeInterval?, now: TimeInterval) -> Bool {
        started.map { now - $0 >= 10 } ?? false
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
    var serviceClass = false
    var query: [String: Any] { [kSecClass as String:kSecClassGenericPassword,
        kSecAttrService as String:(Bundle.main.bundleIdentifier ?? "io.zxf.flowsplice.pty") + ".private-key",
        kSecAttrAccount as String:InstallationScope.account + (serviceClass ? ".service-class" : homeID == "default" ? "" : ".home." + homeID)] }
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
        var openOptions: String?
        var polling = true
        var actionUntil: TimeInterval = 0
        var recovered = false
        var disconnectStarted: TimeInterval?
        var snapshot: [String:Any] = [:]
    }
    private var homes: [Home] = []
    private var classMode = false
    private var timer: Timer?
    private var ready = false
    private var rendering = false
    private var renderWatchdog: Timer?
    private var visible = true
    private var configurationError: String?
    private var workspaceURL: URL?
    private var pending: [[String:Any]] = []
    private var pendingBytes = 0
    private var inFlightBytes = 0
    private var generation = 0
    private var disconnecting = Set<String>()
    private var disconnectRequested = Set<String>()
#if os(iOS)
    private var backgroundTask: UIBackgroundTaskIdentifier = .invalid
    private var expiration: Timer?
#endif

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
            let configurationNames = ["service-class.json", "homes.json", "business.json"].filter { FileManager.default.fileExists(atPath:resources.appendingPathComponent("bootstrap/" + $0).path) }
            guard configurationNames.count == 1 else { throw HostError.invalidResponse }
            classMode = configurationNames[0] == "service-class.json"
            let entries: [[String:Any]]
            if classMode {
                let descriptor = try JSONSerialization.jsonObject(with:Data(contentsOf:resources.appendingPathComponent("bootstrap/service-class.json")))
                entries = [["id":"service-class", "name":"PTY", "platform":"host", "descriptor":descriptor]]
            } else if FileManager.default.fileExists(atPath:catalogURL.path) {
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
                      let platform = entry["platform"] as? String, (classMode ? platform == "host" : ["linux","macos"].contains(platform)),
                      entry["descriptor"] is [String:Any],
                      entry["relay"] == nil || entry["relay"] is String,
                      (entry["relay"] as? String ?? "").utf8.count <= 512 else { throw HostError.invalidResponse }
            }
            var support = try FileManager.default.url(for:.applicationSupportDirectory, in:.userDomainMask, appropriateFor:nil, create:true)
                .appendingPathComponent(Bundle.main.bundleIdentifier!, isDirectory:true)
            if let namespace = InstallationScope.namespace { support = support.appendingPathComponent("tests").appendingPathComponent(namespace) }
            try FileManager.default.createDirectory(at:support, withIntermediateDirectories:true, attributes:[.posixPermissions:0o700])
            try FileManager.default.setAttributes([.posixPermissions:0o700], ofItemAtPath:support.path)
            workspaceURL = support.appendingPathComponent(classMode ? "workspace-class.json" : "workspace-legacy.json")
            for entry in entries {
                let id = entry["id"] as! String
                var home = Home(id:id, info:["id":id,"name":entry["name"]!,"platform":entry["platform"]!,"relay":entry["relay"] as? String ?? ""])
                do {
                    let directory = classMode ? support.appendingPathComponent("service-class") : id == "default" ? support : support.appendingPathComponent("homes").appendingPathComponent(id)
                    try FileManager.default.createDirectory(at:directory, withIntermediateDirectories:true, attributes:[.posixPermissions:0o700])
                    try FileManager.default.setAttributes([.posixPermissions:0o700], ofItemAtPath:directory.path)
                    let identityFile = directory.appendingPathComponent("travel-id")
                    let identity: String
                    if let existing = try? String(contentsOf:identityFile, encoding:.utf8) { identity = existing }
                    else {
                        identity = "pty-" + UUID().uuidString.lowercased()
                        try Data(identity.utf8).write(to:identityFile, options:.atomic)
                    }
                    try FileManager.default.setAttributes([.posixPermissions:0o600], ofItemAtPath:identityFile.path)
#if os(macOS)
                    let label = DeviceLabel.make(Host.current().localizedName ?? Host.current().name, fallback:"Mac")
#else
                    let label = DeviceLabel.make(UIDevice.current.name, fallback:UIDevice.current.userInterfaceIdiom == .pad ? "iPad" : "iPhone")
#endif
                    let options: [String:Any] = ["install_dir":directory.path,"root_public_key":root.trimmingCharacters(in:.whitespacesAndNewlines),(classMode ? "service_class" : "descriptor"):entry["descriptor"]!,"travel_id":identity,"label":label]
                    let json = String(data:try JSONSerialization.data(withJSONObject:options), encoding:.utf8)!
                    home.openOptions = json
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
        if case HostError.keychain(let status) = error {
            let detail = SecCopyErrorMessageString(status, nil) as String? ?? "Keychain access failed"
            return "Keychain error (\(status)): \(detail)"
        }
        return "Private terminal operation failed. Check installation and Keychain access."
    }
    private func deliver(_ event: [String:Any]) {
        enqueue([event])
    }
    private func enqueue(_ events: [[String:Any]]) {
        let bytes = (try? JSONSerialization.data(withJSONObject:events).count) ?? 0
        guard pendingBytes + inFlightBytes + bytes <= 2 * 1024 * 1024 else {
            suspendTransports()
            return
        }
        pending.append(contentsOf:events); pendingBytes += bytes
        flush()
    }
    private func flush() {
        guard ready, visible, !rendering, !pending.isEmpty else { return }
        let batch = pending; pending = []; inFlightBytes = pendingBytes; pendingBytes = 0
        rendering = true
        let token = generation
        armRenderWatchdog()
        web.callAsyncJavaScript("await window.flowsplice.receiveBatch(events)", arguments:["events":batch], in:nil, in:.page) { [weak self] result in
            guard let self, self.generation == token else { return }
            self.renderWatchdog?.invalidate(); self.renderWatchdog = nil
            if case .failure = result { self.recoverRenderer(); return }
            self.rendering = false; self.inFlightBytes = 0
            self.flush()
        }
    }
    private func armRenderWatchdog() {
        renderWatchdog?.invalidate()
        guard visible, rendering else { return }
        let token = generation
        let watchdog = Timer(timeInterval:10, repeats:false) { [weak self] _ in
            guard let self, self.generation == token, self.visible, self.rendering else { return }
            self.recoverRenderer()
        }
        renderWatchdog = watchdog; RunLoop.main.add(watchdog, forMode:.common)
    }
    private func recoverRenderer() {
        renderWatchdog?.invalidate(); renderWatchdog = nil
        ready = false; generation += 1; rendering = false; inFlightBytes = 0
        suspendTransports(); startPolling()
        web.load(URLRequest(url:URL(string:"flowsplice-pty://app/index.html")!))
    }
    private func suspendTransports() {
        pending = []; pendingBytes = 0
        // Discard the entire discontinuous stream; only fresh disconnected snapshots
        // may reach the UI before it rejoins the original remote sessions.
        for index in homes.indices where homes[index].handle != 0 {
            homes[index].pendingPassword = nil
            disconnecting.insert(homes[index].id)
            if homes[index].disconnectStarted == nil { homes[index].disconnectStarted = ProcessInfo.processInfo.systemUptime }
        }
        startPolling()
    }
    private func send(_ action: [String:Any], handle:UInt64) throws {
        let text = String(data:try JSONSerialization.data(withJSONObject:action), encoding:.utf8)!
        _ = try text.withCString { try response(flowsplice_pty_send(handle,$0)) }
        if let index = homes.firstIndex(where:{ $0.handle == handle }) { homes[index].polling = true; homes[index].actionUntil = ProcessInfo.processInfo.systemUptime + 10 }
        startPolling()
    }
    func userContentController(_ userContentController: WKUserContentController, didReceive message: WKScriptMessage) {
        guard message.frameInfo.isMainFrame, LocalOrigin.accepts(message.frameInfo.request.url),
              let text = message.body as? String,
              text.utf8.count <= 128 * 1024 else { return }
        do {
            guard var action = try JSONSerialization.jsonObject(with:Data(text.utf8)) as? [String:Any] else { throw HostError.invalidResponse }
            if action["op"] as? String == "save_workspace", action.count == 2 {
                guard let workspaceURL else { return }
                try WorkspaceStore.save(action["value"] as Any, to:workspaceURL, classMode:classMode)
                return
            }
            // Module scripts may initialize after WKNavigationDelegate.didFinish.
            // The bundled main frame announces readiness after installing its bridge.
            if action["op"] as? String == "ready", action.count == 1 {
                guard !ready else { return }
                let queued = pending.filter { $0["type"] as? String != "lifecycle" }
                pending = []; pendingBytes = 0
#if os(macOS)
                let platform = "macos"
#else
                let platform = "ios"
#endif
                // Pause restoration until workspace and queued native state have arrived.
                var bootstrap: [[String:Any]] = [
                    ["type":"lifecycle","active":false],
                    ["type":"workspace","value":workspaceURL.flatMap { WorkspaceStore.load(from:$0, classMode:classMode) } as Any? ?? NSNull()],
                    ["type":"platform","platform":platform]
                ]
                if !classMode { bootstrap.append(["type":"homes","platform":platform,"homes":homes.map { $0.info }]) }
                bootstrap.append(contentsOf:queued)
                if let error = configurationError { bootstrap.append(["type":"error","message":error]) }
                for home in homes { if let error = home.error { bootstrap.append(classMode ? ["type":"error","message":error] : ["type":"error","home_id":home.id,"message":error]) } }
                bootstrap.append(["type":"lifecycle","active":visible && disconnecting.isEmpty])
                ready = true
                enqueue(bootstrap)
                startPolling()
                return
            }
            guard ready, visible, disconnecting.isEmpty else { return }
            let id = classMode ? "service-class" : action.removeValue(forKey:"home_id") as? String ?? ""
            guard let index = homes.firstIndex(where:{ $0.id == id }) else { return }
            guard homes[index].handle != 0 else {
                deliver(["type":"error", "home_id":action["home_id"] ?? id, "input_id":action["input_id"] ?? NSNull(), "message":homes[index].error ?? "Please reopen the app to restore the native terminal."]); return
            }
            do {
                if let op = action["op"] as? String, op == "enroll" || op == "connect" {
                    let supplied = action["password"] as? String ?? ""
                    let password = supplied.isEmpty ? try PasswordStore(homeID:id, serviceClass:classMode).load() ?? "" : supplied
                    guard !password.isEmpty else {
                        deliver(classMode ? ["type":"error","code":"credential_required","message":"缺少已保存的私钥密码，请输入密码。"] : ["type":"error","home_id":id,"code":"credential_required","message":"此 Home 缺少已保存的私钥密码，请输入密码。"]); return
                    }
                    action["password"] = password
                    if op == "enroll" { try PasswordStore(homeID:id, serviceClass:classMode).save(password) }
                    else if !supplied.isEmpty { homes[index].pendingPassword = supplied }
                }
                try send(action, handle:homes[index].handle)
                if action["op"] as? String == "disconnect" { homes[index].pendingPassword = nil }
            } catch {
                homes[index].pendingPassword = nil
                deliver(["type":"error", "home_id":classMode ? action["home_id"] ?? NSNull() : id, "input_id":action["input_id"] ?? NSNull(), "message":describe(error)])
            }
        } catch { deliver(["type":"error","message":describe(error)]) }
    }
    func setVisible(_ value: Bool) {
        guard visible != value else { if value { startPolling(); flush() }; return }
        visible = value
        if value {
#if os(iOS)
            endBackgroundTask()
#endif
            armRenderWatchdog()
            poll()
            deliver(["type":"lifecycle","active":disconnecting.isEmpty])
            startPolling(); flush()
        } else {
            renderWatchdog?.invalidate(); renderWatchdog = nil
            // Deliver the pause before WebKit is suspended; native polling continues.
            web.callAsyncJavaScript("window.flowsplice.receive(event)", arguments:["event":["type":"lifecycle","active":false]], in:nil, in:.page, completionHandler:nil)
#if os(iOS)
            prepareForInactive()
            expiration?.invalidate()
            expiration = Timer.scheduledTimer(withTimeInterval:30, repeats:false) { [weak self] _ in self?.expireBackground() }
#endif
        }
    }
#if os(iOS)
    func prepareForInactive() {
        guard backgroundTask == .invalid else { return }
        backgroundTask = UIApplication.shared.beginBackgroundTask(withName:"PTY continuity") { [weak self] in self?.expireBackground() }
    }
    private func expireBackground() {
        if !visible { suspendTransports() }
        endBackgroundTask()
    }
    private func endBackgroundTask() {
        expiration?.invalidate(); expiration = nil
        if backgroundTask != .invalid {
            UIApplication.shared.endBackgroundTask(backgroundTask); backgroundTask = .invalid
        }
    }
    func becameActive() { endBackgroundTask(); setVisible(true) }
#endif
    private func startPolling() {
        guard timer == nil, homes.contains(where:{ NativeLifecycle.shouldPoll(handle:$0.handle, demand:$0.polling, draining:disconnecting.contains($0.id), actionUntil:$0.actionUntil, now:ProcessInfo.processInfo.systemUptime) }) else { return }
        let pollingTimer = Timer(timeInterval:0.025, repeats:true) { [weak self] _ in self?.poll() }
        timer = pollingTimer; RunLoop.main.add(pollingTimer, forMode:.common)
    }
    private func poll() {
        var batch: [[String:Any]] = []
        let wasDisconnecting = !disconnecting.isEmpty
        // Drain one shared class queue or up to eight legacy queues into one awaited batch.
        for index in homes.indices where NativeLifecycle.shouldPoll(handle:homes[index].handle, demand:homes[index].polling, draining:disconnecting.contains(homes[index].id), actionUntil:homes[index].actionUntil, now:ProcessInfo.processInfo.systemUptime) {
            let id = homes[index].id
            do {
                if NativeLifecycle.drainExpired(started:homes[index].disconnectStarted, now:ProcessInfo.processInfo.systemUptime) { throw HostError.native("Native disconnect timed out") }
                guard let events = try response(flowsplice_pty_poll(homes[index].handle)) as? [[String:Any]] else { throw HostError.invalidResponse }
                if disconnecting.contains(id), !disconnectRequested.contains(id) {
                    // Only events queued after this request may acknowledge its drain.
                    do { try send(["op":"disconnect"], handle:homes[index].handle); disconnectRequested.insert(id) }
                    catch { /* A full action queue is retried until the bounded deadline. */ }
                    continue
                }
                for var event in events {
                    if event["type"] as? String == (classMode ? "identity" : "state") {
                        homes[index].snapshot = event
                        if event["connected"] as? Bool == true, event["busy"] as? Bool == false { homes[index].recovered = false }
                        homes[index].polling = event["connected"] as? Bool == true || event["busy"] as? Bool == true
                    }
                    if event["type"] as? String == (classMode ? "identity" : "state"), event["connected"] as? Bool == true, let password = homes[index].pendingPassword {
                        do { try PasswordStore(homeID:id, serviceClass:classMode).save(password) }
                        catch { batch.append(classMode ? ["type":"error","message":describe(error)] : ["type":"error","home_id":id,"message":describe(error)]) }
                        homes[index].pendingPassword = nil
                    }
                    if event["type"] as? String == "error" { homes[index].pendingPassword = nil }
                    if !classMode { event["home_id"] = id }
                    let type = event["type"] as? String ?? ""
                    if disconnecting.contains(id) {
                        guard disconnectRequested.contains(id), type == (classMode ? "identity" : "state"), event["connected"] as? Bool == false, event["busy"] as? Bool == false else { continue }
                        disconnecting.remove(id); disconnectRequested.remove(id); homes[index].disconnectStarted = nil
                        if classMode { batch.append(["type":"homes","homes":[]]) }
                    }
                    batch.append(event)
                }
            } catch {
                batch.append(contentsOf:recoverHandle(index, reason:describe(error)))
            }
        }
        if wasDisconnecting, disconnecting.isEmpty, visible { batch.append(["type":"lifecycle","active":true]) }
        if !batch.isEmpty { enqueue(batch) }
        if !homes.contains(where:{ NativeLifecycle.shouldPoll(handle:$0.handle, demand:$0.polling, draining:disconnecting.contains($0.id), actionUntil:$0.actionUntil, now:ProcessInfo.processInfo.systemUptime) }) { timer?.invalidate(); timer = nil }
#if os(iOS)
        if !visible && backgroundTask == .invalid && disconnecting.isEmpty {
            timer?.invalidate(); timer = nil
        }
#endif
    }
    private func recoverHandle(_ index: Int, reason: String) -> [[String:Any]] {
        let id = homes[index].id
        homes[index].pendingPassword = nil; homes[index].polling = false; homes[index].actionUntil = 0
        var message = reason
        var closed = false
        do { _ = try response(flowsplice_pty_close(homes[index].handle)); closed = true }
        catch { message += "; close failed: " + describe(error) }
        homes[index].handle = 0
        if NativeLifecycle.shouldReopen(closed:closed, recovered:homes[index].recovered, hasOptions:homes[index].openOptions != nil), let options = homes[index].openOptions {
            homes[index].recovered = true
            do {
                let value = try options.withCString { try response(flowsplice_pty_open($0)) }
                guard let handle = (value as? [String:Any])?["handle"] as? NSNumber, handle.uint64Value != 0 else { throw HostError.invalidResponse }
                homes[index].handle = handle.uint64Value; homes[index].polling = true
            } catch { message += "; reopen failed: " + describe(error) }
        }
        disconnecting.remove(id); disconnectRequested.remove(id); homes[index].disconnectStarted = nil
        var result: [[String:Any]] = []
        if closed {
            var state = homes[index].snapshot
            state["type"] = classMode ? "identity" : "state"; state["connected"] = false; state["busy"] = false
            if !classMode { state["home_id"] = id }
            result.append(state)
            if classMode { result.append(["type":"homes","homes":[]]) }
        }
        if homes[index].handle == 0 { message += ". Please reopen the app to restore the native terminal." }
        homes[index].error = homes[index].handle == 0 ? message : nil
        result.append(classMode ? ["type":"error","message":message] : ["type":"error","home_id":id,"message":message])
        return result
    }
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        decisionHandler(LocalOrigin.accepts(action.request.url) && action.targetFrame?.isMainFrame == true && !action.shouldPerformDownload ? .allow : .cancel)
    }
    func webView(_ webView: WKWebView, decidePolicyFor response: WKNavigationResponse, decisionHandler: @escaping (WKNavigationResponsePolicy) -> Void) {
        decisionHandler(LocalOrigin.accepts(response.response.url) && response.canShowMIMEType ? .allow : .cancel)
    }
    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration, for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? { nil }
    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        recoverRenderer()
    }
#if os(macOS)
    @objc private func windowChanged(_ note: Notification) {
        guard let window = note.object as? NSWindow, window === web.window else { return }
        // Occlusion and minimization do not suspend transports. Reopening a
        // previously closed window resumes its retained workspace.
        if window.occlusionState.contains(.visible) && !NSApp.isHidden { setVisible(true) }
        else { startPolling() }
    }
    @objc private func windowClosed(_ note: Notification) {
        guard let window = note.object as? NSWindow, window === web.window else { return }; setVisible(false); suspendTransports()
    }
    // App hiding leaves the terminal alive. Continue delivering output and its
    // acknowledgements so background commands cannot fill the bridge queue.
    @objc private func applicationHidden() { startPolling(); flush() }
    @objc private func applicationShown() { setVisible(true) }
#endif
    deinit { renderWatchdog?.invalidate(); timer?.invalidate(); for home in homes where home.handle != 0 { flowsplice_pty_string_free(flowsplice_pty_close(home.handle)) }; NotificationCenter.default.removeObserver(self) }
}
