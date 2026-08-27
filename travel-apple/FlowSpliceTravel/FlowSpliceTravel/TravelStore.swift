import Combine
import Foundation
import Network
import OSLog
import SwiftUI
import UIKit

@MainActor
final class TravelStore: ObservableObject {
    enum Section: String, CaseIterable, Identifiable {
        case overview
        case mappings
        case diagnostics
        case device

        var id: Self { self }

        var title: String {
            switch self {
            case .overview: "Overview"
            case .mappings: "Local Mappings"
            case .diagnostics: "Diagnostics"
            case .device: "Device"
            }
        }

        var symbol: String {
            switch self {
            case .overview: "circle.grid.2x2.fill"
            case .mappings: "arrow.left.arrow.right"
            case .diagnostics: "waveform.path.ecg"
            case .device: "ipad.and.iphone"
            }
        }
    }

    @Published var snapshot = TravelSnapshot()
    @Published var enrollment = EnrollmentSnapshot()
    @Published var catalog = TravelCatalog()
    @Published var recoveryEvents: [RecoveryEvent] = []
    @Published var selectedSection: Section? = .overview
    @Published var isWorking = false
    @Published var presentedError: String?
    @Published private(set) var networkAvailable = true
    @Published private(set) var interfaceLabel = "Checking…"
    @Published private(set) var backgroundAudioStatus: TravelBackgroundAudioStatus = .inactive
    @Published private(set) var credentialAvailable = false

    let defaultTravelID: String
    let defaultHomeID = "home-1"

    private enum StartOutcome {
        case started
        case waitingForAudio
        case failed
    }

    private let native: NativeTravelClient
    private let backgroundAudio: TravelBackgroundAudioControlling
    private let networkMonitor = NWPathMonitor()
    private let networkQueue = DispatchQueue(label: "io.zxf.flowsplice.travel.network")
    private let logger = Logger(subsystem: "io.zxf.flowsplice.travel", category: "lifecycle")
    private var lastNetworkSignature: String?
    private var enrollmentTask: Task<Void, Never>?
    private var statusTask: Task<Void, Never>?
    private var lifecycleTask: Task<Void, Never>?
    private var bootstrapped = false
    private var runtimeStartedInProcess = false
    private var desiredRunning = false
    private var currentScenePhase: ScenePhase = .active
    private var statusGeneration: UInt64 = 0
    private var pendingUserInitiatedStart = false

    init(
        native: NativeTravelClient = NativeTravelClient(),
        backgroundAudio: TravelBackgroundAudioControlling? = nil
    ) {
        self.native = native
        let backgroundAudio = backgroundAudio ?? TravelBackgroundAudioController()
        self.backgroundAudio = backgroundAudio
        TravelFiles.resetForUITesting()
        let environment = ProcessInfo.processInfo.environment
        let normalizedName = TravelValidation.normalizedID(UIDevice.current.name)
        let e2eTravelID = environment["FLOWSPLICE_E2E"] == "1"
            ? environment["FLOWSPLICE_E2E_TRAVEL_ID"].map(TravelValidation.normalizedID)
            : nil
        if let e2eTravelID, !e2eTravelID.isEmpty {
            defaultTravelID = e2eTravelID
        } else {
            defaultTravelID = normalizedName.isEmpty ? "apple-travel" : normalizedName
        }
        snapshot.enrolled = TravelFiles.isInstalled
        desiredRunning = EnrollmentStore.autoStart
        credentialAvailable = CredentialStore.load() != nil
        if environment["FLOWSPLICE_E2E_RELAY"] != nil {
            EnrollmentStore.lastRelay = environment["FLOWSPLICE_E2E_RELAY"] ?? ""
        }
        backgroundAudioStatus = backgroundAudio.status
        backgroundAudio.onStatusChange = { [weak self] status in
            guard let self else { return }
            backgroundAudioStatus = status
            if status == .active {
                TravelFiles.writeE2EPhase("background-audio-active")
                if desiredRunning, !runtimeStartedInProcess {
                    requestLifecycleDrain(reason: "Background audio recovered")
                }
            }
            if case .failed(let message) = status {
                appendEvent(.error, title: "Background audio unavailable", detail: message)
            }
        }
    }

