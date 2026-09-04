import Foundation
import Combine
import Network
import OSLog
import ServiceManagement
import SwiftUI

@MainActor
final class TravelStore: ObservableObject {
    enum Section: String, CaseIterable, Identifiable {
        case overview
        case mappings
        case diagnostics
        case settings

        var id: Self { self }

        var title: String {
            switch self {
            case .overview: "Overview"
            case .mappings: "Mappings"
            case .diagnostics: "Diagnostics"
            case .settings: "Settings"
            }
        }

        var symbol: String {
            switch self {
            case .overview: "square.grid.2x2"
            case .mappings: "point.3.connected.trianglepath.dotted"
            case .diagnostics: "waveform.path.ecg.rectangle"
            case .settings: "gearshape"
            }
        }
    }

    struct Activity: Identifiable, Equatable {
        let id = UUID()
        var date: Date
        var symbol: String
        var title: String
        var detail: String
        var isError: Bool
    }

    @Published var snapshot = TravelSnapshot()
    @Published var enrollment = EnrollmentSnapshot()
    @Published var catalog = TravelCatalog()
    @Published var diagnostics = DiagnosticsSnapshot.empty
    @Published var activities: [Activity] = []
    @Published var selectedSection: Section? = .overview
    @Published var selectedFlowID: String?
    @Published var isWorking = false
    @Published var presentedError: String?
    @Published private(set) var networkAvailable = true
    @Published private(set) var interfaceLabel = "Checking…"
    @Published private(set) var credentialAvailable = false
    @Published private(set) var launchAtLoginEnabled = false
    @Published private(set) var launchAtLoginDetail = "Not registered"

    let defaultTravelID: String
    let defaultHomeID = "home-1"
    #if DEBUG
    let isUITesting: Bool
    #else
    let isUITesting = false
    #endif

    private let native: any TravelClient
    private let networkMonitor = NWPathMonitor()
    private let networkQueue = DispatchQueue(label: "io.zxf.flowsplice.travel.macos.network")
    private let logger = Logger(subsystem: "io.zxf.flowsplice.travel.macos", category: "lifecycle")
    #if DEBUG
    private let mockMode: Bool
    #else
    private let mockMode = false
    #endif
    private var installed: Bool
    private var desiredRunning: Bool
    private var runtimeStartedInProcess = false
    private var bootstrapped = false
    private var statusGeneration: UInt64 = 0
    private var lastNetworkSignature: String?
    private var enrollmentTask: Task<Void, Never>?
    private var statusTask: Task<Void, Never>?
    private var lifecycleTask: Task<Void, Never>?
    private var isTerminating = false

    init(client: (any TravelClient)? = nil) {
        #if DEBUG
        TravelFiles.resetForUITesting()
        let environment = ProcessInfo.processInfo.environment
        let arguments = ProcessInfo.processInfo.arguments
        isUITesting = environment["FLOWSPLICE_UI_TESTING"] == "1"
        mockMode = isUITesting && !arguments.contains("--real-backend")
        native = client ?? (mockMode ? DemoTravelClient() : NativeTravelClient())
        installed = mockMode ? arguments.contains("--mock-enrolled") : TravelFiles.isInstalled
        desiredRunning = mockMode ? installed : EnrollmentStore.autoStart
        #else
        native = client ?? NativeTravelClient()
        installed = TravelFiles.isInstalled
        desiredRunning = EnrollmentStore.autoStart
        #endif
        let name = TravelValidation.normalizedID(Host.current().localizedName ?? "")
        defaultTravelID = name.isEmpty ? "mac-travel" : name
        snapshot.enrolled = installed
        credentialAvailable = mockMode ? installed : CredentialStore.load() != nil
        refreshLaunchAtLoginStatus()
    }

    deinit {
        enrollmentTask?.cancel()
        statusTask?.cancel()
        lifecycleTask?.cancel()
        networkMonitor.cancel()
    }

    var stateTitle: String {
        switch snapshot.phase {
        case .stopped: "Stopped"
        case .starting: "Starting"
        case .running: snapshot.online ? "Online" : "Reconnecting"
        case .stopping: "Stopping"
        case .error: "Needs attention"
        }
    }

    var stateSymbol: String {
        switch snapshot.phase {
        case .stopped: "point.3.filled.connected.trianglepath.dotted"
        case .starting, .stopping: "arrow.trianglehead.2.clockwise.rotate.90"
        case .running: snapshot.online ? "point.3.connected.trianglepath.dotted" : "bolt.horizontal.circle"
        case .error: "exclamationmark.triangle"
        }
    }

    var stateTint: Color {
        switch snapshot.phase {
        case .stopped: .secondary
        case .starting, .stopping: .orange
        case .running: snapshot.online ? Color("FlowMint") : .orange
        case .error: .red
        }
    }

