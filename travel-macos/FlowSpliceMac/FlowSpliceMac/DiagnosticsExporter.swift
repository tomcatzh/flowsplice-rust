import AppKit
import Foundation
import UniformTypeIdentifiers

nonisolated enum DiagnosticsExporter {
    private struct Export: Codable {
        let format: String
        let appVersion: String
        let generatedAt: String
        let note: String
        let diagnostics: DiagnosticsSnapshot
    }

    static func data(
        snapshot: DiagnosticsSnapshot,
        appVersion: String = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "unknown",
        now: Date = .now
    ) throws -> Data {
        let payload = Export(
            format: "flowsplice-route-diagnostics-v1",
            appVersion: appVersion,
            generatedAt: ISO8601DateFormatter().string(from: now),
            note: "Relay endpoints and native error reasons are redacted by Travel Core before export.",
            diagnostics: snapshot
        )
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        return try encoder.encode(payload)
    }

    @MainActor
    static func save(snapshot: DiagnosticsSnapshot) throws -> URL? {
        let panel = NSSavePanel()
        panel.title = "Export FlowSplice Diagnostics"
        panel.nameFieldStringValue = "FlowSplice-Diagnostics-\(Date.now.formatted(.iso8601.year().month().day())).json"
        panel.allowedContentTypes = [.json]
        guard panel.runModal() == .OK, let url = panel.url else { return nil }
        try data(snapshot: snapshot).write(to: url, options: [.atomic, .completeFileProtection])
        return url
    }
}
