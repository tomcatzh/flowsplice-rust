import AppKit
import SwiftUI

struct SettingsView: View {
    @EnvironmentObject private var store: TravelStore
    @State private var exportMessage: String?

    private var version: String {
        let short = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "—"
        let build = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "—"
        return "\(short) (\(build))"
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Settings").font(.title2.weight(.semibold))
                    Text("Startup, local storage, and privacy-safe support data.").foregroundStyle(.secondary)
                }

                FlowPanel("Startup") {
                    SettingToggle(
                        title: "Launch FlowSplice at login",
                        detail: store.launchAtLoginDetail,
                        symbol: "person.crop.circle.badge.checkmark",
                        isOn: Binding(
                            get: { store.launchAtLoginEnabled },
                            set: { store.setLaunchAtLogin($0) }
                        )
                    )
                    Divider()
                    SettingToggle(
                        title: "Start Travel automatically",
                        detail: "Reconnect after FlowSplice launches.",
                        symbol: "bolt.horizontal.circle",
                        isOn: Binding(
                            get: { store.automaticStartEnabled },
                            set: { store.setAutomaticStart($0) }
                        )
                    )
                }

                FlowPanel("Security & storage") {
                    SettingRow(
                        symbol: store.credentialAvailable ? "checkmark.shield" : "exclamationmark.shield",
                        title: "Private-key password",
                        detail: store.credentialAvailable ? "Stored in your login Keychain" : "Unavailable",
                        tint: store.credentialAvailable ? Color("FlowMint") : .orange
                    )
                    Divider()
                    HStack(alignment: .top, spacing: 12) {
                        Image(systemName: "internaldrive").foregroundStyle(Color("FlowMint")).frame(width: 28)
                        VStack(alignment: .leading, spacing: 4) {
                            Text("Application Support").font(.callout.weight(.medium))
                            Text(TravelFiles.installationDirectory.path)
                                .font(.caption.monospaced())
                                .foregroundStyle(.secondary)
                                .textSelection(.enabled)
                        }
                        Spacer()
                        Button("Show in Finder") {
                            NSWorkspace.shared.activateFileViewerSelecting([TravelFiles.installationDirectory])
                        }
                    }
                }

                FlowPanel("Diagnostics export", subtitle: "Safe to attach to a support report") {
                    Text("The export includes current Flow routes, Relay observation categories, control-plane generations, and a bounded event timeline. Relay hostnames/IPs, credentials, private keys, and configuration paths are excluded.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                    HStack {
                        Button {
                            do {
                                if let url = try DiagnosticsExporter.save(snapshot: store.diagnostics) {
                                    exportMessage = "Saved \(url.lastPathComponent)"
                                }
                            } catch {
                                exportMessage = error.localizedDescription
                            }
                        } label: {
                            Label("Export Diagnostics…", systemImage: "square.and.arrow.up")
                        }
                        .accessibilityIdentifier("export-diagnostics-button")
                        if let exportMessage {
                            Text(exportMessage).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }

                FlowPanel("About") {
                    LabeledContent("FlowSplice", value: "Travel for macOS")
                    LabeledContent("Version", value: version)
                    LabeledContent("Minimum system", value: "macOS 26")
                    LabeledContent("Architecture", value: "Apple silicon")
                }
            }
            .padding(24)
        }
    }
}

private struct SettingToggle: View {
    let title: String
    let detail: String
    let symbol: String
    @Binding var isOn: Bool

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: symbol).foregroundStyle(Color("FlowMint")).frame(width: 28)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.callout.weight(.medium))
                Text(detail).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            Toggle("", isOn: $isOn).labelsHidden().toggleStyle(.switch)
        }
    }
}

private struct SettingRow: View {
    let symbol: String
    let title: String
    let detail: String
    let tint: Color

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: symbol).foregroundStyle(tint).frame(width: 28)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.callout.weight(.medium))
                Text(detail).font(.caption).foregroundStyle(.secondary)
            }
        }
    }
}
