import SwiftUI

struct MenuBarView: View {
    @EnvironmentObject private var store: TravelStore
    @EnvironmentObject private var lifecycle: AppLifecycleController
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 10) {
                    ZStack {
                        Circle().fill(store.stateTint.opacity(0.16))
                        Image("FlowSpliceMark")
                            .resizable()
                            .scaledToFit()
                            .clipShape(.rect(cornerRadius: 7))
                    }
                    .frame(width: 36, height: 36)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(store.stateTitle).font(.headline)
                        Text(store.snapshot.travelID).font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    if store.isWorking { ProgressView().controlSize(.small) }
                }
                if let error = store.snapshot.error {
                    Text(error)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                } else {
                    Text(store.relaySummary)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }
            .padding(14)

            Divider()

            HStack(spacing: 0) {
                MenuMetric(value: "\(store.snapshot.activeFlows)", label: "Flows")
                Divider().frame(height: 34)
                MenuMetric(value: "\(store.snapshot.activeRelays.count)", label: "Relays")
                Divider().frame(height: 34)
                MenuMetric(value: TravelFormatting.bytes(store.snapshot.uploadedBytes + store.snapshot.downloadedBytes), label: "Traffic")
            }
            .padding(.vertical, 10)

            Divider()

            VStack(spacing: 4) {
                MenuAction(title: "Open FlowSplice", symbol: "macwindow") {
                    openWindow(id: "main")
                    DispatchQueue.main.async { lifecycle.showMainWindow() }
                }
                MenuAction(
                    title: store.snapshot.phase == .running ? "Stop Travel" : "Start Travel",
                    symbol: store.snapshot.phase == .running ? "stop.fill" : "play.fill"
                ) {
                    store.snapshot.phase == .running ? store.stop() : store.start()
                }
                .disabled(!store.snapshot.enrolled || store.isWorking)
                MenuAction(title: "Open Diagnostics", symbol: "waveform.path.ecg.rectangle") {
                    store.select(.diagnostics)
                    openWindow(id: "main")
                    DispatchQueue.main.async { lifecycle.showMainWindow() }
                }
            }
            .padding(8)

            Divider()

            Toggle("Launch at Login", isOn: Binding(
                get: { store.launchAtLoginEnabled },
                set: { store.setLaunchAtLogin($0) }
            ))
            .toggleStyle(.switch)
            .controlSize(.small)
            .padding(.horizontal, 14)
            .padding(.vertical, 10)

            Divider()

            Button(role: .destructive) {
                lifecycle.requestTrueQuit()
            } label: {
                Label("Quit FlowSplice…", systemImage: "power")
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .contentShape(.rect)
            }
            .buttonStyle(.plain)
            .padding(14)
            .accessibilityIdentifier("menu-true-quit")
        }
        .frame(width: 320)
        .background(.regularMaterial)
    }
}

private struct MenuMetric: View {
    let value: String
    let label: String

    var body: some View {
        VStack(spacing: 2) {
            Text(value).font(.callout.monospacedDigit().weight(.semibold)).lineLimit(1)
            Text(label).font(.caption2).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity)
    }
}

private struct MenuAction: View {
    let title: String
    let symbol: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Label(title, systemImage: symbol)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 7)
                .padding(.vertical, 6)
                .contentShape(.rect)
        }
        .buttonStyle(.plain)
    }
}
