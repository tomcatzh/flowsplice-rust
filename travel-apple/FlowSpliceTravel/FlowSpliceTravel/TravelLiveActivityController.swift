import ActivityKit
import Foundation

nonisolated enum TravelLiveActivityStatus: Equatable, Sendable {
    case checking
    case inactive
    case active
    case disabled
    case failed(String)

    var label: String {
        switch self {
        case .checking: "Checking"
        case .inactive: "Ready"
        case .active: "Visible"
        case .disabled: "Disabled in Settings"
        case .failed: "Unavailable"
        }
    }

    var detail: String {
        switch self {
        case .checking:
            "Checking whether Live Activities are available."
        case .inactive:
            "Optional status presentation starts automatically with Travel."
        case .active:
            "Travel is visible on the Lock Screen and supported system surfaces."
        case .disabled:
            "Optional status presentation is disabled. Travel continues independently."
        case .failed(let message):
            message
        }
    }
}

@MainActor
final class TravelLiveActivityController {
    private let forceDisabled: Bool
    private var lastState: TravelActivityAttributes.ContentState?
    private var lastUpdate = Date.distantPast

    init(forceDisabled: Bool = ProcessInfo.processInfo.environment["FLOWSPLICE_E2E_DISABLE_LIVE_ACTIVITY"] == "1") {
        self.forceDisabled = forceDisabled
    }

    var currentStatus: TravelLiveActivityStatus {
        guard !forceDisabled, ActivityAuthorizationInfo().areActivitiesEnabled else { return .disabled }
        return Activity<TravelActivityAttributes>.activities.isEmpty ? .inactive : .active
    }

    func synchronize(
        snapshot: TravelSnapshot,
        interfaceLabel: String,
        force: Bool = false
    ) async throws -> TravelLiveActivityStatus {
        guard !forceDisabled, ActivityAuthorizationInfo().areActivitiesEnabled else {
            return .disabled
        }
        guard snapshot.phase == .running || snapshot.phase == .starting else {
            await endAll(using: contentState(snapshot: snapshot, interfaceLabel: interfaceLabel))
            lastState = nil
            return .inactive
        }

        let state = contentState(snapshot: snapshot, interfaceLabel: interfaceLabel)
        var activities = Activity<TravelActivityAttributes>.activities
        if activities.isEmpty {
            let attributes = TravelActivityAttributes(
                travelID: snapshot.travelID,
                startedAt: Date().addingTimeInterval(-TimeInterval(snapshot.uptimeSeconds))
            )
            let content = ActivityContent(state: state, staleDate: nil)
            _ = try Activity.request(attributes: attributes, content: content, pushType: nil)
            activities = Activity<TravelActivityAttributes>.activities
            lastState = state
            lastUpdate = .now
        } else if shouldUpdate(to: state, force: force) {
            let content = ActivityContent(state: state, staleDate: nil)
            for activity in activities {
                await activity.update(content)
            }
            lastState = state
            lastUpdate = .now
        }

        if activities.count > 1 {
            for duplicate in activities.dropFirst() {
                await duplicate.end(nil, dismissalPolicy: .immediate)
            }
        }
        return .active
    }

    func end(snapshot: TravelSnapshot, interfaceLabel: String) async -> TravelLiveActivityStatus {
        await endAll(using: contentState(snapshot: snapshot, interfaceLabel: interfaceLabel))
        lastState = nil
        return !forceDisabled && ActivityAuthorizationInfo().areActivitiesEnabled ? .inactive : .disabled
    }

    private func shouldUpdate(to state: TravelActivityAttributes.ContentState, force: Bool) -> Bool {
        guard !force else { return true }
        guard let previous = lastState else { return true }
        let importantChange = previous.phase != state.phase ||
            previous.online != state.online ||
            previous.activeFlows != state.activeFlows ||
            previous.relayCount != state.relayCount ||
            previous.mappingCount != state.mappingCount ||
            previous.interfaceLabel != state.interfaceLabel
        return importantChange || Date().timeIntervalSince(lastUpdate) >= 15
    }

    private func contentState(
        snapshot: TravelSnapshot,
        interfaceLabel: String
    ) -> TravelActivityAttributes.ContentState {
        TravelActivityAttributes.ContentState(
            phase: snapshot.phase.rawValue,
            online: snapshot.online,
            activeFlows: snapshot.activeFlows,
            uploadedBytes: snapshot.uploadedBytes,
            downloadedBytes: snapshot.downloadedBytes,
            relayCount: snapshot.relayCount,
            mappingCount: snapshot.mappings.count,
            interfaceLabel: interfaceLabel,
            updatedAt: .now
        )
    }

    private func endAll(using state: TravelActivityAttributes.ContentState) async {
        let finalContent = ActivityContent(state: state, staleDate: nil)
        for activity in Activity<TravelActivityAttributes>.activities {
            await activity.end(finalContent, dismissalPolicy: .immediate)
        }
    }
}
