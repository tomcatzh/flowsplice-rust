import ActivityKit
import SwiftUI
import WidgetKit

struct FlowSpliceTravelLiveActivity: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: TravelActivityAttributes.self) { context in
            TravelLockScreenView(context: context)
                .activityBackgroundTint(Color(uiColor: .systemBackground))
                .activitySystemActionForegroundColor(.mint)
        } dynamicIsland: { context in
            DynamicIsland {
                DynamicIslandExpandedRegion(.leading) {
                    FlowSpliceActivityMark(online: context.state.online)
                }
                DynamicIslandExpandedRegion(.trailing) {
                    Text(context.attributes.startedAt, style: .timer)
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
                DynamicIslandExpandedRegion(.center) {
                    Text(context.attributes.travelID)
                        .font(.headline)
                        .lineLimit(1)
                }
                DynamicIslandExpandedRegion(.bottom) {
                    HStack {
                        Label("\(context.state.activeFlows)", systemImage: "point.3.connected.trianglepath.dotted")
                        Spacer()
                        Label(TravelActivityFormatting.bytes(context.state.downloadedBytes), systemImage: "arrow.down")
                        Spacer()
                        Label(context.state.interfaceLabel, systemImage: "network")
                    }
                    .font(.caption.weight(.medium))
                    .padding(.horizontal, 4)
                }
            } compactLeading: {
                Image(systemName: context.state.online ? "link.circle.fill" : "link.circle")
                    .foregroundStyle(context.state.online ? .mint : .orange)
                    .accessibilityLabel(context.state.statusLabel)
            } compactTrailing: {
                Text("\(context.state.activeFlows)")
                    .font(.caption.monospacedDigit().weight(.semibold))
                    .accessibilityLabel("\(context.state.activeFlows) active flows")
            } minimal: {
                Image(systemName: context.state.online ? "link" : "arrow.trianglehead.2.clockwise.rotate.90")
                    .foregroundStyle(context.state.online ? .mint : .orange)
                    .accessibilityLabel(context.state.statusLabel)
            }
            .keylineTint(.mint)
        }
    }
}

private struct TravelLockScreenView: View {
    let context: ActivityViewContext<TravelActivityAttributes>

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                FlowSpliceActivityMark(online: context.state.online)
                VStack(alignment: .leading, spacing: 2) {
                    Text(context.attributes.travelID)
                        .font(.headline)
                        .lineLimit(1)
                    Label(context.state.statusLabel, systemImage: "circle.fill")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(context.state.online ? .green : .orange)
                }
                Spacer()
                VStack(alignment: .trailing, spacing: 2) {
                    Text(context.attributes.startedAt, style: .timer)
                        .font(.subheadline.monospacedDigit().weight(.semibold))
                    Text(context.state.interfaceLabel)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }

            HStack(spacing: 18) {
                ActivityMetric(
                    label: "Flows",
                    value: "\(context.state.activeFlows)",
                    symbol: "point.3.connected.trianglepath.dotted"
                )
                ActivityMetric(
                    label: "Down",
                    value: TravelActivityFormatting.bytes(context.state.downloadedBytes),
                    symbol: "arrow.down"
                )
                ActivityMetric(
                    label: "Up",
                    value: TravelActivityFormatting.bytes(context.state.uploadedBytes),
                    symbol: "arrow.up"
                )
                Spacer(minLength: 0)
            }
        }
        .padding(16)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("FlowSplice Travel \(context.state.statusLabel)")
    }
}

private struct ActivityMetric: View {
    let label: String
    let value: String
    let symbol: String

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Label(label, systemImage: symbol)
                .font(.caption2)
                .foregroundStyle(.secondary)
            Text(value)
                .font(.caption.monospacedDigit().weight(.semibold))
                .lineLimit(1)
        }
    }
}

private struct FlowSpliceActivityMark: View {
    let online: Bool

    var body: some View {
        Image("FlowSpliceMark")
            .resizable()
            .clipShape(.rect(cornerRadius: 9))
            .frame(width: 34, height: 34)
            .overlay(alignment: .bottomTrailing) {
                Circle()
                    .fill(online ? Color.green : Color.orange)
                    .frame(width: 9, height: 9)
                    .overlay(Circle().stroke(Color(uiColor: .systemBackground), lineWidth: 2))
            }
            .accessibilityHidden(true)
    }
}

private enum TravelActivityFormatting {
    static func bytes(_ value: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: value), countStyle: .file)
    }
}
