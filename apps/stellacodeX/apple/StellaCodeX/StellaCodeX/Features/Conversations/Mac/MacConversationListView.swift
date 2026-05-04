#if os(macOS)
import AppKit
import SwiftUI

struct MacConversationListView: View {
    @ObservedObject var viewModel: AppViewModel
    @State private var renameDraft = MacConversationRenameDraft()
    @State private var isNewConversationPresented = false

    var body: some View {
        VStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 7) {
                Button {
                    isNewConversationPresented = true
                } label: {
                    SidebarActionRow(systemName: "square.and.pencil", title: "新对话", isSelected: false)
                }
                .buttonStyle(.plain)
            }
            .padding(.horizontal, 10)
            .padding(.top, 16)

            ScrollView {
                LazyVStack(alignment: .leading, spacing: 18) {
                    ConversationSection(
                        title: "Conversations",
                        conversations: viewModel.conversations,
                        selectedConversationID: viewModel.selectedConversationID,
                        select: viewModel.selectConversation,
                        markRead: viewModel.markConversationReadNow,
                        rename: { conversation in
                            renameDraft = MacConversationRenameDraft(conversation: conversation)
                        },
                        pin: { conversation in
                            viewModel.toggleConversationPinned(id: conversation.id)
                        },
                        delete: viewModel.deleteConversationWithUndo
                    )

                    Color.clear
                        .frame(height: viewModel.pendingConversationDeletion == nil ? 0 : 76)
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 14)
            }
            .overlay {
                if viewModel.conversations.isEmpty {
                    MacEmptyConversationsView()
                }
            }
        }
        .background(PlatformColor.sidebarBackground)
        .overlay(alignment: .bottom) {
            if let deletion = viewModel.pendingConversationDeletion {
                MacUndoDeleteBanner(deletion: deletion) {
                    viewModel.undoPendingConversationDeletion(id: deletion.id)
                }
                .padding(.horizontal, 12)
                .padding(.bottom, 12)
                .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .animation(.easeInOut(duration: 0.18), value: viewModel.conversations.map(\.id))
        .animation(.easeInOut(duration: 0.18), value: viewModel.pendingConversationDeletion)
        .sheet(isPresented: $isNewConversationPresented) {
            NewConversationSheetView(
                viewModel: viewModel,
                isPresented: $isNewConversationPresented
            )
        }
        .alert("Rename Conversation", isPresented: $renameDraft.isPresented) {
            TextField("Name", text: $renameDraft.name)

            Button("Cancel", role: .cancel) {
                renameDraft = MacConversationRenameDraft()
            }

            Button("Rename") {
                if let id = renameDraft.conversationID {
                    viewModel.renameConversation(id: id, nickname: renameDraft.name)
                }
                renameDraft = MacConversationRenameDraft()
            }
            .disabled(renameDraft.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        } message: {
            Text("Set a display name for this conversation.")
        }
    }
}

private struct MacConversationRenameDraft {
    var conversationID: ConversationSummary.ID?
    var name = ""

    init() {
    }

    init(conversation: ConversationSummary) {
        self.conversationID = conversation.id
        self.name = conversation.title
    }

    var isPresented: Bool {
        get {
            conversationID != nil
        }
        set {
            if !newValue {
                conversationID = nil
                name = ""
            }
        }
    }
}

private struct SidebarActionRow: View {
    let systemName: String
    let title: String
    let isSelected: Bool

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: systemName)
                .font(.system(size: 13, weight: .medium))
                .frame(width: 18)

            Text(title)
                .font(.system(size: 14))
                .lineLimit(1)

            Spacer(minLength: 0)
        }
        .foregroundStyle(isSelected ? .primary : .secondary)
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .background(isSelected ? PlatformColor.sidebarSelection : Color.clear)
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        .contentShape(Rectangle())
    }
}

private struct ConversationSection: View {
    let title: String
    let conversations: [ConversationSummary]
    let selectedConversationID: ConversationSummary.ID?
    let select: (ConversationSummary.ID) -> Void
    let markRead: (ConversationSummary.ID) -> Void
    let rename: (ConversationSummary) -> Void
    let pin: (ConversationSummary) -> Void
    let delete: (ConversationSummary.ID) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.tertiary)
                .padding(.horizontal, 8)

            ForEach(conversations) { conversation in
                Button {
                    select(conversation.id)
                } label: {
                    ConversationRow(
                        conversation: conversation,
                        isSelected: selectedConversationID == conversation.id
                    )
                }
                .buttonStyle(.plain)
                .contextMenu {
                    Button {
                        rename(conversation)
                    } label: {
                        Label("重命名", systemImage: "pencil")
                    }

                    Button {
                        pin(conversation)
                    } label: {
                        Label(conversation.isPinned ? "取消置顶" : "置顶", systemImage: conversation.isPinned ? "pin.fill" : "pin")
                    }

                    Button {
                        markRead(conversation.id)
                    } label: {
                        Label("标记为已读", systemImage: "checkmark.circle")
                    }

                    Button(role: .destructive) {
                        delete(conversation.id)
                    } label: {
                        Label("删除", systemImage: "trash")
                    }
                }
            }
        }
    }
}

