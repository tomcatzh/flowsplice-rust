#if DEBUG
import Foundation

nonisolated final class DemoTravelClient: TravelClient, @unchecked Sendable {
    private struct State {
        var running = true
        var enrollmentPolls = 0
        var generation: UInt64 = 1
        var mappings = [
            TravelMapping(homeID: "home-1", serviceID: "ssh", protocol: "tcp", bind: "127.0.0.1:10022"),
            TravelMapping(homeID: "home-1", serviceID: "dns", protocol: "udp", bind: "127.0.0.1:1053"),
        ]
    }

    private let lock = NSLock()
    private var state = State()

    func beginEnrollment(
        installDirectory: URL,
        travelID: String,
        homeID: String,
        relay: String,
        password: String
    ) async throws -> EnrollmentSnapshot {
        lock.withLock { state.enrollmentPolls = 0 }
        return EnrollmentSnapshot(
            phase: .waitingForApproval,
            travelID: travelID,
            requestID: UUID().uuidString,
            verificationCode: "482 731",
            configPath: nil,
            credentialID: nil,
            error: nil
        )
    }

    func enrollmentStatus() async throws -> EnrollmentSnapshot {
        lock.withLock {
            state.enrollmentPolls += 1
            if state.enrollmentPolls > 3 {
                return EnrollmentSnapshot(
                    phase: .installed,
                    travelID: "demo-mac",
                    requestID: nil,
                    verificationCode: nil,
                    configPath: "/mock/FlowSplice/travelagent.toml",
                    credentialID: UUID().uuidString,
                    error: nil
                )
            }
            return EnrollmentSnapshot(
                phase: .waitingForApproval,
                travelID: "demo-mac",
                requestID: UUID().uuidString,
                verificationCode: "482 731",
                configPath: nil,
                credentialID: nil,
                error: nil
            )
        }
    }

    func cancelEnrollment() async throws {}

    func start(config: URL, password: String) async throws -> NativeTravelStatus {
        lock.withLock {
            state.running = true
            state.generation &+= 1
            return makeStatus(state)
        }
    }

    func stop() async throws {
        lock.withLock {
            state.running = false
            state.generation &+= 1
        }
    }

    func notifyNetworkChanged() async throws {
        lock.withLock { state.generation &+= 1 }
    }

    func status() async throws -> NativeTravelStatus {
        lock.withLock { makeStatus(state) }
    }

    func waitForStatusChange(generation: UInt64, timeoutMillis: UInt64) async throws -> NativeTravelStatusUpdate {
        try await Task.sleep(for: .milliseconds(min(timeoutMillis, 350)))
        return lock.withLock {
            NativeTravelStatusUpdate(generation: state.generation, status: makeStatus(state))
        }
    }

    func wakeStatusWaiters() async {}

    func catalog() async throws -> TravelCatalog {
        TravelCatalog(
            generation: 42,
            homes: [
                CatalogHome(
                    homeID: "home-1",
                    homeAlias: "Studio",
                    services: [
                        CatalogService(id: "ssh", alias: "Secure Shell", protocol: "tcp", target: "127.0.0.1:22"),
                        CatalogService(id: "dns", alias: "DNS", protocol: "udp", target: "127.0.0.1:53"),
                        CatalogService(id: "photos", alias: "Photo Library", protocol: "tcp", target: "127.0.0.1:8080"),
                    ]
                ),
            ]
        )
    }

    func diagnostics() async throws -> DiagnosticsSnapshot {
        let now = UInt64(Date().timeIntervalSince1970)
        return DiagnosticsSnapshot(
            generatedAtUnixSeconds: now,
            flows: [
                FlowRouteSnapshot(
                    flowID: "8D5367B1-A375-4C6A-A80A-1ED67C1D7C7C",
                    homeID: "home-1",
                    serviceID: "ssh",
                    protocol: "tcp",
                    localBind: "127.0.0.1:10022",
                    selectedRelay: "relay-shanghai",
                    startedAtUnixSeconds: now - 1_840,
                    lastSwitchUnixSeconds: now - 420,
                    switchCount: 1,
                    uploadedBytes: 4_820_132,
                    downloadedBytes: 18_402_100,
                    recovering: false
                ),
                FlowRouteSnapshot(
                    flowID: "9E6478C2-B486-5D7B-B91B-2FE78D2E8D8D",
                    homeID: "home-1",
                    serviceID: "dns",
                    protocol: "udp",
                    localBind: "127.0.0.1:1053",
                    selectedRelay: "relay-tokyo",
                    startedAtUnixSeconds: now - 82,
                    lastSwitchUnixSeconds: nil,
                    switchCount: 0,
                    uploadedBytes: 12_220,
                    downloadedBytes: 48_912,
                    recovering: false
                ),
            ],
            relays: [
                RelayRouteSnapshot(
                    relayID: "relay-shanghai",
                    redactedEndpoint: "•••:8443",
                    currentMember: true,
                    observation: .inUse,
                    activeFlowCount: 1,
                    lastSeenUnixSeconds: now - 4,
                    lastSuccessUnixSeconds: now - 4,
                    lastFailureUnixSeconds: nil,
                    consecutiveFailures: 0
                ),
                RelayRouteSnapshot(
                    relayID: "relay-tokyo",
                    redactedEndpoint: "•••:8443",
                    currentMember: true,
                    observation: .inUse,
                    activeFlowCount: 1,
                    lastSeenUnixSeconds: now - 8,
                    lastSuccessUnixSeconds: now - 8,
                    lastFailureUnixSeconds: nil,
                    consecutiveFailures: 0
                ),
                RelayRouteSnapshot(
                    relayID: "relay-frankfurt",
                    redactedEndpoint: "•••:8443",
                    currentMember: true,
                    observation: .eligibleUnverified,
                    activeFlowCount: 0,
                    lastSeenUnixSeconds: now - 38,
                    lastSuccessUnixSeconds: nil,
                    lastFailureUnixSeconds: nil,
                    consecutiveFailures: 0
                ),
                RelayRouteSnapshot(
                    relayID: "relay-seattle",
                    redactedEndpoint: "•••:8443",
                    currentMember: true,
                    observation: .recentFailure,
                    activeFlowCount: 0,
                    lastSeenUnixSeconds: now - 64,
                    lastSuccessUnixSeconds: now - 4_500,
                    lastFailureUnixSeconds: now - 64,
                    consecutiveFailures: 2
                ),
                RelayRouteSnapshot(
                    relayID: nil,
                    redactedEndpoint: "•••:8443",
                    currentMember: false,
                    observation: .bootstrapOnly,
                    activeFlowCount: 0,
                    lastSeenUnixSeconds: nil,
                    lastSuccessUnixSeconds: nil,
                    lastFailureUnixSeconds: nil,
                    consecutiveFailures: 0
                ),
            ],
            controlPlane: ControlPlaneSnapshot(
                catalogGeneration: 42,
                relayDirectoryGeneration: 17,
                lastAcceptedUnixSeconds: now - 8,
                catalogSubscriptionRelay: "relay-shanghai",
                connectedRelays: ["relay-shanghai"],
                directorySize: 4,
                degradedReason: nil
            ),
            events: [
                RouteEvent(id: 3, timestampUnixSeconds: now - 420, flowID: "8D5367B1-A375-4C6A-A80A-1ED67C1D7C7C", relayID: "relay-shanghai", protocol: "tcp", phase: "carrier", outcome: "switched", latencyMilliseconds: 38, reason: nil),
                RouteEvent(id: 2, timestampUnixSeconds: now - 422, flowID: "8D5367B1-A375-4C6A-A80A-1ED67C1D7C7C", relayID: "relay-seattle", protocol: "tcp", phase: "route_attempt", outcome: "failed", latencyMilliseconds: 1_004, reason: "Route attempt timed out."),
                RouteEvent(id: 1, timestampUnixSeconds: now - 1_840, flowID: "8D5367B1-A375-4C6A-A80A-1ED67C1D7C7C", relayID: "relay-tokyo", protocol: "tcp", phase: "flow", outcome: "started", latencyMilliseconds: nil, reason: nil),
            ]
        )
    }

    func upsert(mapping: TravelMapping) async throws -> TravelMapping {
        lock.withLock {
            state.mappings.removeAll { $0.id == mapping.id }
            state.mappings.append(mapping)
            state.generation &+= 1
        }
        return mapping
    }

    func delete(mapping: TravelMapping) async throws -> TravelMapping {
        lock.withLock {
            state.mappings.removeAll { $0.id == mapping.id }
            state.generation &+= 1
        }
        return mapping
    }

    private func makeStatus(_ state: State) -> NativeTravelStatus {
        NativeTravelStatus(
            ok: true,
            online: state.running,
            travelID: "studio-mac",
            uptimeSeconds: state.running ? 18_420 : 0,
            activeFlows: state.running ? 2 : 0,
            catalogGeneration: 42,
            relayDirectoryGeneration: 17,
            activeRelays: state.running ? ["relay-shanghai", "relay-tokyo"] : [],
            uploadedBytes: state.running ? 4_832_352 : 0,
            downloadedBytes: state.running ? 18_451_012 : 0,
            mappings: state.mappings,
            privateKeyPasswordRotationAvailable: true
        )
    }
}
#endif
