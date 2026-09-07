import Foundation
import Security

enum TravelFiles {
    private static let installationFolder = "FlowSplice"
    private static let managedConfigPaths = [
        "deployment_root_public_key": "cert/deployment-root.pub",
        "deployment_trust": "cert/deployment-trust.json",
        "management_cert": "cert/travel-management.crt",
        "management_key": "cert/travel-management.key",
        "management_ca": "cert/management-ca.crt",
        "business_cert": "cert/travel-business.crt",
        "business_key": "cert/travel-business.key",
        "business_ca": "cert/business-ca.crt",
        "state_store": "state/travel-state.redb",
        "enrollment_work_dir": "state/enrollment",
    ]

    static var installationDirectory: URL {
        #if DEBUG
        if ProcessInfo.processInfo.environment["FLOWSPLICE_UI_TESTING"] == "1" {
            return FileManager.default.temporaryDirectory
                .appending(path: "FlowSpliceMacUITests", directoryHint: .isDirectory)
        }
        #endif
        return FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appending(path: installationFolder, directoryHint: .isDirectory)
    }

    static var config: URL {
        installationDirectory.appending(path: "travelagent.toml", directoryHint: .notDirectory)
    }

    static var isInstalled: Bool { FileManager.default.fileExists(atPath: config.path) }

    static func prepareInstallationDirectory() throws {
        try FileManager.default.createDirectory(
            at: installationDirectory,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
    }

    static func prepareRuntimeStorage(at directory: URL = installationDirectory) throws {
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
        try FileManager.default.createDirectory(
            at: directory.appending(path: "state", directoryHint: .isDirectory),
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
        let config = directory.appending(path: "travelagent.toml", directoryHint: .notDirectory)
        guard FileManager.default.fileExists(atPath: config.path) else { return }
        let source = try String(contentsOf: config, encoding: .utf8)
        let migrated = rebasedGeneratedConfig(source, installationDirectory: directory)
        if migrated != source {
            try Data(migrated.utf8).write(to: config, options: .atomic)
        }
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: config.path)
    }

    static func rebasedGeneratedConfig(_ source: String, installationDirectory: URL) -> String {
        source
            .split(separator: "\n", omittingEmptySubsequences: false)
            .map { substring in
                let line = String(substring)
                let indentation = line.prefix { $0 == " " || $0 == "\t" }
                let content = line.dropFirst(indentation.count)
                guard let (key, relativePath) = managedConfigPaths.first(where: { content.hasPrefix("\($0.key) = ") }) else {
                    return line
                }
                let target = installationDirectory.appending(path: relativePath).path
                return "\(indentation)\(key) = \"\(tomlEscaped(target))\""
            }
            .joined(separator: "\n")
    }

    static func discardPendingInstallation() throws {
        guard !isInstalled, FileManager.default.fileExists(atPath: installationDirectory.path) else { return }
        try FileManager.default.removeItem(at: installationDirectory)
    }

    static func resetForUITesting() {
        #if DEBUG
        guard ProcessInfo.processInfo.environment["FLOWSPLICE_UI_TESTING"] == "1",
              ProcessInfo.processInfo.arguments.contains("--reset-state") else { return }
        try? FileManager.default.removeItem(at: installationDirectory)
        CredentialStore.clear()
        EnrollmentStore.reset()
        #endif
    }

    private static func tomlEscaped(_ value: String) -> String {
        value.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"")
    }
}

nonisolated struct PendingEnrollment: Codable, Equatable, Sendable {
    var travelID: String
    var homeID: String
    var relay: String
}

enum EnrollmentStore {
    private static let pendingKey = "flowsplice.macos.pending-enrollment"
    private static let relayKey = "flowsplice.macos.last-relay"
    private static let autoStartKey = "flowsplice.macos.auto-start"

    #if DEBUG
    static let uiTestSuiteName = "io.zxf.flowsplice.travel.macos.ui-tests"
    #endif

    private static var defaults: UserDefaults {
        #if DEBUG
        if ProcessInfo.processInfo.environment["FLOWSPLICE_UI_TESTING"] == "1" {
            guard let suite = UserDefaults(suiteName: uiTestSuiteName) else {
                preconditionFailure("Could not open the isolated UI-test preferences suite")
            }
            return suite
        }
        #endif
        return UserDefaults.standard
    }

    static var pending: PendingEnrollment? {
        get {
            guard let data = defaults.data(forKey: pendingKey) else { return nil }
            return try? JSONDecoder().decode(PendingEnrollment.self, from: data)
        }
        set {
            if let newValue, let data = try? JSONEncoder().encode(newValue) {
                defaults.set(data, forKey: pendingKey)
            } else {
                defaults.removeObject(forKey: pendingKey)
            }
        }
    }

    static var lastRelay: String {
        get { defaults.string(forKey: relayKey) ?? "" }
        set { defaults.set(newValue.trimmingCharacters(in: .whitespacesAndNewlines), forKey: relayKey) }
    }