    var relaySummary: String {
        let groups = Dictionary(grouping: diagnostics.flows.compactMap(\.selectedRelay), by: { $0 })
        if groups.isEmpty { return "No active routes" }
        return groups
            .sorted { $0.key < $1.key }
            .map { "\($0.value.count) via \($0.key)" }
            .joined(separator: " · ")
    }

    var isEnrolled: Bool { installed }
    var automaticStartEnabled: Bool { desiredRunning }

    func bootstrap() {
        guard !bootstrapped else { return }
        bootstrapped = true
        if !mockMode { startNetworkMonitor() }
        Task {
            if installed, desiredRunning {
                await startRuntime(reason: "App launched")
            } else if installed {
                await refreshStoppedState()
            } else if let pending = EnrollmentStore.pending, !mockMode {
                resumeEnrollment(pending)
            }
        }
    }

    func select(_ section: Section) {
        selectedSection = section
    }

    func start() {
        guard installed else {
            selectedSection = .overview
            presentedError = "Enroll this Mac before starting Travel."
            return
        }
        desiredRunning = true
        if !mockMode { EnrollmentStore.autoStart = true }
        var next = snapshot
        next.phase = .starting
        next.error = nil
        snapshot = next
        requestLifecycleDrain(reason: "User requested Start")
    }

    func stop() {
        desiredRunning = false
        if !mockMode { EnrollmentStore.autoStart = false }
        var next = snapshot
        next.phase = runtimeStartedInProcess ? .stopping : .stopped
        next.error = nil
        snapshot = next
        Task { await native.wakeStatusWaiters() }
        requestLifecycleDrain(reason: "User requested Stop")
    }

    func prepareForTermination() async {
        guard !isTerminating else { return }
        isTerminating = true
        desiredRunning = false
        enrollmentTask?.cancel()
        statusTask?.cancel()
        await native.wakeStatusWaiters()

        let pendingLifecycle = lifecycleTask
        pendingLifecycle?.cancel()
        await pendingLifecycle?.value

        statusTask?.cancel()
        statusTask = nil
        await native.wakeStatusWaiters()
        if runtimeStartedInProcess {
            try? await native.stop()
            runtimeStartedInProcess = false
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
            defer { isWorking = false }
            do {
                if !mockMode {
                    logger.notice("Enrollment is preparing local storage")
                    try TravelFiles.prepareInstallationDirectory()
                    logger.notice("Enrollment is saving its Keychain credential")
                    try CredentialStore.save(password: password)
                    logger.notice("Enrollment saved its Keychain credential")
                    EnrollmentStore.lastRelay = relay
                    EnrollmentStore.pending = PendingEnrollment(travelID: travelID, homeID: homeID, relay: relay)
                }
                credentialAvailable = true
                logger.notice("Enrollment is starting Travel Core")
                enrollment = try await native.beginEnrollment(
                    installDirectory: TravelFiles.installationDirectory,
                    travelID: travelID,
                    homeID: homeID,
                    relay: relay,
                    password: password
                )
                logger.notice("Travel Core accepted the enrollment operation")
                appendActivity("key", "Enrollment started", "Waiting for \(homeID) approval.")
                beginEnrollmentObservation()
            } catch {
                fail(error)
            }
        }
    }

    func cancelEnrollment() {
        enrollmentTask?.cancel()
        isWorking = true
        Task {
            defer { isWorking = false }
            do {
                try await native.cancelEnrollment()
                if !mockMode {
                    EnrollmentStore.pending = nil
                    CredentialStore.clear()
                    try TravelFiles.discardPendingInstallation()
                }
                credentialAvailable = false
                installed = false
                enrollment = EnrollmentSnapshot()
                snapshot = TravelSnapshot()
                appendActivity("xmark.circle", "Enrollment cancelled", "Pending device credentials were removed.")
            } catch {
                fail(error)
            }
        }
    }

