import SwiftUI

struct DiagnosticsView: View {
    @EnvironmentObject private var store: TravelStore

    private var selectedFlow: FlowRouteSnapshot? {
        store.diagnostics.flows.first { $0.id == store.selectedFlowID } ?? store.diagnostics.flows.first
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                HStack(alignment: .firstTextBaseline) {
                    VStack(alignment: .leading, spacing: 3) {
                        Text("Route diagnostics").font(.title2.weight(.semibold))
                        Text("Observed routes and Relay eligibility from the running Travel Core.")
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Text("Updated \(TravelFormatting.date(store.diagnostics.generatedAtUnixSeconds == 0 ? nil : store.diagnostics.generatedAtUnixSeconds))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                FlowPanel("Selected Flow", subtitle: "The route this Flow is using now") {
                    if let flow = selectedFlow {
                        RoutePathView(flow: flow)
                        Divider()
                        HStack(spacing: 24) {
                            RouteFact(label: "Protocol", value: flow.protocol.uppercased())
                            RouteFact(label: "Local bind", value: flow.localBind)
                            RouteFact(label: "Switches", value: "\(flow.switchCount)")
                            RouteFact(label: "Traffic", value: TravelFormatting.bytes(flow.uploadedBytes + flow.downloadedBytes))
                            RouteFact(label: "Started", value: TravelFormatting.date(flow.startedAtUnixSeconds))
                        }
                    } else {
                        EmptyPanel(symbol: "arrow.triangle.branch", title: "No active Flow", detail: "Open a mapped service to observe its exact route.")
                    }
                }

                HStack(alignment: .top, spacing: 14) {
                    FlowPanel("Active Flows", subtitle: "Choose one to inspect its route") {
                        if store.diagnostics.flows.isEmpty {
                            EmptyPanel(symbol: "arrow.left.arrow.right", title: "No active Flows", detail: "The route list is populated by live traffic.")
                        } else {
                            VStack(spacing: 0) {
                                ForEach(store.diagnostics.flows) { flow in
                                    Button {
                                        store.selectedFlowID = flow.id
                                    } label: {
                                        HStack(spacing: 10) {
                                            VStack(alignment: .leading, spacing: 2) {
                                                Text("\(flow.serviceID) · \(flow.protocol.uppercased())")
                                                    .font(.callout.weight(.semibold))
                                                Text("via \(flow.selectedRelay ?? "selecting")")
                                                    .font(.caption.monospaced())
                                                    .foregroundStyle(.secondary)
                                            }
                                            Spacer()
                                            if flow.recovering {
                                                StatusPill(title: "Recovering", symbol: "arrow.clockwise", tint: .orange)
                                            }
                                            Image(systemName: store.selectedFlowID == flow.id ? "checkmark.circle.fill" : "circle")
                                                .foregroundStyle(store.selectedFlowID == flow.id ? Color("FlowMint") : .secondary)
                                        }
                                        .padding(.vertical, 8)
                                        .contentShape(.rect)
                                    }
                                    .buttonStyle(.plain)
                                    .accessibilityIdentifier("flow-row-\(flow.id)")
                                    if flow.id != store.diagnostics.flows.last?.id { Divider() }
                                }
                            }
                        }
                    }

                    FlowPanel("Control plane", subtitle: "Catalog and Relay directory freshness") {
                        ControlPlaneView(snapshot: store.diagnostics.controlPlane)
                    }
                }

                FlowPanel("Available routes", subtitle: "Observed availability—not a synthetic health check") {
                    if store.diagnostics.relays.isEmpty {
                        EmptyPanel(symbol: "server.rack", title: "No Relay observations", detail: "Start Travel to load the signed Relay directory.")
                    } else {
                        Grid(alignment: .leading, horizontalSpacing: 20, verticalSpacing: 0) {
                            GridRow {
                                Text("Relay").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                                Text("Observed state").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                                Text("Endpoint").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                                Text("Flows").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                                Text("Last success").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                                Text("Failures").font(.caption.weight(.semibold)).foregroundStyle(.secondary)
                            }
                            Divider().gridCellColumns(6).padding(.vertical, 8)
                            ForEach(store.diagnostics.relays) { relay in
                                GridRow {
                                    Text(relay.relayID ?? "Bootstrap")
                                        .font(.callout.weight(.medium))
                                    StatusPill(title: relay.observation.title, symbol: relay.observation.symbol, tint: relay.observation.tint)
                                    Text(relay.redactedEndpoint).font(.caption.monospaced()).foregroundStyle(.secondary)
                                    Text("\(relay.activeFlowCount)").monospacedDigit()
                                    Text(TravelFormatting.date(relay.lastSuccessUnixSeconds)).font(.caption)
                                    Text("\(relay.consecutiveFailures)").monospacedDigit()
                                }
                                .padding(.vertical, 5)
                                .accessibilityIdentifier("relay-row-\(relay.relayID ?? "bootstrap")")
                            }
                        }
                    }
                }

                FlowPanel("Route event timeline", subtitle: "The latest 256 sanitized route decisions") {
                    if store.diagnostics.events.isEmpty {
                        EmptyPanel(symbol: "list.bullet.rectangle", title: "No route events", detail: "Connection and recovery decisions will appear here.")
                    } else {
                        VStack(spacing: 0) {
                            ForEach(store.diagnostics.events.prefix(20)) { event in
                                HStack(alignment: .top, spacing: 12) {
                                    Circle()
                                        .fill(event.outcome == "failed" ? Color.orange : Color("FlowMint"))
                                        .frame(width: 7, height: 7)
                                        .padding(.top, 6)
                                    VStack(alignment: .leading, spacing: 3) {
                                        HStack {
                                            Text("\(event.phase.replacingOccurrences(of: "_", with: " ").capitalized) · \(event.outcome.capitalized)")
                                                .font(.callout.weight(.medium))
                                            if let relay = event.relayID {
                                                Text(relay).font(.caption.monospaced()).foregroundStyle(.secondary)
                                            }
                                            Spacer()
                                            Text(TravelFormatting.date(event.timestampUnixSeconds)).font(.caption2).foregroundStyle(.tertiary)
                                        }
                                        HStack(spacing: 10) {
                                            if let flow = event.flowID { Text("Flow \(TravelFormatting.shortID(flow))") }
                                            if let latency = event.latencyMilliseconds { Text("\(latency) ms") }
                                            if let reason = event.reason { Text(reason) }
                                        }
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                    }
                                }
                                .padding(.vertical, 8)
                                if event.id != store.diagnostics.events.prefix(20).last?.id { Divider() }
                            }
                        }
                    }
                }
            }
            .padding(24)
        }
    }
}

