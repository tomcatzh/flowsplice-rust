import AVFAudio
import Foundation
import OSLog

nonisolated enum TravelBackgroundAudioStatus: Equatable, Sendable {
    case inactive
    case starting
    case active
    case interrupted
    case recovering
    case failed(String)

    var label: String {
        switch self {
        case .inactive: "Inactive"
        case .starting: "Starting"
        case .active: "Active"
        case .interrupted: "Interrupted"
        case .recovering: "Recovering"
        case .failed: "Unavailable"
        }
    }

    var detail: String {
        switch self {
        case .inactive:
            "Starts and stops with the user-controlled Travel runtime."
        case .starting:
            "Preparing a mixable playback session for background continuity."
        case .active:
            "The mixable playback session is active. Other media remains independent."
        case .interrupted:
            "The system temporarily interrupted playback. Travel will resume it when permitted."
        case .recovering:
            "The playback session is recovering automatically without opening FlowSplice."
        case .failed(let message):
            message
        }
    }
}

@MainActor
protocol TravelBackgroundAudioControlling: AnyObject {
    var status: TravelBackgroundAudioStatus { get }
    var onStatusChange: ((TravelBackgroundAudioStatus) -> Void)? { get set }

    func start() async throws
    func stop() async throws
    func reconcile() async
}

enum TravelBackgroundAudioError: LocalizedError {
    case invalidOutputFormat
    case engineDidNotStart

    var errorDescription: String? {
        switch self {
        case .invalidOutputFormat:
            "The active audio route did not provide a usable playback format."
        case .engineDidNotStart:
            "The background audio engine did not start."
        }
    }
}

nonisolated enum TravelBackgroundAudioRecoveryPolicy {
    static func delay(attempt: Int) -> TimeInterval {
        let boundedAttempt = max(1, attempt)
        let base: TimeInterval
        if boundedAttempt <= 6 {
            base = 0.25 * pow(2, Double(boundedAttempt - 1))
        } else {
            base = min(300, 15 * pow(2, Double(min(boundedAttempt - 7, 5))))
        }
        let deterministicJitter = 0.9 + Double((boundedAttempt * 37) % 21) / 100
        return base * deterministicJitter
    }
}

@MainActor
final class TravelBackgroundAudioController: TravelBackgroundAudioControlling {
    private let worker: TravelBackgroundAudioWorker

    private(set) var status: TravelBackgroundAudioStatus = .inactive {
        didSet {
            guard status != oldValue else { return }
            onStatusChange?(status)
        }
    }

    var onStatusChange: ((TravelBackgroundAudioStatus) -> Void)?

    init(
        session: AVAudioSession = .sharedInstance(),
        notificationCenter: NotificationCenter = .default
    ) {
        worker = TravelBackgroundAudioWorker(
            session: session,
            notificationCenter: notificationCenter
        )
        worker.setStatusHandler { [weak self] next in
            Task { @MainActor [weak self] in
                self?.status = next
            }
        }
    }

    func start() async throws {
        status = .starting
        do {
            try await worker.start()
            status = .active
        } catch {
            status = .failed("Background audio could not start: \(error.localizedDescription)")
            throw error
        }
    }

    func stop() async throws {
        do {
            try await worker.stop()
            status = .inactive
        } catch {
            // The graph is already stopped and the worker has cleared its run intent. Session
            // deactivation is cleanup, so it must not leave the product in a failed runtime state.
            status = .inactive
            throw error
        }
    }

    func reconcile() async {
        await worker.reconcile()
    }
}

