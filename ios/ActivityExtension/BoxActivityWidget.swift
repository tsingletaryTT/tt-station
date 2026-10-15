import ActivityKit
import SwiftUI
import WidgetKit

@main
struct TTStationActivityBundle: WidgetBundle {
    var body: some Widget {
        BoxActivityWidget()
    }
}

private let teal = Color(red: 0x46 / 255, green: 0xAF / 255, blue: 0xD2 / 255)
private let mint = Color(red: 0x39 / 255, green: 0xC6 / 255, blue: 0xAC / 255)
private let stone = Color(red: 0x0B / 255, green: 0x18 / 255, blue: 0x17 / 255)

struct BoxActivityWidget: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: BoxActivityAttributes.self) { context in
            LockScreen(context: context)
                .activityBackgroundTint(stone.opacity(0.92))
                .activitySystemActionForegroundColor(teal)
        } dynamicIsland: { context in
            DynamicIsland {
                DynamicIslandExpandedRegion(.leading) {
                    Image(systemName: context.state.symbol).foregroundStyle(teal).font(.title2)
                }
                DynamicIslandExpandedRegion(.trailing) {
                    Remaining(state: context.state).font(.caption.monospacedDigit())
                }
                DynamicIslandExpandedRegion(.bottom) {
                    VStack(alignment: .leading, spacing: 6) {
                        Text(context.state.title).font(.headline).lineLimit(1)
                        Text(context.state.detail).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                        Bar(state: context.state)
                    }
                }
            } compactLeading: {
                Image(systemName: context.state.symbol).foregroundStyle(context.state.done ? mint : teal)
            } compactTrailing: {
                if context.state.done {
                    Image(systemName: "checkmark").foregroundStyle(mint)
                } else if let p = context.state.progress {
                    Text("\(Int(p * 100))%").font(.caption2.monospacedDigit())
                } else {
                    Remaining(state: context.state).font(.caption2.monospacedDigit()).frame(maxWidth: 44)
                }
            } minimal: {
                Image(systemName: context.state.done ? "checkmark" : context.state.symbol)
                    .foregroundStyle(context.state.done ? mint : teal)
            }
        }
    }
}

private struct LockScreen: View {
    let context: ActivityViewContext<BoxActivityAttributes>

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                Image(systemName: context.state.done ? "checkmark.circle.fill" : context.state.symbol)
                    .font(.title3)
                    .foregroundStyle(context.state.done ? mint : teal)
                VStack(alignment: .leading, spacing: 2) {
                    Text(context.state.title).font(.headline).lineLimit(1)
                    Text(context.state.detail).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer(minLength: 0)
                Text(context.attributes.boxName.uppercased())
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
            }
            if !context.state.done {
                HStack {
                    Bar(state: context.state)
                    Remaining(state: context.state).font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                }
            }
        }
        .padding(16)
    }
}

/// A progress bar that keeps moving between updates when there's an estimate.
private struct Bar: View {
    let state: BoxActivityAttributes.ContentState

    var body: some View {
        if let start = state.estimatedStart, let end = state.estimatedEnd, end > start, state.progress == nil {
            ProgressView(timerInterval: start...end, countsDown: false) { EmptyView() } currentValueLabel: { EmptyView() }
                .tint(teal)
        } else if let p = state.progress {
            ProgressView(value: p).tint(teal)
        } else {
            ProgressView(value: 0).tint(teal)
        }
    }
}

private struct Remaining: View {
    let state: BoxActivityAttributes.ContentState

    var body: some View {
        if let end = state.estimatedEnd, end > .now, !state.done {
            Text(timerInterval: Date.now...end, countsDown: true)
        } else {
            Text(state.done ? "Done" : "")
        }
    }
}
