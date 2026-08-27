import AVFAudio
import Foundation
import OSLog

nonisolated enum TravelBackgroundAudioStatus: Equatable, Sendable {
    case inactive
    case starting
    case active
    case interrupted
    case failed(String)

    var label: String {
        switch self {
        case .inactive: "Inactive"
        case .starting: "Starting"
        case .active: "Active"
        case .interrupted: "Interrupted"
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
        case .failed(let message):
            message
        }
    }
}

@MainActor
protocol TravelBackgroundAudioControlling: AnyObject {
    var status: TravelBackgroundAudioStatus { get }
    var onStatusChange: ((TravelBackgroundAudioStatus) -> Void)? { get set }

    func start() throws
    func stop() throws
    func reconcile()
}

enum TravelBackgroundAudioError: LocalizedError {
    case engineDidNotStart

    var errorDescription: String? {
        switch self {
        case .engineDidNotStart:
            "The background audio engine did not start."
        }
    }
}

@MainActor
final class TravelBackgroundAudioController: NSObject, TravelBackgroundAudioControlling {
    private static let sampleRate = 44_100.0
    private static let channelCount: AVAudioChannelCount = 2

    private let session: AVAudioSession
    private let notificationCenter: NotificationCenter
    private let logger = Logger(subsystem: "io.zxf.flowsplice.travel", category: "background-audio")

