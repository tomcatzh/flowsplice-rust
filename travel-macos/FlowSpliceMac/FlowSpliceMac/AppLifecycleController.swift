import AppKit
import Combine
import SwiftUI

@MainActor
final class AppLifecycleController: NSObject, NSApplicationDelegate, ObservableObject {
    @Published private(set) var isMenuBarOnly = false

    private weak var store: TravelStore?
    private var openMainWindow: (() -> Void)?
    private var trueQuitAuthorized = false
    private var systemTerminationExpected = false

    func attach(store: TravelStore) {
        self.store = store
    }

    func registerMainWindowOpener(_ action: @escaping () -> Void) {
        openMainWindow = action
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSWorkspace.shared.notificationCenter.addObserver(
            self,
            selector: #selector(systemWillPowerOff),
            name: NSWorkspace.willPowerOffNotification,
            object: nil
        )
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        DispatchQueue.main.async { [weak self] in
            self?.hideToMenuBar()
        }
        return false
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        showMainWindow()
        return true
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        #if DEBUG
        if ProcessInfo.processInfo.environment["FLOWSPLICE_UI_TESTING"] == "1",
           ProcessInfo.processInfo.arguments.contains("--ui-test-allow-termination") {
            return .terminateNow
        }
        #endif
        if trueQuitAuthorized || systemTerminationExpected {
            return .terminateNow
        }
        hideToMenuBar()
        return .terminateCancel
    }

    func hideToMenuBar() {
        for window in NSApp.windows where window.canBecomeMain {
            window.orderOut(nil)
        }
        NSApp.setActivationPolicy(.accessory)
        isMenuBarOnly = true
    }

    func showMainWindow() {
        NSApp.setActivationPolicy(.regular)
        isMenuBarOnly = false
        NSApp.activate()
        if let window = NSApp.windows.first(where: { $0.canBecomeMain }) {
            window.makeKeyAndOrderFront(nil)
        } else {
            openMainWindow?()
            DispatchQueue.main.async {
                NSApp.windows.first(where: { $0.canBecomeMain })?.makeKeyAndOrderFront(nil)
            }
        }
    }

    func requestTrueQuit() {
        guard let store else {
            trueQuitAuthorized = true
            NSApp.terminate(nil)
            return
        }
        let activeFlows = store.snapshot.activeFlows
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "Quit FlowSplice?"
        if activeFlows > 0 {
            alert.informativeText = "This will disconnect \(store.snapshot.activeFlows) active Flow\(store.snapshot.activeFlows == 1 ? "" : "s"). Closing the window or using Dock Quit keeps Travel running."
            alert.addButton(withTitle: "Quit and Disconnect")
        } else {
            alert.informativeText = "This fully exits FlowSplice. Closing the window, pressing Command-Q, or using Dock Quit only hides it."
            alert.addButton(withTitle: "Quit FlowSplice")
        }
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        Task {
            await store.prepareForTermination()
            trueQuitAuthorized = true
            NSApp.terminate(nil)
        }
    }

    @objc private func systemWillPowerOff() {
        systemTerminationExpected = true
    }
}
