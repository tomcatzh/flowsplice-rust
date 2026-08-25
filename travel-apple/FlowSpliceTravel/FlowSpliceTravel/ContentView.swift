import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var store: TravelStore
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass

    var body: some View {
        Group {
            if !store.snapshot.enrolled {
                NavigationStack {
                    EnrollmentView()
                        .navigationTitle("FlowSplice Travel")
                        .navigationBarTitleDisplayMode(.inline)
                        .toolbar { brandToolbar }
                }
            } else if horizontalSizeClass == .regular {
                regularLayout
            } else {
                compactLayout
            }
        }
        .tint(Color("AccentColor"))
        .alert(
            "Needs Attention",
            isPresented: Binding(
                get: { store.presentedError != nil },
                set: { if !$0 { store.presentedError = nil } }
            )
        ) {
            Button("OK", role: .cancel) { store.presentedError = nil }
        } message: {
            Text(store.presentedError ?? "Unknown error")
        }
    }

    private var compactLayout: some View {
        TabView(selection: selectedSection) {
            NavigationStack { OverviewView() }
                .tabItem { Label("Overview", systemImage: TravelStore.Section.overview.symbol) }
                .tag(TravelStore.Section.overview)
            NavigationStack { MappingsView() }
                .tabItem { Label("Mappings", systemImage: TravelStore.Section.mappings.symbol) }
                .tag(TravelStore.Section.mappings)
            NavigationStack { DiagnosticsView() }
                .tabItem { Label("Diagnostics", systemImage: TravelStore.Section.diagnostics.symbol) }
                .tag(TravelStore.Section.diagnostics)
            NavigationStack { DeviceView() }
                .tabItem { Label("Device", systemImage: TravelStore.Section.device.symbol) }
                .tag(TravelStore.Section.device)
        }
    }

    private var regularLayout: some View {
        NavigationSplitView {
            List(TravelStore.Section.allCases, selection: $store.selectedSection) { section in
                Label(section.title, systemImage: section.symbol)
                    .tag(section)
                    .accessibilityIdentifier("nav-\(section.rawValue)")
            }
            .navigationTitle("FlowSplice")
            .safeAreaInset(edge: .bottom) {
                SidebarConnectionSummary()
                    .padding(.horizontal)
                    .padding(.bottom, 8)
            }
        } detail: {
            NavigationStack {
                switch store.selectedSection ?? .overview {
                case .overview: OverviewView()
                case .mappings: MappingsView()
                case .diagnostics: DiagnosticsView()
                case .device: DeviceView()
                }
            }
        }
        .navigationSplitViewStyle(.balanced)
    }

    private var selectedSection: Binding<TravelStore.Section> {
        Binding(
            get: { store.selectedSection ?? .overview },
            set: { store.selectedSection = $0 }
        )
    }

    @ToolbarContentBuilder
    private var brandToolbar: some ToolbarContent {
        ToolbarItem(placement: .topBarTrailing) {
            Image("FlowSpliceMark")
                .resizable()
                .frame(width: 28, height: 28)
                .clipShape(.rect(cornerRadius: 7))
                .accessibilityLabel("FlowSplice")
        }
    }
}

private struct SidebarConnectionSummary: View {
    @EnvironmentObject private var store: TravelStore

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label(store.snapshot.online ? "Travel online" : "Travel offline", systemImage: "circle.fill")
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(store.snapshot.online ? Color.green : .secondary)
            Text("\(store.interfaceLabel) · \(store.snapshot.relayCount) Relay")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
        .background(.thinMaterial, in: .rect(cornerRadius: 14))
        .accessibilityIdentifier("sidebar-connection-summary")
    }
}

#Preview {
    ContentView()
        .environmentObject(TravelStore())
}
