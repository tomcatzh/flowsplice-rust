import SwiftUI

struct DeviceView: View {
    @EnvironmentObject private var store: TravelStore
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        Form {
            Section {
                HStack(spacing: 14) {
                    Image("FlowSpliceMark")
                        .resizable()
                        .frame(width: 48, height: 48)
                        .clipShape(.rect(cornerRadius: 12))
                    VStack(alignment: .leading, spacing: 3) {
                        Text(store.snapshot.travelID)
                            .font(.headline)
                        Text("FlowSplice Travel")
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Image(systemName: store.snapshot.online ? "checkmark.seal.fill" : "checkmark.seal")
                        .font(.title2)
                        .foregroundStyle(store.snapshot.online ? Color.green : .secondary)
                }
                .padding(.vertical, 4)
            }

            Section {
                LabeledContent("Enrollment", value: store.snapshot.enrolled ? "Installed" : "Not installed")
                LabeledContent("Configuration", value: TravelFiles.isInstalled ? "On device" : "Unavailable")
                LabeledContent("Private Key", value: CredentialStore.load() == nil ? "Unavailable" : "Protected by Keychain")
                LabeledContent("Appearance", value: colorScheme == .dark ? "System Dark" : "System Light")
            } header: {
                Text("Installation")
            } footer: {
                Text("The app follows the iPhone or iPad appearance automatically. The private-key password is stored only in this device's Keychain.")
            }

            Section {
                LabeledContent("Live Activity", value: store.liveActivityStatus.label)
                    .accessibilityIdentifier("device-live-activity-status")
                Text(store.liveActivityStatus.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("device-live-activity-detail")
            } header: {
                Text("System Status")
            } footer: {
                Text("The Live Activity follows the system appearance and provides a Stop action from the Lock Screen. iOS still controls suspension and final background lifetime.")
            }

            Section {
                if store.snapshot.phase == .running || store.snapshot.phase == .starting {
                    Button("Stop Travel", role: .destructive) { store.stop() }
                        .disabled(store.isWorking)
                        .accessibilityIdentifier("device-stop")
                } else {
                    Button("Start Travel") { store.start() }
                        .disabled(store.isWorking || !store.snapshot.enrolled)
                        .accessibilityIdentifier("device-start")
                }
            } header: {
                Text("Runtime")
            } footer: {
                Text("Travel remains visible through a Live Activity. When iOS resumes the app, FlowSplice immediately reconciles the runtime, Relay connection, service catalog, and local mappings.")
            }

            Section("Privacy") {
                Label("Local mappings listen only on 127.0.0.1", systemImage: "lock.shield.fill")
                Label("Enrollment keys are generated on this device", systemImage: "key.fill")
                Label("No account sign-in is required", systemImage: "person.crop.circle.badge.checkmark")
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Device")
        .accessibilityIdentifier("device-form")
    }
}
