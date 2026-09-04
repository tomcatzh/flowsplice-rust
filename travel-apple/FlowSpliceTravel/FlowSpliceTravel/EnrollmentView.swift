import SwiftUI

struct EnrollmentView: View {
    @EnvironmentObject private var store: TravelStore
    @State private var travelID = ""
    @State private var homeID = ""
    @State private var relay = ""
    @State private var password = ""
    @State private var confirmation = ""

    var body: some View {
        Form {
            Section {
                EnrollmentHero()
                    .listRowInsets(EdgeInsets())
                    .listRowBackground(Color.clear)
            }

            Section {
                TextField("Travel ID", text: $travelID)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .disabled(store.enrollment.phase.isActive)
                    .accessibilityIdentifier("enrollment-travel-id")
                TextField("Home ID", text: $homeID)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .disabled(store.enrollment.phase.isActive)
                    .accessibilityIdentifier("enrollment-home-id")
            } header: {
                Text("Device Identity")
            } footer: {
                Text("Travel ID defaults to this Apple device. Home ID identifies the Home that approves enrollment.")
            }

            Section {
                TextField("host:port", text: $relay)
                    .textInputAutocapitalization(.never)
                    .keyboardType(.URL)
                    .autocorrectionDisabled()
                    .disabled(store.enrollment.phase.isActive)
                    .accessibilityIdentifier("enrollment-relay")
            } header: {
                Text("Relay")
            } footer: {
                Text("Enter the Relay management address. The app does not contain a built-in deployment address.")
            }

            Section {
                SecureField("At least 12 characters", text: $password)
                    .textContentType(.newPassword)
                    .disabled(store.enrollment.phase.isActive)
                    .accessibilityIdentifier("enrollment-password")
                SecureField("Confirm password", text: $confirmation)
                    .textContentType(.newPassword)
                    .disabled(store.enrollment.phase.isActive)
                    .accessibilityIdentifier("enrollment-confirmation")
            } header: {
                Text("Private-Key Password")
            } footer: {
                Label("Stored in Keychain for start and foreground recovery.", systemImage: "key.fill")
            }

            if store.enrollment.phase.isActive || store.enrollment.phase == .error {
                EnrollmentStatusSection()
            } else {
                Section {
                    Button {
                        store.enroll(
                            travelID: travelID,
                            homeID: homeID,
                            relay: relay,
                            password: password,
                            confirmation: confirmation
                        )
                    } label: {
                        HStack {
                            Spacer()
                            if store.isWorking { ProgressView().padding(.trailing, 4) }
                            Text(store.enrollment.phase == .error ? "Retry Enrollment" : "Enroll This Device")
                                .fontWeight(.semibold)
                            Spacer()
                        }
                    }
                    .disabled(!formIsValid || store.isWorking)
                    .accessibilityIdentifier("enrollment-submit")
                }
            }
        }
        .formStyle(.grouped)
        .onAppear {
            if travelID.isEmpty { travelID = store.defaultTravelID }
            if homeID.isEmpty { homeID = store.defaultHomeID }
            if relay.isEmpty { relay = EnrollmentStore.lastRelay }
            #if DEBUG
            let environment = ProcessInfo.processInfo.environment
            if environment["FLOWSPLICE_E2E"] == "1",
               let e2ePassword = environment["FLOWSPLICE_E2E_PASSWORD"] {
                if password.isEmpty { password = e2ePassword }
                if confirmation.isEmpty { confirmation = e2ePassword }
            }
            #endif
        }
    }

    private var formIsValid: Bool {
        !TravelValidation.normalizedID(travelID).isEmpty &&
            !TravelValidation.normalizedID(homeID).isEmpty &&
            TravelValidation.relayIsValid(relay) &&
            password.count >= 12 &&
            password == confirmation
    }
}

private struct EnrollmentHero: View {
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        HStack(spacing: 16) {
            Image("FlowSpliceMark")
                .resizable()
                .frame(width: 52, height: 52)
                .clipShape(.rect(cornerRadius: 13))
            VStack(alignment: .leading, spacing: 5) {
                Text("Connect to your Home")
                    .font(.title2.weight(.semibold))
                Text("Keys are generated on this device. Home approval completes enrollment automatically.")
                    .font(.subheadline)
                    .foregroundStyle(colorScheme == .dark ? Color.white.opacity(0.7) : .secondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(20)
        .foregroundStyle(colorScheme == .dark ? .white : Color.primary)
        .background(
            colorScheme == .dark ? Color("DeepInk") : Color(uiColor: .secondarySystemGroupedBackground),
            in: .rect(cornerRadius: 22)
        )
        .padding(.vertical, 8)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("enrollment-hero")
    }
}

private struct EnrollmentStatusSection: View {
    @EnvironmentObject private var store: TravelStore

    var body: some View {
        Section {
            VStack(alignment: .leading, spacing: 12) {
                Label(title, systemImage: store.enrollment.phase == .error ? "exclamationmark.triangle.fill" : "hourglass")
                    .font(.headline)
                if let code = store.enrollment.verificationCode {
                    Text("Compare this code on Home")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                    Text(code)
                        .font(.system(.title2, design: .monospaced, weight: .bold))
                        .textSelection(.enabled)
                        .accessibilityIdentifier("enrollment-verification-code")
                }
                if let error = store.enrollment.error {
                    Text(error)
                        .font(.subheadline)
                        .foregroundStyle(.red)
                }
                Text(store.enrollment.phase.isActive ? "Approval remains pending until FlowSplice can reconcile again." : "Start over to change the enrollment inputs.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                Button(store.enrollment.phase.isActive ? "Cancel Enrollment" : "Start Over", role: .destructive) {
                    store.cancelEnrollment()
                }
                .accessibilityIdentifier("enrollment-cancel")
            }
            .padding(.vertical, 4)
        }
    }

    private var title: String {
        switch store.enrollment.phase {
        case .preparing: "Generating Device Keys"
        case .waitingForApproval: "Waiting for Home Approval"
        case .error: "Enrollment Needs Attention"
        default: "Remote Enrollment"
        }
    }
}