    private var engine: AVAudioEngine?
    private var sourceNode: AVAudioSourceNode?
    private var shouldRun = false
    private var recoveryTask: Task<Void, Never>?
    private var recoveryAttempt = 0

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
        self.session = session
        self.notificationCenter = notificationCenter
        super.init()
        registerForNotifications()
    }

    deinit {
        notificationCenter.removeObserver(self)
        recoveryTask?.cancel()
    }

    func start() throws {
        shouldRun = true
        recoveryTask?.cancel()
        recoveryTask = nil
        recoveryAttempt = 0

        if engine?.isRunning == true {
            status = .active
            logger.debug("Background audio start was already satisfied")
            return
        }

        status = .starting
        do {
            try activateAndStart(reason: "user-runtime-start")
        } catch {
            shouldRun = false
            tearDownGraph()
            try? deactivateSession()
            let message = "Background audio could not start: \(error.localizedDescription)"
            status = .failed(message)
            logger.error("\(message, privacy: .public)")
            throw error
        }
    }

    func stop() throws {
        shouldRun = false
        recoveryAttempt = 0
        recoveryTask?.cancel()
        recoveryTask = nil
        tearDownGraph()

        do {
            try deactivateSession()
            status = .inactive
            logger.notice("Background audio stopped and the session was deactivated")
        } catch {
            let message = "Background audio stopped, but the audio session could not deactivate: \(error.localizedDescription)"
            status = .failed(message)
            logger.error("\(message, privacy: .public)")
            throw error
        }
    }

    func reconcile() {
        guard shouldRun else { return }
        guard engine?.isRunning != true else {
            if status != .active { status = .active }
            return
        }
        scheduleRecovery(reason: "runtime-reconciliation", delay: .zero)
    }

    private func activateAndStart(reason: String) throws {
        tearDownGraph()

        try session.setCategory(.playback, mode: .default, options: [.mixWithOthers])
        try session.setActive(true)

        guard let format = AVAudioFormat(
            standardFormatWithSampleRate: Self.sampleRate,
            channels: Self.channelCount
        ) else {
            throw TravelBackgroundAudioError.engineDidNotStart
        }

        let nextEngine = AVAudioEngine()
        let nextSource = AVAudioSourceNode(format: format) { _, _, _, audioBufferList -> OSStatus in
            for buffer in UnsafeMutableAudioBufferListPointer(audioBufferList) {
                guard let data = buffer.mData else { continue }
                data.initializeMemory(
                    as: UInt8.self,
                    repeating: 0,
                    count: Int(buffer.mDataByteSize)
                )
            }
            return 0
        }

        nextEngine.attach(nextSource)
        nextEngine.connect(nextSource, to: nextEngine.mainMixerNode, format: format)
        nextEngine.mainMixerNode.outputVolume = 1
        nextEngine.prepare()
        try nextEngine.start()
        guard nextEngine.isRunning else {
            nextEngine.stop()
            nextEngine.detach(nextSource)
            throw TravelBackgroundAudioError.engineDidNotStart
        }

        engine = nextEngine
        sourceNode = nextSource
        recoveryAttempt = 0
        status = .active
        let route = session.currentRoute.outputs.map(\.portType.rawValue).joined(separator: ",")
        logger.notice("Background audio active reason=\(reason, privacy: .public) sample_rate=\(self.session.sampleRate, format: .fixed(precision: 0)) channels=\(self.session.outputNumberOfChannels) mixable=true route=\(route, privacy: .public)")
    }

    private func tearDownGraph() {
        guard let engine else {
            sourceNode = nil
            return
        }
        engine.stop()
        if let sourceNode {
            engine.disconnectNodeOutput(sourceNode)
            engine.detach(sourceNode)
        }
        engine.reset()
        self.sourceNode = nil
        self.engine = nil
    }

    private func deactivateSession() throws {
        try session.setActive(false, options: [.notifyOthersOnDeactivation])
    }

    private func scheduleRecovery(reason: String, delay: Duration? = nil) {
        guard shouldRun else { return }
        recoveryTask?.cancel()
        recoveryAttempt += 1
        let attempt = recoveryAttempt
        let retryDelay = delay ?? .milliseconds(min(8_000, 250 * (1 << min(attempt - 1, 5))))
        logger.notice(
            "Background audio recovery scheduled reason=\(reason, privacy: .public) attempt=\(attempt)"
        )
        recoveryTask = Task { @MainActor [weak self] in
            do {
                try await Task.sleep(for: retryDelay)
            } catch {
                return
            }
            guard let self, self.shouldRun else { return }
            do {
                self.status = .starting
                try self.activateAndStart(reason: reason)
                self.recoveryTask = nil
            } catch {
                let message = "Background audio recovery failed: \(error.localizedDescription)"
                self.status = .failed(message)
                self.logger.error("\(message, privacy: .public)")
                self.scheduleRecovery(reason: reason)
            }
        }
    }

    private func registerForNotifications() {
        notificationCenter.addObserver(
            self,
            selector: #selector(receiveInterruption(_:)),
            name: AVAudioSession.interruptionNotification,
            object: session
        )
        notificationCenter.addObserver(
            self,
            selector: #selector(receiveRouteChange(_:)),
            name: AVAudioSession.routeChangeNotification,
            object: session
        )
        notificationCenter.addObserver(
            self,
            selector: #selector(receiveMediaServicesReset(_:)),
            name: AVAudioSession.mediaServicesWereResetNotification,
            object: session
        )
        notificationCenter.addObserver(
            self,
            selector: #selector(receiveEngineConfigurationChange(_:)),
            name: .AVAudioEngineConfigurationChange,
            object: nil
        )
    }

    @objc nonisolated private func receiveInterruption(_ notification: Notification) {
        let typeValue = notification.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt
        let optionValue = notification.userInfo?[AVAudioSessionInterruptionOptionKey] as? UInt ?? 0
        Task { @MainActor [weak self] in
            self?.handleInterruption(typeValue: typeValue, optionValue: optionValue)
        }
    }

    @objc nonisolated private func receiveRouteChange(_ notification: Notification) {
        let reasonValue = notification.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt
        Task { @MainActor [weak self] in
            self?.handleRouteChange(reasonValue: reasonValue)
        }
    }

    @objc nonisolated private func receiveMediaServicesReset(_ notification: Notification) {
        Task { @MainActor [weak self] in
            self?.handleMediaServicesReset()
        }
    }

    @objc nonisolated private func receiveEngineConfigurationChange(_ notification: Notification) {
        Task { @MainActor [weak self] in
            self?.handleEngineConfigurationChange()
        }
    }

    private func handleInterruption(typeValue: UInt?, optionValue: UInt) {
        guard let typeValue, let type = AVAudioSession.InterruptionType(rawValue: typeValue) else {
            return
        }
        switch type {
        case .began:
            guard shouldRun else { return }
            engine?.pause()
            status = .interrupted
            logger.notice("Background audio interruption began")
        case .ended:
            guard shouldRun else { return }
            let options = AVAudioSession.InterruptionOptions(rawValue: optionValue)
            logger.notice("Background audio interruption ended should_resume=\(options.contains(.shouldResume))")
            if options.contains(.shouldResume) {
                scheduleRecovery(reason: "interruption-ended", delay: .zero)
            } else {
                status = .interrupted
            }
        @unknown default:
            guard shouldRun else { return }
            status = .interrupted
            logger.error("Unknown background audio interruption type")
        }
    }

    private func handleRouteChange(reasonValue: UInt?) {
        let reason = reasonValue.flatMap(AVAudioSession.RouteChangeReason.init(rawValue:))
        logger.notice("Background audio route changed reason=\(String(describing: reason), privacy: .public)")
        guard shouldRun, engine?.isRunning != true else { return }
        scheduleRecovery(reason: "route-change", delay: .zero)
    }

    private func handleMediaServicesReset() {
        logger.notice("Audio media services reset")
        guard shouldRun else { return }
        tearDownGraph()
        scheduleRecovery(reason: "media-services-reset", delay: .zero)
    }

    private func handleEngineConfigurationChange() {
        guard shouldRun, engine?.isRunning != true else { return }
        logger.notice("Background audio engine configuration changed while stopped")
        scheduleRecovery(reason: "engine-configuration-change", delay: .zero)
    }
}
