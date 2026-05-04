#if os(macOS)
import SwiftUI

struct MacConversationActionsView: View {
    @Environment(\.colorScheme) private var colorScheme
    @ObservedObject var viewModel: AppViewModel
    @State private var selectedPage: MacConversationActionPage = .overview
    @State private var remoteHost = ""
    @State private var remoteCwd = ""

    var body: some View {
        HStack(spacing: 0) {
            sidebar

            Rectangle()
                .fill(PlatformColor.separator)
                .frame(width: 1)

            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    conversationHeader
                    pageContent
                }
                .padding(22)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .background(PlatformColor.appBackground)
        }
        .task(id: viewModel.selectedConversationID) {
            await viewModel.loadSelectedConversationStatus()
            if viewModel.availableModels.isEmpty {
                await viewModel.loadModels()
            }
            remoteHost = viewModel.selectedConversation?.remote ?? ""
            remoteCwd = viewModel.selectedConversation?.workspacePath ?? ""
        }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Actions")
                .font(.headline)
                .padding(.horizontal, 12)
                .padding(.top, 16)

            ForEach(MacConversationActionPage.allCases) { page in
                Button {
                    selectedPage = page
                } label: {
                    HStack(spacing: 10) {
                        Image(systemName: page.systemImage)
                            .frame(width: 18)
                        Text(page.title)
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 8)
                    .foregroundStyle(selectedPage == page ? .primary : .secondary)
                    .background(selectedPage == page ? PlatformColor.sidebarSelection : Color.clear)
                    .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
                }
                .buttonStyle(.plain)
            }