private struct ConversationRow: View {
    let conversation: ConversationSummary
    let isSelected: Bool

    var body: some View {
        HStack(alignment: .center, spacing: 9) {
            avatar

            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(conversation.title)
                        .font(.system(size: 14, weight: isSelected ? .semibold : .regular))
                        .lineLimit(1)

                    if conversation.isPinned {
                        Image(systemName: "pin.fill")
                            .font(.system(size: 10, weight: .semibold))
                            .foregroundStyle(.tertiary)
                    }

                    Spacer(minLength: 0)

                    rightStatus
                }

                Text(conversation.lastMessagePreview.isEmpty ? conversation.workspacePath : conversation.lastMessagePreview)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 8)
        .background(isSelected ? PlatformColor.sidebarSelection : Color.clear)
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        .contentShape(Rectangle())
    }

    private var avatar: some View {
        ZStack(alignment: .bottomTrailing) {
            Circle()
                .fill(
                    LinearGradient(
                        colors: avatarColors,
                        startPoint: .topLeading,
                        endPoint: .bottomTrailing
                    )
                )
                .frame(width: 30, height: 30)
                .overlay {
                    Text(avatarText)
                        .font(.system(size: 12, weight: .bold))
                        .foregroundStyle(.white)
                        .lineLimit(1)
                }

            if conversation.status != .idle {
                Circle()
                    .fill(statusColor)
                    .frame(width: 9, height: 9)
                    .overlay {
                        Circle()
                            .stroke(PlatformColor.sidebarBackground, lineWidth: 2)
                    }
                    .offset(x: 1, y: 1)
            }
        }
        .accessibilityLabel(conversation.status.rawValue)
    }

    private var statusColor: Color {
        switch conversation.status {
        case .idle:
            .secondary
        case .running:
            .green
        case .failed:
            .red
        }
    }

    private var avatarText: String {
        conversation.title.trimmingCharacters(in: .whitespacesAndNewlines).first.map(String.init) ?? "S"
    }

    private var avatarColors: [Color] {
        let palette: [[Color]] = [
            [.blue, .purple],
            [.orange, .pink],
            [.teal, .blue],
            [.indigo, .cyan],
            [.green, .mint],
            [.red, .orange]
        ]
        let sum = conversation.id.unicodeScalars.reduce(0) { $0 + Int($1.value) }
        return palette[sum % palette.count]
    }

    @ViewBuilder
    private var rightStatus: some View {
        HStack(spacing: 5) {
            if conversation.status == .running {
                HStack(spacing: 4) {
                    ProgressView()
                        .controlSize(.mini)

                    Text("工作中")
                        .font(.caption2.weight(.semibold))
                }
                .foregroundStyle(.green)
                .padding(.horizontal, 6)
                .padding(.vertical, 3)
                .background(Color.green.opacity(0.12))
                .clipShape(Capsule())
            } else if conversation.status == .failed {
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.red)
            }

            if conversation.isUnread && !isSelected {
                Text(unreadBadgeText)
                    .font(.caption2.weight(.bold))
                    .foregroundStyle(.white)
                    .monospacedDigit()
                    .padding(.horizontal, unreadCount > 9 ? 5 : 6)
                    .frame(minWidth: 18, minHeight: 18)
                    .background(Color.red)
                    .clipShape(Capsule())
                    .shadow(color: Color.red.opacity(0.2), radius: 2)
            }
        }
    }

    private var unreadCount: Int {
        guard conversation.isUnread,
              let last = Int(conversation.lastMessageID ?? "")
        else {
            return 0
        }
        let seen = Int(conversation.lastSeenMessageID ?? "") ?? -1
        return max(last - seen, 1)
    }

    private var unreadBadgeText: String {
        unreadCount > 99 ? "99+" : "\(max(unreadCount, 1))"
    }
}

private struct MacEmptyConversationsView: View {
    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "bubble.left.and.bubble.right")
                .font(.system(size: 28, weight: .medium))
                .foregroundStyle(.tertiary)

            Text("No Conversations")
                .font(.headline)

            Text("Start a new chat from the sidebar.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(.horizontal, 24)
    }
}

private struct MacUndoDeleteBanner: View {
    let deletion: PendingConversationDeletion
    let undo: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Conversation deleted")
                    .font(.caption.weight(.semibold))
                Text(deletion.title)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            Spacer(minLength: 8)

            Button("Undo") {
                undo()
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(.regularMaterial)
        .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        .shadow(color: .black.opacity(0.16), radius: 18, y: 8)
    }
}

#Preview {
    MacConversationListView(viewModel: .mock())
}
#endif