nonisolated private final class TravelBackgroundAudioWorker: @unchecked Sendable {
    private let session: AVAudioSession
    private let notificationCenter: NotificationCenter
    private let queue = DispatchQueue(label: "io.zxf.flowsplice.travel.background-audio")
    private let logger = Logger(subsystem: "io.zxf.flowsplice.travel", category: "background-audio")

    private var engine: AVAudioEngine?
    private var player: AVAudioPlayerNode?
    private var silentBuffer: AVAudioPCMBuffer?
    private var shouldRun = false
    private var rebuilding = false
    private var recoveryAttempt = 0
    private var recoveryWorkItem: DispatchWorkItem?
    private var observerTokens: [NSObjectProtocol] = []
    private var statusHandler: (@Sendable (TravelBackgroundAudioStatus) -> Void)?

    init(session: AVAudioSession, notificationCenter: NotificationCenter) {
        self.session = session
        self.notificationCenter = notificationCenter
        registerForNotifications()
    }

    deinit {
        recoveryWorkItem?.cancel()
        for token in observerTokens {
            notificationCenter.removeObserver(token)
        }
    }

    func setStatusHandler(_ handler: @escaping @Sendable (TravelBackgroundAudioStatus) -> Void) {
        queue.async { [weak self] in self?.statusHandler = handler }
    }

    func start() async throws {
        try await perform {
            self.shouldRun = true
            self.recoveryWorkItem?.cancel()
            self.recoveryWorkItem = nil
            self.recoveryAttempt = 0
            if self.engine?.isRunning == true, self.player?.isPlaying == true {
                return
            }
            do {
                try self.activateAndStart(reason: "user-runtime-start", rebuild: true)
            } catch {
                self.scheduleRecovery(reason: "initial-start", rebuild: true)
                throw error
            }
        }
    }

    func stop() async throws {
        try await perform {
            self.shouldRun = false
            self.recoveryAttempt = 0
            self.recoveryWorkItem?.cancel()
            self.recoveryWorkItem = nil
            self.tearDownGraph()
            try self.session.setActive(false, options: [.notifyOthersOnDeactivation])
            self.logger.notice("Background audio stopped and the session was deactivated")
        }
    }

    func reconcile() async {
        await performWithoutThrowing {
            guard self.shouldRun else { return }
            guard self.engine?.isRunning != true || self.player?.isPlaying != true else {
                self.emit(.active)
                return
            }
            do {
                try self.activateAndStart(reason: "runtime-reconciliation", rebuild: false)
            } catch {
                self.scheduleRecovery(reason: "runtime-reconciliation", rebuild: true)
            }
        }
    }

    private func activateAndStart(reason: String, rebuild: Bool) throws {
        rebuilding = true
        defer { rebuilding = false }

        try session.setCategory(.playback, mode: .default, options: [.mixWithOthers])
        try session.setActive(true)

        if !rebuild,
           let engine,
           let player,
           silentBuffer != nil {
            if !engine.isRunning { try engine.start() }
            if !player.isPlaying { player.play() }
            guard engine.isRunning, player.isPlaying else {
                throw TravelBackgroundAudioError.engineDidNotStart
            }
            recoveryAttempt = 0
            recoveryWorkItem?.cancel()
            recoveryWorkItem = nil
            emit(.active)
            logger.notice("Background audio resumed reason=\(reason, privacy: .public)")
            return
        }

        tearDownGraph()
        let nextEngine = AVAudioEngine()
        let nextPlayer = AVAudioPlayerNode()
        let outputFormat = nextEngine.outputNode.outputFormat(forBus: 0)
        guard outputFormat.sampleRate > 0, outputFormat.channelCount > 0 else {
            throw TravelBackgroundAudioError.invalidOutputFormat
        }

        let capacity = AVAudioFrameCount(max(1_024, min(outputFormat.sampleRate, 48_000)))
        guard let buffer = AVAudioPCMBuffer(pcmFormat: outputFormat, frameCapacity: capacity) else {
            throw TravelBackgroundAudioError.invalidOutputFormat
        }
        buffer.frameLength = capacity
        zero(buffer)

        nextEngine.attach(nextPlayer)
        nextEngine.connect(nextPlayer, to: nextEngine.mainMixerNode, format: outputFormat)
        nextPlayer.scheduleBuffer(buffer, at: nil, options: [.loops])
        nextEngine.prepare()
        try nextEngine.start()
        nextPlayer.play()
        guard nextEngine.isRunning, nextPlayer.isPlaying else {
            nextPlayer.stop()
            nextEngine.stop()
            nextEngine.detach(nextPlayer)
            throw TravelBackgroundAudioError.engineDidNotStart
        }

        engine = nextEngine
        player = nextPlayer
        silentBuffer = buffer
        recoveryAttempt = 0
        recoveryWorkItem?.cancel()
        recoveryWorkItem = nil
        emit(.active)
        let route = session.currentRoute.outputs.map(\.portType.rawValue).joined(separator: ",")
        logger.notice(
            "Background audio active reason=\(reason, privacy: .public) sample_rate=\(outputFormat.sampleRate, format: .fixed(precision: 0)) channels=\(outputFormat.channelCount) mixable=true route=\(route, privacy: .public)"
        )
    }

    private func zero(_ buffer: AVAudioPCMBuffer) {
        for audioBuffer in UnsafeMutableAudioBufferListPointer(buffer.mutableAudioBufferList) {
            guard let data = audioBuffer.mData else { continue }
            data.initializeMemory(
                as: UInt8.self,
                repeating: 0,
                count: Int(audioBuffer.mDataByteSize)
            )
        }
    }

    private func tearDownGraph() {
        player?.stop()
        if let engine {
            engine.stop()
            if let player {
                engine.disconnectNodeOutput(player)
                engine.detach(player)
            }
            engine.reset()
        }
        player = nil
        silentBuffer = nil
        engine = nil
    }

    private func scheduleRecovery(reason: String, rebuild: Bool) {
        guard shouldRun else { return }
        recoveryWorkItem?.cancel()
        recoveryAttempt += 1
        let attempt = recoveryAttempt
        let delay = TravelBackgroundAudioRecoveryPolicy.delay(attempt: attempt)
        emit(.recovering)
        logger.notice(
            "Background audio recovery scheduled reason=\(reason, privacy: .public) attempt=\(attempt) delay=\(delay, format: .fixed(precision: 2))"
        )
        let item = DispatchWorkItem { [weak self] in
            guard let self, self.shouldRun else { return }
            do {
                try self.activateAndStart(reason: reason, rebuild: rebuild)
            } catch {
                self.logger.error(
                    "Background audio recovery failed reason=\(reason, privacy: .public) attempt=\(attempt)"
                )
                self.scheduleRecovery(reason: reason, rebuild: true)
            }
        }
        recoveryWorkItem = item
        queue.asyncAfter(deadline: .now() + delay, execute: item)
    }

    private func registerForNotifications() {
        observerTokens.append(notificationCenter.addObserver(
            forName: AVAudioSession.interruptionNotification,
            object: session,
            queue: nil
        ) { [weak self] notification in
            let typeValue = notification.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt
            let optionValue = notification.userInfo?[AVAudioSessionInterruptionOptionKey] as? UInt ?? 0
            self?.queue.async { [weak self] in
                self?.handleInterruption(typeValue: typeValue, optionValue: optionValue)
            }
        })
        observerTokens.append(notificationCenter.addObserver(
            forName: AVAudioSession.routeChangeNotification,
            object: session,
            queue: nil
        ) { [weak self] notification in
            let reasonValue = notification.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt
            self?.queue.async { [weak self] in self?.handleRouteChange(reasonValue: reasonValue) }
        })
        observerTokens.append(notificationCenter.addObserver(
            forName: AVAudioSession.mediaServicesWereResetNotification,
            object: session,
            queue: nil
        ) { [weak self] _ in
            self?.queue.async { [weak self] in self?.handleMediaServicesReset() }
        })
        observerTokens.append(notificationCenter.addObserver(
            forName: .AVAudioEngineConfigurationChange,
            object: nil,
            queue: nil
        ) { [weak self] _ in
            self?.queue.async { [weak self] in self?.handleEngineConfigurationChange() }
        })
    }

    private func handleInterruption(typeValue: UInt?, optionValue: UInt) {
        guard shouldRun,
              let typeValue,
              let type = AVAudioSession.InterruptionType(rawValue: typeValue) else { return }
        switch type {
        case .began:
            player?.pause()
            engine?.pause()
            emit(.interrupted)
            logger.notice("Background audio interruption began")
        case .ended:
            let options = AVAudioSession.InterruptionOptions(rawValue: optionValue)
            logger.notice(
                "Background audio interruption ended should_resume=\(options.contains(.shouldResume))"
            )
            guard options.contains(.shouldResume) else {
                // The system recommendation is not a permanent terminal state. Keep retries
                // bounded and infrequent until playback is permitted again.
                scheduleRecovery(reason: "interruption-ended-without-resume", rebuild: false)
                return
            }
            do {
                try activateAndStart(reason: "interruption-ended", rebuild: false)
            } catch {
                scheduleRecovery(reason: "interruption-ended", rebuild: true)
            }
        @unknown default:
            emit(.interrupted)
        }
    }

    private func handleRouteChange(reasonValue: UInt?) {
        guard shouldRun, !rebuilding else { return }
        let reason = reasonValue.flatMap(AVAudioSession.RouteChangeReason.init(rawValue:))
        logger.notice("Background audio route changed reason=\(String(describing: reason), privacy: .public)")
        scheduleRecovery(reason: "route-change", rebuild: true)
    }

    private func handleMediaServicesReset() {
        guard shouldRun, !rebuilding else { return }
        logger.notice("Audio media services reset")
        tearDownGraph()
        scheduleRecovery(reason: "media-services-reset", rebuild: true)
    }

    private func handleEngineConfigurationChange() {
        guard shouldRun, !rebuilding else { return }
        logger.notice("Background audio engine configuration changed")
        scheduleRecovery(reason: "engine-configuration-change", rebuild: true)
    }

    private func emit(_ status: TravelBackgroundAudioStatus) {
        statusHandler?(status)
    }

    private func perform<Value: Sendable>(
        _ operation: @escaping @Sendable () throws -> Value
    ) async throws -> Value {
        try await withCheckedThrowingContinuation { continuation in
            queue.async {
                continuation.resume(with: Result { try operation() })
            }
        }
    }

    private func performWithoutThrowing(_ operation: @escaping @Sendable () -> Void) async {
        await withCheckedContinuation { continuation in
            queue.async {
                operation()
                continuation.resume()
            }
        }
    }
}
