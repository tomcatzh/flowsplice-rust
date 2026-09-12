import Foundation
import Security
import Testing
@testable import FlowSpliceTravel

@Suite(.serialized)
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
        let backupValues = try directory.resourceValues(forKeys: [.isExcludedFromBackupKey])
        #expect(backupValues.isExcludedFromBackup == true)
    }

    @Test("An already protected installation survives a real directory relocation")
    func protectedInstallationRelocation() throws {
        let files = FileManager.default
        let fixture = files.temporaryDirectory.appending(path: "travel-relocation-\(UUID().uuidString)", directoryHint: .isDirectory)
        let old = fixture.appending(path: "old installation", directoryHint: .isDirectory)
        let current = fixture.appending(path: "new container with spaces", directoryHint: .isDirectory)
        defer { try? files.removeItem(at: fixture) }
        let paths = [
            ("deployment_root_public_key", "cert/deployment-root.pub"),
            ("deployment_trust", "cert/deployment-trust.json"),
            ("management_cert", "cert/travel-management.crt"),
            ("management_key", "cert/travel-management.key"),
            ("management_ca", "cert/management-ca.crt"),
            ("business_cert", "cert/travel-business.crt"),
            ("business_key", "cert/travel-business.key"),
            ("business_ca", "cert/business-ca.crt"),
            ("state_store", "state/travel-state.redb"),
            ("enrollment_work_dir", "state/enrollment"),
        ]
        var preserved: [String: Data] = [:]
        for (_, relative) in paths where relative != "state/enrollment" {
            let url = old.appending(path: relative)
            try files.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            let bytes = Data("immutable dummy fixture: \(relative)\n".utf8)
            try bytes.write(to: url)
            preserved[relative] = bytes
        }
        let enrollmentMarker = "state/enrollment/pending-marker"
        try files.createDirectory(at: old.appending(path: "state/enrollment"), withIntermediateDirectories: true)
        preserved[enrollmentMarker] = Data("pending-state-preserved\n".utf8)
        try preserved[enrollmentMarker]!.write(to: old.appending(path: enrollmentMarker))
        let unchanged = """
        ui_listen = "127.0.0.1:9080"

        [[homes]]
        id = "fixture-home"

        [[seed_relays]]
        management_addr = "127.0.0.1:18446"
        """
        let source = (["id = \"relocation-test\""] + paths.map {
            "\($0.0) = \"\(old.appending(path: $0.1).path)\""
        }).joined(separator: "\n") + "\n" + unchanged + "\n"
        try source.write(to: old.appending(path: "travelagent.toml"), atomically: true, encoding: .utf8)
        try TravelFiles.prepareRuntimeStorage(at: old)
        let marker = ".flowsplice-protection-v1"
        #expect(files.fileExists(atPath: old.appending(path: marker).path))
        preserved[marker] = try Data(contentsOf: old.appending(path: marker))

        try files.moveItem(at: old, to: current)
        #expect(!files.fileExists(atPath: old.path))
        #expect(files.fileExists(atPath: current.appending(path: marker).path))
        try TravelFiles.prepareRuntimeStorage(at: current)
        let config = current.appending(path: "travelagent.toml")
        let migrated = try String(contentsOf: config, encoding: .utf8)
        #expect(!migrated.contains(old.path))
        for (key, relative) in paths {
            #expect(migrated.contains("\(key) = \"\(current.appending(path: relative).path)\""))
        }
        let rootLine = try #require(migrated.split(separator: "\n").first { $0.hasPrefix("deployment_root_public_key = ") })
        let rootPath = String(rootLine.dropFirst("deployment_root_public_key = ".count).dropFirst().dropLast())
        #expect(try Data(contentsOf: URL(fileURLWithPath: rootPath)) == preserved["cert/deployment-root.pub"])
        #expect(migrated.contains("id = \"relocation-test\""))
        #expect(migrated.contains(unchanged))
        for (relative, bytes) in preserved {
            #expect(try Data(contentsOf: current.appending(path: relative)) == bytes)
        }
        let first = try Data(contentsOf: config)
        try TravelFiles.prepareRuntimeStorage(at: current)
        #expect(try Data(contentsOf: config) == first)
        let attributes = try files.attributesOfItem(atPath: config.path)
        #expect((attributes[.posixPermissions] as? NSNumber)?.intValue == 0o600)
    }

    @Test("Runtime storage excludes a newly created installation directory from backup")
    func runtimeStorageCreationExcludesBackup() throws {
        let fileManager = FileManager.default
        let directory = fileManager.temporaryDirectory
            .appending(path: "flowsplice-new-storage-\(UUID().uuidString)", directoryHint: .isDirectory)
        defer { try? fileManager.removeItem(at: directory) }

        try TravelFiles.prepareRuntimeStorage(at: directory)

        let backupValues = try directory.resourceValues(forKeys: [.isExcludedFromBackupKey])
        #expect(backupValues.isExcludedFromBackup == true)
    }

    @Test("Credential lookup only treats an absent Keychain item as missing")
    func credentialLookupClassification() {
        #expect(CredentialStore.classifyLookup(status: errSecItemNotFound, data: nil) == .missing)
        #expect(
            CredentialStore.classifyLookup(status: errSecInteractionNotAllowed, data: nil) ==
                .unavailable(errSecInteractionNotAllowed)
        )
        #expect(
            CredentialStore.classifyLookup(status: errSecSuccess, data: Data("test-password".utf8)) ==
                .available("test-password")
        )
    }

    @Test("Re-enrollment removes only this device installation and its credential")
    @MainActor
    func reenrollmentRemovesOnlyDeviceData() async throws {
        let fileManager = FileManager.default
        let directory = fileManager.temporaryDirectory
            .appending(path: "flowsplice-reenrollment-\(UUID().uuidString)", directoryHint: .isDirectory)
        let config = directory.appending(path: "travelagent.toml", directoryHint: .notDirectory)
        try TravelFiles.prepareInstallationDirectory(at: directory)
        try Data("test".utf8).write(to: config)
        defer { try? fileManager.removeItem(at: directory) }

        let oldPending = EnrollmentStore.pending
        let oldAutoStart = EnrollmentStore.autoStart
        let oldRelay = EnrollmentStore.lastRelay
        defer {
            EnrollmentStore.pending = oldPending
            EnrollmentStore.autoStart = oldAutoStart
            EnrollmentStore.lastRelay = oldRelay
        }
        EnrollmentStore.pending = PendingEnrollment(
            travelID: "travel-1",
            homeID: "home-1",
            relay: "relay.example:443"
        )
        EnrollmentStore.autoStart = true
        EnrollmentStore.lastRelay = "keep-this-setting"

        var credentialClearCalls = 0
        let files = TravelFileAccess(
            installationDirectory: { directory },
            config: { config },
            isInstalled: { fileManager.fileExists(atPath: config.path) },
            prepareInstallationDirectory: {},
            prepareRuntimeStorage: {},
            discardPendingInstallation: {},
            removeInstallationForReenrollment: {
                try TravelFiles.removeInstallationForReenrollment(at: directory)
            }
        )
        let native = TravelStoreTestNative(delaysCancelEnrollment: true)
        let store = TravelStore(
            native: native,
            backgroundAudio: TravelStoreTestAudio(mode: .immediate),
            liveActivity: TravelStoreTestLiveActivity(),
            files: files,
            credentialLookup: { .missing },
            credentialSave: { _ in },
            credentialClear: { credentialClearCalls += 1 }
        )

        #expect(store.needsReenrollmentOnThisDevice)
        store.reenrollOnThisDevice()

        #expect(await waitForCondition { native.cancelEnrollmentCalls == 1 })
        #expect(fileManager.fileExists(atPath: directory.path))
        #expect(credentialClearCalls == 0)
        native.completeCancelEnrollment()
        #expect(await waitForCondition {
            !fileManager.fileExists(atPath: directory.path) && credentialClearCalls == 1
        })
        #expect(EnrollmentStore.pending == nil)
        #expect(!EnrollmentStore.autoStart)
        #expect(EnrollmentStore.lastRelay == "keep-this-setting")
        #expect(!store.snapshot.enrolled)
    }

    @Test("Re-enrollment rechecks a stale missing Keychain result before removing data")
    @MainActor
    func reenrollmentRechecksCredentialBeforeDestructiveReset() async {
        EnrollmentStore.autoStart = false
        defer { EnrollmentStore.autoStart = false }
        let directory = URL(fileURLWithPath: "/tmp/flowsplice-stale-reenrollment", isDirectory: true)
        var credential = CredentialLookupResult.missing
        var removedInstallationCalls = 0
        var credentialClearCalls = 0
        let files = TravelFileAccess(
            installationDirectory: { directory },
            config: { directory.appending(path: "travelagent.toml") },
            isInstalled: { true },
            prepareInstallationDirectory: {},
            prepareRuntimeStorage: {},
            discardPendingInstallation: {},
            removeInstallationForReenrollment: { removedInstallationCalls += 1 }
        )
        let native = TravelStoreTestNative()
        let store = testStore(
            native: native,
            audio: TravelStoreTestAudio(mode: .immediate),
            files: files,
            credentialLookup: { credential },
            credentialClear: { credentialClearCalls += 1 }
        )

        #expect(store.needsReenrollmentOnThisDevice)
        credential = .unavailable(errSecInteractionNotAllowed)
        store.reenrollOnThisDevice()
        for _ in 0..<20 { await Task.yield() }

        #expect(store.credentialAvailability == .unavailable)
        #expect(native.cancelEnrollmentCalls == 0)
        #expect(removedInstallationCalls == 0)
        #expect(credentialClearCalls == 0)
    }

    @Test("Reconciliation applies installation backup protection without auto-starting")
    @MainActor
    func reconciliationProtectsInstalledDirectoryWithoutStartingRuntime() async {
        let oldAutoStart = EnrollmentStore.autoStart
        defer { EnrollmentStore.autoStart = oldAutoStart }
        EnrollmentStore.autoStart = false
        var preparedInstallationCalls = 0
        let directory = URL(fileURLWithPath: "/tmp/flowsplice-reconcile-storage", isDirectory: true)
        let files = TravelFileAccess(
            installationDirectory: { directory },
            config: { directory.appending(path: "travelagent.toml") },
            isInstalled: { true },
            prepareInstallationDirectory: { preparedInstallationCalls += 1 },
            prepareRuntimeStorage: {},
            discardPendingInstallation: {},
            removeInstallationForReenrollment: {}
        )
        let native = TravelStoreTestNative()
        let store = testStore(
            native: native,
            audio: TravelStoreTestAudio(mode: .immediate),
            files: files,
            credentialLookup: { .missing }
        )

        store.bootstrap()

        #expect(await waitForCondition { preparedInstallationCalls == 1 })
        #expect(native.startCalls == 0)
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

    @Test("Background audio starts and stops idempotently")
    @MainActor
    func backgroundAudioLifecycle() async throws {
        let controller = TravelBackgroundAudioController()

        try await controller.start()
        #expect(controller.status == .active)

        try await controller.start()
        #expect(controller.status == .active)

        try await controller.stop()
        #expect(controller.status == .inactive)

        try await controller.stop()
        #expect(controller.status == .inactive)
    }

    @Test("Native status merge preserves lifecycle and error ownership")
    func statusMergePreservesLifecycle() throws {
        let json = #"{"ok":true,"online":true,"travel_id":"apple-e2e","uptime_secs":12,"active_flows":1,"catalog_generation":4,"relay_directory_generation":2,"active_relays":["relay-1"],"session_uploaded_bytes":8,"session_downloaded_bytes":9,"mappings":[],"private_key_password_rotation_available":true}"#
        let status = try JSONDecoder().decode(NativeTravelStatus.self, from: Data(json.utf8))
        var snapshot = TravelSnapshot()
        snapshot.phase = .stopping
        snapshot.error = "Keep this error"

        snapshot.merge(native: status)

        #expect(snapshot.phase == .stopping)
        #expect(snapshot.error == "Keep this error")
        #expect(snapshot.online)
    }

    @Test("Audio recovery becomes low frequency without ending")
    func audioRecoveryBackoff() {
        #expect(TravelBackgroundAudioRecoveryPolicy.delay(attempt: 1) < 1)
        #expect(TravelBackgroundAudioRecoveryPolicy.delay(attempt: 8) >= 20)
        #expect(TravelBackgroundAudioRecoveryPolicy.delay(attempt: 100) <= 330)
    }

    @Test("Stop cancels a recovering audio start before it can restart Travel")
    @MainActor
    func stopCancelsRecoveringAudioStart() async {
        EnrollmentStore.autoStart = false
        defer { EnrollmentStore.autoStart = false }
        let native = TravelStoreTestNative()
        let audio = TravelStoreTestAudio(mode: .failsAndRecovers)
        let store = testStore(native: native, audio: audio)

        store.start()
        #expect(await waitForCondition { audio.startCalls == 1 && store.canStop })

        store.stop()
        #expect(await waitForCondition { audio.stopCalls == 1 && !store.canStop })
        #expect(native.startCalls == 0)
        #expect(!EnrollmentStore.autoStart)

        audio.recover()
        for _ in 0..<20 { await Task.yield() }
        #expect(native.startCalls == 0)
        #expect(!EnrollmentStore.autoStart)
    }

    @Test("Stop wins while background audio startup is delayed")
    @MainActor
    func stopWinsDuringDelayedAudioStart() async {
        EnrollmentStore.autoStart = false
        defer { EnrollmentStore.autoStart = false }
        let native = TravelStoreTestNative()
        let audio = TravelStoreTestAudio(mode: .delayed)
        let store = testStore(native: native, audio: audio)

        store.start()
        #expect(await waitForCondition { audio.startCalls == 1 && store.canStop })

        store.stop()
        #expect(store.canStop)
        audio.completeStart()

        #expect(await waitForCondition { audio.stopCalls == 1 && !store.canStop })
        #expect(native.startCalls == 0)
        #expect(!EnrollmentStore.autoStart)
    }

    @Test("Stop wins when native startup completes after the user stops")
    @MainActor
    func stopWinsAfterDelayedNativeStart() async {
        EnrollmentStore.autoStart = false
        defer { EnrollmentStore.autoStart = false }
        let native = TravelStoreTestNative(delaysStart: true)
        let audio = TravelStoreTestAudio(mode: .immediate)
        let store = testStore(native: native, audio: audio)

        store.start()
        #expect(await waitForCondition { native.startCalls == 1 && store.canStop })

        store.stop()
        #expect(!EnrollmentStore.autoStart)
        native.completeStart()

        #expect(await waitForCondition { native.stopCalls == 1 && !store.canStop })
        #expect(native.startCalls == 1)
        #expect(!EnrollmentStore.autoStart)
    }

    @Test("Failed stop operations wait for a new lifecycle request before retrying")
    @MainActor
    func failedStopDoesNotSpinLifecycleDrain() async {
        EnrollmentStore.autoStart = false
        defer { EnrollmentStore.autoStart = false }
        let native = TravelStoreTestNative(failsStop: true)
        let audio = TravelStoreTestAudio(mode: .immediate, failsStop: true)
        let store = testStore(native: native, audio: audio)

        store.start()
        #expect(await waitForCondition { native.startCalls == 1 })

        store.stop()
        #expect(await waitForCondition { native.stopCalls == 1 && audio.stopCalls == 1 })
        for _ in 0..<50 { await Task.yield() }

        #expect(native.stopCalls == 1)
        #expect(audio.stopCalls == 1)
        #expect(store.canStop)
    }

    @Test("Temporary Keychain unavailability does not immediately retry an active-audio start")
    @MainActor
    func temporaryKeychainFailureDoesNotSpinStart() async {
        EnrollmentStore.autoStart = false
        defer { EnrollmentStore.autoStart = false }
        var lookupCalls = 0
        let native = TravelStoreTestNative()
        let audio = TravelStoreTestAudio(mode: .immediate, initialStatus: .active)
        let store = testStore(
            native: native,
            audio: audio,
            credentialLookup: {
                lookupCalls += 1
                return .unavailable(errSecInteractionNotAllowed)
            }
        )
        let initialLookupCalls = lookupCalls

        store.start()
        #expect(await waitForCondition { store.presentedError != nil })
        for _ in 0..<50 { await Task.yield() }

        #expect(lookupCalls == initialLookupCalls + 1)
        #expect(native.startCalls == 0)
        #expect(audio.startCalls == 0)
        #expect(store.canStop)
    }

    @Test("A Start arriving during an awaited Stop runs after cleanup")
    @MainActor
    func startDuringAwaitedStopRunsAfterCleanup() async {
        EnrollmentStore.autoStart = false
        defer { EnrollmentStore.autoStart = false }
        let native = TravelStoreTestNative(delaysStop: true)
        let audio = TravelStoreTestAudio(mode: .immediate)
        let store = testStore(native: native, audio: audio)

        store.start()
        #expect(await waitForCondition { native.startCalls == 1 && store.snapshot.phase == .running })

        store.stop()
        #expect(await waitForCondition { native.stopCalls == 1 })
        store.start()
        native.completeStop()

        #expect(await waitForCondition { native.startCalls == 2 && store.snapshot.phase == .running })
        #expect(EnrollmentStore.autoStart)
    }

    @Test("Live Activity is presentation for a running runtime only")
    func liveActivityVisibilityPolicy() {
        var snapshot = TravelSnapshot()
        snapshot.phase = .starting
        #expect(!TravelLiveActivityPresentation(snapshot: snapshot, interfaceLabel: "Wi‑Fi").shouldBeVisible)

        snapshot.phase = .running
        #expect(TravelLiveActivityPresentation(snapshot: snapshot, interfaceLabel: "Wi‑Fi").shouldBeVisible)

        snapshot.phase = .stopping
        #expect(!TravelLiveActivityPresentation(snapshot: snapshot, interfaceLabel: "Wi‑Fi").shouldBeVisible)
    }

    @Test("Live Activity updates state immediately but rate-limits traffic-only changes")
    func liveActivityUpdatePolicy() {
        var snapshot = TravelSnapshot()
        snapshot.phase = .running
        snapshot.online = true
        let previous = TravelLiveActivityPresentation(snapshot: snapshot, interfaceLabel: "Wi‑Fi")
        let lastUpdated = Date(timeIntervalSince1970: 1_000)

        snapshot.downloadedBytes = 4_096
        let trafficOnly = TravelLiveActivityPresentation(snapshot: snapshot, interfaceLabel: "Wi‑Fi")
        #expect(!trafficOnly.requiresUpdate(
            comparedWith: previous,
            lastUpdated: lastUpdated,
            now: lastUpdated.addingTimeInterval(1),
            telemetryInterval: 60
        ))
        #expect(trafficOnly.requiresUpdate(
            comparedWith: previous,
            lastUpdated: lastUpdated,
            now: lastUpdated.addingTimeInterval(60),
            telemetryInterval: 60
        ))

        snapshot.online = false
        let connectivityChange = TravelLiveActivityPresentation(snapshot: snapshot, interfaceLabel: "Cellular")
        #expect(connectivityChange.requiresUpdate(
            comparedWith: trafficOnly,
            lastUpdated: lastUpdated,
            now: lastUpdated.addingTimeInterval(1),
            telemetryInterval: 60
        ))
    }

    @Test("Disabled Live Activities stay optional for running Travel state")
    @MainActor
    func disabledLiveActivityIsOptional() async {
        let controller = TravelLiveActivityController(forceDisabled: true)
        var snapshot = TravelSnapshot()
        snapshot.phase = .running
        snapshot.online = true

        await controller.synchronize(snapshot: snapshot, interfaceLabel: "Wi‑Fi")

        #expect(controller.status == .disabled)
        #expect(controller.status.detail.contains("continues independently"))
    }

    @MainActor
    private func testStore(
        native: TravelStoreTestNative,
        audio: TravelStoreTestAudio,
        files: TravelFileAccess? = nil,
        credentialLookup: @escaping () -> CredentialLookupResult = { .available("test-password") },
        credentialClear: @escaping () -> Void = {}
    ) -> TravelStore {
        let directory = URL(fileURLWithPath: "/tmp/flowsplice-travel-store-tests", isDirectory: true)
        let defaultFiles = TravelFileAccess(
            installationDirectory: { directory },
            config: { directory.appending(path: "travelagent.toml") },
            isInstalled: { true },
            prepareInstallationDirectory: {},
            prepareRuntimeStorage: {},
            discardPendingInstallation: {},
            removeInstallationForReenrollment: {}
        )
        return TravelStore(
            native: native,
            backgroundAudio: audio,
            liveActivity: TravelStoreTestLiveActivity(),
            files: files ?? defaultFiles,
            credentialLookup: credentialLookup,
            credentialSave: { _ in },
            credentialClear: credentialClear
        )
    }

    @MainActor
    private func waitForCondition(_ condition: @escaping () -> Bool) async -> Bool {
        for _ in 0..<200 {
            if condition() { return true }
            await Task.yield()
        }
        return condition()
    }
}