    deinit {
        enrollmentTask?.cancel()
        statusTask?.cancel()
        lifecycleTask?.cancel()
        networkMonitor.cancel()
    }

    func bootstrap() {
        guard !bootstrapped else { return }
        bootstrapped = true
        startNetworkMonitor()
        Task { await reconcile(reason: "App launched") }
    }

    func handleScenePhase(_ phase: ScenePhase) {
        currentScenePhase = phase
        Task {
            await native.wakeStatusWaiters()
            if phase == .active {
                await backgroundAudio.reconcile()
                await reconcile(reason: "Returned to foreground")
            }
        }
    }

    func enroll(
        travelID: String,
        homeID: String,
        relay: String,
        password: String,
        confirmation: String
    ) {
        let travelID = TravelValidation.normalizedID(travelID)
        let homeID = TravelValidation.normalizedID(homeID)
        guard !travelID.isEmpty, !homeID.isEmpty else {
            presentedError = "Travel ID and Home ID are required."
            return
        }
        guard TravelValidation.relayIsValid(relay) else {
            presentedError = TravelError.invalidRelay.localizedDescription
            return
        }
        guard password.count >= 12 else {
            presentedError = "Use a private-key password with at least 12 characters."
            return
        }
        guard password == confirmation else {
            presentedError = "The password confirmation does not match."
            return
        }

        isWorking = true
        Task {
            do {
                try TravelFiles.prepareInstallationDirectory()
                try CredentialStore.save(password: password)
                credentialAvailable = true
                EnrollmentStore.lastRelay = relay
                EnrollmentStore.pending = PendingEnrollment(
                    travelID: travelID,
                    homeID: homeID,
                    relay: relay.trimmingCharacters(in: .whitespacesAndNewlines)
                )
                enrollment = try await native.beginEnrollment(
                    installDirectory: TravelFiles.installationDirectory,
                    travelID: travelID,
                    homeID: homeID,
                    relay: relay,
                    password: password
                )
                appendEvent(.lifecycle, title: "Enrollment started", detail: "Waiting for \(homeID) approval.")
                beginEnrollmentPolling()
            } catch {
                fail(error)
            }
            isWorking = false
        }
    }

    func cancelEnrollment() {
        isWorking = true
        enrollmentTask?.cancel()
        Task {
            do {
                try await native.cancelEnrollment()
                EnrollmentStore.pending = nil
                CredentialStore.clear()
                credentialAvailable = false
                try TravelFiles.discardPendingInstallation()
                enrollment = EnrollmentSnapshot()
                publish(TravelSnapshot())
                appendEvent(.lifecycle, title: "Enrollment cancelled", detail: "Pending credentials were removed.")
            } catch {
                fail(error)
            }
            isWorking = false
        }
    }

    func start() {
        desiredRunning = true
        pendingUserInitiatedStart = true
        EnrollmentStore.autoStart = true
        var next = snapshot
        next.phase = .starting
        next.error = nil
        publish(next)
        logger.notice("Travel start requested")
        requestLifecycleDrain(reason: "User Start")
    }

    func stop() {
        desiredRunning = false
        pendingUserInitiatedStart = false
        EnrollmentStore.autoStart = false
        statusTask?.cancel()
        var next = snapshot
        if runtimeStartedInProcess || backgroundAudioStatus != .inactive {
            next.phase = .stopping
        } else {
            next.phase = .stopped
        }
        next.error = nil
        publish(next)
        logger.notice("Travel stop requested")
        Task { await native.wakeStatusWaiters() }
        requestLifecycleDrain(reason: "User Stop")
    }

    func refreshCatalog() {
        Task {
            do {
                let next = try await native.catalog()
                if next != catalog { catalog = next }
                appendEvent(.catalog, title: "Catalog refreshed", detail: "Generation \(next.generation), \(next.availableHomes.count) Home(s).")
            } catch {
                fail(error)
            }
        }
    }

