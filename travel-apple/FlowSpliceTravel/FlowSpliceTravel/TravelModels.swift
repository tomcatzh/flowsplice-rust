import Foundation

nonisolated enum TravelPhase: String, Codable, Sendable {
    case stopped
    case starting
    case running
    case stopping
    case error
}

nonisolated enum EnrollmentPhase: String, Codable, Sendable {
    case idle
    case preparing
    case waitingForApproval = "waiting_for_approval"
    case installed
    case cancelled
    case error

    var isActive: Bool {
        self == .preparing || self == .waitingForApproval
    }
}

nonisolated struct EnrollmentSnapshot: Codable, Equatable, Sendable {
    var phase: EnrollmentPhase = .idle
    var travelID = ""
    var requestID: String?
    var verificationCode: String?
    var configPath: String?
    var credentialID: String?
    var error: String?

    enum CodingKeys: String, CodingKey {
        case phase
        case travelID = "travel_id"
        case requestID = "request_id"
        case verificationCode = "verification_code"
        case configPath = "config_path"
        case credentialID = "credential_id"
        case error
    }
}

nonisolated struct TravelMapping: Codable, Equatable, Hashable, Identifiable, Sendable {
    var homeID: String
    var serviceID: String
    var `protocol`: String
    var bind: String

    var id: String { "\(homeID)/\(serviceID)/\(`protocol`)" }

    enum CodingKeys: String, CodingKey {
        case homeID = "home_id"
        case serviceID = "service_id"
        case `protocol`
        case bind
    }
}

nonisolated struct NativeTravelStatus: Codable, Equatable, Sendable {
    var ok: Bool
    var online: Bool
    var travelID: String
    var uptimeSeconds: UInt64
    var activeFlows: Int
    var catalogGeneration: UInt64
    var relayDirectoryGeneration: UInt64
    var activeRelays: [String]
    var uploadedBytes: UInt64
    var downloadedBytes: UInt64
    var mappings: [TravelMapping]
    var privateKeyPasswordRotationAvailable: Bool

    enum CodingKeys: String, CodingKey {
        case ok
        case online
        case travelID = "travel_id"
        case uptimeSeconds = "uptime_secs"
        case activeFlows = "active_flows"
        case catalogGeneration = "catalog_generation"
        case relayDirectoryGeneration = "relay_directory_generation"
        case activeRelays = "active_relays"
        case uploadedBytes = "session_uploaded_bytes"
        case downloadedBytes = "session_downloaded_bytes"
        case mappings
        case privateKeyPasswordRotationAvailable = "private_key_password_rotation_available"
    }
}

nonisolated struct TravelSnapshot: Equatable, Sendable {
    var phase: TravelPhase = .stopped
    var online = false
    var enrolled = false
    var travelID = "Travel"
    var uptimeSeconds: UInt64 = 0
    var activeFlows = 0
    var uploadedBytes: UInt64 = 0
    var downloadedBytes: UInt64 = 0
    var relayCount = 0
    var catalogGeneration: UInt64 = 0
    var mappings: [TravelMapping] = []
    var error: String?

    init() {}

    init(native: NativeTravelStatus, enrolled: Bool = true) {
        phase = .running
        online = native.online
        self.enrolled = enrolled
        travelID = native.travelID
        uptimeSeconds = native.uptimeSeconds
        activeFlows = native.activeFlows
        uploadedBytes = native.uploadedBytes
        downloadedBytes = native.downloadedBytes
        relayCount = native.activeRelays.count
        catalogGeneration = native.catalogGeneration
        mappings = native.mappings
    }
}

nonisolated struct CatalogService: Codable, Equatable, Hashable, Identifiable, Sendable {
    var id: String
    var alias: String
    var `protocol`: String
    var target: String

    var key: String { "\(id)/\(`protocol`)" }
    var displayName: String { alias.isEmpty || alias == id ? id : "\(alias) (\(id))" }
}

nonisolated struct CatalogHome: Codable, Equatable, Identifiable, Sendable {
    var homeID: String
    var homeAlias: String
    var services: [CatalogService]

    var id: String { homeID }
    var displayName: String {
        homeAlias.isEmpty || homeAlias == homeID ? homeID : "\(homeAlias) (\(homeID))"
    }

    enum CodingKeys: String, CodingKey {
        case homeID = "home_id"
        case homeAlias = "home_alias"
        case services
    }
}

nonisolated struct TravelCatalog: Codable, Equatable, Sendable {
    var generation: UInt64 = 0
    var homes: [CatalogHome] = []

    var availableHomes: [CatalogHome] {
        homes
            .filter { !$0.services.isEmpty }
            .map { home in
                var sorted = home
                sorted.services.sort {
                    ($0.displayName, $0.id, $0.protocol) < ($1.displayName, $1.id, $1.protocol)
                }
                return sorted
            }
            .sorted { ($0.displayName, $0.id) < ($1.displayName, $1.id) }
    }
}

nonisolated struct NativeEnvelope<Value: Decodable>: Decodable {
    var ok: Bool
    var data: Value?
    var error: String?
}

nonisolated enum TravelError: LocalizedError, Equatable {
    case native(String)
    case invalidResponse
    case missingCredential
    case invalidRelay
    case invalidPort

    var errorDescription: String? {
        switch self {
        case .native(let message): return message
        case .invalidResponse: return "Travel Core returned an invalid response."
        case .missingCredential: return "The private-key password is unavailable in Keychain."
        case .invalidRelay: return "Enter a Relay as host:port or IP:port."
        case .invalidPort: return "Enter a local port from 1 to 65535."
        }
    }
}

struct RecoveryEvent: Identifiable, Equatable, Sendable {
    enum Kind: String, Sendable {
        case network
        case lifecycle
        case catalog
        case error
    }

    let id = UUID()
    var date: Date
    var kind: Kind
    var title: String
    var detail: String
}

enum TravelFormatting {
    static func bytes(_ value: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: value), countStyle: .file)
    }

    static func duration(_ seconds: UInt64) -> String {
        let hours = seconds / 3_600
        let minutes = (seconds % 3_600) / 60
        let remaining = seconds % 60
        if hours > 0 { return String(format: "%02llu:%02llu:%02llu", hours, minutes, remaining) }
        return String(format: "%02llu:%02llu", minutes, remaining)
    }
}

enum TravelValidation {
    static func normalizedID(_ value: String) -> String {
        let allowed = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")
        let mapped = value.trimmingCharacters(in: .whitespacesAndNewlines).unicodeScalars.map {
            allowed.contains($0) ? Character(String($0)) : "-"
        }
        var result = String(mapped)
        while result.contains("--") { result = result.replacingOccurrences(of: "--", with: "-") }
        return String(result.trimmingCharacters(in: CharacterSet(charactersIn: "-._")).prefix(128))
    }

    static func relayIsValid(_ value: String) -> Bool {
        let address = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let separator = address.lastIndex(of: ":") else { return false }
        let host = String(address[..<separator])
        guard let port = Int(address[address.index(after: separator)...]), (1...65_535).contains(port) else {
            return false
        }
        if host.hasPrefix("[") || host.hasSuffix("]") {
            return host.count > 2 && host.hasPrefix("[") && host.hasSuffix("]")
        }
        return !host.isEmpty && !host.contains(":") && host.rangeOfCharacter(from: .whitespacesAndNewlines) == nil
    }
}