@MainActor
private final class TravelStoreTestAudio: TravelBackgroundAudioControlling {
    enum Mode {
        case immediate
        case delayed
        case failsAndRecovers
    }

    private let mode: Mode
    private let failsStop: Bool
    private var startContinuation: CheckedContinuation<Void, Error>?
    private(set) var startCalls = 0
    private(set) var stopCalls = 0
    private(set) var status: TravelBackgroundAudioStatus = .inactive {
        didSet { onStatusChange?(status) }
    }
    var onStatusChange: ((TravelBackgroundAudioStatus) -> Void)?

    init(
        mode: Mode,
        initialStatus: TravelBackgroundAudioStatus = .inactive,
        failsStop: Bool = false
    ) {
        self.mode = mode
        self.failsStop = failsStop
        status = initialStatus
    }

    func start() async throws {
        startCalls += 1
        status = .starting
        switch mode {
        case .immediate:
            status = .active
        case .delayed:
            try await withCheckedThrowingContinuation { continuation in
                startContinuation = continuation
            }
            status = .active
        case .failsAndRecovers:
            status = .recovering
            throw TravelStoreTestError.audioUnavailable
        }
    }

    func stop() async throws {
        stopCalls += 1
        if failsStop { throw TravelStoreTestError.audioStopUnavailable }
        status = .inactive
    }