    func addMapping(home: CatalogHome, service: CatalogService, port: Int) async -> Bool {
        guard (1...65_535).contains(port) else {
            presentedError = TravelError.invalidPort.localizedDescription
            return false
        }
        do {
            let mapping = TravelMapping(
                homeID: home.id,
                serviceID: service.id,
                protocol: service.protocol,
                bind: "127.0.0.1:\(port)"
            )
            _ = try await native.upsert(mapping: mapping)
            await refreshStatusAndCatalog(forceCatalog: false)
            appendEvent(.catalog, title: "Mapping activated", detail: "\(service.displayName) → \(mapping.bind)")
            return true
        } catch {
            fail(error)
            return false
        }
    }

    func deleteMapping(_ mapping: TravelMapping) {
        Task {
            do {
                _ = try await native.delete(mapping: mapping)
                await refreshStatusAndCatalog(forceCatalog: false)
                appendEvent(.catalog, title: "Mapping removed", detail: "\(mapping.serviceID) no longer listens on \(mapping.bind).")
            } catch {
                fail(error)
            }
        }
    }

    func simulateNetworkChangeForTesting() {
        guard ProcessInfo.processInfo.environment["FLOWSPLICE_E2E"] == "1" else { return }
        Task {
            do {
                try await native.notifyNetworkChanged()
                appendEvent(.network, title: "Network change injected", detail: "Simulator E2E requested immediate connection retirement.")
            } catch {
                fail(error)
            }
        }
    }

    func prepareE2EPhase(_ phase: String) {
        TravelFiles.writeE2EPhase(phase)
    }

    private func reconcile(reason: String) async {
        var next = snapshot
        next.enrolled = TravelFiles.isInstalled
        publish(next)
        if let pending = EnrollmentStore.pending, !TravelFiles.isInstalled {
            guard let password = CredentialStore.load() else {
                fail(TravelError.missingCredential)
                return
            }
            do {
                try TravelFiles.prepareInstallationDirectory()
                enrollment = try await native.beginEnrollment(
                    installDirectory: TravelFiles.installationDirectory,
                    travelID: pending.travelID,
                    homeID: pending.homeID,
                    relay: pending.relay,
                    password: password
                )
                beginEnrollmentPolling()
            } catch {
                fail(error)
            }
            return
        }

        guard TravelFiles.isInstalled else { return }
        desiredRunning = EnrollmentStore.autoStart
        if runtimeStartedInProcess {
            await refreshStatusAndCatalog(forceCatalog: true)
            beginStatusObservation()
        }
        requestLifecycleDrain(reason: reason)
        appendEvent(.lifecycle, title: "State reconciled", detail: reason)
    }

    private func requestLifecycleDrain(reason: String) {
        guard lifecycleTask == nil else { return }
        lifecycleTask = Task { [weak self] in
            guard let self else { return }
            await drainLifecycle(reason: reason)
        }
    }

    private func drainLifecycle(reason: String) async {
        isWorking = true
        defer {
            isWorking = false
            lifecycleTask = nil
            let mismatch = desiredRunning != runtimeStartedInProcess
            if mismatch, !desiredRunning || backgroundAudioStatus == .active {
                requestLifecycleDrain(reason: "Pending desired state")
            }
        }

        while !Task.isCancelled {
            if desiredRunning {
                if runtimeStartedInProcess {
                    await backgroundAudio.reconcile()
                    beginStatusObservation()
                    return
                }
                switch await startRuntime(userInitiated: pendingUserInitiatedStart) {
                case .started:
                    pendingUserInitiatedStart = false
                    if desiredRunning { return }
                case .waitingForAudio, .failed:
                    return
                }
            } else {
                if runtimeStartedInProcess || backgroundAudioStatus != .inactive {
                    await stopRuntime(reason: reason)
                } else {
                    var next = snapshot
                    next.phase = .stopped
                    next.online = false
                    next.error = nil
                    publish(next)
                }
                return
            }
        }
    }

