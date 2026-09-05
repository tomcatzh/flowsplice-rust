import Foundation

nonisolated protocol TravelClient: Sendable {
    func beginEnrollment(
        installDirectory: URL,
        travelID: String,
        homeID: String,
        relay: String,
        password: String
    ) async throws -> EnrollmentSnapshot
    func enrollmentStatus() async throws -> EnrollmentSnapshot
    func cancelEnrollment() async throws
    func start(config: URL, password: String) async throws -> NativeTravelStatus
    func stop() async throws
    func notifyNetworkChanged() async throws
    func status() async throws -> NativeTravelStatus
    func waitForStatusChange(generation: UInt64, timeoutMillis: UInt64) async throws -> NativeTravelStatusUpdate
    func wakeStatusWaiters() async
    func catalog() async throws -> TravelCatalog
    func diagnostics() async throws -> DiagnosticsSnapshot
    func upsert(mapping: TravelMapping) async throws -> TravelMapping
    func delete(mapping: TravelMapping) async throws -> TravelMapping
}

nonisolated final class NativeTravelClient: TravelClient, @unchecked Sendable {
    private let operationQueue = DispatchQueue(label: "io.zxf.flowsplice.travel.macos.ffi")
    private let statusQueue = DispatchQueue(label: "io.zxf.flowsplice.travel.macos.ffi.status")
    private let controlQueue = DispatchQueue(label: "io.zxf.flowsplice.travel.macos.ffi.control")

    func beginEnrollment(
        installDirectory: URL,
        travelID: String,
        homeID: String,
        relay: String,
        password: String
    ) async throws -> EnrollmentSnapshot {
        try await perform(on: operationQueue) {
            let trustedRoot = try Self.packagedDeploymentRoot()
            let pointer = trustedRoot.withCString { rootPointer in
            installDirectory.path.withCString { installDirectoryPointer in
                travelID.withCString { travelIDPointer in
                    homeID.withCString { homeIDPointer in
                        relay.withCString { relayPointer in
                            password.withCString { passwordPointer in
                                flowsplice_travel_begin_enrollment(
                                    installDirectoryPointer,
                                    travelIDPointer,
                                    homeIDPointer,
                                    relayPointer,
                                    passwordPointer,
                                    rootPointer
                                )
                            }
                        }
                    }
                }
            }
            }
            return try Self.decode(pointer, as: EnrollmentSnapshot.self)
        }
    }

    func enrollmentStatus() async throws -> EnrollmentSnapshot {
        try await perform(on: operationQueue) {
            try Self.decode(flowsplice_travel_enrollment_status(), as: EnrollmentSnapshot.self)
        }
    }

    func cancelEnrollment() async throws {
        try await perform(on: operationQueue) { try Self.ensureSuccess(flowsplice_travel_cancel_enrollment()) }
    }

    func start(config: URL, password: String) async throws -> NativeTravelStatus {
        try await perform(on: operationQueue) {
            let trustedRoot = try Self.packagedDeploymentRoot()
            let pointer = trustedRoot.withCString { rootPointer in
            config.path.withCString { configPointer in
                password.withCString { passwordPointer in
                    flowsplice_travel_start(configPointer, passwordPointer, rootPointer)
                }
            }
            }
            return try Self.decode(pointer, as: NativeTravelStatus.self)
        }
    }

    func stop() async throws {
        try await perform(on: operationQueue) { try Self.ensureSuccess(flowsplice_travel_stop()) }
    }

    func notifyNetworkChanged() async throws {
        try await perform(on: controlQueue) { try Self.ensureSuccess(flowsplice_travel_network_changed()) }
    }

    func status() async throws -> NativeTravelStatus {
        try await perform(on: statusQueue) {
            try Self.decode(flowsplice_travel_status(), as: NativeTravelStatus.self)
        }
    }

    func waitForStatusChange(generation: UInt64, timeoutMillis: UInt64) async throws -> NativeTravelStatusUpdate {
        try await perform(on: statusQueue) {
            try Self.decode(
                flowsplice_travel_wait_for_status_change(generation, timeoutMillis),
                as: NativeTravelStatusUpdate.self
            )
        }
    }

    func wakeStatusWaiters() async {
        try? await perform(on: controlQueue) {
            try Self.ensureSuccess(flowsplice_travel_wake_status_waiters())
        }
    }

    func catalog() async throws -> TravelCatalog {
        try await perform(on: operationQueue) {
            try Self.decode(flowsplice_travel_catalog(), as: TravelCatalog.self)
        }
    }

    func diagnostics() async throws -> DiagnosticsSnapshot {
        try await perform(on: operationQueue) {
            try Self.decode(flowsplice_travel_diagnostics(), as: DiagnosticsSnapshot.self)
        }
    }

    func upsert(mapping: TravelMapping) async throws -> TravelMapping {
        try await perform(on: operationQueue) {
            let payload = try JSONEncoder().encode(mapping)
            guard let json = String(data: payload, encoding: .utf8) else { throw TravelError.invalidResponse }
            return try Self.decode(
                json.withCString { flowsplice_travel_upsert_mapping($0) },
                as: TravelMapping.self
            )
        }
    }

    func delete(mapping: TravelMapping) async throws -> TravelMapping {
        try await perform(on: operationQueue) {
            let pointer = mapping.homeID.withCString { homePointer in
                mapping.serviceID.withCString { servicePointer in
                    mapping.protocol.withCString { protocolPointer in
                        flowsplice_travel_delete_mapping(homePointer, servicePointer, protocolPointer)
                    }
                }
            }
            return try Self.decode(pointer, as: TravelMapping.self)
        }
    }

    private static func packagedDeploymentRoot() throws -> String {
        guard let url = Bundle.main.url(forResource: "deployment-root", withExtension: "pub", subdirectory: "bootstrap") else {
            throw TravelError.native("This app package has no deployment trust key. Install the private package for your deployment.")
        }
        let file = try FileHandle(forReadingFrom: url)
        defer { try? file.close() }
        guard let bytes = try file.read(upToCount: 257), bytes.count <= 256,
              let root = String(data: bytes, encoding: .utf8) else {
            throw TravelError.native("This app package contains an invalid deployment trust key.")
        }
        return root.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func perform<Value: Sendable>(
        on queue: DispatchQueue,
        _ operation: @escaping @Sendable () throws -> Value
    ) async throws -> Value {
        try await withCheckedThrowingContinuation { continuation in
            queue.async { continuation.resume(with: Result { try operation() }) }
        }
    }

    private static func data(from pointer: UnsafeMutablePointer<CChar>?) throws -> Data {
        guard let pointer else { throw TravelError.invalidResponse }
        defer { flowsplice_travel_string_free(pointer) }
        guard let data = String(validatingUTF8: pointer)?.data(using: .utf8) else {
            throw TravelError.invalidResponse
        }
        return data
    }

    private static func decode<Value: Decodable>(
        _ pointer: UnsafeMutablePointer<CChar>?,
        as type: Value.Type
    ) throws -> Value {
        let payload = try data(from: pointer)
        let envelope = try JSONDecoder().decode(NativeEnvelope<Value>.self, from: payload)
        guard envelope.ok else {
            throw TravelError.native(friendly(code: envelope.errorCode, message: envelope.error))
        }
        guard let value = envelope.data else { throw TravelError.invalidResponse }
        return value
    }

    private static func ensureSuccess(_ pointer: UnsafeMutablePointer<CChar>?) throws {
        let payload = try data(from: pointer)
        let envelope = try JSONDecoder().decode(NativeStatusEnvelope.self, from: payload)
        guard envelope.ok else {
            throw TravelError.native(friendly(code: envelope.errorCode, message: envelope.error))
        }
    }

    private static func friendly(code: String?, message: String?) -> String {
        switch code {
        case "not_running": "Travel is not running."
        case "secure_connection_failed": "Could not establish a secure connection. Check the Relay address and FlowSplice versions."
        case "local_port_unavailable": "The local mapping port is unavailable. Choose another port or stop the conflicting listener."
        case "invalid_request": "The Travel request is invalid."
        default: message ?? "Travel Core could not complete the operation."
        }
    }
}

nonisolated private struct NativeStatusEnvelope: Decodable {
    let ok: Bool
    let errorCode: String?
    let error: String?

    enum CodingKeys: String, CodingKey {
        case ok
        case errorCode = "error_code"
        case error
    }
}
