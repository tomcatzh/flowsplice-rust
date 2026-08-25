import Foundation
import Security

enum TravelFiles {
    private static let installationFolder = "FlowSpliceTravel"

    static var installationDirectory: URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return base.appending(path: installationFolder, directoryHint: .isDirectory)
    }

    static var config: URL {
        installationDirectory.appending(path: "travelagent.toml", directoryHint: .notDirectory)
    }

    static var isInstalled: Bool {
        FileManager.default.fileExists(atPath: config.path)
    }

    static func prepareInstallationDirectory() throws {
        try FileManager.default.createDirectory(
            at: installationDirectory,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
    }

    static func discardPendingInstallation() throws {
        guard !isInstalled, FileManager.default.fileExists(atPath: installationDirectory.path) else {
            return
        }
        try FileManager.default.removeItem(at: installationDirectory)
    }

    static func writeE2EVerificationCode(_ code: String) {
        guard ProcessInfo.processInfo.environment["FLOWSPLICE_E2E"] == "1" else { return }
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        try? code.write(
            to: documents.appending(path: "e2e-verification-code"),
            atomically: true,
            encoding: .utf8
        )
    }

    static func writeE2EPhase(_ phase: String) {
        guard ProcessInfo.processInfo.environment["FLOWSPLICE_E2E"] == "1" else { return }
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let safePhase = TravelValidation.normalizedID(phase)
        try? "ready\n".write(
            to: documents.appending(path: "e2e-phase-\(safePhase)"),
            atomically: true,
            encoding: .utf8
        )
    }

    static func resetForUITesting() {
        guard ProcessInfo.processInfo.environment["FLOWSPLICE_UI_TEST_RESET"] == "1" else { return }
        try? FileManager.default.removeItem(at: installationDirectory)
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        try? FileManager.default.removeItem(at: documents.appending(path: "e2e-verification-code"))
        if let files = try? FileManager.default.contentsOfDirectory(at: documents, includingPropertiesForKeys: nil) {
            for file in files where file.lastPathComponent.hasPrefix("e2e-phase-") {
                try? FileManager.default.removeItem(at: file)
            }
        }
        EnrollmentStore.reset()
        UserDefaults.standard.removeObject(forKey: "flowsplice.continued-session-identifier")
        CredentialStore.clear()
    }
}

nonisolated struct PendingEnrollment: Codable, Equatable, Sendable {
    var travelID: String
    var homeID: String
    var relay: String
}

enum EnrollmentStore {
    private static let key = "flowsplice.pending-enrollment"
    private static let relayKey = "flowsplice.last-relay"
    private static let autoStartKey = "flowsplice.auto-start"

    static var pending: PendingEnrollment? {
        get {
            guard let data = UserDefaults.standard.data(forKey: key) else { return nil }
            return try? JSONDecoder().decode(PendingEnrollment.self, from: data)
        }
        set {
            if let newValue, let data = try? JSONEncoder().encode(newValue) {
                UserDefaults.standard.set(data, forKey: key)
            } else {
                UserDefaults.standard.removeObject(forKey: key)
            }
        }
    }

    static var lastRelay: String {
        get { UserDefaults.standard.string(forKey: relayKey) ?? "" }
        set { UserDefaults.standard.set(newValue.trimmingCharacters(in: .whitespacesAndNewlines), forKey: relayKey) }
    }

    static var autoStart: Bool {
        get { UserDefaults.standard.object(forKey: autoStartKey) as? Bool ?? true }
        set { UserDefaults.standard.set(newValue, forKey: autoStartKey) }
    }

    static func reset() {
        pending = nil
        UserDefaults.standard.removeObject(forKey: relayKey)
        UserDefaults.standard.removeObject(forKey: autoStartKey)
    }
}

enum CredentialStore {
    private static let service = "io.zxf.flowsplice.travel"
    private static let account = "private-key-password"

    static func save(password: String) throws {
        guard let data = password.data(using: .utf8) else { throw TravelError.invalidResponse }
        clear()
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
            kSecAttrAccessible: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecValueData: data,
        ]
        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw TravelError.native("Could not save the private-key password in Keychain (\(status)).")
        }
    }

    static func load() -> String? {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
            kSecReturnData: true,
            kSecMatchLimit: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else {
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    static func clear() {
        let query: [CFString: Any] = [
            kSecClass: kSecClassGenericPassword,
            kSecAttrService: service,
            kSecAttrAccount: account,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
