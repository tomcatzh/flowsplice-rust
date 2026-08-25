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
    @Published private(set) var liveActivityStatus: TravelLiveActivityStatus = .checking

    let defaultTravelID: String
    let defaultHomeID = "home-1"

    private let native = NativeTravelClient()
    private let liveActivity = TravelLiveActivityController()
    private let continuedSession = TravelContinuedSessionController.shared
    private let networkMonitor = NWPathMonitor()
    private let networkQueue = DispatchQueue(label: "io.zxf.flowsplice.travel.network")
    private let logger = Logger(subsystem: "io.zxf.flowsplice.travel", category: "lifecycle")
    private var lastNetworkSignature: String?
    private var pollTask: Task<Void, Never>?
    private var bootstrapped = false
    private var reconciling = false
    private var runtimeStartedInProcess = false
    private var requiresUserStart = false

    init() {
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
        if environment["FLOWSPLICE_E2E_RELAY"] != nil {
            EnrollmentStore.lastRelay = environment["FLOWSPLICE_E2E_RELAY"] ?? ""
        }
        liveActivityStatus = liveActivity.currentStatus
        continuedSession.onExpiration = { [weak self] in
            guard let self else { return }
            handleContinuedSessionExpiration()
        }
    }

    deinit {
        pollTask?.cancel()
        networkMonitor.cancel()
    }

    func bootstrap() {
        guard !bootstrapped else { return }
        bootstrapped = true
        startNetworkMonitor()
        Task { await reconcile(reason: "App launched") }
    }

    func handleScenePhase(_ phase: ScenePhase) {
        switch phase {
        case .active:
            appendEvent(.lifecycle, title: "App active", detail: "Runtime and catalog reconciliation started.")
            Task { await reconcile(reason: "Returned to foreground") }
        case .background:
            appendEvent(.lifecycle, title: "App backgrounded", detail: "Live Activity, durable enrollment, and mapping state were preserved.")
            if snapshot.phase == .running && !continuedSession.isActiveOrRequested {
                appendEvent(
                    .error,
                    title: "Foreground-only session",
                    detail: "iPadOS did not grant continued processing; the local mappings may suspend in the background."
                )
            }
            Task { await synchronizeLiveActivity(force: true, presentFailure: false) }
        case .inactive:
            break
        @unknown default:
            break
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
        pollTask?.cancel()
        Task {
            do {
                try await native.cancelEnrollment()
                EnrollmentStore.pending = nil
                CredentialStore.clear()
                try TravelFiles.discardPendingInstallation()
                enrollment = EnrollmentSnapshot()
                snapshot = TravelSnapshot()
                appendEvent(.lifecycle, title: "Enrollment cancelled", detail: "Pending credentials were removed.")
            } catch {
                fail(error)
            }
            isWorking = false
        }
    }

    func start() {
        guard !isWorking else { return }
        isWorking = true
        requiresUserStart = false
        snapshot.phase = .starting
        snapshot.error = nil
        logger.notice("Travel start requested")
        Task {
            await startRuntime(userInitiated: true, requestContinuation: true)
            isWorking = false
        }
    }

    func stop() {
        guard !isWorking else { return }
        isWorking = true
        pollTask?.cancel()
        EnrollmentStore.autoStart = false
        requiresUserStart = false
        snapshot.phase = .stopping
        logger.notice("Travel stop requested")
        Task {
            do {
                try await native.stop()
                runtimeStartedInProcess = false
                snapshot.phase = .stopped
                snapshot.online = false
                snapshot.activeFlows = 0
                snapshot.relayCount = 0
                snapshot.error = nil
                continuedSession.complete(success: true)
                liveActivityStatus = await liveActivity.end(snapshot: snapshot, interfaceLabel: interfaceLabel)
                appendEvent(.lifecycle, title: "Travel stopped", detail: "Mappings remain stored for the next start.")
                logger.notice("Travel stop completed")
            } catch {
                continuedSession.complete(success: false)
                liveActivityStatus = await liveActivity.end(snapshot: snapshot, interfaceLabel: interfaceLabel)
                failRuntime(error, operation: "stop")
            }
            isWorking = false
        }
    }

    func handleDeepLink(_ url: URL) {
        guard url.scheme?.lowercased() == "flowsplice" else { return }
        selectedSection = .overview
        if url.host?.lowercased() == "stop" {
            stop()
        }
    }

    func refreshCatalog() {
        Task {
            do {
                let next = try await native.catalog()
                catalog = next
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

    func simulateContinuedSessionExpirationForTesting() {
        continuedSession.simulateExpirationForTesting()
    }

    private func reconcile(reason: String) async {
        guard !reconciling else { return }
        guard !isWorking else {
            logger.debug("Skipped lifecycle reconciliation while an operation is running")
            return
        }
        reconciling = true
        defer { reconciling = false }
        snapshot.enrolled = TravelFiles.isInstalled
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
        if requiresUserStart {
            snapshot.enrolled = true
            snapshot.phase = .stopped
        } else if runtimeStartedInProcess {
            await refreshStatusAndCatalog(forceCatalog: true)
            if snapshot.phase == .running { beginStatusPolling() }
        } else if EnrollmentStore.autoStart && continuedSession.isActive {
            isWorking = true
            snapshot.phase = .starting
            await startRuntime(userInitiated: false, requestContinuation: false)
            isWorking = false
        } else {
            if EnrollmentStore.autoStart {
                appendEvent(
                    .lifecycle,
                    title: "Start required after launch",
                    detail: "Tap Start to create a new user-initiated iPadOS background session."
                )
            }
            EnrollmentStore.autoStart = false
            requiresUserStart = true
            snapshot.enrolled = true
            snapshot.phase = .stopped
        }
        appendEvent(.lifecycle, title: "State reconciled", detail: reason)
    }

    private func startRuntime(userInitiated: Bool, requestContinuation: Bool) async {
        guard TravelFiles.isInstalled else {
            presentedError = "Complete remote enrollment before starting Travel."
            return
        }
        guard let password = CredentialStore.load() else {
            fail(TravelError.missingCredential)
            return
        }
        snapshot.phase = .starting
        snapshot.enrolled = true
        snapshot.error = nil
        let startedAt = Date()
        if requestContinuation {
            requestContinuedSessionIfAvailable()
        }
        do {
            try TravelFiles.prepareRuntimeStorage()
            let status = try await native.start(config: TravelFiles.config, password: password)
            runtimeStartedInProcess = true
            apply(status)
            if requiresUserStart {
                snapshot.phase = .stopped
                snapshot.online = false
                snapshot.error = "Background permission ended while Travel was starting. Tap Start to resume without reloading credentials."
                logger.error("Continued processing expired before Travel start completed")
                return
            }
            EnrollmentStore.autoStart = true
            await synchronizeLiveActivity(force: true, presentFailure: userInitiated)
            await refreshStatusAndCatalog(forceCatalog: true)
            beginStatusPolling()
            let elapsed = Date().timeIntervalSince(startedAt)
            logger.notice("Travel start completed in \(elapsed, format: .fixed(precision: 2)) seconds")
            if userInitiated {
                appendEvent(.lifecycle, title: "Travel started", detail: "Runtime and local mappings are active.")
            }
        } catch {
            runtimeStartedInProcess = false
            continuedSession.complete(success: false)
            failRuntime(error, operation: "start")
        }
    }

    private func beginEnrollmentPolling() {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                do {
                    let next = try await native.enrollmentStatus()
                    enrollment = next
                    if let code = next.verificationCode {
                        TravelFiles.writeE2EVerificationCode(code)
                    }
                    switch next.phase {
                    case .installed:
                        EnrollmentStore.pending = nil
                        EnrollmentStore.autoStart = true
                        snapshot.enrolled = true
                        appendEvent(.lifecycle, title: "Enrollment installed", detail: "Home approval and credential verification completed.")
                        snapshot.phase = .starting
                        await startRuntime(userInitiated: false, requestContinuation: true)
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

    private func beginStatusPolling() {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                await refreshStatusAndCatalog(forceCatalog: false)
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    private func refreshStatusAndCatalog(forceCatalog: Bool) async {
        do {
            let previousGeneration = snapshot.catalogGeneration
            let status = try await native.status()
            apply(status)
            await synchronizeLiveActivity(force: forceCatalog, presentFailure: false)
            if forceCatalog || status.catalogGeneration != previousGeneration || catalog.generation == 0 {
                let next = try await native.catalog()
                let changed = next.generation != catalog.generation
                catalog = next
                if changed {
                    appendEvent(.catalog, title: "Service catalog updated", detail: "Accepted generation \(next.generation).")
                }
            }
        } catch {
            snapshot.online = false
            snapshot.error = error.localizedDescription
        }
    }

    private func apply(_ status: NativeTravelStatus) {
        snapshot = TravelSnapshot(native: status)
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
                if snapshot.phase == .running {
                    await synchronizeLiveActivity(force: true, presentFailure: false)
                }
                guard let previous, previous != signature else { return }
                appendEvent(.network, title: "Network path changed", detail: "Current path: \(label). Reconnection requested.")
                do {
                    try await native.notifyNetworkChanged()
                } catch {
                    if snapshot.phase == .running { fail(error) }
                }
            }
        }
        networkMonitor.start(queue: networkQueue)
    }

    private func appendEvent(_ kind: RecoveryEvent.Kind, title: String, detail: String) {
        recoveryEvents.insert(RecoveryEvent(date: .now, kind: kind, title: title, detail: detail), at: 0)
        if recoveryEvents.count > 30 { recoveryEvents.removeLast(recoveryEvents.count - 30) }
    }

    private func synchronizeLiveActivity(force: Bool, presentFailure: Bool) async {
        if #available(iOS 26.0, *), continuedSession.isActiveOrRequested {
            continuedSession.update(snapshot: snapshot)
            liveActivityStatus = .active
            _ = await liveActivity.end(snapshot: snapshot, interfaceLabel: interfaceLabel)
            TravelFiles.writeE2EPhase("live-activity-active")
            return
        }
        do {
            liveActivityStatus = try await liveActivity.synchronize(
                snapshot: snapshot,
                interfaceLabel: interfaceLabel,
                force: force
            )
            if liveActivityStatus == .active {
                TravelFiles.writeE2EPhase("live-activity-active")
            } else if liveActivityStatus == .disabled, presentFailure {
                presentedError = "Travel is running, but Live Activities are disabled. Enable them for FlowSplice Travel in Settings to keep the session visible."
            }
        } catch {
            let message = "Live Activity could not start: \(error.localizedDescription)"
            liveActivityStatus = .failed(message)
            appendEvent(.error, title: "Live Activity unavailable", detail: message)
            if presentFailure { presentedError = message }
        }
    }

    private func requestContinuedSessionIfAvailable() {
        guard #available(iOS 26.0, *) else { return }
        do {
            try continuedSession.request(travelID: snapshot.travelID)
            logger.notice("Continued-processing request submitted")
            appendEvent(
                .lifecycle,
                title: "Continued session requested",
                detail: "iOS 26 continued processing now owns the user-visible background session."
            )
        } catch {
            logger.error("Continued-processing request failed: \(error.localizedDescription, privacy: .private)")
            appendEvent(
                .error,
                title: "Continued processing unavailable",
                detail: "Falling back to the standard Live Activity: \(error.localizedDescription)"
            )
        }
    }

    private func fail(_ error: Error) {
        let message = error.localizedDescription
        presentedError = message
        snapshot.error = message
        appendEvent(.error, title: "Needs attention", detail: message)
    }

    private func failRuntime(_ error: Error, operation: String) {
        let message = error.localizedDescription
        snapshot.phase = .error
        snapshot.online = false
        snapshot.error = message
        presentedError = message
        appendEvent(.error, title: "Needs attention", detail: message)
        logger.error("Travel \(operation, privacy: .public) failed: \(message, privacy: .private)")
    }

    private func handleContinuedSessionExpiration() {
        pollTask?.cancel()
        EnrollmentStore.autoStart = false
        requiresUserStart = true
        liveActivityStatus = .inactive
        let message = "iPadOS ended the background session. Open FlowSplice and tap Start to resume local mappings."
        if !isWorking {
            snapshot.phase = .stopped
            snapshot.online = false
            snapshot.activeFlows = 0
            snapshot.relayCount = 0
        }
        snapshot.error = message
        appendEvent(.lifecycle, title: "Background session ended", detail: message)
        logger.error("iPadOS expired the continued-processing session")
    }
}
