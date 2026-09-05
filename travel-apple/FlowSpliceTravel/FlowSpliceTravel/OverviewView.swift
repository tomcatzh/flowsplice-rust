import SwiftUI

struct OverviewView: View {
    @EnvironmentObject private var store: TravelStore

    var body: some View {
        ScrollView {
            VStack(spacing: 16) {
                ConnectionStatusCard()
                if let error = store.snapshot.error {
                    AttentionBanner(message: error)
                }
                metrics
                NetworkRecoveryCard()
                if !store.snapshot.mappings.isEmpty {
                    RecentMappingsCard()
                }
            }
            .frame(maxWidth: 840)
            .padding()
            .frame(maxWidth: .infinity)
        }
        .background(Color(uiColor: .systemGroupedBackground))
        .navigationTitle("Overview")
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Image("FlowSpliceMark")
                    .resizable()
                    .frame(width: 28, height: 28)
                    .clipShape(.rect(cornerRadius: 7))
                    .accessibilityLabel("FlowSplice")
            }
        }
    }

    private var metrics: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 132), spacing: 12)], spacing: 12) {
            MetricCell(title: "Upload", value: TravelFormatting.bytes(store.snapshot.uploadedBytes), symbol: "arrow.up")
            MetricCell(title: "Download", value: TravelFormatting.bytes(store.snapshot.downloadedBytes), symbol: "arrow.down")
            MetricCell(title: "Active Flows", value: "\(store.snapshot.activeFlows)", symbol: "point.3.connected.trianglepath.dotted")
            MetricCell(title: "Relays", value: "\(store.snapshot.relayCount)", symbol: "antenna.radiowaves.left.and.right")
        }
        .accessibilityIdentifier("overview-metrics")
    }
}
private struct ConnectionStatusCard: View {
    @EnvironmentObject private var store: TravelStore
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        VStack(spacing: 18) {
            HStack(alignment: .top, spacing: 14) {
                ZStack {
                    Circle()
                        .fill(statusColor.opacity(0.16))
                    Image(systemName: statusSymbol)
                        .font(.title2.weight(.bold))
                        .foregroundStyle(statusColor)
                }
                .frame(width: 54, height: 54)

                VStack(alignment: .leading, spacing: 5) {
                    Text(store.snapshot.travelID)
                        .font(.title2.weight(.semibold))
                    Label(statusTitle, systemImage: "circle.fill")
                        .font(.subheadline.weight(.medium))
                        .foregroundStyle(statusColor)
                        .accessibilityIdentifier("overview-status")
                    Text("\(store.interfaceLabel) · \(store.snapshot.relayCount) active Relay")
                        .font(.caption)
                        .foregroundStyle(colorScheme == .dark ? Color.white.opacity(0.62) : .secondary)
                }
                Spacer()
                actionButton
            }

            if store.snapshot.phase == .running {
                Divider().overlay(colorScheme == .dark ? Color.white.opacity(0.15) : Color.secondary.opacity(0.18))
                HStack {
                    Text("Uptime")
                        .foregroundStyle(colorScheme == .dark ? Color.white.opacity(0.66) : .secondary)
                    Spacer()
                    Text(TravelFormatting.duration(store.snapshot.uptimeSeconds))
                        .font(.body.monospacedDigit().weight(.semibold))
                }
            }
        }
        .padding(20)
        .foregroundStyle(colorScheme == .dark ? .white : Color.primary)
        .background(cardBackground, in: .rect(cornerRadius: 24))
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("connection-status-card")
    }

    @ViewBuilder
    private var actionButton: some View {
        if store.canStop {
            Button("Stop") { store.stop() }
                .buttonStyle(.bordered)
                .tint(colorScheme == .dark ? Color("FlowMint") : Color("AccentColor"))
                .accessibilityIdentifier("travel-stop")
        } else {
            Button("Start") { store.start() }
                .buttonStyle(.borderedProminent)
                .disabled(store.isWorking)
                .accessibilityIdentifier("travel-start")
        }
    }

    private var cardBackground: Color {
        colorScheme == .dark ? Color("DeepInk") : Color(uiColor: .secondarySystemGroupedBackground)
    }

    private var statusColor: Color {
        if store.snapshot.phase == .error { return .red }
        if store.snapshot.phase == .starting || store.snapshot.phase == .stopping { return .orange }
        if store.snapshot.phase == .running && store.snapshot.online {
            return colorScheme == .dark ? Color("FlowMint") : .green
        }
        return .secondary
    }

    private var statusTitle: String {
        switch store.snapshot.phase {
        case .running: store.snapshot.online ? "Online" : "Reconnecting"
        case .starting: "Starting"
        case .stopping: "Stopping"
        case .error: "Needs attention"
        case .stopped: "Stopped"
        }
    }

    private var statusSymbol: String {
        switch store.snapshot.phase {
        case .running: store.snapshot.online ? "checkmark" : "arrow.trianglehead.2.clockwise.rotate.90"
        case .starting, .stopping: "ellipsis"
        case .error: "exclamationmark"
        case .stopped: "pause.fill"
        }
    }
}

private struct MetricCell: View {
    let title: String
    let value: String
    let symbol: String

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(title, systemImage: symbol)
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(value)
                .font(.title3.weight(.semibold).monospacedDigit())
                .lineLimit(1)
                .minimumScaleFactor(0.75)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(16)
        .background(Color(uiColor: .secondarySystemGroupedBackground), in: .rect(cornerRadius: 18))
    }
}

private struct NetworkRecoveryCard: View {
    @EnvironmentObject private var store: TravelStore

    var body: some View {
        HStack(spacing: 14) {
            Image(systemName: store.networkAvailable ? "network" : "network.slash")
                .font(.title2)
                .foregroundStyle(store.networkAvailable ? Color("AccentColor") : .orange)
                .frame(width: 38)
            VStack(alignment: .leading, spacing: 3) {
                Text(store.networkAvailable ? "Network available" : "Waiting for network")
                    .font(.headline)
                Text("Path: \(store.interfaceLabel). FlowSplice retires old connections and restores them on the current route.")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(16)
        .background(.regularMaterial, in: .rect(cornerRadius: 18))
        .accessibilityIdentifier("network-recovery-card")
    }
}

private struct RecentMappingsCard: View {
    @EnvironmentObject private var store: TravelStore

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Local Mappings").font(.headline)
                Spacer()
                Button("View All") { store.selectedSection = .mappings }
            }
            ForEach(store.snapshot.mappings.prefix(3)) { mapping in
                Divider()
                HStack {
                    Text(mapping.protocol.uppercased())
                        .font(.caption2.monospaced().weight(.bold))
                        .foregroundStyle(Color("AccentColor"))
                    VStack(alignment: .leading) {
                        Text(mapping.serviceID).fontWeight(.medium)
                        Text("\(mapping.homeID) · \(mapping.bind)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                }
            }
        }
        .padding(16)
        .background(Color(uiColor: .secondarySystemGroupedBackground), in: .rect(cornerRadius: 18))
    }
}

struct AttentionBanner: View {
    let message: String

    var body: some View {
        Label {
            VStack(alignment: .leading, spacing: 3) {
                Text("Needs attention").fontWeight(.semibold)
                Text(message).font(.subheadline)
            }
        } icon: {
            Image(systemName: "exclamationmark.triangle.fill")
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(16)
        .foregroundStyle(.red)
        .background(Color.red.opacity(0.12), in: .rect(cornerRadius: 16))
    }
}
