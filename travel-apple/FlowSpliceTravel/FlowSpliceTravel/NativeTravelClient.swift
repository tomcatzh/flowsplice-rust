import Foundation

nonisolated final class NativeTravelClient: @unchecked Sendable {
    private let operationQueue = DispatchQueue(label: "io.zxf.flowsplice.travel.ffi")
    private let statusQueue = DispatchQueue(label: "io.zxf.flowsplice.travel.ffi.status")
    private let controlQueue = DispatchQueue(label: "io.zxf.flowsplice.travel.ffi.control")

    func beginEnrollment(
        installDirectory: URL,
        travelID: String,
        homeID: String,
        relay: String,
        password: String
    ) async throws -> EnrollmentSnapshot {
        try await perform(on: operationQueue) {
            let pointer = installDirectory.path.withCString { installDirectoryPointer in
                travelID.withCString { travelIDPointer in
                    homeID.withCString { homeIDPointer in
                        relay.withCString { relayPointer in
                            password.withCString { passwordPointer in
                                flowsplice_travel_begin_enrollment(
                                    installDirectoryPointer,
                                    travelIDPointer,
                                    homeIDPointer,
                                    relayPointer,
                                    passwordPointer
                                )
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
        try await perform(on: operationQueue) {
            try Self.ensureSuccess(flowsplice_travel_cancel_enrollment())
        }
    }

    func start(config: URL, password: String) async throws -> NativeTravelStatus {
        try await perform(on: operationQueue) {
            let pointer = config.path.withCString { configPointer in
                password.withCString { passwordPointer in
                    flowsplice_travel_start(configPointer, passwordPointer)
                }
            }
            return try Self.decode(pointer, as: NativeTravelStatus.self)
        }
    }

    func stop() async throws {
        try await perform(on: operationQueue) {
            try Self.ensureSuccess(flowsplice_travel_stop())
        }
    }

    func notifyNetworkChanged() async throws {
        try await perform(on: controlQueue) {
            try Self.ensureSuccess(flowsplice_travel_network_changed())
        }
    }

    func status() async throws -> NativeTravelStatus {
        try await perform(on: statusQueue) {
            try Self.decode(flowsplice_travel_status(), as: NativeTravelStatus.self)
        }
    }

    func waitForStatusChange(
        generation: UInt64,
        timeoutMillis: UInt64
    ) async throws -> NativeTravelStatusUpdate {
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

    func upsert(mapping: TravelMapping) async throws -> TravelMapping {
        try await perform(on: operationQueue) {
            let payload = try JSONEncoder().encode(mapping)
            guard let json = String(data: payload, encoding: .utf8) else {
                throw TravelError.invalidResponse
            }
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

    private func perform<Value: Sendable>(
        on queue: DispatchQueue,
        _ operation: @escaping @Sendable () throws -> Value
    ) async throws -> Value {
        try await withCheckedThrowingContinuation { continuation in
            queue.async {
                continuation.resume(with: Result { try operation() })
            }
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
        case "not_running":
            "Travel is not running."
        case "secure_connection_failed":
            "Could not establish a secure connection. Check the Relay address and FlowSplice versions."
        case "local_port_unavailable":
            "The local mapping port is unavailable. Choose another port or stop the conflicting listener."
        case "invalid_request":
            "The Travel request is invalid."
        default:
            message ?? "Travel Core could not complete the operation."
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
