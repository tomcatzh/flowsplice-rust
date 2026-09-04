import SwiftUI

struct MappingsView: View {
    @EnvironmentObject private var store: TravelStore
    @State private var showingAdd = false

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Local service mappings").font(.title2.weight(.semibold))
                    Text("Bind a Home service to a loopback port on this Mac.").foregroundStyle(.secondary)
                }
                Spacer()
                Button {
                    showingAdd = true
                } label: {
                    Label("Add Mapping", systemImage: "plus")
                }
                .buttonStyle(.borderedProminent)
                .disabled(store.snapshot.phase != .running || store.catalog.availableHomes.isEmpty)
                .accessibilityIdentifier("add-mapping-button")
            }
            .padding(24)

            Divider()

            if store.snapshot.mappings.isEmpty {
                EmptyPanel(symbol: "point.3.connected.trianglepath.dotted", title: "No mappings yet", detail: "Start Travel, then add a service from the Home catalog.")
                    .frame(maxHeight: .infinity)
            } else {
                Table(store.snapshot.mappings) {
                    TableColumn("Service") { mapping in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(mapping.serviceID).fontWeight(.medium)
                            Text(mapping.homeID).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    TableColumn("Protocol") { mapping in
                        Text(mapping.protocol.uppercased()).font(.caption.monospaced().weight(.semibold))
                    }
                    .width(90)
                    TableColumn("Local address") { mapping in
                        Text(mapping.bind).font(.body.monospaced())
                    }
                    TableColumn("Status") { mapping in
                        StatusPill(
                            title: store.snapshot.phase == .running ? "Listening" : "Stopped",
                            symbol: store.snapshot.phase == .running ? "checkmark.circle" : "pause.circle",
                            tint: store.snapshot.phase == .running ? Color("FlowMint") : .secondary
                        )
                    }
                    .width(120)
                    TableColumn("") { mapping in
                        Button(role: .destructive) {
                            store.deleteMapping(mapping)
                        } label: {
                            Image(systemName: "trash")
                        }
                        .buttonStyle(.borderless)
                        .help("Remove mapping")
                        .accessibilityLabel("Remove \(mapping.serviceID) mapping")
                    }
                    .width(44)
                }
                .accessibilityIdentifier("mappings-table")
            }
        }
        .sheet(isPresented: $showingAdd) {
            AddMappingSheet(isPresented: $showingAdd)
                .environmentObject(store)
        }
    }
}

private struct AddMappingSheet: View {
    @EnvironmentObject private var store: TravelStore
    @Binding var isPresented: Bool
    @State private var homeID = ""
    @State private var serviceKey = ""
    @State private var port = ""
    @State private var saving = false

    private var home: CatalogHome? {
        store.catalog.availableHomes.first { $0.id == homeID } ?? store.catalog.availableHomes.first
    }

    private var service: CatalogService? {
        home?.services.first { $0.key == serviceKey } ?? home?.services.first
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Add Local Mapping").font(.title2.weight(.semibold))
                Text("Only loopback is exposed. Other devices cannot connect to this port directly.")
                    .foregroundStyle(.secondary)
            }

            Form {
                Picker("Home", selection: $homeID) {
                    ForEach(store.catalog.availableHomes) { home in
                        Text(home.displayName).tag(home.id)
                    }
                }
                .accessibilityIdentifier("mapping-home-picker")
                Picker("Service", selection: $serviceKey) {
                    ForEach(home?.services ?? []) { service in
                        Text("\(service.displayName) · \(service.protocol.uppercased())")
                            .tag(service.key)
                            .accessibilityIdentifier("mapping-service-\(service.id)-\(service.protocol)")
                    }
                }
                .accessibilityIdentifier("mapping-service-picker")
                TextField("Local port", text: $port, prompt: Text("10022"))
                    .accessibilityIdentifier("mapping-port-field")
            }
            .formStyle(.grouped)

            HStack {
                Spacer()
                Button("Cancel") { isPresented = false }
                    .keyboardShortcut(.cancelAction)
                Button("Add Mapping") {
                    guard let home, let service, let port = Int(port) else { return }
                    saving = true
                    Task {
                        if await store.addMapping(home: home, service: service, port: port) {
                            isPresented = false
                        }
                        saving = false
                    }
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(saving || home == nil || service == nil || Int(port) == nil)
                .accessibilityIdentifier("mapping-save-button")
            }
        }
        .padding(24)
        .frame(width: 520)
        .onAppear {
            homeID = store.catalog.availableHomes.first?.id ?? ""
            serviceKey = home?.services.first?.key ?? ""
        }
        .onChange(of: homeID) {
            serviceKey = home?.services.first?.key ?? ""
        }
    }
}