    func reconcile() async {}

    func completeStart() {
        startContinuation?.resume()
        startContinuation = nil
    }

    func recover() {
        status = .active
    }
}

@MainActor
private final class TravelStoreTestNative: TravelRuntimeControlling {
    private let delaysStart: Bool
    private let delaysStop: Bool
    private let delaysCancelEnrollment: Bool
    private let failsStop: Bool
    private var startContinuation: CheckedContinuation<NativeTravelStatus, Error>?
    private var stopContinuation: CheckedContinuation<Void, Error>?
    private var cancelEnrollmentContinuation: CheckedContinuation<Void, Error>?
    private(set) var startCalls = 0
    private(set) var stopCalls = 0
    private(set) var cancelEnrollmentCalls = 0

    init(
        delaysStart: Bool = false,
        delaysStop: Bool = false,
        delaysCancelEnrollment: Bool = false,
        failsStop: Bool = false
    ) {
        self.delaysStart = delaysStart
        self.delaysStop = delaysStop
        self.delaysCancelEnrollment = delaysCancelEnrollment
        self.failsStop = failsStop
    }

    func beginEnrollment(
        installDirectory: URL,
        travelID: String,
        homeID: String,
        relay: String,
        password: String
    ) async throws -> EnrollmentSnapshot {
        EnrollmentSnapshot()
    }

