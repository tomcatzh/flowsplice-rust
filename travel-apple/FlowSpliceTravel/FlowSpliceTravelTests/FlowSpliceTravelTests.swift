import Foundation
import Testing
@testable import FlowSpliceTravel

struct FlowSpliceTravelTests {
    @Test("IDs are normalized for generated device identities")
    func normalizedIDs() {
        #expect(TravelValidation.normalizedID(" Example's iPad mini ") == "Example-s-iPad-mini")
        #expect(TravelValidation.normalizedID("___") == "")
        #expect(TravelValidation.normalizedID(String(repeating: "a", count: 140)).count == 128)
    }

    @Test("Relay addresses require an explicit valid port")
    func relayValidation() {
        #expect(TravelValidation.relayIsValid("relay.example:8443"))
        #expect(TravelValidation.relayIsValid("127.0.0.1:18446"))
        #expect(TravelValidation.relayIsValid("[::1]:18446"))
        #expect(!TravelValidation.relayIsValid("relay.example"))
        #expect(!TravelValidation.relayIsValid("relay.example:0"))
        #expect(!TravelValidation.relayIsValid("relay example:8443"))
    }

    @Test("Catalog hides empty Homes and sorts services consistently")
    func catalogProjection() {
        let catalog = TravelCatalog(
            generation: 7,
            homes: [
                CatalogHome(homeID: "empty", homeAlias: "", services: []),
                CatalogHome(
                    homeID: "home-b",
                    homeAlias: "Zulu",
                    services: [
                        CatalogService(id: "ssh", alias: "", protocol: "tcp", target: "127.0.0.1:22"),
                    ]
                ),
                CatalogHome(
                    homeID: "home-a",
                    homeAlias: "Alpha",
                    services: [
                        CatalogService(id: "z", alias: "Zulu", protocol: "tcp", target: "127.0.0.1:2"),
                        CatalogService(id: "a", alias: "Alpha", protocol: "udp", target: "127.0.0.1:1"),
                    ]
                ),
            ]
        )

        #expect(catalog.availableHomes.map(\.homeID) == ["home-a", "home-b"])
        #expect(catalog.availableHomes[0].services.map(\.id) == ["a", "z"])
    }

    @Test("Native JSON keys decode into Apple view models")
    func nativeStatusDecoding() throws {
        let json = #"{"ok":true,"online":true,"travel_id":"apple-e2e","uptime_secs":12,"active_flows":1,"catalog_generation":4,"relay_directory_generation":2,"active_relays":["relay-1"],"session_uploaded_bytes":8,"session_downloaded_bytes":9,"mappings":[{"home_id":"home-1","service_id":"tcp-echo","protocol":"tcp","bind":"127.0.0.1:10080"}],"private_key_password_rotation_available":true}"#
        let status = try JSONDecoder().decode(NativeTravelStatus.self, from: Data(json.utf8))
        #expect(status.travelID == "apple-e2e")
        #expect(status.mappings.first?.id == "home-1/tcp-echo/tcp")
        #expect(TravelSnapshot(native: status).online)
    }

    @Test("Live Activity content derives stable user-facing status")
    func liveActivityContentStatus() {
        let online = TravelActivityAttributes.ContentState(
            phase: "running",
            online: true,
            activeFlows: 2,
            uploadedBytes: 10,
            downloadedBytes: 20,
            relayCount: 1,
            mappingCount: 3,
            interfaceLabel: "Wi-Fi",
            updatedAt: .now
        )
        var reconnecting = online
        reconnecting.online = false
        var stopped = reconnecting
        stopped.phase = "stopped"

        #expect(online.statusLabel == "Online")
        #expect(reconnecting.statusLabel == "Reconnecting")
        #expect(stopped.statusLabel == "Stopped")
    }
}
