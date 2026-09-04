import ActivityKit
import Foundation

nonisolated enum TravelLiveActivityStatus: Equatable, Sendable {
    case inactive
    case active
    case disabled
    case failed(String)

    var label: String {
        switch self {
        case .inactive: "Ready"
        case .active: "Visible"
        case .disabled: "Disabled in Settings"
        case .failed: "Unavailable"
        }
    }

    var detail: String {
        switch self {
        case .inactive:
            "Optional system status appears automatically while Travel is running."
        case .active:
            "Travel status is visible on the Lock Screen and supported system surfaces."
        case .disabled:
            "Live Activities are disabled for FlowSplice. Travel continues independently."
        case .failed(let message):
            "\(message) Travel continues independently."
        }
    }
}

@MainActor
protocol TravelLiveActivityControlling: AnyObject {
    var status: TravelLiveActivityStatus { get }
    var onStatusChange: ((TravelLiveActivityStatus) -> Void)? { get set }

    func synchronize(snapshot: TravelSnapshot, interfaceLabel: String) async
}

nonisolated struct TravelLiveActivityPresentation: Equatable, Sendable {
    var phase: TravelPhase
    var online: Bool
    var activeFlows: Int
    var uploadedBytes: UInt64
    var downloadedBytes: UInt64
    var relayCount: Int
    var mappingCount: Int
    var interfaceLabel: String

    init(snapshot: TravelSnapshot, interfaceLabel: String) {
        phase = snapshot.phase
        online = snapshot.online
        activeFlows = snapshot.activeFlows
        uploadedBytes = snapshot.uploadedBytes
        downloadedBytes = snapshot.downloadedBytes
        relayCount = snapshot.relayCount
        mappingCount = snapshot.mappings.count
        self.interfaceLabel = interfaceLabel
    }

    var shouldBeVisible: Bool { phase == .running }

    func requiresImmediateUpdate(comparedWith previous: Self) -> Bool {
        phase != previous.phase ||
            online != previous.online ||
            activeFlows != previous.activeFlows ||
            relayCount != previous.relayCount ||
            mappingCount != previous.mappingCount ||
            interfaceLabel != previous.interfaceLabel
    }

    func requiresUpdate(
        comparedWith previous: Self,
        lastUpdated: Date,
        now: Date,
        telemetryInterval: TimeInterval
    ) -> Bool {
        if requiresImmediateUpdate(comparedWith: previous) { return true }
        let trafficChanged = uploadedBytes != previous.uploadedBytes ||
            downloadedBytes != previous.downloadedBytes
        return trafficChanged && now.timeIntervalSince(lastUpdated) >= telemetryInterval
    }

    func contentState(at date: Date) -> TravelActivityAttributes.ContentState {
        TravelActivityAttributes.ContentState(
            phase: phase.rawValue,
            online: online,
            activeFlows: activeFlows,
            uploadedBytes: uploadedBytes,
            downloadedBytes: downloadedBytes,
            relayCount: relayCount,
            mappingCount: mappingCount,
            interfaceLabel: interfaceLabel,
            updatedAt: date
        )
    }
}

@MainActor
final class TravelLiveActivityController: TravelLiveActivityControlling {
    private static let telemetryRefreshInterval: TimeInterval = 60
    private static let failureRetryInterval: TimeInterval = 300

    private let forceDisabled: Bool
    private var lastPresentation: TravelLiveActivityPresentation?
    private var lastUpdate = Date.distantPast
    private var retryAfter = Date.distantPast

    private(set) var status: TravelLiveActivityStatus {
        didSet {
            guard status != oldValue else { return }
            onStatusChange?(status)
        }
    }

    var onStatusChange: ((TravelLiveActivityStatus) -> Void)?

    convenience init() {
        self.init(forceDisabled: Self.defaultForceDisabled)
    }

    init(forceDisabled: Bool) {
        self.forceDisabled = forceDisabled
        status = Self.activitiesAreEnabled(forceDisabled: forceDisabled) ? .inactive : .disabled
    }

    func synchronize(snapshot: TravelSnapshot, interfaceLabel: String) async {
        let now = Date()
        let presentation = TravelLiveActivityPresentation(
            snapshot: snapshot,
            interfaceLabel: interfaceLabel
        )

        guard Self.activitiesAreEnabled(forceDisabled: forceDisabled) else {
            status = .disabled
            lastPresentation = nil
            return
        }

        guard presentation.shouldBeVisible else {
            await endAll(with: presentation.contentState(at: now))
            lastPresentation = nil
            retryAfter = .distantPast
            status = .inactive
            return
        }

        guard now >= retryAfter else { return }

        if status == .active,
           let previous = lastPresentation,
           !presentation.requiresUpdate(
                comparedWith: previous,
                lastUpdated: lastUpdate,
                now: now,
                telemetryInterval: Self.telemetryRefreshInterval
           ) {
            return
        }

        do {
            var activities = Activity<TravelActivityAttributes>.activities
            if activities.isEmpty {
                let attributes = TravelActivityAttributes(
                    travelID: snapshot.travelID,
                    startedAt: now.addingTimeInterval(-TimeInterval(snapshot.uptimeSeconds))
                )
                let content = ActivityContent(
                    state: presentation.contentState(at: now),
                    staleDate: nil
                )
                _ = try Activity.request(attributes: attributes, content: content, pushType: nil)
                activities = Activity<TravelActivityAttributes>.activities
                lastPresentation = presentation
                lastUpdate = now
            } else if shouldUpdate(to: presentation, at: now) {
                let content = ActivityContent(
                    state: presentation.contentState(at: now),
                    staleDate: nil
                )
                for activity in activities {
                    await activity.update(content)
                }
                lastPresentation = presentation
                lastUpdate = now
            }

            if activities.count > 1 {
                for duplicate in activities.dropFirst() {
                    await duplicate.end(nil, dismissalPolicy: .immediate)
                }
            }
            retryAfter = .distantPast
            status = .active
        } catch {
            retryAfter = now.addingTimeInterval(Self.failureRetryInterval)
            status = .failed("Live Activity could not be presented: \(error.localizedDescription)")
        }
    }

    private func shouldUpdate(to presentation: TravelLiveActivityPresentation, at now: Date) -> Bool {
        guard let previous = lastPresentation else { return true }
        return presentation.requiresUpdate(
            comparedWith: previous,
            lastUpdated: lastUpdate,
            now: now,
            telemetryInterval: Self.telemetryRefreshInterval
        )
    }

    private func endAll(with state: TravelActivityAttributes.ContentState) async {
        let finalContent = ActivityContent(state: state, staleDate: nil)
        for activity in Activity<TravelActivityAttributes>.activities {
            await activity.end(finalContent, dismissalPolicy: .immediate)
        }
    }

    private static func activitiesAreEnabled(forceDisabled: Bool) -> Bool {
        !forceDisabled && ActivityAuthorizationInfo().areActivitiesEnabled
    }

    private static var defaultForceDisabled: Bool {
        #if DEBUG
        ProcessInfo.processInfo.environment["FLOWSPLICE_E2E_DISABLE_LIVE_ACTIVITY"] == "1"
        #else
        false
        #endif
    }
}