    private func startRuntime(userInitiated: Bool) async -> StartOutcome {
        guard TravelFiles.isInstalled else {
            presentedError = "Complete remote enrollment before starting Travel."
            desiredRunning = false
            EnrollmentStore.autoStart = false
            return .failed
        }
        guard let password = CredentialStore.load() else {
            fail(TravelError.missingCredential)
            desiredRunning = false
            EnrollmentStore.autoStart = false
            return .failed
        }
        var next = snapshot
        next.phase = .starting
        next.enrolled = true
        next.error = nil
        publish(next)
        let startedAt = Date()

        do {
            try await backgroundAudio.start()
        } catch {
            failRuntime(error, operation: "start background keeper")
            return desiredRunning ? .waitingForAudio : .failed
        }
        guard desiredRunning else {
            try? await backgroundAudio.stop()
            return .failed
        }

        do {
            try TravelFiles.prepareRuntimeStorage()
            let status = try await native.start(config: TravelFiles.config, password: password)
            runtimeStartedInProcess = true
            statusGeneration = 0
            apply(status, phase: .running, clearError: true)
            EnrollmentStore.autoStart = true
            await refreshStatusAndCatalog(forceCatalog: true)
            beginStatusObservation()
            let elapsed = Date().timeIntervalSince(startedAt)
            logger.notice("Travel start completed in \(elapsed, format: .fixed(precision: 2)) seconds")
            if userInitiated {
                appendEvent(.lifecycle, title: "Travel started", detail: "Runtime and local mappings are active.")
            }
            return .started
        } catch {
            runtimeStartedInProcess = false
            desiredRunning = false
            EnrollmentStore.autoStart = false
            try? await native.stop()
            try? await backgroundAudio.stop()
            failRuntime(error, operation: "start")
            return .failed
        }
    }

    private func stopRuntime(reason: String) async {
        statusTask?.cancel()
        await native.wakeStatusWaiters()
        var audioError: Error?
        do {
            try await backgroundAudio.stop()
        } catch {
            audioError = error
        }
        do {
            try await native.stop()
            runtimeStartedInProcess = false
            statusGeneration = 0
            var next = snapshot
            next.phase = .stopped
            next.online = false
            next.activeFlows = 0
            next.relayCount = 0
            next.error = nil
            publish(next)
            appendEvent(.lifecycle, title: "Travel stopped", detail: "Mappings remain stored for the next start.")
            logger.notice("Travel stop completed reason=\(reason, privacy: .public)")
            if let audioError {
                appendEvent(
                    .error,
                    title: "Audio session cleanup incomplete",
                    detail: audioError.localizedDescription
                )
            }
        } catch {
            failRuntime(error, operation: "stop")
        }
    }

