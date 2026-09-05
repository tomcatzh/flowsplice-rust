import SwiftUI

struct DeviceView: View {
    @EnvironmentObject private var store: TravelStore
    @Environment(\.colorScheme) private var colorScheme
    @State private var showsReenrollmentConfirmation = false

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
                LabeledContent("Configuration", value: store.snapshot.enrolled ? "On device" : "Unavailable")
                LabeledContent("Private Key", value: credentialLabel)
                LabeledContent("Appearance", value: colorScheme == .dark ? "System Dark" : "System Light")
            } header: {
                Text("Installation")
            } footer: {
                Text("The app follows the system appearance automatically. The private-key password is stored only on this device.")
            }

            if store.needsReenrollmentOnThisDevice {
                Section {
                    Text("This device no longer has the private-key password required for its installed Travel configuration.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                    Button("Re-enroll on this device", role: .destructive) {
                        showsReenrollmentConfirmation = true
                    }
                    .disabled(store.isReenrolling)
                    .accessibilityIdentifier("device-reenroll")
                } header: {
                    Text("Private Key Missing")
                } footer: {
                    Text("Re-enrolling removes this device's Travel installation and its Keychain password, then returns to enrollment. Other app settings stay unchanged.")
                }
            } else if store.snapshot.enrolled, store.credentialAvailability == .unavailable {
                Section {
                    Text("Keychain is temporarily unavailable. Unlock this device and try again. Installed Travel data has not been changed.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                } header: {
                    Text("Keychain Unavailable")
                }
            }

            Section {
                if store.canStop {
                    Button("Stop Travel", role: .destructive) { store.stop() }
                        .accessibilityIdentifier("device-stop")
                } else {
                    Button("Start Travel") { store.start() }
                        .disabled(store.isWorking || !store.snapshot.enrolled)
                        .accessibilityIdentifier("device-start")
                }
            } header: {
                Text("Runtime")
            }

            Section {
                LabeledContent("Background Audio", value: store.backgroundAudioStatus.label)
                    .accessibilityIdentifier("device-background-audio-status")
                Text(store.backgroundAudioStatus.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("device-background-audio-detail")

                LabeledContent("Live Activity", value: store.liveActivityStatus.label)
                    .accessibilityIdentifier("device-live-activity-status")
                Text(store.liveActivityStatus.detail)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("device-live-activity-detail")
            } header: {
                Text("System Status")
            } footer: {
                Text("Background audio owns continuity. Live Activity is optional presentation only; it never starts, stops, or keeps Travel alive.")
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
        .confirmationDialog(
            "Re-enroll on this device?",
            isPresented: $showsReenrollmentConfirmation,
            titleVisibility: .visible
        ) {
            Button("Re-enroll on this device", role: .destructive) {
                store.reenrollOnThisDevice()
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This removes this device's Travel configuration and Keychain password, then returns to enrollment.")
        }
    }

    private var credentialLabel: String {
        switch store.credentialAvailability {
        case .available:
            "Protected by Keychain"
        case .missing:
            "Missing"
        case .unavailable:
            "Temporarily unavailable"
        }
    }
}
