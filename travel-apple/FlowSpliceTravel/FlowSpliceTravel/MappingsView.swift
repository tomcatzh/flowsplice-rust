import SwiftUI

struct MappingsView: View {
    @EnvironmentObject private var store: TravelStore
    @State private var showingAddMapping = false

    var body: some View {
        List {
            if store.snapshot.mappings.isEmpty {
                ContentUnavailableView {
                    Label("No Local Mappings", systemImage: "arrow.left.arrow.right")
                } description: {
                    Text(store.snapshot.phase == .running ? "Add a Home service and choose its local loopback port." : "Start Travel before adding a Home service.")
                } actions: {
                    Button("Add Mapping") { showingAddMapping = true }
                        .disabled(store.snapshot.phase != .running)
                }
                .listRowBackground(Color.clear)
            } else {
                Section {
                    ForEach(store.snapshot.mappings) { mapping in
                        MappingRow(mapping: mapping)
                            .swipeActions {
                                Button("Remove", role: .destructive) { store.deleteMapping(mapping) }
                            }
                            .accessibilityIdentifier("mapping-\(mapping.homeID)-\(mapping.serviceID)")
                    }
                } header: {
                    Text("Active on this device")
                } footer: {
                    Text("All listeners bind only to the loopback interface and restore from durable Travel state.")
                }
            }

            Section("Service Catalog") {
                if store.catalog.availableHomes.isEmpty {
                    HStack {
                        ProgressView()
                        Text("Waiting for Home services…")
                            .foregroundStyle(.secondary)
                    }
                } else {
                    ForEach(store.catalog.availableHomes) { home in
                        DisclosureGroup {
                            ForEach(home.services) { service in
                                HStack {
                                    VStack(alignment: .leading, spacing: 3) {
                                        Text(service.displayName)
                                        Text(service.protocol.uppercased())
                                            .font(.caption.monospaced().weight(.semibold))
                                            .foregroundStyle(.secondary)
                                    }
                                    Spacer()
                                    if store.snapshot.mappings.contains(where: {
                                        $0.homeID == home.id && $0.serviceID == service.id && $0.protocol == service.protocol
                                    }) {
                                        Image(systemName: "checkmark.circle.fill")
                                            .foregroundStyle(.green)
                                    }
                                }
                            }
                        } label: {
                            Label(home.displayName, systemImage: "house.fill")
                        }
                    }
                }
            }
        }
        .navigationTitle("Local Mappings")
        .toolbar {
            ToolbarItemGroup(placement: .topBarTrailing) {
                Button {
                    store.refreshCatalog()
                } label: {
                    Label("Refresh Catalog", systemImage: "arrow.clockwise")
                }
                .disabled(store.snapshot.phase != .running)
                .accessibilityIdentifier("catalog-refresh")
                Button {
                    showingAddMapping = true
                } label: {
                    Label("Add Mapping", systemImage: "plus")
                }
                .disabled(store.snapshot.phase != .running)
                .accessibilityIdentifier("mapping-add")
            }
        }
        .sheet(isPresented: $showingAddMapping) {
            AddMappingSheet(isPresented: $showingAddMapping)
                .environmentObject(store)
                .presentationDetents([.large])
        }
    }
}

private struct MappingRow: View {
    let mapping: TravelMapping

    var body: some View {
        HStack(spacing: 12) {
            Text(mapping.protocol.uppercased())
                .font(.caption2.monospaced().weight(.bold))
                .foregroundStyle(Color("AccentColor"))
                .padding(.horizontal, 8)
                .padding(.vertical, 5)
                .background(Color("AccentColor").opacity(0.12), in: .rect(cornerRadius: 7))
            VStack(alignment: .leading, spacing: 3) {
                Text(mapping.serviceID)
                    .font(.body.weight(.medium))
                Text(mapping.homeID)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Text(mapping.bind)
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
        }
        .padding(.vertical, 3)
    }
}

private struct AddMappingSheet: View {
    @EnvironmentObject private var store: TravelStore
    @Binding var isPresented: Bool
    @State private var selectedHomeID = ""
    @State private var selectedServiceKey = ""
    @State private var port = ""
    @State private var isSaving = false

    private var homes: [CatalogHome] { store.catalog.availableHomes }
    private var selectedHome: CatalogHome? { homes.first { $0.id == selectedHomeID } }
    private var selectedService: CatalogService? {
        selectedHome?.services.first { $0.key == selectedServiceKey }
    }
    private var validPort: Int? {
        guard let value = Int(port), (1...65_535).contains(value) else { return nil }
        return value
    }

    var body: some View {
        NavigationStack {
            Form {
                if homes.isEmpty {
                    ContentUnavailableView("Waiting for Home Services", systemImage: "arrow.clockwise")
                } else {
                    Section("Remote Service") {
                        Picker("Home", selection: $selectedHomeID) {
                            ForEach(homes) { home in
                                Text(home.displayName).tag(home.id)
                            }
                        }
                        .accessibilityIdentifier("mapping-home-picker")
                        Picker("Service", selection: $selectedServiceKey) {
                            ForEach(selectedHome?.services ?? []) { service in
                                Text(service.displayName)
                                    .tag(service.key)
                                    .accessibilityIdentifier("mapping-service-\(service.id)-\(service.protocol)")
                            }
                        }
                        .accessibilityIdentifier("mapping-service-picker")
                        LabeledContent("Protocol", value: selectedService?.protocol.uppercased() ?? "—")
                            .accessibilityIdentifier("mapping-protocol")
                    }

                    Section {
                        HStack {
                            Text("127.0.0.1:")
                                .foregroundStyle(.secondary)
                            TextField("Port", text: $port)
                                .keyboardType(.numberPad)
                                .onChange(of: port) { _, value in
                                    port = String(value.filter(\.isNumber).prefix(5))
                                }
                                .accessibilityIdentifier("mapping-port")
                        }
                    } header: {
                        Text("Local Access")
                    } footer: {
                        Text("The mapping is reachable only from apps on this device.")
                    }
                }
            }
            .navigationTitle("Add Local Mapping")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { isPresented = false }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        save()
                    }
                    .disabled(selectedHome == nil || selectedService == nil || validPort == nil || isSaving)
                    .accessibilityIdentifier("mapping-save")
                }
            }
            .onAppear { normalizeSelection() }
            .onChange(of: selectedHomeID) { _, _ in normalizeServiceSelection() }
        }
    }

    private func normalizeSelection() {
        if !homes.contains(where: { $0.id == selectedHomeID }) {
            selectedHomeID = homes.first?.id ?? ""
        }
        normalizeServiceSelection()
    }

    private func normalizeServiceSelection() {
        if selectedService == nil {
            selectedServiceKey = selectedHome?.services.first?.key ?? ""
        }
    }

    private func save() {
        guard let home = selectedHome, let service = selectedService, let port = validPort else { return }
        isSaving = true
        Task {
            if await store.addMapping(home: home, service: service, port: port) {
                isPresented = false
            }
            isSaving = false
        }
    }
}