            Spacer(minLength: 0)
        }
        .padding(.horizontal, 10)
        .padding(.bottom, 12)
        .frame(width: 210)
        .background(PlatformColor.sidebarBackground)
    }

    private var conversationHeader: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(viewModel.selectedConversation?.title ?? "Conversation")
                        .font(.title2.weight(.semibold))
                        .lineLimit(2)

                    Text(viewModel.selectedConversation?.id ?? "No conversation selected")
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }

                Spacer()

                if viewModel.selectedConversation?.status == .running || viewModel.activeTurnProgress?.isActive == true {
                    Label(activeSubtitle, systemImage: "circle.dotted")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.green)
                        .padding(.horizontal, 9)
                        .padding(.vertical, 5)
                        .background(Color.green.opacity(0.12))
                        .clipShape(Capsule())
                }
            }

            if let conversation = viewModel.selectedConversation {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 160), spacing: 10)], spacing: 10) {
                    MacActionFactTile(title: "Model", value: conversation.model.isEmpty ? "pending" : conversation.model)
                    MacActionFactTile(title: "Reasoning", value: conversation.reasoning.isEmpty ? "default" : conversation.reasoning)
                    MacActionFactTile(title: "Sandbox", value: sandboxSummary(conversation))
                    MacActionFactTile(title: "Remote", value: conversation.remote.isEmpty ? "local" : conversation.remote)
                }
            }
        }
        .padding(18)
        .background(macPanelBackground(colorScheme))
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(macPanelBorder(colorScheme))
        }
        .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    @ViewBuilder
    private var pageContent: some View {
        switch selectedPage {
        case .overview:
            overviewPage
        case .model:
            modelPage
        case .reasoning:
            reasoningPage
        case .remote:
            remotePage
        case .sandbox:
            sandboxPage
        }
    }

    private var overviewPage: some View {
        VStack(alignment: .leading, spacing: 16) {
            MacActionSection(title: "Usage") {
                if let status = currentStatus {
                    MacUsageOverviewView(status: status)
                } else if let error = viewModel.selectedConversationStatusError {
                    VStack(alignment: .leading, spacing: 10) {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.red)
                        Button("Reload Usage") {
                            Task { await viewModel.loadSelectedConversationStatus() }
                        }
                    }
                } else {
                    HStack(spacing: 10) {
                        ProgressView()
                        Text("Loading usage")
                            .foregroundStyle(.secondary)
                    }
                }
            }

            MacActionSection(title: "Actions") {
                HStack(spacing: 10) {
                    Button {
                        Task {
                            await viewModel.loadSelectedConversation()
                            await viewModel.loadSelectedConversationStatus()
                        }
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }

                    Button {
                        viewModel.sendConversationCommand("/status")
                    } label: {
                        Label("Request Status", systemImage: "info.circle")
                    }

                    Button {
                        viewModel.sendConversationCommand("/continue")
                    } label: {
                        Label("Continue", systemImage: "play")
                    }

                    Button(role: .destructive) {
                        viewModel.sendConversationCommand("/cancel")
                    } label: {
                        Label("Cancel Run", systemImage: "stop.circle")
                    }
                }
                .buttonStyle(.bordered)
            }
        }
    }

    private var modelPage: some View {
        MacActionSection(title: "Model") {
            if let error = viewModel.modelsError {
                VStack(alignment: .leading, spacing: 10) {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                    Button("Reload Models") {
                        Task { await viewModel.loadModels() }
                    }
                }
            }

            LazyVStack(alignment: .leading, spacing: 8) {
                ForEach(viewModel.availableModels) { model in
                    Button {
                        viewModel.switchModel(model)
                    } label: {
                        MacCommandOptionRow(
                            title: model.alias,
                            detail: "\(model.providerType) - \(model.modelName)",
                            isSelected: model.alias == viewModel.selectedConversation?.model
                        )
                    }
                    .buttonStyle(.plain)
                }
            }
        }
    }

    private var reasoningPage: some View {
        MacActionSection(title: "Reasoning") {
            ForEach(reasoningOptions, id: \.value) { option in
                Button {
                    viewModel.switchReasoning(option.value)
                } label: {
                    MacCommandOptionRow(
                        title: option.title,
                        detail: option.detail,
                        isSelected: option.value == viewModel.selectedConversation?.reasoning
                    )
                }
                .buttonStyle(.plain)
            }
        }
    }

    private var remotePage: some View {
        MacActionSection(title: "Remote Workspace") {
            if let conversation = viewModel.selectedConversation {
                MacActionFactTile(title: "Current", value: conversation.remote.isEmpty ? "local" : conversation.remote)
            }

            VStack(alignment: .leading, spacing: 10) {
                TextField("SSH host", text: $remoteHost)
                    .textFieldStyle(.roundedBorder)
                TextField("Workspace directory", text: $remoteCwd)
                    .textFieldStyle(.roundedBorder)

                HStack {
                    Button {
                        let host = remoteHost.trimmingCharacters(in: .whitespacesAndNewlines)
                        let cwd = remoteCwd.trimmingCharacters(in: .whitespacesAndNewlines)
                        guard !host.isEmpty, !cwd.isEmpty else {
                            return
                        }
                        viewModel.sendConversationCommand("/remote \(host) \(cwd)")
                    } label: {
                        Label("Apply Remote", systemImage: "checkmark.circle")
                    }
                    .disabled(remoteHost.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || remoteCwd.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)

                    Button {
                        viewModel.sendConversationCommand("/remote off")
                    } label: {
                        Label("Use Local Workspace", systemImage: "desktopcomputer")
                    }
                }
                .buttonStyle(.bordered)
            }
        }
    }

    private var sandboxPage: some View {
        MacActionSection(title: "Sandbox") {
            ForEach(sandboxOptions) { option in
                Button {
                    viewModel.sendConversationCommand("/sandbox \(option.value)")
                } label: {
                    MacCommandOptionRow(
                        title: option.title,
                        detail: option.detail,
                        isSelected: isSandboxSelected(option)
                    )
                }
                .buttonStyle(.plain)
            }
        }
    }

    private var currentStatus: ConversationStatusSnapshot? {
        guard viewModel.selectedConversationStatus?.conversationID == viewModel.selectedConversationID else {
            return nil
        }
        return viewModel.selectedConversationStatus
    }

    private var activeSubtitle: String {
        if let progress = viewModel.activeTurnProgress, progress.isActive {
            return progress.subtitle.isEmpty ? progress.title : progress.subtitle
        }
        return "Working"
    }

    private func sandboxSummary(_ conversation: ConversationSummary) -> String {
        let mode = conversation.sandbox.isEmpty ? "pending" : conversation.sandbox
        guard let source = conversation.sandboxSource, !source.isEmpty else {
            return mode
        }
        return "\(mode) - \(source)"
    }

    private func isSandboxSelected(_ option: MacSandboxOption) -> Bool {
        guard let conversation = viewModel.selectedConversation else {
            return false
        }
        if option.value == "default" {
            return conversation.sandboxSource == "default"
        }
        return conversation.sandbox == option.value && conversation.sandboxSource != "default"
    }
}