    func enrollmentStatus() async throws -> EnrollmentSnapshot {
        EnrollmentSnapshot()
    }

    func cancelEnrollment() async throws {
        cancelEnrollmentCalls += 1
        if delaysCancelEnrollment {
            try await withCheckedThrowingContinuation { continuation in
                cancelEnrollmentContinuation = continuation
            }
        }
    }

    func start(config: URL, password: String) async throws -> NativeTravelStatus {
        startCalls += 1
        if delaysStart {
            return try await withCheckedThrowingContinuation { continuation in
                startContinuation = continuation
            }
        }
        return runningStatus
    }

    func stop() async throws {
        stopCalls += 1
        if failsStop { throw TravelStoreTestError.nativeStopUnavailable }
        if delaysStop {
            try await withCheckedThrowingContinuation { continuation in
                stopContinuation = continuation
            }
        }
    }

    func notifyNetworkChanged() async throws {}

    func status() async throws -> NativeTravelStatus {
        runningStatus
    }

    func waitForStatusChange(
        generation: UInt64,
        timeoutMillis: UInt64
    ) async throws -> NativeTravelStatusUpdate {
        try await Task.sleep(for: .milliseconds(1))
        return NativeTravelStatusUpdate(generation: generation + 1, status: runningStatus)
    }