    static var autoStart: Bool {
        get { defaults.object(forKey: autoStartKey) as? Bool ?? false }
        set { defaults.set(newValue, forKey: autoStartKey) }
    }

    static func reset() {
        pending = nil
        defaults.removeObject(forKey: relayKey)
        defaults.removeObject(forKey: autoStartKey)
    }
}

nonisolated enum CredentialStore {
    private static let service = "io.zxf.flowsplice.travel.macos"
    private static let account = "private-key-password"

    private static var itemQuery: [CFString: Any] {
        [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
        ]
    }

    static func save(password: String) throws {
        guard let data = password.data(using: .utf8) else { throw TravelError.invalidResponse }
        try withTestKeychain { testKeychain in
            let query = matchingQuery(testKeychain: testKeychain)
            let updateStatus = SecItemUpdate(
                query as CFDictionary,
                [kSecValueData: data] as CFDictionary
            )
            if updateStatus == errSecSuccess {
                return
            }
            guard updateStatus == errSecItemNotFound else {
                throw keychainError(operation: "update", status: updateStatus)
            }

            var newItem = addQuery(testKeychain: testKeychain)
            newItem[kSecValueData] = data
            let addStatus = SecItemAdd(newItem as CFDictionary, nil)
            guard addStatus == errSecSuccess else {
                throw keychainError(operation: "save", status: addStatus)
            }
        }
    }

    static func load() -> String? {
        try? loadPromptingIfNeeded(prompt: false)
    }

    static func loadPromptingIfNeeded(prompt: Bool = true) throws -> String? {
        _ = prompt
        var resultData: Data?
        try withTestKeychain { testKeychain in
            var query = matchingQuery(testKeychain: testKeychain)
            query[kSecReturnData] = true
            query[kSecMatchLimit] = kSecMatchLimitOne
            var value: CFTypeRef?
            let status = SecItemCopyMatching(query as CFDictionary, &value)
            if status == errSecItemNotFound {
                return
            }
            guard status == errSecSuccess else {
                throw keychainError(operation: "read", status: status)
            }
            resultData = value as? Data
        }
        guard let data = resultData else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func clear() {
        do {
            try withTestKeychain { testKeychain in
                let query = matchingQuery(testKeychain: testKeychain)
                SecItemDelete(query as CFDictionary)
            }
        } catch {
            return
        }
    }

    private static func requiresKeychainAuthentication(_ status: OSStatus) -> Bool {
        status == errSecAuthFailed
            || status == errSecInteractionNotAllowed
            || status == errSecInteractionRequired
    }

    private static func matchingQuery(testKeychain: SecKeychain?) -> [CFString: Any] {
        var query = itemQuery
        if let testKeychain {
            query[kSecMatchSearchList] = [testKeychain] as CFArray
        }
        return query
    }

    private static func addQuery(testKeychain: SecKeychain?) -> [CFString: Any] {
        var query = itemQuery
        if let testKeychain {
            query[kSecUseKeychain] = testKeychain
        }
        return query
    }

    @discardableResult
    private static func withTestKeychain<T>(
        _ block: (_ testKeychain: SecKeychain?) throws -> T
    ) throws -> T {
        #if DEBUG
        let environment = ProcessInfo.processInfo.environment
        guard environment["FLOWSPLICE_UI_TESTING"] == "1",
              let keychainPath = environment["FLOWSPLICE_UI_TEST_KEYCHAIN_PATH"], !keychainPath.isEmpty,
              let keychainPassword = environment["FLOWSPLICE_UI_TEST_KEYCHAIN_PASSWORD"], !keychainPassword.isEmpty
        else {
            return try block(nil)
        }

        var keychain: SecKeychain?
        let openStatus = keychainPath.withCString {
            SecKeychainOpen($0, &keychain)
        }
        guard openStatus == errSecSuccess, let keychain = keychain else {
            throw keychainError(operation: "open the injected test keychain", status: openStatus)
        }
        let keychainPasswordBytes = Array(keychainPassword.utf8)
        let unlockStatus = keychainPasswordBytes.withUnsafeBufferPointer {
            SecKeychainUnlock(keychain, UInt32($0.count), $0.baseAddress, true)
        }
        guard unlockStatus == errSecSuccess else {
            throw keychainError(operation: "unlock the injected test keychain", status: unlockStatus)
        }

        return try block(keychain)
        #else
        return try block(nil)
        #endif
    }

    private static func keychainError(operation: String, status: OSStatus) -> TravelError {
        let detail = SecCopyErrorMessageString(status, nil) as String? ?? "Unknown Keychain error"
        if requiresKeychainAuthentication(status) {
            return TravelError.native(
                "Could not \(operation) the private-key password in Keychain (\(status): \(detail)). " +
                "Unlock your macOS login Keychain with your Mac login password."
            )
        }
        return TravelError.native("Could not \(operation) the private-key password in Keychain (\(status): \(detail)).")
    }
}