    func refresh() {
        Task { await refreshRuntime(includeCatalog: true) }
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
            await refreshRuntime(includeCatalog: false)
            appendActivity("plus.circle", "Mapping activated", "\(service.displayName) is listening on \(mapping.bind).")
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
                await refreshRuntime(includeCatalog: false)
                appendActivity("trash", "Mapping removed", "\(mapping.serviceID) no longer listens on \(mapping.bind).")
            } catch {
                fail(error)
            }
        }
    }

    func setLaunchAtLogin(_ enabled: Bool) {
        guard !mockMode else {
            launchAtLoginEnabled = enabled
            launchAtLoginDetail = enabled ? "Enabled for UI testing" : "Not registered"
            return
        }
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            refreshLaunchAtLoginStatus()
        } catch {
            refreshLaunchAtLoginStatus()
            fail(error)
        }
    }

    func setAutomaticStart(_ enabled: Bool) {
        desiredRunning = enabled
        if !mockMode { EnrollmentStore.autoStart = enabled }
        if enabled, installed, !runtimeStartedInProcess { start() }
    }

    #if DEBUG
    func simulateNetworkChangeForTesting() {
        guard isUITesting else { return }
        Task {
            try? await native.notifyNetworkChanged()
            appendActivity("network", "Network changed", "Travel retired prior network connections and requested recovery.")
            await refreshRuntime(includeCatalog: false)
        }
    }
    #endif

    private func requestLifecycleDrain(reason: String) {
        guard !isTerminating, lifecycleTask == nil else { return }
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
            if !isTerminating, desiredRunning != runtimeStartedInProcess {
                requestLifecycleDrain(reason: "Apply pending state")
            }
        }
        if desiredRunning, !runtimeStartedInProcess {
            await startRuntime(reason: reason)
        } else if !desiredRunning, runtimeStartedInProcess {
            await stopRuntime(reason: reason)
        }
    }

    private func startRuntime(reason: String) async {
        guard installed, !isTerminating else { return }
        do {
            #if DEBUG
            let password = mockMode
                ? "flowsplice-ui-testing-password"
                : try CredentialStore.loadPromptingIfNeeded()
            #else
            let password = try CredentialStore.loadPromptingIfNeeded()
            #endif
            guard let password else { throw TravelError.missingCredential }
            if !mockMode { try TravelFiles.prepareRuntimeStorage() }
            let nativeStatus = try await native.start(config: TravelFiles.config, password: password)
            runtimeStartedInProcess = true
            if isTerminating || Task.isCancelled {
                try? await native.stop()
                runtimeStartedInProcess = false
                return
            }
            var next = snapshot
            next.merge(native: nativeStatus, phase: .running, clearError: true)
            snapshot = next
            await refreshRuntime(includeCatalog: true)
            beginStatusObservation()
            appendActivity("play.circle", "Travel started", reason)
            logger.notice("Travel runtime started")
        } catch {
            runtimeStartedInProcess = false
            desiredRunning = false
            if !isTerminating { fail(error) }
        }
    }

    private func stopRuntime(reason: String) async {
        statusTask?.cancel()
        statusTask = nil
        await native.wakeStatusWaiters()
        do {
            try await native.stop()
            runtimeStartedInProcess = false
            var next = snapshot
            next.phase = .stopped
            next.online = false
            next.activeFlows = 0
            next.activeRelays = []
            next.error = nil
            snapshot = next
            diagnostics = .empty
            appendActivity("stop.circle", "Travel stopped", reason)
            logger.notice("Travel runtime stopped")
        } catch {
            fail(error)
        }
    }

    private func refreshStoppedState() async {
        var next = snapshot
        next.enrolled = installed
        next.phase = .stopped
        snapshot = next
    }

    private func refreshRuntime(includeCatalog: Bool) async {
        guard runtimeStartedInProcess else { return }
        do {
            async let status = native.status()
            async let routeDiagnostics = native.diagnostics()
            if includeCatalog {
                async let currentCatalog = native.catalog()
                let (nextStatus, nextDiagnostics, nextCatalog) = try await (status, routeDiagnostics, currentCatalog)
                apply(nextStatus, nextDiagnostics)
                catalog = nextCatalog
            } else {
                let (nextStatus, nextDiagnostics) = try await (status, routeDiagnostics)
                apply(nextStatus, nextDiagnostics)
            }
        } catch {
            fail(error)
        }
    }

    private func apply(_ nativeStatus: NativeTravelStatus, _ nextDiagnostics: DiagnosticsSnapshot) {
        var next = snapshot
        next.merge(native: nativeStatus, phase: .running, clearError: true)
        snapshot = next
        diagnostics = nextDiagnostics
        if selectedFlowID == nil || !nextDiagnostics.flows.contains(where: { $0.id == selectedFlowID }) {
            selectedFlowID = nextDiagnostics.flows.first?.id
        }
    }

    private func beginStatusObservation() {
        statusTask?.cancel()
        statusTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled, runtimeStartedInProcess {
                do {
                    let wait: UInt64 = snapshot.activeFlows > 0 ? 2_000 : 15_000
                    let update = try await native.waitForStatusChange(
                        generation: statusGeneration,
                        timeoutMillis: wait
                    )
                    statusGeneration = update.generation
                    let nextDiagnostics = try await native.diagnostics()
                    apply(update.status, nextDiagnostics)
                    if update.status.catalogGeneration != catalog.generation {
                        catalog = try await native.catalog()
                    }
                } catch {
                    if !Task.isCancelled { fail(error) }
                    return
                }
            }
        }
    }

    private func beginEnrollmentObservation() {
        enrollmentTask?.cancel()
        enrollmentTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled, enrollment.phase.isActive {
                try? await Task.sleep(for: mockMode ? .milliseconds(350) : .seconds(1))
                do {
                    let previousPhase = enrollment.phase
                    let previousCodeAvailable = enrollment.verificationCode != nil
                    let update = try await native.enrollmentStatus()
                    if update.phase != previousPhase || (update.verificationCode != nil) != previousCodeAvailable {
                        logger.notice(
                            "Enrollment status changed phase=\(update.phase.rawValue, privacy: .public) verification_code_available=\(update.verificationCode != nil, privacy: .public)"
                        )
                    }
                    enrollment = update
                    guard !Task.isCancelled, !isTerminating else { return }
                    if enrollment.phase == .installed {
                        installed = true
                        snapshot.enrolled = true
                        if !mockMode { EnrollmentStore.pending = nil }
                        desiredRunning = true
                        if !mockMode { EnrollmentStore.autoStart = true }
                        appendActivity("checkmark.seal", "Enrollment completed", "This Mac can now start Travel.")
                        await startRuntime(reason: "Enrollment completed")
                        return
                    }
                    if enrollment.phase == .error {
                        fail(TravelError.native(enrollment.error ?? "Enrollment failed."))
                        return
                    }
                } catch {
                    fail(error)
                    return
                }
            }
        }
    }

    private func resumeEnrollment(_ pending: PendingEnrollment) {
        let password: String
        do {
            guard let stored = try CredentialStore.loadPromptingIfNeeded() else {
                throw TravelError.missingCredential
            }
            password = stored
        } catch {
            fail(error)
            return
        }
        Task {
            do {
                enrollment = try await native.beginEnrollment(
                    installDirectory: TravelFiles.installationDirectory,
                    travelID: pending.travelID,
                    homeID: pending.homeID,
                    relay: pending.relay,
                    password: password
                )
                beginEnrollmentObservation()
            } catch {
                fail(error)
            }
        }
    }

    private func startNetworkMonitor() {
        networkMonitor.pathUpdateHandler = { [weak self] path in
            let available = path.status == .satisfied
            let label = Self.interfaceLabel(for: path)
            let signature = "\(path.status)-\(label)-\(path.isExpensive)-\(path.isConstrained)"
            Task { @MainActor [weak self] in
                guard let self else { return }
                networkAvailable = available
                interfaceLabel = label
                guard lastNetworkSignature != nil, lastNetworkSignature != signature else {
                    lastNetworkSignature = signature
                    return
                }
                lastNetworkSignature = signature
                if runtimeStartedInProcess {
                    do {
                        try await native.notifyNetworkChanged()
                        appendActivity("network", "Network path changed", "Using \(label). Existing TCP Flows are recovering if required.")
                    } catch {
                        fail(error)
                    }
                }
            }
        }
        networkMonitor.start(queue: networkQueue)
    }

    private nonisolated static func interfaceLabel(for path: NWPath) -> String {
        if path.usesInterfaceType(.wiredEthernet) { return "Ethernet" }
        if path.usesInterfaceType(.wifi) { return "Wi-Fi" }
        if path.usesInterfaceType(.cellular) { return "Cellular" }
        if path.usesInterfaceType(.loopback) { return "Loopback" }
        if path.usesInterfaceType(.other) { return "Other" }
        return path.status == .satisfied ? "Available" : "Unavailable"
    }

    private func refreshLaunchAtLoginStatus() {
        guard !mockMode else {
            launchAtLoginEnabled = false
            launchAtLoginDetail = "Not registered"
            return
        }
        switch SMAppService.mainApp.status {
        case .enabled:
            launchAtLoginEnabled = true
            launchAtLoginDetail = "Enabled"
        case .requiresApproval:
            launchAtLoginEnabled = false
            launchAtLoginDetail = "Requires approval in System Settings"
        case .notFound:
            launchAtLoginEnabled = false
            launchAtLoginDetail = "Install FlowSplice in Applications first"
        default:
            launchAtLoginEnabled = false
            launchAtLoginDetail = "Not registered"
        }
    }

    private func appendActivity(_ symbol: String, _ title: String, _ detail: String, error: Bool = false) {
        activities.insert(Activity(date: .now, symbol: symbol, title: title, detail: detail, isError: error), at: 0)
        if activities.count > 20 { activities.removeLast(activities.count - 20) }
    }

    private func fail(_ error: Error) {
        let message = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
        presentedError = message
        var next = snapshot
        next.phase = .error
        next.error = message
        snapshot = next
        appendActivity("exclamationmark.triangle", "Needs attention", message, error: true)
        logger.error("Travel operation failed: \(message, privacy: .public)")
    }
}