    func wakeStatusWaiters() async {}

    func catalog() async throws -> TravelCatalog {
        TravelCatalog()
    }

    func upsert(mapping: TravelMapping) async throws -> TravelMapping {
        mapping
    }

    func delete(mapping: TravelMapping) async throws -> TravelMapping {
        mapping
    }

    func completeStart() {
        startContinuation?.resume(returning: runningStatus)
        startContinuation = nil
    }

    func completeStop() {
        stopContinuation?.resume()
        stopContinuation = nil
    }

    func completeCancelEnrollment() {
        cancelEnrollmentContinuation?.resume()
        cancelEnrollmentContinuation = nil
    }

    private var runningStatus: NativeTravelStatus {
        NativeTravelStatus(
            ok: true,
            online: true,
            travelID: "test-travel",
            uptimeSeconds: 1,
            activeFlows: 0,
            catalogGeneration: 1,
            relayDirectoryGeneration: 1,
            activeRelays: ["relay-1"],
            uploadedBytes: 0,
            downloadedBytes: 0,
            mappings: [],
            privateKeyPasswordRotationAvailable: false
        )
    }
}

@MainActor
private final class TravelStoreTestLiveActivity: TravelLiveActivityControlling {
    var status: TravelLiveActivityStatus = .inactive
    var onStatusChange: ((TravelLiveActivityStatus) -> Void)?

    func synchronize(snapshot: TravelSnapshot, interfaceLabel: String) async {}
}

private enum TravelStoreTestError: LocalizedError {
    case audioUnavailable
    case audioStopUnavailable
    case nativeStopUnavailable

    var errorDescription: String? {
        switch self {
        case .audioUnavailable:
            "Audio unavailable for test"
        case .audioStopUnavailable:
            "Audio stop unavailable for test"
        case .nativeStopUnavailable:
            "Native stop unavailable for test"
        }
    }
}
