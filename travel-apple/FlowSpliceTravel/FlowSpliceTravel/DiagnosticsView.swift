import SwiftUI

struct DiagnosticsView: View {
    @EnvironmentObject private var store: TravelStore

    private var e2eEnabled: Bool {
        ProcessInfo.processInfo.environment["FLOWSPLICE_E2E"] == "1"
    }

    var body: some View {
        List {
            Section("Current Path") {
                LabeledContent {
                    Label(store.interfaceLabel, systemImage: store.networkAvailable ? "network" : "network.slash")
                        .foregroundStyle(store.networkAvailable ? Color.primary : .orange)
                } label: {
                    Text("Interface")
                }
                LabeledContent("Runtime", value: runtimeLabel)
                LabeledContent("Catalog Generation", value: "\(store.snapshot.catalogGeneration)")
                LabeledContent("Active Relays", value: "\(store.snapshot.relayCount)")
            }

            if e2eEnabled {
                Section {
                    Button {
                        store.prepareE2EPhase("network-outage-ready")
                    } label: {
                        Label("Prepare Relay Outage", systemImage: "bolt.horizontal.circle")
                    }
                    .accessibilityIdentifier("diagnostics-prepare-network-outage")

                    Button {
                        store.simulateNetworkChangeForTesting()
                    } label: {
                        Label("Simulate Network Change", systemImage: "arrow.trianglehead.2.clockwise.rotate.90")
                    }
                    .disabled(store.snapshot.phase != .running)
                    .accessibilityIdentifier("diagnostics-simulate-network-change")

                    Button {
                        store.prepareE2EPhase("screen-off-ready")
                    } label: {
                        Label("Prepare Screen-Off Recovery", systemImage: "lock.circle")
                    }
                    .accessibilityIdentifier("diagnostics-prepare-screen-off")

                    Button {
                        store.prepareE2EPhase("live-activity-stop-ready")
                    } label: {
                        Label("Prepare Background Stop", systemImage: "stop.circle")
                    }
                    .accessibilityIdentifier("diagnostics-prepare-live-activity-stop")
                } header: {
                    Text("Simulator Validation")
                } footer: {
                    Text("Available only in the automated E2E environment.")
                }
            }

            Section("Recovery Timeline") {
                if store.recoveryEvents.isEmpty {
                    ContentUnavailableView {
                        Label("No Recovery Events", systemImage: "waveform.path.ecg")
                    } description: {
                        Text("Lifecycle, catalog, network-path, and error events appear here.")
                    }
                    .listRowBackground(Color.clear)
                } else {
                    ForEach(store.recoveryEvents) { event in
                        RecoveryEventRow(event: event)
                    }
                }
            }
        }
        .navigationTitle("Diagnostics")
        .accessibilityIdentifier("diagnostics-list")
    }

    private var runtimeLabel: String {
        switch store.snapshot.phase {
        case .stopped: "Stopped"
        case .starting: "Starting"
        case .running: store.snapshot.online ? "Online" : "Reconnecting"
        case .stopping: "Stopping"
        case .error: "Needs attention"
        }
    }
}

private struct RecoveryEventRow: View {
    let event: RecoveryEvent

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: symbol)
                .foregroundStyle(color)
                .frame(width: 24)
                .padding(.top, 2)
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Text(event.title)
                        .font(.body.weight(.medium))
                    Spacer()
                    Text(event.date, style: .time)
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
                Text(event.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 3)
        .accessibilityElement(children: .combine)
    }

    private var symbol: String {
        switch event.kind {
        case .network: "network"
        case .lifecycle: "app.badge.checkmark"
        case .catalog: "list.bullet.rectangle"
        case .error: "exclamationmark.triangle.fill"
        }
    }

    private var color: Color {
        switch event.kind {
        case .network: Color("AccentColor")
        case .lifecycle: .green
        case .catalog: .blue
        case .error: .red
        }
    }
}
