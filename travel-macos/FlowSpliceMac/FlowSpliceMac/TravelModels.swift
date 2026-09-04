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

    var isActive: Bool { self == .preparing || self == .waitingForApproval }
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

nonisolated struct NativeTravelStatusUpdate: Codable, Equatable, Sendable {
    var generation: UInt64
    var status: NativeTravelStatus
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
    var activeRelays: [String] = []
    var catalogGeneration: UInt64 = 0
    var relayDirectoryGeneration: UInt64 = 0
    var mappings: [TravelMapping] = []
    var error: String?

    init() {}

    init(native: NativeTravelStatus, enrolled: Bool = true) {
        phase = .running
        self.enrolled = enrolled
        merge(native: native, phase: .running, clearError: true)
    }

    mutating func merge(
        native: NativeTravelStatus,
        phase nextPhase: TravelPhase? = nil,
        clearError: Bool = false
    ) {
        if let nextPhase { phase = nextPhase }
        online = native.online
        enrolled = true
        travelID = native.travelID
        uptimeSeconds = native.uptimeSeconds
        activeFlows = native.activeFlows
        uploadedBytes = native.uploadedBytes
        downloadedBytes = native.downloadedBytes
        activeRelays = native.activeRelays
        catalogGeneration = native.catalogGeneration
        relayDirectoryGeneration = native.relayDirectoryGeneration
        mappings = native.mappings
        if clearError { error = nil }
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
    var displayName: String { homeAlias.isEmpty || homeAlias == homeID ? homeID : "\(homeAlias) (\(homeID))" }

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
                sorted.services.sort { ($0.displayName, $0.id, $0.protocol) < ($1.displayName, $1.id, $1.protocol) }
                return sorted
            }
            .sorted { ($0.displayName, $0.id) < ($1.displayName, $1.id) }
    }
}

nonisolated struct FlowRouteSnapshot: Codable, Equatable, Identifiable, Sendable {
    var flowID: String
    var homeID: String
    var serviceID: String
    var `protocol`: String
    var localBind: String
    var selectedRelay: String?
    var startedAtUnixSeconds: UInt64
    var lastSwitchUnixSeconds: UInt64?
    var switchCount: UInt32
    var uploadedBytes: UInt64
    var downloadedBytes: UInt64
    var recovering: Bool

    var id: String { flowID }

    enum CodingKeys: String, CodingKey {
        case flowID = "flow_id"
        case homeID = "home_id"
        case serviceID = "service_id"
        case `protocol`
        case localBind = "local_bind"
        case selectedRelay = "selected_relay"
        case startedAtUnixSeconds = "started_at_unix_secs"
        case lastSwitchUnixSeconds = "last_switch_unix_secs"
        case switchCount = "switch_count"
        case uploadedBytes = "uploaded_bytes"
        case downloadedBytes = "downloaded_bytes"
        case recovering
    }
}

nonisolated enum RelayObservation: String, Codable, CaseIterable, Sendable {
    case inUse = "in_use"
    case recentlyReachable = "recently_reachable"
    case eligibleUnverified = "eligible_unverified"
    case recentFailure = "recent_failure"
    case bootstrapOnly = "bootstrap_only"
    case removed

    var title: String {
        switch self {
        case .inUse: "In use"
        case .recentlyReachable: "Recently reachable"
        case .eligibleUnverified: "Eligible, unverified"
        case .recentFailure: "Recent failure"
        case .bootstrapOnly: "Bootstrap only"
        case .removed: "Removed"
        }
    }
}

nonisolated struct RelayRouteSnapshot: Codable, Equatable, Identifiable, Sendable {
    var relayID: String?
    var redactedEndpoint: String
    var currentMember: Bool
    var observation: RelayObservation
    var activeFlowCount: Int
    var lastSeenUnixSeconds: UInt64?
    var lastSuccessUnixSeconds: UInt64?
    var lastFailureUnixSeconds: UInt64?
    var consecutiveFailures: UInt32

    var id: String { relayID ?? "bootstrap-\(redactedEndpoint)" }

    enum CodingKeys: String, CodingKey {
        case relayID = "relay_id"
        case redactedEndpoint = "redacted_endpoint"
        case currentMember = "current_member"
        case observation
        case activeFlowCount = "active_flow_count"
        case lastSeenUnixSeconds = "last_seen_unix_secs"
        case lastSuccessUnixSeconds = "last_success_unix_secs"
        case lastFailureUnixSeconds = "last_failure_unix_secs"
        case consecutiveFailures = "consecutive_failures"
    }
}

