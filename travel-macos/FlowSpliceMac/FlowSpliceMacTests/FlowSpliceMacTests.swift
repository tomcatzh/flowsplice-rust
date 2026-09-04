import Foundation
import Testing
@testable import FlowSpliceMac

struct FlowSpliceMacTests {
    @Test func normalizesIdentifiersWithoutInventingCharacters() {
        #expect(TravelValidation.normalizedID("  Tomcat’s Mac / Studio  ") == "Tomcat-s-Mac-Studio")
        #expect(TravelValidation.normalizedID("...---") == "")
        #expect(TravelValidation.normalizedID(String(repeating: "a", count: 150)).count == 128)
    }

    @Test(arguments: [
        ("relay.example.com:8443", true),
        ("127.0.0.1:1", true),
        ("[::1]:65535", true),
        ("relay.example.com", false),
        ("relay.example.com:0", false),
        ("::1:8443", false),
    ])
    func validatesRelayAddresses(value: String, expected: Bool) {
        #expect(TravelValidation.relayIsValid(value) == expected)
    }

    @Test func decodesNativeDiagnosticsContract() throws {
        let json = #"{"generated_at_unix_secs":1700000000,"flows":[{"flow_id":"flow-1","home_id":"home-1","service_id":"ssh","protocol":"tcp","local_bind":"127.0.0.1:10022","selected_relay":"relay-a","started_at_unix_secs":1699999990,"last_switch_unix_secs":null,"switch_count":0,"uploaded_bytes":12,"downloaded_bytes":34,"recovering":false}],"relays":[{"relay_id":"relay-a","redacted_endpoint":"•••:8443","current_member":true,"observation":"in_use","active_flow_count":1,"last_seen_unix_secs":1699999999,"last_success_unix_secs":1699999999,"last_failure_unix_secs":null,"consecutive_failures":0}],"control_plane":{"catalog_generation":4,"relay_directory_generation":9,"last_accepted_unix_secs":1699999999,"catalog_subscription_relay":"relay-a","connected_relays":["relay-a"],"directory_size":1,"degraded_reason":null},"events":[]}"#
        let decoded = try JSONDecoder().decode(DiagnosticsSnapshot.self, from: Data(json.utf8))
        #expect(decoded.flows.first?.selectedRelay == "relay-a")
        #expect(decoded.relays.first?.observation == .inUse)
        #expect(decoded.controlPlane.directorySize == 1)
    }

    @Test func diagnosticExportContainsOnlyRedactedEndpoint() throws {
        let snapshot = DiagnosticsSnapshot(
            generatedAtUnixSeconds: 1,
            flows: [],
            relays: [RelayRouteSnapshot(
                relayID: "relay-a",
                redactedEndpoint: "•••:8443",
                currentMember: true,
                observation: .recentlyReachable,
                activeFlowCount: 0,
                lastSeenUnixSeconds: 1,
                lastSuccessUnixSeconds: 1,
                lastFailureUnixSeconds: nil,
                consecutiveFailures: 0
            )],
            controlPlane: .init(
                catalogGeneration: 1,
                relayDirectoryGeneration: 1,
                lastAcceptedUnixSeconds: 1,
                catalogSubscriptionRelay: "relay-a",
                connectedRelays: [],
                directorySize: 1,
                degradedReason: nil
            ),
            events: []
        )
        let data = try DiagnosticsExporter.data(snapshot: snapshot, appVersion: "0.3.1", now: Date(timeIntervalSince1970: 0))
        let text = String(decoding: data, as: UTF8.self)
        #expect(text.contains("flowsplice-route-diagnostics-v1"))
        #expect(text.contains("•••:8443"))
        #expect(!text.contains("relay.example.com"))
        #expect(!text.localizedCaseInsensitiveContains("password"))
        #expect(!text.localizedCaseInsensitiveContains("private_key"))
    }

    @Test func rebasesOnlyGeneratedManagedPaths() {
        let source = """
        travel_id = "mac"
        management_cert = "/tmp/old/cert/travel-management.crt"
        state_store = "/tmp/old/state/travel-state.redb"
        custom_path = "/leave/this/alone"
        """
        let root = URL(fileURLWithPath: "/Applications Support/FlowSplice")
        let result = TravelFiles.rebasedGeneratedConfig(source, installationDirectory: root)
        #expect(result.contains(#"management_cert = "/Applications Support/FlowSplice/cert/travel-management.crt""#))
        #expect(result.contains(#"state_store = "/Applications Support/FlowSplice/state/travel-state.redb""#))
        #expect(result.contains(#"custom_path = "/leave/this/alone""#))
    }

    @Test func statusMergePreservesTheNativeTruth() {
        let native = NativeTravelStatus(
            ok: true,
            online: true,
            travelID: "studio-mac",
            uptimeSeconds: 90,
            activeFlows: 2,
            catalogGeneration: 3,
            relayDirectoryGeneration: 4,
            activeRelays: ["relay-a"],
            uploadedBytes: 10,
            downloadedBytes: 20,
            mappings: [TravelMapping(homeID: "home-1", serviceID: "ssh", protocol: "tcp", bind: "127.0.0.1:10022")],
            privateKeyPasswordRotationAvailable: true
        )
        let snapshot = TravelSnapshot(native: native)
        #expect(snapshot.online)
        #expect(snapshot.activeFlows == 2)
        #expect(snapshot.activeRelays == ["relay-a"])
        #expect(snapshot.mappings.count == 1)
    }
}
