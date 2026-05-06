#if os(iOS)
import SwiftUI

struct IOSConversationListView: View {
    @ObservedObject var viewModel: AppViewModel
    var openConversation: (ConversationSummary.ID) -> Void = { _ in }
    @State private var editMode: EditMode = .inactive
    @State private var renameDraft = ConversationRenameDraft()
    @State private var isNewConversationPresented = false

    var body: some View {
        List {
            ForEach(viewModel.conversations) { conversation in
                Button {
                    guard !editMode.isEditing else {
                        return
                    }
                    openConversation(conversation.id)
                } label: {
                    IOSConversationRow(conversation: conversation)
                }
                .buttonStyle(.plain)
                .listRowInsets(EdgeInsets())
                .listRowSeparator(.hidden)
                .listRowBackground(PlatformColor.appBackground)
                .swipeActions(edge: .trailing, allowsFullSwipe: false) {
                    Button(role: .destructive) {
                        withAnimation(StellaCodeXMotion.standard) {
                            viewModel.deleteConversationWithUndo(id: conversation.id)
                        }
                    } label: {
                        Label("Delete", systemImage: "trash")
                    }

                    Button {
                        withAnimation(StellaCodeXMotion.quick) {
                            viewModel.toggleConversationPinned(id: conversation.id)
                        }
                    } label: {
                        Label(conversation.isPinned ? "Unpin" : "Pin", systemImage: conversation.isPinned ? "pin.slash" : "pin")
                    }
                    .tint(.orange)
                }
                .swipeActions(edge: .leading, allowsFullSwipe: true) {
                    Button {
                        viewModel.markConversationReadNow(id: conversation.id)
                    } label: {
                        Label("Read", systemImage: "checkmark.circle")
                    }
                    .tint(.blue)
                    .disabled(!conversation.isUnread)
                }
                .contextMenu {
                    Button {
                        renameDraft = ConversationRenameDraft(conversation: conversation)
                    } label: {
                        Label("Rename", systemImage: "pencil")
                    }

                    Button {
                        withAnimation(StellaCodeXMotion.quick) {
                            viewModel.toggleConversationPinned(id: conversation.id)
                        }
                    } label: {
                        Label(conversation.isPinned ? "Unpin" : "Pin", systemImage: conversation.isPinned ? "pin.fill" : "pin")
                    }

                    Button {
                        viewModel.markConversationReadNow(id: conversation.id)
                    } label: {
                        Label("Mark as Read", systemImage: "checkmark.circle")
                    }
                }
            }
            .onDelete { offsets in
                let ids = offsets.compactMap { index in
                    viewModel.conversations.indices.contains(index) ? viewModel.conversations[index].id : nil
                }
                withAnimation(StellaCodeXMotion.standard) {
                    ids.forEach { id in
                        viewModel.deleteConversationWithUndo(id: id)
                    }
                }
            }
        }
        .listStyle(.plain)
        .contentMargins(.top, 0, for: .scrollContent)
        .scrollContentBackground(.hidden)
        .environment(\.editMode, $editMode)
        .background(PlatformColor.appBackground)
        .safeAreaInset(edge: .top, spacing: 0) {
            IOSChatsHeader(
                onCreate: {
                    isNewConversationPresented = true
                }
            )
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            Color.clear
                .frame(height: viewModel.pendingConversationDeletion == nil ? 92 : 154)
        }
        .overlay {
            if viewModel.conversations.isEmpty {
                IOSNoChatsView()
            }
        }
        .overlay(alignment: .bottom) {
            if let deletion = viewModel.pendingConversationDeletion {
                IOSUndoDeleteBanner(deletion: deletion) {
                    withAnimation(StellaCodeXMotion.standard) {
                        viewModel.undoPendingConversationDeletion(id: deletion.id)
                    }
                }
                .padding(.horizontal, 16)
                .padding(.bottom, 92)
                .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .animation(StellaCodeXMotion.standard, value: viewModel.pendingConversationDeletion?.id)
        .toolbar(.hidden, for: .navigationBar)
        .sheet(isPresented: $isNewConversationPresented) {
            NewConversationSheetView(
                viewModel: viewModel,
                isPresented: $isNewConversationPresented
            )
        }
        .alert("Rename Conversation", isPresented: $renameDraft.isPresented) {
            TextField("Name", text: $renameDraft.name)
                .textInputAutocapitalization(.words)

            Button("Cancel", role: .cancel) {
                renameDraft = ConversationRenameDraft()
            }

            Button("Rename") {
                if let id = renameDraft.conversationID {
                    viewModel.renameConversation(id: id, nickname: renameDraft.name)
                }
                renameDraft = ConversationRenameDraft()
            }
            .disabled(renameDraft.name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        } message: {
            Text("Set a display name for this conversation.")
        }
    }
}

private struct ConversationRenameDraft {
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

private struct IOSChatsHeader: View {
    let onCreate: () -> Void

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                Text("Chats")
                    .font(.headline.weight(.semibold))
                    .frame(maxWidth: .infinity)

                HStack(alignment: .center) {
                    EditButton()
                    .font(.body.weight(.semibold))
                    .padding(.horizontal, 15)
                    .frame(height: 44)
                    .background(PlatformColor.secondaryBackground)
                    .clipShape(Capsule())

                    Spacer()

                    Button {
                        onCreate()
                    } label: {
                        Image(systemName: "square.and.pencil")
                            .font(.system(size: 20, weight: .medium))
                            .frame(width: 44, height: 44)
                            .background(PlatformColor.secondaryBackground)
                            .clipShape(Circle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel("New Chat")
                }
            }
            .padding(.horizontal, 20)
        }
        .padding(.top, 8)
        .padding(.bottom, 10)
        .background(.bar)
    }
}

private struct IOSConversationRow: View {
    let conversation: ConversationSummary

    var body: some View {
        HStack(alignment: .center, spacing: 14) {
            avatar

            VStack(alignment: .leading, spacing: 3) {
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(conversation.title)
                        .font(.body.weight(.semibold))
                        .foregroundStyle(.primary)
                        .lineLimit(1)

                    if conversation.isPinned {
                        Image(systemName: "pin.fill")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.tertiary)
                    }
                }

                Text("StellaClaw")
                    .font(.body)
                    .foregroundStyle(.primary)
                    .lineLimit(1)

                Text(preview)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            .padding(.trailing, hasRightStatus ? 42 : 0)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.leading, 20)
        .padding(.trailing, 16)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, minHeight: 86, idealHeight: 86, alignment: .center)
        .contentShape(Rectangle())
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(PlatformColor.separator)
                .frame(height: 0.5)
                .padding(.leading, 96)
                .padding(.trailing, 16)
        }
        .overlay(alignment: .trailing) {
            rightStatus
                .padding(.trailing, 20)
        }
        .background(PlatformColor.appBackground)
    }