nonisolated struct ControlPlaneSnapshot: Codable, Equatable, Sendable {
    var catalogGeneration: UInt64
    var relayDirectoryGeneration: UInt64
    var lastAcceptedUnixSeconds: UInt64?
    var catalogSubscriptionRelay: String?
    var connectedRelays: [String]
    var directorySize: Int
    var degradedReason: String?

    enum CodingKeys: String, CodingKey {
        case catalogGeneration = "catalog_generation"
        case relayDirectoryGeneration = "relay_directory_generation"
        case lastAcceptedUnixSeconds = "last_accepted_unix_secs"
        case catalogSubscriptionRelay = "catalog_subscription_relay"
        case connectedRelays = "connected_relays"
        case directorySize = "directory_size"
        case degradedReason = "degraded_reason"
    }
}

nonisolated struct RouteEvent: Codable, Equatable, Identifiable, Sendable {
    var id: UInt64
    var timestampUnixSeconds: UInt64
    var flowID: String?
    var relayID: String?
    var `protocol`: String
    var phase: String
    var outcome: String
    var latencyMilliseconds: UInt64?
    var reason: String?

    enum CodingKeys: String, CodingKey {
        case id
        case timestampUnixSeconds = "timestamp_unix_secs"
        case flowID = "flow_id"
        case relayID = "relay_id"
        case `protocol`
        case phase
        case outcome
        case latencyMilliseconds = "latency_ms"
        case reason
    }
}

nonisolated struct DiagnosticsSnapshot: Codable, Equatable, Sendable {
    var generatedAtUnixSeconds: UInt64
    var flows: [FlowRouteSnapshot]
    var relays: [RelayRouteSnapshot]
    var controlPlane: ControlPlaneSnapshot
    var events: [RouteEvent]

    enum CodingKeys: String, CodingKey {
        case generatedAtUnixSeconds = "generated_at_unix_secs"
        case flows
        case relays
        case controlPlane = "control_plane"
        case events
    }

    static let empty = DiagnosticsSnapshot(
        generatedAtUnixSeconds: 0,
        flows: [],
        relays: [],
        controlPlane: ControlPlaneSnapshot(
            catalogGeneration: 0,
            relayDirectoryGeneration: 0,
            lastAcceptedUnixSeconds: nil,
            catalogSubscriptionRelay: nil,
            connectedRelays: [],
            directorySize: 0,
            degradedReason: "Travel is stopped."
        ),
        events: []
    )
}

nonisolated struct NativeEnvelope<Value: Decodable>: Decodable {
    var ok: Bool
    var data: Value?
    var errorCode: String?
    var error: String?

    enum CodingKeys: String, CodingKey {
        case ok
        case data
        case errorCode = "error_code"
        case error
    }
}

nonisolated enum TravelError: LocalizedError, Equatable {
    case native(String)
    case invalidResponse
    case missingCredential
    case invalidRelay
    case invalidPort

    var errorDescription: String? {
        switch self {
        case .native(let message): message
        case .invalidResponse: "Travel Core returned an invalid response."
        case .missingCredential: "The private-key password is unavailable in Keychain."
        case .invalidRelay: "Enter a Relay as host:port or IP:port."
        case .invalidPort: "Enter a local port from 1 to 65535."
        }
    }
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

    static func date(_ seconds: UInt64?) -> String {
        guard let seconds else { return "Never" }
        return Date(timeIntervalSince1970: TimeInterval(seconds)).formatted(date: .abbreviated, time: .standard)
    }

    static func shortID(_ value: String?) -> String {
        guard let value, !value.isEmpty else { return "—" }
        return value.count > 16 ? "\(value.prefix(8))…\(value.suffix(5))" : value
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
