#if os(iOS)
import SwiftUI

struct IOSConversationActionsView: View {
    @ObservedObject var viewModel: AppViewModel
    let onRename: () -> Void

    var body: some View {
        List {
            if let conversation = viewModel.selectedConversation {
                Section {
                    VStack(alignment: .leading, spacing: 10) {
                        Text(conversation.title)
                            .font(.title3.weight(.semibold))
                            .lineLimit(2)

                        HStack(spacing: 8) {
                            Label(conversation.status.rawValue, systemImage: statusIcon(conversation.status))
                            Text(conversation.model.isEmpty ? "model pending" : conversation.model)
                        }
                        .font(.caption)
                        .foregroundStyle(.secondary)

                        if !conversation.workspacePath.isEmpty {
                            Label(conversation.workspacePath, systemImage: "folder")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .lineLimit(2)
                        }
                    }
                    .padding(.vertical, 4)

                    LabeledContent("Reasoning", value: conversation.reasoning.isEmpty ? "default" : conversation.reasoning)
                    LabeledContent("Sandbox", value: sandboxSummary(conversation))
                    LabeledContent("Remote", value: conversation.remote.isEmpty ? "local" : conversation.remote)
                    LabeledContent("Messages", value: "\(conversation.messageCount)")
                } header: {
                    Text("Conversation")
                }

                Section {
                    if let status = currentStatus {
                        UsageOverviewView(status: status)
                    } else if let error = viewModel.selectedConversationStatusError {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.red)
                        Button {
                            Task {
                                await viewModel.loadSelectedConversationStatus()
                            }
                        } label: {
                            Label("Reload Usage", systemImage: "arrow.clockwise")
                        }
                    } else {
                        HStack(spacing: 10) {
                            ProgressView()
                            Text("Loading usage")
                                .foregroundStyle(.secondary)
                        }
                    }
                } header: {
                    Text("Usage")
                }
            }

            Section("Commands") {
                NavigationLink {
                    IOSModelCommandView(viewModel: viewModel)
                } label: {
                    CommandRow(icon: "cpu", title: "Model", detail: "Switch current conversation model")
                }

                NavigationLink {
                    IOSReasoningCommandView(viewModel: viewModel)
                } label: {
                    CommandRow(icon: "brain.head.profile", title: "Reasoning", detail: "Adjust reasoning effort")
                }

                NavigationLink {
                    IOSRemoteCommandView(viewModel: viewModel)
                } label: {
                    CommandRow(icon: "point.3.connected.trianglepath.dotted", title: "Remote", detail: "Set SSH host and workspace directory")
                }

                NavigationLink {
                    IOSSandboxCommandView(viewModel: viewModel)
                } label: {
                    CommandRow(icon: "lock.shield", title: "Sandbox", detail: "Change execution isolation mode")
                }
            }

            Section("Actions") {
                Button {
                    Task {
                        await viewModel.loadSelectedConversation()
                        await viewModel.loadSelectedConversationStatus()
                    }
                } label: {
                    Label("Refresh Conversation", systemImage: "arrow.clockwise")
                }

                Button {
                    onRename()
                } label: {
                    Label("Rename Conversation", systemImage: "pencil")
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
                    Label("Cancel Current Run", systemImage: "stop.circle")
                }
            }
        }
        .navigationTitle("Actions")
        .navigationBarTitleDisplayMode(.inline)
        .task(id: viewModel.selectedConversationID) {
            await viewModel.loadSelectedConversationStatus()
        }
    }

    private var currentStatus: ConversationStatusSnapshot? {
        guard viewModel.selectedConversationStatus?.conversationID == viewModel.selectedConversationID else {
            return nil
        }
        return viewModel.selectedConversationStatus
    }

    private func statusIcon(_ status: ConversationStatus) -> String {
        switch status {
        case .idle:
            "checkmark.circle"
        case .running:
            "circle.dotted"
        case .failed:
            "exclamationmark.triangle"
        }
    }

    private func sandboxSummary(_ conversation: ConversationSummary) -> String {
        let mode = conversation.sandbox.isEmpty ? "pending" : conversation.sandbox
        guard let source = conversation.sandboxSource, !source.isEmpty else {
            return mode
        }
        return "\(mode) · \(source)"
    }
}

