import SwiftUI

@main
struct FlowSpliceTravelApp: App {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var store = TravelStore()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(store)
                .task { store.bootstrap() }
                .onChange(of: scenePhase) { _, next in
                    store.handleScenePhase(next)
                }
        }
    }
}