private struct RoutePathView: View {
    let flow: FlowRouteSnapshot

    var body: some View {
        HStack(spacing: 12) {
            RouteNode(symbol: "macbook", title: "Travel", detail: flow.localBind, tint: Color("FlowMint"))
            connector
            RouteNode(
                symbol: flow.recovering ? "arrow.clockwise" : "server.rack",
                title: flow.selectedRelay ?? "Selecting Relay",
                detail: flow.recovering ? "Recovering" : "Current carrier",
                tint: flow.recovering ? .orange : .blue
            )
            connector
            RouteNode(symbol: "house", title: flow.homeID, detail: flow.serviceID, tint: .purple)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("Travel through \(flow.selectedRelay ?? "no Relay selected") to \(flow.homeID), service \(flow.serviceID)")
        .accessibilityIdentifier("route-path")
    }

    private var connector: some View {
        HStack(spacing: 4) {
            Rectangle().fill(.separator).frame(height: 1)
            Image(systemName: "chevron.right").font(.caption2).foregroundStyle(.secondary)
        }
        .frame(maxWidth: 62)
    }
}

private struct RouteNode: View {
    let symbol: String
    let title: String
    let detail: String
    let tint: Color

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: symbol).foregroundStyle(tint).frame(width: 28, height: 28).background(tint.opacity(0.12), in: .rect(cornerRadius: 8))
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.callout.weight(.semibold)).lineLimit(1)
                Text(detail).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct RouteFact: View {
    let label: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(label).font(.caption).foregroundStyle(.secondary)
            Text(value).font(.callout.monospacedDigit().weight(.medium)).lineLimit(1)
        }
    }
}

private struct ControlPlaneView: View {
    let snapshot: ControlPlaneSnapshot

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let reason = snapshot.degradedReason {
                StatusPill(title: "Degraded", symbol: "exclamationmark.triangle", tint: .orange)
                Text(reason).font(.caption).foregroundStyle(.secondary)
            } else {
                StatusPill(title: "Current", symbol: "checkmark.seal", tint: Color("FlowMint"))
            }
            LabeledContent("Catalog generation", value: "\(snapshot.catalogGeneration)")
            LabeledContent("Directory generation", value: "\(snapshot.relayDirectoryGeneration)")
            LabeledContent("Directory members", value: "\(snapshot.directorySize)")
            LabeledContent("Subscription Relay", value: snapshot.catalogSubscriptionRelay ?? "None")
            LabeledContent("Last accepted update", value: TravelFormatting.date(snapshot.lastAcceptedUnixSeconds))
        }
        .font(.callout)
    }
}
