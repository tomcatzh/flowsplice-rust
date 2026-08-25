import Foundation

actor NativeTravelClient {
    private let decoder: JSONDecoder
    private let encoder: JSONEncoder

    init() {
        decoder = JSONDecoder()
        encoder = JSONEncoder()
    }

    func beginEnrollment(
        installDirectory: URL,
        travelID: String,
        homeID: String,
        relay: String,
        password: String
    ) throws -> EnrollmentSnapshot {
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
        return try decode(pointer, as: EnrollmentSnapshot.self)
    }

    func enrollmentStatus() throws -> EnrollmentSnapshot {
        try decode(flowsplice_travel_enrollment_status(), as: EnrollmentSnapshot.self)
    }

    func cancelEnrollment() throws {
        try ensureSuccess(flowsplice_travel_cancel_enrollment())
    }

    func start(config: URL, password: String) throws -> NativeTravelStatus {
        let pointer = config.path.withCString { configPointer in
            password.withCString { passwordPointer in
                flowsplice_travel_start(configPointer, passwordPointer)
            }
        }
        return try decode(pointer, as: NativeTravelStatus.self)
    }

    func stop() throws {
        try ensureSuccess(flowsplice_travel_stop())
    }

    func notifyNetworkChanged() throws {
        try ensureSuccess(flowsplice_travel_network_changed())
    }

    func status() throws -> NativeTravelStatus {
        try decode(flowsplice_travel_status(), as: NativeTravelStatus.self)
    }

    func catalog() throws -> TravelCatalog {
        try decode(flowsplice_travel_catalog(), as: TravelCatalog.self)
    }

    func upsert(mapping: TravelMapping) throws -> TravelMapping {
        let payload = try encoder.encode(mapping)
        guard let json = String(data: payload, encoding: .utf8) else {
            throw TravelError.invalidResponse
        }
        let pointer = json.withCString { flowsplice_travel_upsert_mapping($0) }
        return try decode(pointer, as: TravelMapping.self)
    }

    func delete(mapping: TravelMapping) throws -> TravelMapping {
        let pointer = mapping.homeID.withCString { homePointer in
            mapping.serviceID.withCString { servicePointer in
                mapping.protocol.withCString { protocolPointer in
                    flowsplice_travel_delete_mapping(homePointer, servicePointer, protocolPointer)
                }
            }
        }
        return try decode(pointer, as: TravelMapping.self)
    }

    private func data(from pointer: UnsafeMutablePointer<CChar>?) throws -> Data {
        guard let pointer else { throw TravelError.invalidResponse }
        defer { flowsplice_travel_string_free(pointer) }
        guard let data = String(validatingUTF8: pointer)?.data(using: .utf8) else {
            throw TravelError.invalidResponse
        }
        return data
    }

    private func decode<Value: Decodable>(
        _ pointer: UnsafeMutablePointer<CChar>?,
        as type: Value.Type
    ) throws -> Value {
        let payload = try data(from: pointer)
        let envelope = try decoder.decode(NativeEnvelope<Value>.self, from: payload)
        guard envelope.ok else {
            throw TravelError.native(Self.friendly(error: envelope.error ?? "Travel Core failed."))
        }
        guard let value = envelope.data else { throw TravelError.invalidResponse }
        return value
    }

    private func ensureSuccess(_ pointer: UnsafeMutablePointer<CChar>?) throws {
        let payload = try data(from: pointer)
        let envelope = try decoder.decode(NativeStatusEnvelope.self, from: payload)
        guard envelope.ok else {
            throw TravelError.native(Self.friendly(error: envelope.error ?? "Travel Core failed."))
        }
    }

    private static func friendly(error: String) -> String {
        let lower = error.lowercased()
        if lower.contains("relay discovery tls handshake failed") ||
            lower.contains("peer closed connection without sending tls close_notify") {
            return "Could not establish a secure enrollment connection. Check the Relay management port and confirm both sides use the same FlowSplice 0.3 build."
        }
        return error
    }
}

nonisolated private struct NativeStatusEnvelope: Decodable {
    let ok: Bool
    let error: String?
}
