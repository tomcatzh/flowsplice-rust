//
//  FlowSpliceMacApp.swift
//  FlowSpliceMac
//
//  Created by Tomcat on 2026/8/31.
//

import SwiftUI

@main
struct FlowSpliceMacApp: App {
    @NSApplicationDelegateAdaptor(AppLifecycleController.self) private var lifecycle
    @StateObject private var store = TravelStore()

    var body: some Scene {
        Window("FlowSplice", id: "main") {
            ContentView()
                .environmentObject(store)
                .environmentObject(lifecycle)
                .frame(minWidth: 920, minHeight: 620)
                .background {
                    MainWindowOpenerRegistration()
                        .environmentObject(lifecycle)
                }
                .task {
                    lifecycle.attach(store: store)
                    store.bootstrap()
                }
        }
        .defaultSize(width: 1_180, height: 760)
        .commands {
            CommandGroup(replacing: .appTermination) {
                Button("Hide FlowSplice to Menu Bar") {
                    lifecycle.hideToMenuBar()
                }
                .keyboardShortcut("q")
            }
            CommandMenu("Travel") {
                Button(store.snapshot.phase == .running ? "Stop Travel" : "Start Travel") {
                    store.snapshot.phase == .running ? store.stop() : store.start()
                }
                .disabled(!store.snapshot.enrolled || store.isWorking)
                Divider()
                Button("Open Diagnostics") {
                    store.select(.diagnostics)
                    lifecycle.showMainWindow()
                }
                .keyboardShortcut("d", modifiers: [.command, .shift])
            }
        }

        MenuBarExtra(isInserted: .constant(true)) {
            MenuBarView()
                .environmentObject(store)
                .environmentObject(lifecycle)
        } label: {
            Image("FlowSpliceMenuBar")
                .renderingMode(.template)
                .resizable()
                .scaledToFit()
                .frame(width: 18, height: 18)
                .accessibilityLabel("FlowSplice: \(store.stateTitle)")
        }
        .menuBarExtraStyle(.window)
    }
}

private struct MainWindowOpenerRegistration: View {
    @Environment(\.openWindow) private var openWindow
    @EnvironmentObject private var lifecycle: AppLifecycleController

    var body: some View {
        Color.clear
            .frame(width: 0, height: 0)
            .onAppear {
                lifecycle.registerMainWindowOpener {
                    openWindow(id: "main")
                }
            }
    }
}
