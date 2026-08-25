import ActivityKit
import Foundation

struct TravelActivityAttributes: ActivityAttributes {
    struct ContentState: Codable, Hashable {
        var phase: String
        var online: Bool
        var activeFlows: Int
        var uploadedBytes: UInt64
        var downloadedBytes: UInt64
        var relayCount: Int
        var mappingCount: Int
        var interfaceLabel: String
        var updatedAt: Date

        var statusLabel: String {
            switch phase {
            case "running": online ? "Online" : "Reconnecting"
            case "starting": "Starting"
            case "stopping": "Stopping"
            case "error": "Needs attention"
            default: "Stopped"
            }
        }
    }

    var travelID: String
    var startedAt: Date
}