    private func beginEnrollmentPolling() {
        enrollmentTask?.cancel()
        enrollmentTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                do {
                    let next = try await native.enrollmentStatus()
                    if next != enrollment { enrollment = next }
                    if let code = next.verificationCode {
                        TravelFiles.writeE2EVerificationCode(code)
                    }
                    switch next.phase {
                    case .installed:
                        EnrollmentStore.pending = nil
                        EnrollmentStore.autoStart = true
                        desiredRunning = true
                        var installed = snapshot
                        installed.enrolled = true
                        installed.phase = .starting
                        publish(installed)
                        appendEvent(.lifecycle, title: "Enrollment installed", detail: "Home approval and credential verification completed.")
                        requestLifecycleDrain(reason: "Enrollment installed")
                        return
                    case .error:
                        if let error = next.error { fail(TravelError.native(error)) }
                        return
                    case .cancelled:
                        return
                    default:
                        break
                    }
                } catch {
                    fail(error)
                    return
                }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    private func beginStatusObservation() {
        guard runtimeStartedInProcess, desiredRunning, statusTask == nil else { return }
        statusTask = Task { [weak self] in
            guard let self else { return }
            defer { statusTask = nil }
            while !Task.isCancelled, runtimeStartedInProcess, desiredRunning {
                do {
                    let update = try await native.waitForStatusChange(
                        generation: statusGeneration,
                        timeoutMillis: statusDeadlineMillis
                    )
                    guard !Task.isCancelled, runtimeStartedInProcess, desiredRunning else { return }
                    statusGeneration = update.generation
                    let previousCatalogGeneration = snapshot.catalogGeneration
                    apply(update.status)
                    if update.status.catalogGeneration != previousCatalogGeneration || catalog.generation == 0 {
                        let nextCatalog = try await native.catalog()
                        if nextCatalog != catalog {
                            catalog = nextCatalog
                            appendEvent(.catalog, title: "Service catalog updated", detail: "Accepted generation \(nextCatalog.generation).")
                        }
                    }
                } catch {
                    guard !Task.isCancelled, runtimeStartedInProcess, desiredRunning else { return }
                    var next = snapshot
                    next.online = false
                    next.error = error.localizedDescription
                    publish(next)
                    return
                }
            }
        }
    }

    private var statusDeadlineMillis: UInt64 {
        switch currentScenePhase {
        case .active:
            1_000
        case .inactive:
            5_000
        case .background:
            snapshot.activeFlows > 0 ? 5_000 : 30_000
        @unknown default:
            30_000
        }
    }

    private func refreshStatusAndCatalog(forceCatalog: Bool) async {
        do {
            let previousGeneration = snapshot.catalogGeneration
            let status = try await native.status()
            apply(status)
            if forceCatalog || status.catalogGeneration != previousGeneration || catalog.generation == 0 {
                let next = try await native.catalog()
                let changed = next.generation != catalog.generation
                if next != catalog { catalog = next }
                if changed {
                    appendEvent(.catalog, title: "Service catalog updated", detail: "Accepted generation \(next.generation).")
                }
            }
        } catch {
            guard runtimeStartedInProcess else { return }
            var next = snapshot
            next.online = false
            next.error = error.localizedDescription
            publish(next)
        }
    }

    private func apply(
        _ status: NativeTravelStatus,
        phase: TravelPhase? = nil,
        clearError: Bool = false
    ) {
        var next = snapshot
        next.merge(native: status, phase: phase, clearError: clearError)
        publish(next)
    }

    private func publish(_ next: TravelSnapshot) {
        guard next != snapshot else { return }
        snapshot = next
    }

    private func startNetworkMonitor() {
        networkMonitor.pathUpdateHandler = { [weak self] path in
            let signature = "\(path.status)-\(path.usesInterfaceType(.wifi))-\(path.usesInterfaceType(.cellular))-\(path.usesInterfaceType(.wiredEthernet))"
            let label: String
            if path.status != .satisfied {
                label = "No network"
            } else if path.usesInterfaceType(.wifi) {
                label = "Wi‑Fi"
            } else if path.usesInterfaceType(.cellular) {
                label = "Cellular"
            } else if path.usesInterfaceType(.wiredEthernet) {
                label = "Ethernet"
            } else {
                label = "Available"
            }
            Task { @MainActor [weak self] in
                guard let self else { return }
                let previous = lastNetworkSignature
                lastNetworkSignature = signature
                networkAvailable = path.status == .satisfied
                interfaceLabel = label
                guard let previous, previous != signature else { return }
                appendEvent(.network, title: "Network path changed", detail: "Current path: \(label). Reconnection requested.")
                guard runtimeStartedInProcess else { return }
                do {
                    try await native.notifyNetworkChanged()
                } catch {
                    if runtimeStartedInProcess { fail(error) }
                }
            }
        }
        networkMonitor.start(queue: networkQueue)
    }

    private func appendEvent(_ kind: RecoveryEvent.Kind, title: String, detail: String) {
        recoveryEvents.insert(RecoveryEvent(date: .now, kind: kind, title: title, detail: detail), at: 0)
        if recoveryEvents.count > 30 { recoveryEvents.removeLast(recoveryEvents.count - 30) }
    }

    private func fail(_ error: Error) {
        let message = error.localizedDescription
        presentedError = message
        var next = snapshot
        next.error = message
        publish(next)
        appendEvent(.error, title: "Needs attention", detail: message)
    }

    private func failRuntime(_ error: Error, operation: String) {
        let message = error.localizedDescription
        var next = snapshot
        next.phase = .error
        next.online = false
        next.error = message
        publish(next)
        presentedError = message
        appendEvent(.error, title: "Needs attention", detail: message)
        logger.error("Travel \(operation, privacy: .public) failed: \(message, privacy: .private)")
    }
}