    private var avatar: some View {
        ZStack {
            Circle()
                .fill(avatarGradient)
                .frame(width: 62, height: 62)

            Text(initials)
                .font(.system(size: 28, weight: .bold))
                .foregroundStyle(.white)
        }
    }

    private var initials: String {
        let pieces = conversation.title
            .split(separator: " ")
            .prefix(1)
            .compactMap(\.first)
        let value = String(pieces).uppercased()
        return value.isEmpty ? "S" : value
    }

    private var avatarGradient: LinearGradient {
        let palettes: [[Color]] = [
            [.blue, .indigo],
            [.orange, .yellow],
            [.pink, .red],
            [.purple, .cyan],
            [.teal, .green]
        ]
        let colors = palettes[abs(conversation.id.hashValue) % palettes.count]
        return LinearGradient(colors: colors, startPoint: .topLeading, endPoint: .bottomTrailing)
    }

    private var preview: String {
        let preview = conversation.lastMessagePreview.trimmingCharacters(in: .whitespacesAndNewlines)
        if !preview.isEmpty {
            return preview
        }
        if !conversation.workspacePath.isEmpty {
            return conversation.workspacePath
        }
        return conversation.model.isEmpty ? "No recent activity" : conversation.model
    }

    @ViewBuilder
    private var rightStatus: some View {
        if hasRightStatus {
            HStack(spacing: 6) {
                if conversation.status == .running {
                    ProgressView()
                        .controlSize(.mini)
                        .tint(.green)
                        .frame(width: 22, height: 22)
                        .background(Color.green.opacity(0.12))
                        .clipShape(Circle())
                        .accessibilityLabel("Agent working")
                } else if conversation.status == .failed {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.red)
                        .frame(width: 22, height: 22)
                        .background(Color.red.opacity(0.12))
                        .clipShape(Circle())
                }

                if conversation.isUnread {
                    Text(unreadBadgeText)
                        .font(.caption2.weight(.bold))
                        .foregroundStyle(.white)
                        .monospacedDigit()
                        .lineLimit(1)
                        .minimumScaleFactor(0.75)
                        .padding(.horizontal, unreadCount > 9 ? 6 : 7)
                        .frame(minWidth: 22, minHeight: 22)
                        .background(Color.red)
                        .clipShape(Capsule())
                        .shadow(color: Color.red.opacity(0.25), radius: 3)
                }
            }
        }
    }

    private var hasRightStatus: Bool {
        conversation.status == .running || conversation.status == .failed || conversation.isUnread
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

private struct IOSUndoDeleteBanner: View {
    let deletion: PendingConversationDeletion
    let undo: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "trash")
                .font(.system(size: 17, weight: .semibold))
                .foregroundStyle(.red)

            VStack(alignment: .leading, spacing: 2) {
                Text("Conversation deleted")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.primary)
                    .lineLimit(1)

                Text(deletion.title)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            Spacer(minLength: 12)

            Button("Undo") {
                undo()
            }
            .font(.subheadline.weight(.semibold))
            .buttonStyle(.borderedProminent)
            .controlSize(.small)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 12)
        .background(.regularMaterial)
        .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 18, style: .continuous)
                .strokeBorder(Color.primary.opacity(0.08))
        }
        .shadow(color: Color.black.opacity(0.18), radius: 18, x: 0, y: 8)
    }
}

private struct IOSNoChatsView: View {
    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "bubble.left.and.bubble.right")
                .font(.system(size: 58, weight: .regular))
                .foregroundStyle(.secondary)

            Text("No Chats")
                .font(.title3.weight(.bold))

            Text("Conversations will appear here.")
                .font(.subheadline)
                .foregroundStyle(.secondary)
        }
        .multilineTextAlignment(.center)
        .padding(.horizontal, 28)
    }
}

#Preview {
    NavigationStack {
        IOSConversationListView(viewModel: .mock())
    }
}
#endif
