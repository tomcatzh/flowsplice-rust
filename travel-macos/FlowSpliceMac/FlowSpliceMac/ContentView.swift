//
//  ContentView.swift
//  FlowSpliceMac
//
//  Created by Tomcat on 2026/8/31.
//

import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var store: TravelStore

    var body: some View {
        Group {
            if store.isEnrolled {
                NavigationSplitView {
                    List(TravelStore.Section.allCases) { section in
                        Button {
                            store.select(section)
                        } label: {
                            Label(section.title, systemImage: section.symbol)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .contentShape(.rect)
                        }
                        .buttonStyle(.plain)
                        .listRowBackground(
                            RoundedRectangle(cornerRadius: 7)
                                .fill(store.selectedSection == section ? Color.accentColor.opacity(0.16) : .clear)
                        )
                        .accessibilityIdentifier("sidebar-\(section.rawValue)")
                    }
                    .navigationSplitViewColumnWidth(min: 184, ideal: 208, max: 248)
                    .safeAreaInset(edge: .bottom) {
                        SidebarStatus()
                    }
                } detail: {
                    detail
                }
                .navigationTitle(store.selectedSection?.title ?? "FlowSplice")
            } else {
                EnrollmentView()
            }
        }
        .alert("FlowSplice", isPresented: Binding(
            get: { store.presentedError != nil },
            set: { if !$0 { store.presentedError = nil } }
        )) {
            Button("OK", role: .cancel) { store.presentedError = nil }
        } message: {
            Text(store.presentedError ?? "")
        }
        .toolbar {
            if store.isEnrolled {
                ToolbarItemGroup(placement: .primaryAction) {
                    Button {
                        store.refresh()
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                    .disabled(store.isWorking)
                    .accessibilityIdentifier("refresh-button")

                    Button {
                        store.snapshot.phase == .running ? store.stop() : store.start()
                    } label: {
                        Label(
                            store.snapshot.phase == .running ? "Stop Travel" : "Start Travel",
                            systemImage: store.snapshot.phase == .running ? "stop.fill" : "play.fill"
                        )
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(store.isWorking)
                    .accessibilityIdentifier("travel-toggle")
                }
            }
        }
    }

    @ViewBuilder
    private var detail: some View {
        switch store.selectedSection ?? .overview {
        case .overview:
            OverviewView()
        case .mappings:
            MappingsView()
        case .diagnostics:
            DiagnosticsView()
        case .settings:
            SettingsView()
        }
    }
}

private struct SidebarStatus: View {
    @EnvironmentObject private var store: TravelStore

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: store.stateSymbol)
                .foregroundStyle(store.stateTint)
                .symbolEffect(.pulse, isActive: store.snapshot.phase == .starting || store.snapshot.phase == .stopping)
            VStack(alignment: .leading, spacing: 1) {
                Text(store.stateTitle).font(.callout.weight(.semibold))
                Text(store.snapshot.travelID).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer()
            if store.isWorking { ProgressView().controlSize(.small) }
        }
        .padding(12)
        .background(.bar)
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("sidebar-status")
    }
}
