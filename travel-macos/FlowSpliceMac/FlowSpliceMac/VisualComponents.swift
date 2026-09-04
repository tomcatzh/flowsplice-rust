import SwiftUI

struct FlowPanel<Content: View>: View {
    let title: String
    var subtitle: String?
    @ViewBuilder let content: Content

    init(_ title: String, subtitle: String? = nil, @ViewBuilder content: () -> Content) {
        self.title = title
        self.subtitle = subtitle
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 3) {
                Text(title).font(.headline)
                if let subtitle {
                    Text(subtitle).font(.caption).foregroundStyle(.secondary)
                }
            }
            content
        }
        .padding(18)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.background.secondary, in: .rect(cornerRadius: 14))
        .overlay {
            RoundedRectangle(cornerRadius: 14)
                .stroke(.separator.opacity(0.45), lineWidth: 1)
        }
    }
}

struct MetricTile: View {
    let title: String
    let value: String
    let detail: String
    let symbol: String
    var tint: Color = Color("FlowMint")

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: symbol)
                .font(.system(size: 17, weight: .semibold))
                .foregroundStyle(tint)
                .frame(width: 34, height: 34)
                .background(tint.opacity(0.12), in: .rect(cornerRadius: 9))
            VStack(alignment: .leading, spacing: 3) {
                Text(value).font(.title3.monospacedDigit().weight(.semibold)).lineLimit(1)
                Text(title).font(.caption.weight(.medium)).foregroundStyle(.secondary)
                Text(detail).font(.caption2).foregroundStyle(.tertiary).lineLimit(1)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 92, alignment: .leading)
        .background(.background.secondary, in: .rect(cornerRadius: 12))
        .overlay { RoundedRectangle(cornerRadius: 12).stroke(.separator.opacity(0.35)) }
    }
}

struct StatusPill: View {
    let title: String
    let symbol: String
    let tint: Color

    var body: some View {
        Label(title, systemImage: symbol)
            .font(.caption.weight(.semibold))
            .foregroundStyle(tint)
            .padding(.horizontal, 9)
            .padding(.vertical, 5)
            .background(tint.opacity(0.12), in: .capsule)
    }
}

struct EmptyPanel: View {
    let symbol: String
    let title: String
    let detail: String

    var body: some View {
        ContentUnavailableView(title, systemImage: symbol, description: Text(detail))
            .frame(maxWidth: .infinity, minHeight: 160)
    }
}

extension RelayObservation {
    var symbol: String {
        switch self {
        case .inUse: "arrow.triangle.branch"
        case .recentlyReachable: "checkmark.circle"
        case .eligibleUnverified: "questionmark.circle"
        case .recentFailure: "exclamationmark.triangle"
        case .bootstrapOnly: "flag.pattern.checkered"
        case .removed: "minus.circle"
        }
    }

    var tint: Color {
        switch self {
        case .inUse: Color("FlowMint")
        case .recentlyReachable: .green
        case .eligibleUnverified: .secondary
        case .recentFailure: .orange
        case .bootstrapOnly: .blue
        case .removed: .secondary
        }
    }
}
