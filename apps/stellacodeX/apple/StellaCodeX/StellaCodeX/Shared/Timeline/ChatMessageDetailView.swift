import SwiftUI

struct ChatMessageDetailView: View {
    let presentation: ChatDetailPresentation

    @State private var selectedToolID: ToolActivity.ID?

    init(presentation: ChatDetailPresentation) {
        self.presentation = presentation
        self._selectedToolID = State(initialValue: presentation.selectedToolID)
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    messageHeader

                    if !presentation.detail.displayText.isEmpty {
                        detailSection("Message") {
                            MarkdownContentView(text: presentation.detail.displayText)
                        }
                    }

                    if !presentation.detail.toolActivities.isEmpty {
                        detailSection("Tool Batch") {
                            toolBatchPreview
                        }

                        if let selectedTool {
                            detailSection(selectedTool.kind == .call ? "Tool Call" : "Tool Result") {
                                toolDetail(selectedTool)
                            }
                        }
                    }

                    if !presentation.detail.attachments.isEmpty || presentation.detail.attachmentCount > 0 || !presentation.detail.attachmentErrors.isEmpty {
                        detailSection("Attachments") {
                            VStack(alignment: .leading, spacing: 8) {
                                if !presentation.detail.attachments.isEmpty {
                                    AttachmentStripView(attachments: presentation.detail.attachments)
                                } else if presentation.detail.attachmentCount > 0 {
                                    Label("\(presentation.detail.attachmentCount) attachment(s)", systemImage: "paperclip")
                                        .foregroundStyle(.secondary)
                                }

                                ForEach(presentation.detail.attachmentErrors, id: \.self) { error in
                                    Text(error)
                                        .font(.caption)
                                        .foregroundStyle(.red)
                                        .textSelection(.enabled)
                                }
                            }
                        }
                    }
                }
                .padding(20)
                .frame(maxWidth: 820, alignment: .leading)
            }
            .navigationTitle("Message Detail")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
        }
    }

    private var messageHeader: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(presentation.detail.message.role.rawValue)
                .font(.headline)

            Text("#\(presentation.detail.message.index)")
                .font(.subheadline)
                .foregroundStyle(.secondary)

            Spacer()

            Text(presentation.detail.message.timestamp, style: .time)
                .font(.subheadline)
                .foregroundStyle(.secondary)
        }
    }

    private var toolBatchPreview: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Label(batchTitle, systemImage: "wrench.and.screwdriver")
                    .font(.subheadline.weight(.semibold))

                Spacer()
            }

            ForEach(presentation.detail.toolActivities) { activity in
                Button {
                    selectedToolID = activity.id
                } label: {
                    HStack(alignment: .top, spacing: 10) {
                        Image(systemName: activity.kind == .call ? "arrow.up.right.circle" : "checkmark.circle")
                            .foregroundStyle(activity.kind == .call ? Color.orange : Color.green)
                            .frame(width: 18)

                        VStack(alignment: .leading, spacing: 4) {
                            HStack(spacing: 6) {
                                Text(activity.kind.rawValue)
                                    .font(.caption.weight(.semibold))
                                    .foregroundStyle(.secondary)

                                Text(activity.name)
                                    .font(.subheadline.weight(.semibold))
                                    .foregroundStyle(.primary)
                            }

                            Text(activity.summary)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .lineLimit(2)
                        }

                        Spacer(minLength: 8)

                        Image(systemName: selectedToolID == activity.id ? "checkmark.circle.fill" : "chevron.right")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(selectedToolID == activity.id ? Color.accentColor : Color.secondary)
                    }
                    .padding(10)
                    .background(selectedToolID == activity.id ? Color.accentColor.opacity(0.12) : PlatformColor.secondaryBackground.opacity(0.65))
                    .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
                }
                .buttonStyle(.plain)
            }
        }
    }

    private func toolDetail(_ activity: ToolActivity) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Text(activity.name)
                    .font(.headline)

                Text(activity.kind.rawValue)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
            }

            ToolDetailContentView(activity: activity)
        }
    }

    private func detailSection<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)

            content()
        }
        .padding(14)
        .background(PlatformColor.controlBackground.opacity(0.72))
        .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    private var selectedTool: ToolActivity? {
        let selectedID = selectedToolID ?? presentation.detail.toolActivities.first?.id
        return presentation.detail.toolActivities.first { $0.id == selectedID }
    }

    private var batchTitle: String {
        let calls = presentation.detail.toolActivities.filter { $0.kind == .call }.count
        let results = presentation.detail.toolActivities.filter { $0.kind == .result }.count
        if calls > 0 && results > 0 {
            return "\(calls) call(s), \(results) result(s)"
        }
        if calls > 0 {
            return "\(calls) tool call(s)"
        }
        return "\(results) tool result(s)"
    }
}
