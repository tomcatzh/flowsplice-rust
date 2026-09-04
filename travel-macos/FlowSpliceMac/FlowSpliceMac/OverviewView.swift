import SwiftUI

struct OverviewView: View {
    @EnvironmentObject private var store: TravelStore

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                hero

                LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: 12), count: 4), spacing: 12) {
                    MetricTile(
                        title: "Active Flows",
                        value: "\(store.snapshot.activeFlows)",
                        detail: "live TCP and UDP",
                        symbol: "arrow.left.arrow.right"
                    )
                    MetricTile(
                        title: "Relays in use",
                        value: "\(store.snapshot.activeRelays.count)",
                        detail: "current carriers",
                        symbol: "point.3.connected.trianglepath.dotted"
                    )
                    MetricTile(
                        title: "Session traffic",
                        value: TravelFormatting.bytes(store.snapshot.uploadedBytes + store.snapshot.downloadedBytes),
                        detail: "up + down",
                        symbol: "chart.line.uptrend.xyaxis"
                    )
                    MetricTile(
                        title: "Uptime",
                        value: TravelFormatting.duration(store.snapshot.uptimeSeconds),
                        detail: store.interfaceLabel,
                        symbol: "clock"
                    )
                }

                HStack(alignment: .top, spacing: 14) {
                    FlowPanel("Current routes", subtitle: "Exact Relay selection for every active Flow") {
                        if store.diagnostics.flows.isEmpty {
                            EmptyPanel(symbol: "arrow.triangle.branch", title: "No active routes", detail: "Routes appear when a mapped service carries traffic.")
                        } else {
                            VStack(spacing: 0) {
                                ForEach(store.diagnostics.flows.prefix(5)) { flow in
                                    Button {
                                        store.selectedFlowID = flow.id
                                        store.select(.diagnostics)
                                    } label: {
                                        HStack(spacing: 10) {
                                            Image(systemName: flow.recovering ? "arrow.trianglehead.2.clockwise.rotate.90" : "arrow.right")
                                                .foregroundStyle(flow.recovering ? .orange : Color("FlowMint"))
                                            VStack(alignment: .leading, spacing: 3) {
                                                Text("\(flow.serviceID) · \(flow.protocol.uppercased())")
                                                    .font(.callout.weight(.semibold))
                                                Text("\(flow.localBind) → \(flow.selectedRelay ?? "Selecting Relay…") → \(flow.homeID)")
                                                    .font(.caption.monospaced())
                                                    .foregroundStyle(.secondary)
                                                    .lineLimit(1)
                                            }
                                            Spacer()
                                            Text(TravelFormatting.bytes(flow.uploadedBytes + flow.downloadedBytes))
                                                .font(.caption.monospacedDigit())
                                                .foregroundStyle(.secondary)
                                        }
                                        .padding(.vertical, 9)
                                        .contentShape(.rect)
                                    }
                                    .buttonStyle(.plain)
                                    if flow.id != store.diagnostics.flows.prefix(5).last?.id { Divider() }
                                }
                            }
                        }
                    }

                    FlowPanel("Recent activity", subtitle: "Lifecycle and connectivity changes") {
                        if store.activities.isEmpty {
                            EmptyPanel(symbol: "clock.arrow.circlepath", title: "No recent activity", detail: "Start Travel or open a mapped service to see events.")
                        } else {
                            VStack(spacing: 0) {
                                ForEach(store.activities.prefix(5)) { item in
                                    HStack(alignment: .top, spacing: 10) {
                                        Image(systemName: item.symbol)
                                            .foregroundStyle(item.isError ? .red : Color("FlowMint"))
                                            .frame(width: 20)
                                        VStack(alignment: .leading, spacing: 2) {
                                            HStack {
                                                Text(item.title).font(.callout.weight(.medium))
                                                Spacer()
                                                Text(item.date, style: .time).font(.caption2).foregroundStyle(.tertiary)
                                            }
                                            Text(item.detail).font(.caption).foregroundStyle(.secondary).lineLimit(2)
                                        }
                                    }
                                    .padding(.vertical, 8)
                                    if item.id != store.activities.prefix(5).last?.id { Divider() }
                                }
                            }
                        }
                    }
                }
            }
            .padding(24)
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var hero: some View {
        HStack(spacing: 18) {
            ZStack {
                RoundedRectangle(cornerRadius: 18).fill(store.stateTint.opacity(0.13))
                Image(systemName: store.stateSymbol)
                    .font(.system(size: 34, weight: .semibold))
                    .foregroundStyle(store.stateTint)
            }
            .frame(width: 68, height: 68)

            VStack(alignment: .leading, spacing: 5) {
                HStack(spacing: 9) {
                    Text(store.stateTitle).font(.largeTitle.weight(.semibold))
                    StatusPill(
                        title: store.networkAvailable ? store.interfaceLabel : "Network unavailable",
                        symbol: store.networkAvailable ? "network" : "network.slash",
                        tint: store.networkAvailable ? Color("FlowMint") : .orange
                    )
                }
                Text(store.relaySummary)
                    .font(.body)
                    .foregroundStyle(.secondary)
                Text("Closing the window or choosing Quit in the Dock keeps Travel running in the menu bar.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
            Spacer()
        }
        .padding(20)
        .background(
            LinearGradient(colors: [Color("DeepInk").opacity(0.96), Color("DeepInk").opacity(0.82)], startPoint: .topLeading, endPoint: .bottomTrailing),
            in: .rect(cornerRadius: 18)
        )
        .foregroundStyle(.white)
    }
}