private struct UsageOverviewView: View {
    let status: ConversationStatusSnapshot

    private var totalUsage: ConversationUsageTotals {
        status.usage.total
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 8) {
                UsageMetricTile(title: "Cache", value: "\(Int((totalUsage.cacheHitRate * 100).rounded()))%")
                UsageMetricTile(title: "Tokens", value: compactNumber(totalUsage.totalTokens))
                UsageMetricTile(title: "Cost", value: formatCost(totalUsage.cost.total))
            }

            VStack(spacing: 10) {
                UsageProgressRow(label: "Cache Read", value: totalUsage.cacheRead, total: totalUsage.totalTokens)
                UsageProgressRow(label: "Cache Write", value: totalUsage.cacheWrite, total: totalUsage.totalTokens)
                UsageProgressRow(label: "Input", value: totalUsage.input, total: totalUsage.totalTokens)
                UsageProgressRow(label: "Output", value: totalUsage.output, total: totalUsage.totalTokens)
            }

            Divider()

            VStack(spacing: 8) {
                UsageBucketRow(title: "Foreground", usage: status.usage.foreground)
                UsageBucketRow(title: "Background", usage: status.usage.background)
                UsageBucketRow(title: "Subagents", usage: status.usage.subagents)
                UsageBucketRow(title: "Media Tools", usage: status.usage.mediaTools)
            }

            Divider()

            VStack(spacing: 8) {
                LabeledContent("Background", value: "\(status.runningBackground) / \(status.totalBackground)")
                LabeledContent("Subagents", value: "\(status.runningSubagents) / \(status.totalSubagents)")
            }
            .font(.caption)
        }
        .padding(.vertical, 6)
    }
}

private struct UsageMetricTile: View {
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
        .background(Color.secondary.opacity(0.10), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

private struct UsageProgressRow: View {
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
                Text(compactNumber(value))
                    .font(.caption.monospacedDigit())
            }
            .font(.caption)

            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule()
                        .fill(Color.secondary.opacity(0.14))
                    Capsule()
                        .fill(Color.accentColor.opacity(0.82))
                        .frame(width: max(percent > 0 ? 4 : 0, proxy.size.width * percent))
                }
            }
            .frame(height: 5)
        }
    }
}

private struct UsageBucketRow: View {
    let title: String
    let usage: ConversationUsageTotals

