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

    @Test("Generated config paths follow the current iOS data container")
    func generatedConfigPathRebasing() {
        let oldRoot = "/private/var/mobile/Containers/Data/Application/OLD/Library/Application Support/FlowSpliceTravel"
        let currentRoot = URL(
            fileURLWithPath: "/private/var/mobile/Containers/Data/Application/CURRENT/Library/Application Support/FlowSpliceTravel",
            isDirectory: true
        )
        let managedPaths = [
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
        let source = (["id = \"example-ipad\""] + managedPaths.map {
            "\($0.key) = \"\(oldRoot)/\($0.value)\""
        } + ["ui_listen = \"127.0.0.1:9080\""]).joined(separator: "\n")

        let migrated = TravelFiles.rebasedGeneratedConfig(
            source,
            installationDirectory: currentRoot
        )

        #expect(!migrated.contains(oldRoot))
        for relativePath in managedPaths.values {
            #expect(migrated.contains("\(currentRoot.path)/\(relativePath)"))
        }
        #expect(migrated.contains("ui_listen = \"127.0.0.1:9080\""))
    }

    @Test("Runtime storage migration rewrites config and prepares the state directory")
    func runtimeStorageMigration() throws {
        let fileManager = FileManager.default
        let directory = fileManager.temporaryDirectory
            .appending(path: "flowsplice-storage-\(UUID().uuidString)", directoryHint: .isDirectory)
        defer { try? fileManager.removeItem(at: directory) }
        try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
        let config = directory.appending(path: "travelagent.toml")
        try """
        state_store = "/private/var/mobile/Containers/Data/Application/OLD/Library/Application Support/FlowSpliceTravel/state/travel-state.redb"
        ui_listen = "127.0.0.1:9080"
        """.write(to: config, atomically: true, encoding: .utf8)

        try TravelFiles.prepareRuntimeStorage(at: directory)

        let migrated = try String(contentsOf: config, encoding: .utf8)
        #expect(migrated.contains("state_store = \"\(directory.path)/state/travel-state.redb\""))
        #expect(fileManager.fileExists(atPath: directory.appending(path: "state").path))
        let configAttributes = try fileManager.attributesOfItem(atPath: config.path)
        #expect((configAttributes[.posixPermissions] as? NSNumber)?.intValue == 0o600)
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

    @Test("A stale continued-session expiration cannot end a replacement session")
    func staleContinuedSessionExpirationIsIgnored() {
        var state = TravelContinuedSessionState()
        let first = state.beginRequest()
        let acceptedFirst = state.accept()
        #expect(acceptedFirst == first)
        let resetFirst = state.reset(ifCurrent: first)
        #expect(resetFirst)

        let replacement = state.beginRequest()
        #expect(replacement != first)
        let staleReset = state.reset(ifCurrent: first)
        #expect(!staleReset)
        #expect(state.isActiveOrRequested)
        let acceptedReplacement = state.accept()
        #expect(acceptedReplacement == replacement)
        #expect(state.isActive)
    }
}