private enum MacConversationActionPage: String, CaseIterable, Identifiable {
    case overview
    case model
    case reasoning
    case remote
    case sandbox

    var id: String { rawValue }

    var title: String {
        switch self {
        case .overview: "Overview"
        case .model: "Model"
        case .reasoning: "Reasoning"
        case .remote: "Remote"
        case .sandbox: "Sandbox"
        }
    }

    var systemImage: String {
        switch self {
        case .overview: "chart.bar"
        case .model: "cpu"
        case .reasoning: "brain.head.profile"
        case .remote: "point.3.connected.trianglepath.dotted"
        case .sandbox: "lock.shield"
        }
    }
}

private struct MacActionSection<Content: View>: View {
    @Environment(\.colorScheme) private var colorScheme
    let title: String
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(title)
                .font(.headline)
            content()
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(macPanelBackground(colorScheme))
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(macPanelBorder(colorScheme))
        }
        .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
    }
}

private struct MacActionFactTile: View {
    @Environment(\.colorScheme) private var colorScheme
    let title: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.subheadline.weight(.medium))
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(macTileBackground(colorScheme))
        .overlay {
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(macTileBorder(colorScheme))
        }
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }
}

private struct MacCommandOptionRow: View {
    let title: String
    let detail: String
    let isSelected: Bool

    var body: some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                    .foregroundStyle(.primary)
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Spacer(minLength: 16)

            if isSelected {
                Image(systemName: "checkmark.circle.fill")
                    .foregroundStyle(Color.accentColor)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(isSelected ? Color.accentColor.opacity(0.12) : PlatformColor.controlBackground)
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }
}

private struct MacUsageOverviewView: View {
    let status: ConversationStatusSnapshot

    private var totalUsage: ConversationUsageTotals {
        status.usage.total
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 10) {
                MacUsageMetricTile(title: "Cache", value: "\(Int((totalUsage.cacheHitRate * 100).rounded()))%")
                MacUsageMetricTile(title: "Tokens", value: macCompactNumber(totalUsage.totalTokens))
                MacUsageMetricTile(title: "Cost", value: macFormatCost(totalUsage.cost.total))
            }

            VStack(spacing: 10) {
                MacUsageProgressRow(label: "Cache Read", value: totalUsage.cacheRead, total: totalUsage.totalTokens)
                MacUsageProgressRow(label: "Cache Write", value: totalUsage.cacheWrite, total: totalUsage.totalTokens)
                MacUsageProgressRow(label: "Input", value: totalUsage.input, total: totalUsage.totalTokens)
                MacUsageProgressRow(label: "Output", value: totalUsage.output, total: totalUsage.totalTokens)
            }

            Divider()

            VStack(spacing: 8) {
                MacUsageBucketRow(title: "Foreground", usage: status.usage.foreground)
                MacUsageBucketRow(title: "Background", usage: status.usage.background)
                MacUsageBucketRow(title: "Subagents", usage: status.usage.subagents)
                MacUsageBucketRow(title: "Media Tools", usage: status.usage.mediaTools)
            }

            Divider()

            HStack {
                Label("Background \(status.runningBackground) / \(status.totalBackground)", systemImage: "gearshape.2")
                Spacer()
                Label("Subagents \(status.runningSubagents) / \(status.totalSubagents)", systemImage: "person.2")
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
    }
}

