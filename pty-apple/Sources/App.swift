import SwiftUI

@main
struct FlowSplicePTYApp: App {
    @StateObject private var host = TerminalHost()
    @Environment(\.scenePhase) private var phase
    var body: some Scene {
#if os(macOS)
        Window("FlowSplice PTY", id: "terminal") {
            TerminalView(host: host).frame(minWidth: 640, minHeight: 480)
        }
        .defaultSize(width: 1000, height: 850)
#else
        WindowGroup {
            TerminalView(host: host)
                .onAppear { host.setVisible(phase != .background) }
                .onChange(of: phase) { _, value in
                    if value == .inactive { host.prepareForInactive() }
                    else if value == .active { host.becameActive() }
                    else { host.setVisible(false) }
                }
        }
#endif
    }
}

#if os(macOS)
struct TerminalView: NSViewRepresentable {
    let host: TerminalHost
    func makeNSView(context: Context) -> WKWebView { host.web }
    func updateNSView(_ view: WKWebView, context: Context) {}
}
#else
struct TerminalView: UIViewRepresentable {
    let host: TerminalHost
    func makeUIView(context: Context) -> WKWebView { host.web }
    func updateUIView(_ view: WKWebView, context: Context) {}
}
#endif
import WebKit
