import BackgroundTasks
import Foundation

nonisolated struct TravelContinuedSessionState: Equatable, Sendable {
    enum Phase: Equatable, Sendable {
        case idle
        case requested(UInt64)
        case active(UInt64)
    }

    private(set) var phase: Phase = .idle
    private var nextToken: UInt64 = 0

    var isActiveOrRequested: Bool {
        phase != .idle
    }

    var isActive: Bool {
        if case .active = phase { return true }
        return false
    }

    mutating func beginRequest() -> UInt64 {
        nextToken &+= 1
        phase = .requested(nextToken)
        return nextToken
    }

    mutating func accept() -> UInt64 {
        let token: UInt64
        if case .requested(let requestedToken) = phase {
            token = requestedToken
        } else {
            nextToken &+= 1
            token = nextToken
        }
        phase = .active(token)
        return token
    }

    mutating func reset(ifCurrent token: UInt64? = nil) -> Bool {
        if let token {
            let currentToken: UInt64?
            switch phase {
            case .idle: currentToken = nil
            case .requested(let value), .active(let value): currentToken = value
            }
            guard currentToken == token else { return false }
        }
        nextToken &+= 1
        phase = .idle
        return true
    }
}

@MainActor
final class TravelContinuedSessionController {
    static let shared = TravelContinuedSessionController()

    private static let identifier = "io.zxf.flowsplice.travel.session"
    private static let maximumSessionSeconds: Int64 = 8 * 60 * 60

    private var registrationAttempted = false
    private var registered = false
    private var state = TravelContinuedSessionState()
    private var continuedTaskStorage: AnyObject?

    var onExpiration: (() -> Void)?

    var isActiveOrRequested: Bool {
        if #available(iOS 26.0, *) { return state.isActiveOrRequested }
        return false
    }

    var isActive: Bool {
        if #available(iOS 26.0, *) { return state.isActive }
        return false
    }

    private init() {}

    func register() {
        guard #available(iOS 26.0, *), !registrationAttempted else { return }
        registrationAttempted = true
        registered = BGTaskScheduler.shared.register(
            forTaskWithIdentifier: Self.identifier,
            using: nil
        ) { task in
            guard let continuedTask = task as? BGContinuedProcessingTask else {
                task.setTaskCompleted(success: false)
                return
            }
            Task { @MainActor in
                Self.shared.accept(continuedTask)
            }
        }
    }

    @available(iOS 26.0, *)
    func request(travelID: String) throws {
        guard registered else { throw ContinuedSessionError.registrationRejected }
        guard !state.isActiveOrRequested else { return }
        let token = state.beginRequest()
        let request = BGContinuedProcessingTaskRequest(
            identifier: Self.identifier,
            title: "FlowSplice Travel",
            subtitle: "Starting \(travelID)…"
        )
        request.strategy = .fail
        do {
            try BGTaskScheduler.shared.submit(request)
        } catch {
            _ = state.reset(ifCurrent: token)
            throw error
        }
    }

    func update(snapshot: TravelSnapshot) {
        guard #available(iOS 26.0, *),
              let continuedTask = continuedTaskStorage as? BGContinuedProcessingTask else { return }
        continuedTask.progress.totalUnitCount = Self.maximumSessionSeconds
        continuedTask.progress.completedUnitCount = min(
            Int64(clamping: snapshot.uptimeSeconds),
            Self.maximumSessionSeconds - 1
        )
        let connection = snapshot.online ? "Online" : "Reconnecting"
        let subtitle = "\(connection) · \(snapshot.activeFlows) flow(s) · ↓\(TravelFormatting.bytes(snapshot.downloadedBytes)) ↑\(TravelFormatting.bytes(snapshot.uploadedBytes))"
        continuedTask.updateTitle("FlowSplice Travel", subtitle: subtitle)
    }

    func complete(success: Bool) {
        guard #available(iOS 26.0, *) else { return }
        let task = continuedTaskStorage as? BGContinuedProcessingTask
        continuedTaskStorage = nil
        _ = state.reset()
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: Self.identifier)
        task?.expirationHandler = nil
        task?.setTaskCompleted(success: success)
    }

    func simulateExpirationForTesting() {
        guard ProcessInfo.processInfo.environment["FLOWSPLICE_E2E"] == "1" else { return }
        if #available(iOS 26.0, *),
           let continuedTask = continuedTaskStorage as? BGContinuedProcessingTask {
            continuedTask.expirationHandler = nil
            continuedTask.setTaskCompleted(success: false)
        }
        continuedTaskStorage = nil
        _ = state.reset()
        onExpiration?()
    }

    @available(iOS 26.0, *)
    private func accept(_ task: BGContinuedProcessingTask) {
        if let previous = continuedTaskStorage as? BGContinuedProcessingTask, previous !== task {
            previous.expirationHandler = nil
            previous.setTaskCompleted(success: false)
        }
        continuedTaskStorage = task
        let token = state.accept()
        task.progress.totalUnitCount = Self.maximumSessionSeconds
        task.progress.completedUnitCount = 1
        task.expirationHandler = { [weak self, weak task] in
            Task { @MainActor in
                guard let self, let task else { return }
                task.setTaskCompleted(success: false)
                guard (self.continuedTaskStorage as? BGContinuedProcessingTask) === task,
                      self.state.reset(ifCurrent: token) else { return }
                self.continuedTaskStorage = nil
                self.onExpiration?()
            }
        }
    }
}

private enum ContinuedSessionError: LocalizedError {
    case registrationRejected

    var errorDescription: String? {
        "iOS rejected the continued-processing launch handler."
    }
}