    var body: some View {
        HStack {
            Text(title)
                .foregroundStyle(.secondary)
            Spacer()
            Text(compactNumber(usage.totalTokens))
                .font(.caption.monospacedDigit())
            if usage.cost.total > 0 {
                Text(formatCost(usage.cost.total))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
        .font(.caption)
    }
}

private func compactNumber(_ value: Int) -> String {
    let value = max(0, value)
    if value >= 1_000_000 {
        return String(format: "%.1fM", Double(value) / 1_000_000)
    }
    if value >= 10_000 {
        return "\(Int((Double(value) / 1_000).rounded()))K"
    }
    return value.formatted()
}

private func formatCost(_ value: Double) -> String {
    "$" + String(format: "%.3f", value)
}

private struct IOSModelCommandView: View {
    @ObservedObject var viewModel: AppViewModel

    var body: some View {
        List {
            if let error = viewModel.modelsError {
                Section {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                    Button("Reload Models") {
                        Task {
                            await viewModel.loadModels()
                        }
                    }
                }
            }

            Section("Available Models") {
                ForEach(viewModel.availableModels) { model in
                    Button {
                        viewModel.switchModel(model)
                    } label: {
                        HStack {
                            VStack(alignment: .leading, spacing: 3) {
                                Text(model.alias)
                                    .foregroundStyle(.primary)
                                Text("\(model.providerType) - \(model.modelName)")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                            if model.alias == viewModel.selectedConversation?.model {
                                Image(systemName: "checkmark.circle.fill")
                                    .foregroundStyle(Color.accentColor)
                            }
                        }
                    }
                }
            }
        }
        .navigationTitle("Model")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            if viewModel.availableModels.isEmpty {
                await viewModel.loadModels()
            }
        }
    }
}

private struct IOSReasoningCommandView: View {
    @ObservedObject var viewModel: AppViewModel

    private let options = [
        ("low", "Low", "Use lower reasoning effort"),
        ("medium", "Medium", "Balanced reasoning effort"),
        ("high", "High", "Use higher reasoning effort"),
        ("xhigh", "XHigh", "Use maximum reasoning effort"),
        ("default", "Default", "Restore model default")
    ]

    var body: some View {
        List {
            Section("Reasoning Effort") {
                ForEach(options, id: \.0) { value, title, detail in
                    Button {
                        viewModel.switchReasoning(value)
                    } label: {
                        HStack {
                            VStack(alignment: .leading, spacing: 3) {
                                Text(title)
                                    .foregroundStyle(.primary)
                                Text(detail)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                            if value == viewModel.selectedConversation?.reasoning {
                                Image(systemName: "checkmark.circle.fill")
                                    .foregroundStyle(Color.accentColor)
                            }
                        }
                    }
                }
            }
        }
        .navigationTitle("Reasoning")
        .navigationBarTitleDisplayMode(.inline)
    }
}

private struct IOSRemoteCommandView: View {
    @ObservedObject var viewModel: AppViewModel
    @State private var host = ""
    @State private var cwd = ""

    var body: some View {
        List {
            if let conversation = viewModel.selectedConversation {
                Section("Current") {
                    LabeledContent("Remote", value: conversation.remote.isEmpty ? "local" : conversation.remote)
                }
            }

            Section("Fixed SSH Workspace") {
                TextField("SSH host", text: $host)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                TextField("Workspace directory", text: $cwd)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()

                Button {
                    let trimmedHost = host.trimmingCharacters(in: .whitespacesAndNewlines)
                    let trimmedCwd = cwd.trimmingCharacters(in: .whitespacesAndNewlines)
                    guard !trimmedHost.isEmpty, !trimmedCwd.isEmpty else {
                        return
                    }
                    viewModel.sendConversationCommand("/remote \(trimmedHost) \(trimmedCwd)")
                } label: {
                    Label("Apply Remote", systemImage: "checkmark.circle")
                }
                .disabled(host.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || cwd.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }

            Section {
                Button {
                    viewModel.sendConversationCommand("/remote off")
                } label: {
                    Label("Use Local Workspace", systemImage: "desktopcomputer")
                }
            }
        }
        .navigationTitle("Remote")
        .navigationBarTitleDisplayMode(.inline)
    }
}

private struct IOSSandboxCommandView: View {
    @ObservedObject var viewModel: AppViewModel

    private let options = [
        SandboxOption(value: "default", title: "Default", detail: "Use the server's global sandbox configuration"),
        SandboxOption(value: "subprocess", title: "Subprocess", detail: "Run tools directly as subprocesses"),
        SandboxOption(value: "bubblewrap", title: "Bubblewrap", detail: "Use Linux bubblewrap isolation when available")
    ]

    var body: some View {
        List {
            Section("Sandbox") {
                ForEach(options) { option in
                    Button {
                        viewModel.sendConversationCommand("/sandbox \(option.value)")
                    } label: {
                        HStack {
                            VStack(alignment: .leading, spacing: 3) {
                                Text(option.title)
                                    .foregroundStyle(.primary)
                                Text(option.detail)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                            if isSelected(option) {
                                Image(systemName: "checkmark.circle.fill")
                                    .foregroundStyle(Color.accentColor)
                            }
                        }
                    }
                }
            }
        }
        .navigationTitle("Sandbox")
        .navigationBarTitleDisplayMode(.inline)
    }

    private func isSelected(_ option: SandboxOption) -> Bool {
        guard let conversation = viewModel.selectedConversation else {
            return false
        }
        if option.value == "default" {
            return conversation.sandboxSource == "default"
        }
        return conversation.sandbox == option.value && conversation.sandboxSource != "default"
    }
}

private struct SandboxOption: Identifiable {
    var id: String { value }
    let value: String
    let title: String
    let detail: String
}

private struct CommandRow: View {
    let icon: String
    let title: String
    let detail: String

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: icon)
                .foregroundStyle(Color.accentColor)
                .frame(width: 24)
            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }
}
#endif
