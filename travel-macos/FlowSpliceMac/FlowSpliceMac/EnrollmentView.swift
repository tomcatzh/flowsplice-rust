import SwiftUI

struct EnrollmentView: View {
    @EnvironmentObject private var store: TravelStore
    @State private var travelID = ""
    @State private var homeID = "home-1"
    @State private var relay = ""
    @State private var password = ""
    @State private var confirmation = ""

    var body: some View {
        ZStack {
            LinearGradient(
                colors: [Color("DeepInk"), Color("DeepInk").opacity(0.86)],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
            .ignoresSafeArea()

            HStack(spacing: 54) {
                VStack(alignment: .leading, spacing: 18) {
                    Image("FlowSpliceMark")
                        .resizable()
                        .scaledToFit()
                        .frame(width: 78, height: 78)
                        .accessibilityHidden(true)
                    Text("Your Home services,\nwherever this Mac goes.")
                        .font(.system(size: 38, weight: .semibold, design: .rounded))
                        .foregroundStyle(.white)
                    Text("FlowSplice Travel keeps running from the menu bar and chooses an observed Relay for each Flow.")
                        .font(.title3)
                        .foregroundStyle(.white.opacity(0.68))
                        .frame(maxWidth: 430, alignment: .leading)
                    HStack(spacing: 18) {
                        Label("Per-Flow routing", systemImage: "arrow.triangle.branch")
                        Label("Keychain protected", systemImage: "lock.shield")
                    }
                    .font(.callout.weight(.medium))
                    .foregroundStyle(Color("FlowMint"))
                }
                .frame(maxWidth: 480, alignment: .leading)

                enrollmentCard
                    .frame(width: 430)
            }
            .padding(50)
        }
        .onAppear {
            if travelID.isEmpty { travelID = store.defaultTravelID }
            if relay.isEmpty { relay = EnrollmentStore.lastRelay }
        }
    }

    @ViewBuilder
    private var enrollmentCard: some View {
        VStack(alignment: .leading, spacing: 18) {
            if store.enrollment.phase.isActive {
                waitingCard
            } else {
                Text("Enroll this Mac").font(.title2.weight(.semibold))
                Text("Use the same Home ID and a reachable Relay. Approval happens on Home.")
                    .foregroundStyle(.secondary)
                Form {
                    TextField("Travel ID", text: $travelID)
                        .accessibilityIdentifier("enrollment-travel-id")
                    TextField("Home ID", text: $homeID)
                        .accessibilityIdentifier("enrollment-home-id")
                    TextField("Relay", text: $relay, prompt: Text("relay.example.com:8443"))
                        .accessibilityIdentifier("enrollment-relay")
                    SecureField("Private-key password", text: $password)
                        .accessibilityIdentifier("enrollment-password")
                    SecureField("Confirm password", text: $confirmation)
                        .accessibilityIdentifier("enrollment-password-confirmation")
                }
                .formStyle(.grouped)
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
                        if store.isWorking { ProgressView().controlSize(.small) }
                        Text("Begin Enrollment")
                    }
                    .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .disabled(store.isWorking)
                .accessibilityIdentifier("enrollment-submit")
            }
        }
        .padding(26)
        .background(.regularMaterial, in: .rect(cornerRadius: 20))
        .shadow(color: .black.opacity(0.22), radius: 28, y: 14)
    }

    private var waitingCard: some View {
        VStack(alignment: .leading, spacing: 18) {
            StatusPill(title: "Waiting for Home approval", symbol: "clock", tint: .orange)
            Text("Approve \(store.enrollment.travelID) on Home")
                .font(.title2.weight(.semibold))
            if let code = store.enrollment.verificationCode {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Verification code").font(.caption).foregroundStyle(.secondary)
                    Text(code).font(.system(size: 34, weight: .semibold, design: .monospaced)).textSelection(.enabled)
                }
                .padding(16)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(.quaternary, in: .rect(cornerRadius: 12))
                .accessibilityElement(children: .ignore)
                .accessibilityLabel(code)
                .accessibilityHint("Verification code")
                .accessibilityIdentifier("enrollment-verification-code")
            }
            Text("FlowSplice keeps this request pending if you close the window. Use Cancel to remove its local credentials.")
                .font(.callout)
                .foregroundStyle(.secondary)
            Button("Cancel Enrollment", role: .destructive) {
                store.cancelEnrollment()
            }
            .disabled(store.isWorking)
            .accessibilityIdentifier("enrollment-cancel")
        }
    }
}