private struct MacUsageMetricTile: View {
    @Environment(\.colorScheme) private var colorScheme
    let title: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption2.weight(.medium))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.headline.monospacedDigit())
                .lineLimit(1)
                .minimumScaleFactor(0.72)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 10)
        .padding(.vertical, 9)
        .background(macTileBackground(colorScheme), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(macTileBorder(colorScheme))
        }
    }
}

private struct MacUsageProgressRow: View {
    let label: String
    let value: Int
    let total: Int

    private var percent: Double {
        guard total > 0 else {
            return 0
        }
        return min(1, max(0, Double(value) / Double(total)))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack {
                Text(label)
                    .foregroundStyle(.secondary)
                Spacer()
                Text(macCompactNumber(value))
                    .font(.caption.monospacedDigit())
            }
            .font(.caption)

            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule()
                        .fill(PlatformColor.separator.opacity(0.55))
                    Capsule()
                        .fill(Color.accentColor.opacity(0.82))
                        .frame(width: max(percent > 0 ? 4 : 0, proxy.size.width * percent))
                }
            }
            .frame(height: 5)
        }
    }
}

private struct MacUsageBucketRow: View {
    let title: String
    let usage: ConversationUsageTotals

    var body: some View {
        HStack {
            Text(title)
                .foregroundStyle(.secondary)
            Spacer()
            Text(macCompactNumber(usage.totalTokens))
                .font(.caption.monospacedDigit())
            if usage.cost.total > 0 {
                Text(macFormatCost(usage.cost.total))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
        .font(.caption)
    }
}

private struct MacReasoningOption {
    let value: String
    let title: String
    let detail: String
}

private let reasoningOptions: [MacReasoningOption] = [
    MacReasoningOption(value: "low", title: "Low", detail: "Use lower reasoning effort"),
    MacReasoningOption(value: "medium", title: "Medium", detail: "Balanced reasoning effort"),
    MacReasoningOption(value: "high", title: "High", detail: "Use higher reasoning effort"),
    MacReasoningOption(value: "xhigh", title: "XHigh", detail: "Use maximum reasoning effort"),
    MacReasoningOption(value: "default", title: "Default", detail: "Restore model default")
]

private struct MacSandboxOption: Identifiable {
    var id: String { value }
    let value: String
    let title: String
    let detail: String
}

private let sandboxOptions = [
    MacSandboxOption(value: "default", title: "Default", detail: "Use the server's global sandbox configuration"),
    MacSandboxOption(value: "subprocess", title: "Subprocess", detail: "Run tools directly as subprocesses"),
    MacSandboxOption(value: "bubblewrap", title: "Bubblewrap", detail: "Use Linux bubblewrap isolation when available")
]

private func macCompactNumber(_ value: Int) -> String {
    let value = max(0, value)
    if value >= 1_000_000 {
        return String(format: "%.1fM", Double(value) / 1_000_000)
    }
    if value >= 10_000 {
        return "\(Int((Double(value) / 1_000).rounded()))K"
    }
    return value.formatted()
}

private func macFormatCost(_ value: Double) -> String {
    "$" + String(format: "%.3f", value)
}

private func macPanelBackground(_ colorScheme: ColorScheme) -> Color {
    colorScheme == .light ? Color.black.opacity(0.035) : PlatformColor.controlBackground.opacity(0.72)
}

private func macPanelBorder(_ colorScheme: ColorScheme) -> Color {
    colorScheme == .light ? Color.black.opacity(0.055) : Color.white.opacity(0.08)
}

private func macTileBackground(_ colorScheme: ColorScheme) -> Color {
    colorScheme == .light ? Color.white.opacity(0.96) : Color.white.opacity(0.08)
}

private func macTileBorder(_ colorScheme: ColorScheme) -> Color {
    colorScheme == .light ? Color.black.opacity(0.055) : Color.white.opacity(0.08)
}
#endif
