import BackgroundTasks
import Foundation

@MainActor
final class TravelContinuedSessionController {
    static let shared = TravelContinuedSessionController()

    private static let identifierPrefix = "io.zxf.flowsplice.travel.session."
    private static let storedIdentifierKey = "flowsplice.continued-session-identifier"
    private static let maximumSessionSeconds: Int64 = 8 * 60 * 60

    private var registeredIdentifiers: Set<String> = []
    private var requestedIdentifier: String?
    private var continuedTaskStorage: AnyObject?

    var onExpiration: (() -> Void)?

    var isActiveOrRequested: Bool {
        if #available(iOS 26.0, *) {
            return requestedIdentifier != nil || continuedTaskStorage as? BGContinuedProcessingTask != nil
        }
        return false
    }

    private init() {}

    func register() {
        guard #available(iOS 26.0, *),
              let identifier = UserDefaults.standard.string(forKey: Self.storedIdentifierKey),
              identifier.hasPrefix(Self.identifierPrefix) else { return }
        guard registerHandler(for: identifier) else {
            clearStoredRequest()
            return
        }
        requestedIdentifier = identifier
    }

    @available(iOS 26.0, *)
    func request(travelID: String) throws {
        guard !isActiveOrRequested else { return }
        let suffix = UUID().uuidString.lowercased()
        let identifier = Self.identifierPrefix + suffix
        guard registerHandler(for: identifier) else {
            throw ContinuedSessionError.registrationRejected
        }
        let request = BGContinuedProcessingTaskRequest(
            identifier: identifier,
            title: "FlowSplice Travel",
            subtitle: "Starting \(travelID)…"
        )
        request.strategy = .fail
        UserDefaults.standard.set(identifier, forKey: Self.storedIdentifierKey)
        requestedIdentifier = identifier
        do {
            try BGTaskScheduler.shared.submit(request)
        } catch {
            clearStoredRequest()
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
        if let requestedIdentifier {
            BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: requestedIdentifier)
        }
        (continuedTaskStorage as? BGContinuedProcessingTask)?.setTaskCompleted(success: success)
        clearStoredRequest()
        continuedTaskStorage = nil
    }

    @available(iOS 26.0, *)
    private func registerHandler(for identifier: String) -> Bool {
        guard !registeredIdentifiers.contains(identifier) else { return true }
        let registered = BGTaskScheduler.shared.register(
            forTaskWithIdentifier: identifier,
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
        if registered {
            registeredIdentifiers.insert(identifier)
        }
        return registered
    }

    private func clearStoredRequest() {
        requestedIdentifier = nil
        UserDefaults.standard.removeObject(forKey: Self.storedIdentifierKey)
    }

    @available(iOS 26.0, *)
    private func accept(_ task: BGContinuedProcessingTask) {
        continuedTaskStorage = task
        if requestedIdentifier == nil {
            requestedIdentifier = task.identifier
        }
        UserDefaults.standard.set(task.identifier, forKey: Self.storedIdentifierKey)
        task.progress.totalUnitCount = Self.maximumSessionSeconds
        task.progress.completedUnitCount = 1
        task.expirationHandler = { [weak self, weak task] in
            Task { @MainActor in
                guard let self else { return }
                task?.setTaskCompleted(success: false)
                self.clearStoredRequest()
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
