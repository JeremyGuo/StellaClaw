import Foundation
import Combine
import SwiftUI
import UserNotifications

@MainActor
final class AppViewModel: ObservableObject {
    @Published private(set) var profile: ServerProfile
    @Published private(set) var conversations: [ConversationSummary] {
        didSet {
            ConversationCacheStore.save(conversations, profile: profile)
        }
    }
    @Published private(set) var messages: [ChatMessage]
    @Published private(set) var availableModels: [ModelSummary]
    @Published private(set) var modelsError: String?
    @Published var selectedConversationID: ConversationSummary.ID?
    @Published var composerText: String
    @Published private(set) var sshPublicKey: String
    @Published private(set) var sshIdentityError: String?
    @Published private(set) var realtimeStatus: String
    @Published private(set) var hasOlderMessages: Bool
    @Published private(set) var isLoadingMessages: Bool
    @Published private(set) var isLoadingOlderMessages: Bool
    @Published private(set) var pendingConversationDeletion: PendingConversationDeletion?
    @Published private(set) var selectedConversationStatus: ConversationStatusSnapshot?
    @Published private(set) var selectedConversationStatusError: String?
    @Published private(set) var activeTurnProgress: TurnProgressFeedback?
    @Published private(set) var messageCacheStats: MessageCacheStats
    @Published var detailPresentation: ChatDetailPresentation?

    private var client: StellaAPIClient
    private let makeClient: (ServerProfile) -> StellaAPIClient
    private var realtimeTask: Task<Void, Never>?
    private var conversationStreamTask: Task<Void, Never>?
    private var seenSaveTasks: [ConversationSummary.ID: Task<Void, Never>] = [:]
    private var deletionTasks: [ConversationSummary.ID: Task<Void, Never>] = [:]
    private var deletionContexts: [ConversationSummary.ID: ConversationDeletionContext] = [:]
    private var pinnedConversationIDs: Set<ConversationSummary.ID>
    private let pageSize = 50
    private let automaticVisibleMessageLimit = 160

    init(
        profile: ServerProfile,
        client: StellaAPIClient,
        makeClient: ((ServerProfile) -> StellaAPIClient)? = nil
    ) {
        self.profile = profile
        self.client = client
        self.makeClient = makeClient ?? { _ in client }
        self.conversations = []
        self.messages = []
        self.availableModels = []
        self.modelsError = nil
        self.selectedConversationID = nil
        self.composerText = ""
        self.sshPublicKey = ""
        self.sshIdentityError = nil
        self.realtimeStatus = "Disconnected"
        self.hasOlderMessages = false
        self.isLoadingMessages = false
        self.isLoadingOlderMessages = false
        self.pendingConversationDeletion = nil
        self.selectedConversationStatus = nil
        self.selectedConversationStatusError = nil
        self.activeTurnProgress = nil
        self.messageCacheStats = MessageCacheStore.stats()
        self.detailPresentation = nil
        self.pinnedConversationIDs = ConversationPinStore.load(profile: profile)
        self.conversations = ConversationCacheStore.load(profile: profile)
            .map { cached in
                var cached = cached
                cached.isPinned = self.pinnedConversationIDs.contains(cached.id)
                return cached
            }
            .sorted(by: { left, right in
                if left.isPinned != right.isPinned {
                    return left.isPinned && !right.isPinned
                }
                if left.updatedAt != right.updatedAt {
                    return left.updatedAt > right.updatedAt
                }
                return left.id < right.id
            })
        refreshSSHIdentity()
        MessageCacheStore.removeExpired()
        messageCacheStats = MessageCacheStore.stats()
    }

    deinit {
        realtimeTask?.cancel()
        conversationStreamTask?.cancel()
        seenSaveTasks.values.forEach { $0.cancel() }
        deletionTasks.values.forEach { $0.cancel() }
    }

    var selectedConversation: ConversationSummary? {
        conversations.first { $0.id == selectedConversationID }
    }

    var selectedConversationRequiresModel: Bool {
        guard let selectedConversation else {
            return false
        }
        return selectedConversation.modelSelectionPending == true
            || selectedConversation.model.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    func conversation(id: ConversationSummary.ID) -> ConversationSummary? {
        conversations.first { $0.id == id }
    }

    func selectConversation(id: ConversationSummary.ID) {
        guard selectedConversationID != id else {
            return
        }
        selectedConversationID = id
        prepareConversationEntry()
        startRealtimeForSelectedConversation()
    }

    func clearSelectedConversationForList() {
        guard selectedConversationID != nil else {
            return
        }
        selectedConversationID = nil
        messages = []
        hasOlderMessages = false
        isLoadingMessages = false
        isLoadingOlderMessages = false
        detailPresentation = nil
        selectedConversationStatus = nil
        selectedConversationStatusError = nil
        realtimeTask?.cancel()
        realtimeStatus = "Disconnected"
    }

    func deleteConversation(id: ConversationSummary.ID) {
        removeConversation(id: id, deleteRemote: true)
    }

    func deleteConversationWithUndo(id: ConversationSummary.ID) {
        scheduleConversationDeletion(id: id)
    }

    func undoPendingConversationDeletion(id: ConversationSummary.ID) {
        guard let context = deletionContexts[id] else {
            return
        }

        deletionTasks[id]?.cancel()
        deletionTasks[id] = nil
        deletionContexts[id] = nil
        if pendingConversationDeletion?.id == id {
            refreshPendingDeletionBanner()
        }

        pinnedConversationIDs = context.pinnedConversationIDs
        ConversationPinStore.save(pinnedConversationIDs, profile: profile)

        var restored = context.conversation
        restored.isPinned = pinnedConversationIDs.contains(restored.id)
        if conversations.contains(where: { $0.id == id }) == false {
            conversations.insert(restored, at: min(context.index, conversations.count))
            conversations.sort(by: sortConversations)
        }

        if context.wasSelected {
            selectedConversationID = id
            messages = context.messages
            isLoadingMessages = false
            hasOlderMessages = context.hasOlderMessages
            detailPresentation = nil
            startRealtimeForSelectedConversation()
        }
    }

    func toggleConversationPinned(id: ConversationSummary.ID) {
        if pinnedConversationIDs.contains(id) {
            pinnedConversationIDs.remove(id)
        } else {
            pinnedConversationIDs.insert(id)
        }
        ConversationPinStore.save(pinnedConversationIDs, profile: profile)

        if let index = conversations.firstIndex(where: { $0.id == id }) {
            conversations[index].isPinned = pinnedConversationIDs.contains(id)
            conversations.sort(by: sortConversations)
        }
    }

    func markConversationReadNow(id: ConversationSummary.ID) {
        guard let conversation = conversations.first(where: { $0.id == id }),
              let lastMessageID = conversation.lastMessageID
        else {
            return
        }
        markConversationRead(conversationID: id, lastMessageID: lastMessageID)
    }

    func renameConversation(id: ConversationSummary.ID, nickname: String) {
        let trimmed = nickname.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let index = conversations.firstIndex(where: { $0.id == id }),
              conversations[index].title != trimmed
        else {
            return
        }

        let previous = conversations[index]
        conversations[index].title = trimmed

        Task {
            do {
                var updated = try await client.renameConversation(id: id, nickname: trimmed)
                updated.isPinned = pinnedConversationIDs.contains(id)
                if let currentIndex = conversations.firstIndex(where: { $0.id == id }) {
                    conversations[currentIndex] = updated
                    conversations.sort(by: sortConversations)
                }
            } catch {
                if let currentIndex = conversations.firstIndex(where: { $0.id == id }) {
                    conversations[currentIndex] = previous
                }
                appendSystemError("Failed to rename conversation: \(error.localizedDescription)")
            }
        }
    }

    private func removeConversation(id: ConversationSummary.ID, deleteRemote: Bool) {
        let removedIndex = conversations.firstIndex { $0.id == id }
        let removedConversation = removedIndex.map { conversations[$0] }
        let removedMessages = messages
        let wasSelected = selectedConversationID == id
        let wasPinned = pinnedConversationIDs.contains(id)
        pinnedConversationIDs.remove(id)
        ConversationPinStore.save(pinnedConversationIDs, profile: profile)
        conversations.removeAll { $0.id == id }
        MessageCacheStore.remove(conversationID: id, profile: profile)
        messageCacheStats = MessageCacheStore.stats()

        if wasSelected {
            selectedConversationID = conversations.first?.id
            prepareConversationEntry()
            realtimeTask?.cancel()
            realtimeStatus = "Disconnected"
        }

        Task {
            do {
                if deleteRemote {
                    try await client.deleteConversation(id: id)
                }
                if wasSelected {
                    startRealtimeForSelectedConversation()
                }
            } catch {
                if let removedConversation {
                    if wasPinned {
                        pinnedConversationIDs.insert(id)
                        ConversationPinStore.save(pinnedConversationIDs, profile: profile)
                    }
                    conversations.insert(removedConversation, at: min(conversations.count, removedIndex ?? 0))
                    conversations.sort(by: sortConversations)
                    cacheMessages(removedMessages, conversationID: id, total: removedConversation.messageCount)
                }
                if wasSelected {
                    selectedConversationID = id
                    messages = removedMessages
                    startRealtimeForSelectedConversation()
                }
                appendSystemError("Failed to delete conversation: \(error.localizedDescription)")
            }
        }
    }

    private func scheduleConversationDeletion(id: ConversationSummary.ID) {
        guard deletionContexts[id] == nil,
              let removedIndex = conversations.firstIndex(where: { $0.id == id })
        else {
            return
        }

        let removedConversation = conversations[removedIndex]
        let wasSelected = selectedConversationID == id
        let context = ConversationDeletionContext(
            conversation: removedConversation,
            index: removedIndex,
            messages: wasSelected ? messages : [],
            hasOlderMessages: hasOlderMessages,
            wasSelected: wasSelected,
            pinnedConversationIDs: pinnedConversationIDs
        )

        deletionContexts[id] = context
        pendingConversationDeletion = PendingConversationDeletion(id: id, title: removedConversation.title)
        pinnedConversationIDs.remove(id)
        ConversationPinStore.save(pinnedConversationIDs, profile: profile)
        conversations.remove(at: removedIndex)

        if wasSelected {
            selectedConversationID = conversations.first?.id
            prepareConversationEntry()
            startRealtimeForSelectedConversation()
        }

        deletionTasks[id]?.cancel()
        deletionTasks[id] = Task { [weak self] in
            do {
                try await Task.sleep(nanoseconds: 5_000_000_000)
                guard let self, !Task.isCancelled else {
                    return
                }
                try await self.commitPendingConversationDeletion(id: id)
            } catch is CancellationError {
            } catch {
                self?.restoreFailedPendingConversationDeletion(id: id, error: error)
            }
        }
    }

    private func commitPendingConversationDeletion(id: ConversationSummary.ID) async throws {
        guard deletionContexts[id] != nil else {
            return
        }

        try await client.deleteConversation(id: id)
        deletionContexts[id] = nil
        deletionTasks[id] = nil
        if pendingConversationDeletion?.id == id {
            refreshPendingDeletionBanner()
        }
    }

    private func restoreFailedPendingConversationDeletion(id: ConversationSummary.ID, error: Error) {
        guard let context = deletionContexts[id] else {
            return
        }

        deletionContexts[id] = nil
        deletionTasks[id] = nil
        if pendingConversationDeletion?.id == id {
            refreshPendingDeletionBanner()
        }

        pinnedConversationIDs = context.pinnedConversationIDs
        ConversationPinStore.save(pinnedConversationIDs, profile: profile)

        var restored = context.conversation
        restored.isPinned = pinnedConversationIDs.contains(restored.id)
        if conversations.contains(where: { $0.id == id }) == false {
            conversations.insert(restored, at: min(context.index, conversations.count))
            conversations.sort(by: sortConversations)
        }
        if context.wasSelected {
            selectedConversationID = id
            messages = context.messages
            hasOlderMessages = context.hasOlderMessages
            startRealtimeForSelectedConversation()
        }
        appendSystemError("Failed to delete conversation: \(error.localizedDescription)")
    }

    private func refreshPendingDeletionBanner() {
        pendingConversationDeletion = deletionContexts.values.first.map {
            PendingConversationDeletion(id: $0.conversation.id, title: $0.conversation.title)
        }
    }

    var connectionLabel: String {
        "\(profile.name) - \(profile.connectionSummary)"
    }

    func handleScenePhaseChange(_ phase: ScenePhase) {
        switch phase {
        case .active:
            resumeRealtimeAfterForeground()
        case .background:
            suspendRealtimeForBackground()
        case .inactive:
            break
        @unknown default:
            break
        }
    }

    private func suspendRealtimeForBackground() {
        realtimeTask?.cancel()
        realtimeTask = nil
        conversationStreamTask?.cancel()
        conversationStreamTask = nil
        realtimeStatus = "Disconnected"
        activeTurnProgress = nil

        Task {
            await AppleSSHTunnelManager.shared.close()
        }
    }

    private func resumeRealtimeAfterForeground() {
        Task {
            await AppleSSHTunnelManager.shared.close()

            guard !Task.isCancelled else {
                return
            }

            startConversationStream()
            if selectedConversationID != nil {
                startRealtimeForSelectedConversation()
                await loadSelectedConversationStatus()
            }
        }
    }

    func updateProfile(_ profile: ServerProfile) {
        self.profile = profile
        self.client = makeClient(profile)
        ServerProfileStore.save(profile)
        realtimeTask?.cancel()
        conversationStreamTask?.cancel()
        seenSaveTasks.values.forEach { $0.cancel() }
        deletionTasks.values.forEach { $0.cancel() }
        seenSaveTasks.removeAll()
        deletionTasks.removeAll()
        deletionContexts.removeAll()
        pendingConversationDeletion = nil
        selectedConversationStatus = nil
        selectedConversationStatusError = nil
        pinnedConversationIDs = ConversationPinStore.load(profile: profile)
        MessageCacheStore.removeExpired()
        messageCacheStats = MessageCacheStore.stats()
        conversations = ConversationCacheStore.load(profile: profile)
            .map { cached in
                var cached = cached
                cached.isPinned = pinnedConversationIDs.contains(cached.id)
                return cached
            }
            .sorted(by: sortConversations)
        realtimeStatus = "Disconnected"

        Task {
            await AppleSSHTunnelManager.shared.close()
        }
    }

    func refreshSSHIdentity() {
        do {
            sshPublicKey = try AppleSSHIdentityStore.publicKey()
            sshIdentityError = nil
        } catch {
            sshPublicKey = ""
            sshIdentityError = error.localizedDescription
        }
    }

    func loadInitialData() async {
        do {
            await loadModels()
            let loadedConversations = try await client.listConversations()
            conversations = applyPinnedState(
                to: loadedConversations.filter { deletionContexts[$0.id] == nil }
            ).sorted(by: sortConversations)
            #if os(iOS)
            selectedConversationID = nil
            prepareConversationEntry()
            startConversationStream()
            #else
            selectedConversationID = conversations.first?.id
            prepareConversationEntry()
            startConversationStream()
            startRealtimeForSelectedConversation()
            #endif
        } catch {
            messages = [
                ChatMessage(
                    id: "load-conversations-error-\(Date().timeIntervalSince1970)",
                    index: -1,
                    role: .system,
                    body: "Failed to load conversations: \(error.localizedDescription)",
                    timestamp: Date(),
                    userName: nil,
                    isOptimistic: false,
                    pending: false,
                    error: error.localizedDescription
                )
            ]
        }
    }

    func loadModels() async {
        do {
            availableModels = try await client.listModels()
            modelsError = nil
        } catch {
            modelsError = error.localizedDescription
        }
    }

    private func prepareConversationEntry() {
        if let selectedConversationID,
           let snapshot = MessageCacheStore.load(conversationID: selectedConversationID, profile: profile),
           !snapshot.messages.isEmpty {
            let cachedMessages = snapshot.latestMessages(limit: pageSize)
            messages = cachedMessages
            hasOlderMessages = snapshot.messages.map(\.index).min().map { $0 > 0 } ?? false
            if let index = conversations.firstIndex(where: { $0.id == selectedConversationID }) {
                conversations[index].messageCount = max(conversations[index].messageCount, snapshot.total, snapshot.messages.count)
            }
            isLoadingMessages = true
        } else {
            messages = []
            hasOlderMessages = false
            isLoadingMessages = selectedConversationID != nil
        }
        detailPresentation = nil
        selectedConversationStatus = nil
        selectedConversationStatusError = nil
        activeTurnProgress = nil
        isLoadingOlderMessages = false
        messageCacheStats = MessageCacheStore.stats()
    }

    func loadSelectedConversationStatus() async {
        guard let selectedConversationID else {
            selectedConversationStatus = nil
            selectedConversationStatusError = nil
            return
        }

        do {
            let snapshot = try await client.conversationStatus(conversationID: selectedConversationID)
            guard self.selectedConversationID == selectedConversationID else {
                return
            }
            selectedConversationStatus = snapshot
            selectedConversationStatusError = nil
            mergeStatusSnapshot(snapshot)
        } catch {
            guard self.selectedConversationID == selectedConversationID else {
                return
            }
            selectedConversationStatusError = error.localizedDescription
        }
    }

    func loadSelectedConversation() async {
        guard let selectedConversationID else {
            messages = []
            hasOlderMessages = false
            isLoadingMessages = false
            realtimeTask?.cancel()
            realtimeStatus = "Disconnected"
            return
        }

        if let snapshot = MessageCacheStore.load(conversationID: selectedConversationID, profile: profile),
           !snapshot.messages.isEmpty {
            messages = snapshot.latestMessages(limit: pageSize)
            hasOlderMessages = snapshot.messages.map(\.index).min().map { $0 > 0 } ?? false
            refreshSelectedConversationTotal(max(snapshot.total, snapshot.messages.count))
            markSelectedConversationReadSoon()
            isLoadingMessages = false
            messageCacheStats = MessageCacheStore.stats()
            return
        }

        isLoadingMessages = true
        defer {
            if self.selectedConversationID == selectedConversationID {
                isLoadingMessages = false
            }
        }

        do {
            let knownTotal = selectedConversation?.messageCount ?? 0
            let offset = max(knownTotal - pageSize, 0)
            var page = try await client.listMessagePage(conversationID: selectedConversationID, offset: offset, limit: pageSize)
            if page.total > page.end {
                let latestOffset = max(page.total - pageSize, 0)
                page = try await client.listMessagePage(conversationID: selectedConversationID, offset: latestOffset, limit: pageSize)
            }
            guard self.selectedConversationID == selectedConversationID else {
                return
            }
            messages = latestVisibleMessages(page.messages)
            hasOlderMessages = page.start > 0
            refreshSelectedConversationTotal(page.total)
            cacheMessages(page.messages, conversationID: selectedConversationID, total: page.total)
            markSelectedConversationReadSoon()
        } catch {
            guard self.selectedConversationID == selectedConversationID else {
                return
            }
            realtimeStatus = "Load failed"
            hasOlderMessages = false
        }
    }

    private func loadInitialMessagesAfterAck(
        conversationID: ConversationSummary.ID,
        total: Int,
        currentMessageID: String?
    ) async {
        guard selectedConversationID == conversationID else {
            return
        }

        isLoadingMessages = true
        defer {
            if selectedConversationID == conversationID {
                isLoadingMessages = false
            }
        }

        let currentIndex = currentMessageID.flatMap(Int.init) ?? (total > 0 ? total - 1 : nil)
        guard let currentIndex, currentIndex >= 0 else {
            messages = []
            hasOlderMessages = false
            refreshSelectedConversationTotal(0)
            return
        }

        if let snapshot = MessageCacheStore.load(conversationID: conversationID, profile: profile),
           !snapshot.messages.isEmpty {
            messages = snapshot.latestMessages(limit: pageSize)
            hasOlderMessages = snapshot.messages.map(\.index).min().map { $0 > 0 } ?? false
            refreshSelectedConversationTotal(max(total, snapshot.total, snapshot.messages.count))
            markSelectedConversationReadSoon()
            let nextAfterCurrent = currentMessageID.flatMap(Int.init).map { "\($0 + 1)" }
            await backfillMissingMessagesAfterAck(
                conversationID: conversationID,
                total: total,
                nextMessageID: nextAfterCurrent
            )
            return
        }

        let offset = max(0, currentIndex - pageSize + 1)
        do {
            let page = try await client.listMessagePage(conversationID: conversationID, offset: offset, limit: pageSize)
            guard selectedConversationID == conversationID else {
                return
            }
            messages = latestVisibleMessages(page.messages)
            hasOlderMessages = page.start > 0
            refreshSelectedConversationTotal(max(total, page.total))
            cacheMessages(page.messages, conversationID: conversationID, total: max(total, page.total))
            markSelectedConversationReadSoon()
        } catch {
            guard selectedConversationID == conversationID else {
                return
            }
            realtimeStatus = "Load failed"
            hasOlderMessages = false
        }
    }

    private func backfillMissingMessagesAfterAck(
        conversationID: ConversationSummary.ID,
        total: Int,
        nextMessageID: String?
    ) async {
        guard selectedConversationID == conversationID else {
            return
        }
        refreshSelectedConversationTotal(total)

        guard let nextIndex = nextMessageID.flatMap(Int.init) else {
            return
        }
        let lastIndex = messages
            .filter { !$0.isOptimistic && !$0.pending && $0.index >= 0 }
            .map(\.index)
            .max() ?? -1
        guard nextIndex > lastIndex + 1 else {
            return
        }

        let offset = lastIndex + 1
        let limit = min(200, nextIndex - offset)
        guard limit > 0 else {
            return
        }

        do {
            let page = try await client.listMessagePage(conversationID: conversationID, offset: offset, limit: limit)
            guard selectedConversationID == conversationID else {
                return
            }
            mergeIncomingMessages(page.messages)
            trimAutomaticVisibleMessagesIfNeeded()
            hasOlderMessages = messages.map(\.index).min().map { $0 > 0 } ?? false
            refreshSelectedConversationTotal(max(total, page.total))
            cacheMessages(page.messages, conversationID: conversationID, total: max(total, page.total))
            markSelectedConversationReadSoon()
        } catch {
            if selectedConversationID == conversationID {
                realtimeStatus = "Realtime gap repair failed"
            }
        }
    }

    func loadOlderMessages() async {
        guard !isLoadingOlderMessages,
              hasOlderMessages,
              let selectedConversationID,
              let oldestIndex = messages.map(\.index).min()
        else {
            return
        }

        isLoadingOlderMessages = true
        defer {
            isLoadingOlderMessages = false
        }

        let offset = max(oldestIndex - pageSize, 0)
        let limit = max(oldestIndex - offset, 1)
        let cachedOlder = MessageCacheStore.loadPage(
            conversationID: selectedConversationID,
            profile: profile,
            offset: offset,
            limit: limit
        )
        if !cachedOlder.isEmpty {
            let existingIDs = Set(messages.map(\.id))
            let older = cachedOlder.filter { !existingIDs.contains($0.id) }
            messages = (older + messages).sorted { left, right in
                if left.index == right.index {
                    return left.timestamp < right.timestamp
                }
                return left.index < right.index
            }
            hasOlderMessages = offset > 0
            markSelectedConversationReadSoon()
            messageCacheStats = MessageCacheStore.stats()
            return
        }

        do {
            let page = try await client.listMessagePage(conversationID: selectedConversationID, offset: offset, limit: limit)
            let existingIDs = Set(messages.map(\.id))
            let older = page.messages.filter { !existingIDs.contains($0.id) }
            messages = (older + messages).sorted { left, right in
                if left.index == right.index {
                    return left.timestamp < right.timestamp
                }
                return left.index < right.index
            }
            hasOlderMessages = page.start > 0
            refreshSelectedConversationTotal(page.total)
            cacheMessages(page.messages, conversationID: selectedConversationID, total: page.total)
            markSelectedConversationReadSoon()
        } catch {
            appendSystemError("Failed to load older messages: \(error.localizedDescription)")
        }
    }

    func createConversation(nickname: String? = nil, model: String? = nil) {
        Task {
            do {
                let id = try await client.createConversation(nickname: nickname, model: model)
                conversations = applyPinnedState(to: try await client.listConversations()).sorted(by: sortConversations)
                selectedConversationID = id
                prepareConversationEntry()
                startConversationStream()
                startRealtimeForSelectedConversation()
            } catch {
                appendSystemError("Failed to create conversation: \(error.localizedDescription)")
            }
        }
    }

    func sendComposerMessage(files: [OutgoingMessageFile] = []) async {
        let trimmedBody = composerText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard (!trimmedBody.isEmpty || !files.isEmpty), let selectedConversationID else {
            return
        }

        composerText = ""
        let remoteMessageID = "apple-\(UUID().uuidString)"
        let optimistic = ChatMessage(
            id: remoteMessageID,
            index: (messages.map(\.index).max() ?? -1) + 1,
            role: .user,
            body: trimmedBody,
            attachments: files.enumerated().map { index, file in
                ChatAttachment(outgoingFile: file, index: index)
            },
            timestamp: Date(),
            userName: profile.username,
            isOptimistic: true,
            pending: true,
            error: nil
        )
        messages.append(optimistic)

        do {
            try await client.sendMessage(
                trimmedBody,
                conversationID: selectedConversationID,
                userName: profile.username,
                remoteMessageID: remoteMessageID,
                files: files
            )
            if let index = messages.firstIndex(where: { $0.id == optimistic.id }) {
                messages[index].pending = false
            }
            refreshSelectedConversationPreview()
            realtimeStatus = "Sent, waiting for response"
            await loadSelectedConversationStatus()
        } catch {
            if let index = messages.firstIndex(where: { $0.id == optimistic.id }) {
                messages[index].pending = false
                messages[index].error = error.localizedDescription
            }
            appendSystemError("Failed to send message: \(error.localizedDescription)")
        }
    }

    func clearMessageCache() {
        MessageCacheStore.clearAll()
        messageCacheStats = MessageCacheStore.stats()
    }

    func pruneExpiredMessageCache() {
        MessageCacheStore.removeExpired()
        messageCacheStats = MessageCacheStore.stats()
    }

    func switchModel(_ model: ModelSummary) {
        Task {
            await sendControlCommand("/model \(model.alias)")
        }
    }

    func selectModelForCurrentConversation(_ model: ModelSummary) {
        guard let selectedConversationID else {
            return
        }
        if let index = conversations.firstIndex(where: { $0.id == selectedConversationID }) {
            conversations[index].model = model.alias
            conversations[index].modelSelectionPending = false
            conversations[index].lastMessagePreview = model.alias
        }
        switchModel(model)
    }

    func sendConversationCommand(_ command: String) {
        Task {
            await sendControlCommand(command)
        }
    }

    func switchReasoning(_ effort: String) {
        Task {
            await sendControlCommand("/reasoning \(effort)")
        }
    }

    func loadWorkspaceListing(path: String = "") async throws -> WorkspaceListing {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        return try await client.listWorkspace(conversationID: selectedConversationID, path: path, limit: 500)
    }

    func loadWorkspaceFile(path: String, previewLimitBytes: Int = 2_000_000, full: Bool = false) async throws -> WorkspaceFile {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        return try await client.workspaceFile(
            conversationID: selectedConversationID,
            path: path,
            limitBytes: max(1, previewLimitBytes),
            full: full
        )
    }

    func uploadWorkspaceFile(fileURL: URL, targetPath: String) async throws -> Int {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        let accessed = fileURL.startAccessingSecurityScopedResource()
        defer {
            if accessed {
                fileURL.stopAccessingSecurityScopedResource()
            }
        }
        let data = try Data(contentsOf: fileURL)
        let archive = try TarGzipArchive.singleFile(name: fileURL.lastPathComponent, data: data)
        return try await client.uploadWorkspaceArchive(conversationID: selectedConversationID, path: targetPath, archive: archive)
    }

    func uploadWorkspaceFiles(fileURLs: [URL], targetPath: String) async throws -> Int {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        let entries = try fileURLs.map { url in
            let accessed = url.startAccessingSecurityScopedResource()
            defer {
                if accessed {
                    url.stopAccessingSecurityScopedResource()
                }
            }
            return TarGzipArchive.Entry(name: url.lastPathComponent, data: try Data(contentsOf: url))
        }
        let archive = try TarGzipArchive.files(entries)
        return try await client.uploadWorkspaceArchive(conversationID: selectedConversationID, path: targetPath, archive: archive)
    }

    func deleteWorkspacePath(_ path: String) async throws {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        try await client.deleteWorkspacePath(conversationID: selectedConversationID, path: path)
    }

    func moveWorkspacePath(_ path: String, to newPath: String) async throws {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        try await client.moveWorkspacePath(conversationID: selectedConversationID, path: path, newPath: newPath)
    }

    func downloadWorkspaceArchive(path: String, suggestedName: String) async throws -> URL {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        let data = try await client.downloadWorkspaceArchive(conversationID: selectedConversationID, path: path)
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("StellaCodeXDownloads", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let safeName = suggestedName.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty ?? "workspace"
        let fileURL = directory.appendingPathComponent("\(safeName.fileNameSafe).tar.gz")
        try data.write(to: fileURL, options: [.atomic])
        return fileURL
    }

    func listTerminals() async throws -> [TerminalSummary] {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        return try await client.listTerminals(conversationID: selectedConversationID)
    }

    func createTerminal(options: TerminalCreateOptions) async throws -> TerminalSummary {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        return try await client.createTerminal(conversationID: selectedConversationID, options: options)
    }

    func terminateTerminal(id: TerminalSummary.ID) async throws -> TerminalSummary {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        return try await client.terminateTerminal(conversationID: selectedConversationID, terminalID: id)
    }

    func openTerminalSession(id: TerminalSummary.ID, offset: UInt64) async throws -> TerminalWebSocketSession {
        guard let selectedConversationID else {
            throw StellaAPIError.invalidResponse
        }
        return try await client.terminalSession(conversationID: selectedConversationID, terminalID: id, offset: offset)
    }

    private func sendControlCommand(_ command: String) async {
        guard let selectedConversationID else {
            return
        }
        do {
            try await client.sendMessage(
                command,
                conversationID: selectedConversationID,
                userName: profile.username,
                remoteMessageID: "apple-control-\(UUID().uuidString)",
                files: []
            )
            realtimeStatus = "Command sent"
            conversations = applyPinnedState(to: try await client.listConversations()).sorted(by: sortConversations)
            await loadSelectedConversation()
            await loadSelectedConversationStatus()
        } catch {
            appendSystemError("Failed to send command: \(error.localizedDescription)")
        }
    }

    func inspectMessage(_ message: ChatMessage) {
        Task {
            await loadDetail(message: message, selectedToolID: nil)
        }
    }

    func inspectTool(message: ChatMessage, tool: ToolActivity) {
        Task {
            await loadDetail(message: message, selectedToolID: tool.id)
        }
    }

    private func loadDetail(message: ChatMessage, selectedToolID: ToolActivity.ID?) async {
        guard let selectedConversationID else {
            return
        }

        if message.id.hasPrefix("apple-") || message.id.hasPrefix("local-") || message.id.hasPrefix("system-error-") {
            detailPresentation = fallbackDetailPresentation(message: message, selectedToolID: selectedToolID)
            return
        }

        do {
            let detail = try await client.messageDetail(conversationID: selectedConversationID, messageID: message.id)
            detailPresentation = ChatDetailPresentation(
                id: "\(detail.id)-\(selectedToolID ?? "message")",
                detail: detail,
                selectedToolID: selectedToolID
            )
        } catch {
            detailPresentation = fallbackDetailPresentation(message: message, selectedToolID: selectedToolID)
            appendSystemError("Failed to load message detail: \(error.localizedDescription)")
        }
    }

    private func fallbackDetailPresentation(message: ChatMessage, selectedToolID: ToolActivity.ID?) -> ChatDetailPresentation {
        let detail = ChatMessageDetail(
            id: "\(selectedConversationID ?? "conversation")-\(message.id)",
            conversationID: selectedConversationID ?? "",
            message: message,
            renderedText: message.body,
            toolActivities: message.toolActivities,
            attachments: message.attachments,
            attachmentCount: message.attachments.count,
            attachmentErrors: []
        )
        return ChatDetailPresentation(
            id: "\(detail.id)-\(selectedToolID ?? "message")",
            detail: detail,
            selectedToolID: selectedToolID
        )
    }

    private func startConversationStream() {
        conversationStreamTask?.cancel()
        conversationStreamTask = Task { [weak self] in
            guard let self else {
                return
            }

            while !Task.isCancelled {
                do {
                    let stream = try await client.conversationEvents()
                    for try await event in stream {
                        guard !Task.isCancelled else {
                            return
                        }
                        handleConversationEvent(event)
                    }
                } catch is CancellationError {
                    return
                } catch {
                    guard !Task.isCancelled else {
                        return
                    }
                    realtimeStatus = "Conversation stream reconnecting"
                    try? await Task.sleep(nanoseconds: 1_600_000_000)
                }
            }
        }
    }

    private func handleConversationEvent(_ event: StellaConversationEvent) {
        switch event {
        case .snapshot(let incoming):
            let existingByID = Dictionary(uniqueKeysWithValues: conversations.map { ($0.id, $0) })
            conversations = incoming
                .filter { deletionContexts[$0.id] == nil }
                .map { mergeConversationSummary(existingByID[$0.id], $0) }
                .sorted(by: sortConversations)
            if selectedConversationID == nil {
                #if os(iOS)
                return
                #else
                selectedConversationID = conversations.first?.id
                prepareConversationEntry()
                startRealtimeForSelectedConversation()
                #endif
            }
        case .upsert(let incoming):
            upsertConversation(incoming)
        case .deleted(let conversationID):
            deletionTasks[conversationID]?.cancel()
            deletionTasks[conversationID] = nil
            deletionContexts[conversationID] = nil
            if pendingConversationDeletion?.id == conversationID {
                refreshPendingDeletionBanner()
            }
            pinnedConversationIDs.remove(conversationID)
            ConversationPinStore.save(pinnedConversationIDs, profile: profile)
            conversations.removeAll { $0.id == conversationID }
            if selectedConversationID == conversationID {
                selectedConversationID = conversations.first?.id
                prepareConversationEntry()
                startRealtimeForSelectedConversation()
            }
        case .processing(let conversationID, let status, let running):
            guard let index = conversations.firstIndex(where: { $0.id == conversationID }) else {
                return
            }
            conversations[index].status = running ? .running : status
        case .turnCompleted(let conversationID, let conversation, let messageCount, let lastMessageID, let lastMessageTime, let unread):
            if let conversation {
                upsertConversation(conversation)
                if selectedConversationID != conversationID,
                   let unread,
                   let index = conversations.firstIndex(where: { $0.id == conversationID }) {
                    conversations[index].isUnread = unread
                }
            } else if let index = conversations.firstIndex(where: { $0.id == conversationID }) {
                conversations[index].status = .idle
                if let messageCount {
                    conversations[index].messageCount = max(conversations[index].messageCount, messageCount)
                }
                if let lastMessageID {
                    conversations[index].lastMessageID = lastMessageID
                }
                if let lastMessageTime {
                    conversations[index].updatedAt = lastMessageTime
                }
                if let unread, selectedConversationID != conversationID {
                    conversations[index].isUnread = unread
                } else {
                    conversations[index].isUnread = selectedConversationID == conversationID ? false : hasUnread(conversations[index])
                }
            }

            if selectedConversationID == conversationID {
                markSelectedConversationReadSoon()
            } else if let conversation = conversations.first(where: { $0.id == conversationID }),
                      conversation.isUnread || unread == true {
                notifyConversationCompleted(conversation)
            }
        case .seen(let conversationID, let seen):
            guard let index = conversations.firstIndex(where: { $0.id == conversationID }) else {
                return
            }
            conversations[index].lastSeenMessageID = maxMessageID(
                conversations[index].lastSeenMessageID,
                seen.lastSeenMessageID
            )
            conversations[index].lastSeenAt = seen.updatedAt
            conversations[index].isUnread = hasUnread(conversations[index])
        case .error(let message):
            realtimeStatus = message
            appendSystemError(message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "Conversation stream error." : message)
        }
    }

    private func upsertConversation(_ incoming: ConversationSummary) {
        guard deletionContexts[incoming.id] == nil else {
            return
        }

        if let index = conversations.firstIndex(where: { $0.id == incoming.id }) {
            conversations[index] = mergeConversationSummary(conversations[index], incoming)
        } else {
            var incoming = incoming
            incoming.isPinned = pinnedConversationIDs.contains(incoming.id)
            conversations.append(incoming)
        }
        conversations.sort(by: sortConversations)
    }

    private func mergeConversationSummary(_ existing: ConversationSummary?, _ incoming: ConversationSummary) -> ConversationSummary {
        guard var existing else {
            var incoming = incoming
            incoming.isPinned = pinnedConversationIDs.contains(incoming.id)
            incoming.isUnread = hasUnread(incoming)
            return incoming
        }

        let incomingHasNewerMessage = compareMessageIDs(incoming.lastMessageID, existing.lastMessageID) >= 0
        let mergedSeen = maxMessageID(existing.lastSeenMessageID, incoming.lastSeenMessageID)
        let incomingSeenIsNewer = compareMessageIDs(incoming.lastSeenMessageID, existing.lastSeenMessageID) >= 0

        existing.title = incoming.title
        existing.workspacePath = incoming.workspacePath
        existing.status = incoming.status
        existing.model = incoming.model
        existing.modelSelectionPending = incoming.modelSelectionPending
        existing.reasoning = incoming.reasoning
        existing.sandbox = incoming.sandbox
        existing.sandboxSource = incoming.sandboxSource
        existing.remote = incoming.remote
        existing.isPinned = pinnedConversationIDs.contains(existing.id)

        if incomingHasNewerMessage {
            existing.lastMessagePreview = incoming.lastMessagePreview
            existing.lastMessageID = incoming.lastMessageID ?? existing.lastMessageID
            existing.updatedAt = incoming.updatedAt == .distantPast ? existing.updatedAt : incoming.updatedAt
            existing.messageCount = max(existing.messageCount, incoming.messageCount)
        }

        if let mergedSeen {
            existing.lastSeenMessageID = mergedSeen
            if incomingSeenIsNewer {
                existing.lastSeenAt = incoming.lastSeenAt ?? existing.lastSeenAt
            }
        }
        existing.isUnread = hasUnread(existing)
        return existing
    }

    private func mergeStatusSnapshot(_ snapshot: ConversationStatusSnapshot) {
        guard let index = conversations.firstIndex(where: { $0.id == snapshot.conversationID }) else {
            return
        }
        conversations[index].model = snapshot.model
        conversations[index].modelSelectionPending = snapshot.model.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        conversations[index].reasoning = snapshot.reasoning
        conversations[index].sandbox = snapshot.sandbox
        conversations[index].sandboxSource = snapshot.sandboxSource
        conversations[index].remote = snapshot.remote
        conversations[index].workspacePath = snapshot.workspace
    }

    private func markSelectedConversationReadSoon() {
        guard let selectedConversationID,
              let index = conversations.firstIndex(where: { $0.id == selectedConversationID })
        else {
            return
        }
        let lastMessageID = conversations[index].lastMessageID
            ?? messages
                .filter { !$0.isOptimistic && !$0.pending && $0.index >= 0 }
                .max(by: { $0.index < $1.index })?
                .id
        guard let lastMessageID,
              Int(lastMessageID) != nil
        else {
            return
        }
        markConversationRead(conversationID: selectedConversationID, lastMessageID: lastMessageID)
    }

    private func markConversationRead(conversationID: ConversationSummary.ID, lastMessageID: String) {
        guard let index = conversations.firstIndex(where: { $0.id == conversationID }) else {
            return
        }
        conversations[index].lastSeenMessageID = maxMessageID(conversations[index].lastSeenMessageID, lastMessageID)
        conversations[index].lastSeenAt = Date()
        conversations[index].isUnread = false

        seenSaveTasks[conversationID]?.cancel()
        seenSaveTasks[conversationID] = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 180_000_000)
            guard let self, !Task.isCancelled else {
                return
            }
            do {
                let seen = try await client.markConversationSeen(
                    conversationID: conversationID,
                    lastSeenMessageID: lastMessageID
                )
                guard !Task.isCancelled,
                      let index = conversations.firstIndex(where: { $0.id == conversationID })
                else {
                    return
                }
                conversations[index].lastSeenMessageID = maxMessageID(
                    conversations[index].lastSeenMessageID,
                    seen.lastSeenMessageID
                )
                conversations[index].lastSeenAt = seen.updatedAt
                conversations[index].isUnread = hasUnread(conversations[index])
            } catch {
                // Keep optimistic seen state; the conversation stream will correct it later.
            }
        }
    }

    private func notifyConversationCompleted(_ conversation: ConversationSummary) {
        Task {
            let center = UNUserNotificationCenter.current()
            let granted = (try? await center.requestAuthorization(options: [.alert, .sound])) ?? false
            guard granted else {
                return
            }
            let content = UNMutableNotificationContent()
            content.title = conversation.title
            content.body = "New reply completed"
            content.sound = .default
            let request = UNNotificationRequest(
                identifier: "conversation-\(conversation.id)-\(conversation.lastMessageID ?? UUID().uuidString)",
                content: content,
                trigger: nil
            )
            try? await center.add(request)
        }
    }

    private func compareMessageIDs(_ left: String?, _ right: String?) -> Int {
        let leftNumber = left.flatMap(Int.init)
        let rightNumber = right.flatMap(Int.init)
        switch (leftNumber, rightNumber) {
        case (nil, nil):
            return 0
        case (nil, _):
            return -1
        case (_, nil):
            return 1
        case (let left?, let right?):
            return left == right ? 0 : (left > right ? 1 : -1)
        }
    }

    private func maxMessageID(_ left: String?, _ right: String?) -> String? {
        compareMessageIDs(left, right) >= 0 ? left : right
    }

    private func hasUnread(_ conversation: ConversationSummary) -> Bool {
        compareMessageIDs(conversation.lastMessageID, conversation.lastSeenMessageID) > 0
    }

    private func sortConversations(_ left: ConversationSummary, _ right: ConversationSummary) -> Bool {
        if left.isPinned != right.isPinned {
            return left.isPinned && !right.isPinned
        }
        if left.updatedAt != right.updatedAt {
            return left.updatedAt > right.updatedAt
        }
        return left.id < right.id
    }

    private func applyPinnedState(to conversations: [ConversationSummary]) -> [ConversationSummary] {
        conversations.map { conversation in
            var conversation = conversation
            conversation.isPinned = pinnedConversationIDs.contains(conversation.id)
            conversation.isUnread = hasUnread(conversation)
            return conversation
        }
    }

    private func startRealtimeForSelectedConversation() {
        realtimeTask?.cancel()
        guard let selectedConversationID else {
            messages = []
            hasOlderMessages = false
            isLoadingMessages = false
            realtimeStatus = "Disconnected"
            activeTurnProgress = nil
            return
        }

        realtimeStatus = "Connecting"
        activeTurnProgress = nil
        realtimeTask = Task { [weak self] in
            guard let self else {
                return
            }
            var didFallbackLoad = false

            while !Task.isCancelled && self.selectedConversationID == selectedConversationID {
                do {
                    realtimeStatus = "Connecting"
                    let stream = try await client.foregroundEvents(conversationID: selectedConversationID)
                    var didReconcileAck = false
                    for try await event in stream {
                        guard !Task.isCancelled else {
                            return
                        }
                        guard self.selectedConversationID == selectedConversationID else {
                            return
                        }
                        if case .subscriptionAck = event, !didReconcileAck {
                            didReconcileAck = true
                            didFallbackLoad = false
                            await reconcileSubscriptionAck(event, selectedConversationID: selectedConversationID)
                        } else {
                            await handleRealtimeEvent(event, selectedConversationID: selectedConversationID)
                        }
                    }

                    guard !Task.isCancelled, self.selectedConversationID == selectedConversationID else {
                        return
                    }
                    if !didReconcileAck {
                        if messages.isEmpty && !didFallbackLoad {
                            didFallbackLoad = true
                            await loadSelectedConversation()
                        } else {
                            isLoadingMessages = false
                        }
                    }
                    realtimeStatus = "Realtime reconnecting"
                } catch is CancellationError {
                    return
                } catch {
                    guard !Task.isCancelled, self.selectedConversationID == selectedConversationID else {
                        return
                    }
                    realtimeStatus = "Realtime reconnecting"
                    if messages.isEmpty && !didFallbackLoad {
                        didFallbackLoad = true
                        await loadSelectedConversation()
                    } else {
                        isLoadingMessages = false
                    }
                }

                try? await Task.sleep(nanoseconds: 1_200_000_000)
                guard !Task.isCancelled, self.selectedConversationID == selectedConversationID else {
                    return
                }
            }
        }
    }

    private func reconcileSubscriptionAck(_ event: StellaRealtimeEvent, selectedConversationID: ConversationSummary.ID) async {
        guard case .subscriptionAck(let conversationID, let total, let currentMessageID, let nextMessageID, let reason) = event else {
            return
        }
        guard conversationID.isEmpty || conversationID == selectedConversationID else {
            return
        }

        realtimeStatus = normalizedRealtimeStatus(reason)

        if messages.isEmpty {
            await loadInitialMessagesAfterAck(
                conversationID: selectedConversationID,
                total: total,
                currentMessageID: currentMessageID
            )
            return
        }

        await backfillMissingMessagesAfterAck(
            conversationID: selectedConversationID,
            total: total,
            nextMessageID: nextMessageID ?? currentMessageID.flatMap(Int.init).map { "\($0 + 1)" }
        )
        if self.selectedConversationID == selectedConversationID {
            isLoadingMessages = false
        }
    }

    private func handleRealtimeEvent(_ event: StellaRealtimeEvent, selectedConversationID: ConversationSummary.ID) async {
        switch event {
        case .subscriptionAck(_, _, _, _, let reason):
            realtimeStatus = normalizedRealtimeStatus(reason)
        case .messages(let conversationID, let incoming, let total):
            guard conversationID.isEmpty || conversationID == selectedConversationID else {
                return
            }
            if total > 0 {
                refreshSelectedConversationTotal(total)
            }
            mergeIncomingMessages(incoming)
            refreshSelectedConversationPreview()
            markSelectedConversationReadSoon()
        case .conversationDeleted(let conversationID):
            guard conversationID.isEmpty || conversationID == selectedConversationID else {
                return
            }
            removeConversation(id: selectedConversationID, deleteRemote: false)
        case .progress(let value):
            realtimeStatus = normalizedRealtimeStatus(value)
        case .turnProgress(let progress):
            applyTurnProgress(progress, selectedConversationID: selectedConversationID)
        case .error(let message):
            let trimmed = message.trimmingCharacters(in: .whitespacesAndNewlines)
            realtimeStatus = trimmed.isEmpty ? "Realtime error" : "Realtime error"
            appendSystemError(trimmed.isEmpty ? "Realtime error." : trimmed)
        }
    }

    private func applyTurnProgress(_ progress: TurnProgressFeedback, selectedConversationID: ConversationSummary.ID) {
        activeTurnProgress = progress.isActive ? progress : nil
        realtimeStatus = progress.isActive ? progress.activity : "Connected"
        updateConversationStatus(id: selectedConversationID, status: progress.isActive ? .running : (progress.finalState == "failed" ? .failed : .idle))
    }

    private func updateConversationStatus(id: ConversationSummary.ID, status: ConversationStatus) {
        guard let index = conversations.firstIndex(where: { $0.id == id }) else {
            return
        }
        conversations[index].status = status
    }

    private func normalizedRealtimeStatus(_ value: String) -> String {
        let normalized = value.trimmingCharacters(in: .whitespacesAndNewlines)
        let lowercased = normalized.lowercased()
        if lowercased == "done"
            || lowercased == "done: done"
            || lowercased == "completed"
            || lowercased == "subscribed"
            || lowercased == "connected" {
            return "Connected"
        }
        return normalized
    }

    private func mergeIncomingMessages(_ incoming: [ChatMessage]) {
        guard !incoming.isEmpty else {
            return
        }

        let incomingUserMessages = incoming.filter { $0.role == .user }
        messages.removeAll { existing in
            guard existing.isOptimistic || existing.pending else {
                return false
            }
            return incomingUserMessages.contains { incoming in
                incoming.body == existing.body && incoming.userName == existing.userName
            }
        }

        var byID = Dictionary(uniqueKeysWithValues: messages.map { ($0.id, $0) })
        for message in incoming {
            byID[message.id] = message
        }
        messages = byID.values.sorted { left, right in
            if left.index == right.index {
                return left.timestamp < right.timestamp
            }
            return left.index < right.index
        }
        trimAutomaticVisibleMessagesIfNeeded()
        if let selectedConversationID {
            let total = conversations.first(where: { $0.id == selectedConversationID })?.messageCount ?? messages.count
            cacheMessages(incoming, conversationID: selectedConversationID, total: total)
        }
    }

    private func latestVisibleMessages(_ source: [ChatMessage]) -> [ChatMessage] {
        let sorted = source.sorted { left, right in
            if left.index == right.index {
                return left.timestamp < right.timestamp
            }
            return left.index < right.index
        }
        guard sorted.count > automaticVisibleMessageLimit else {
            return sorted
        }
        return Array(sorted.suffix(automaticVisibleMessageLimit))
    }

    private func trimAutomaticVisibleMessagesIfNeeded() {
        guard messages.count > automaticVisibleMessageLimit else {
            return
        }
        let trimmed = latestVisibleMessages(messages)
        if let minIndex = trimmed.map(\.index).filter({ $0 >= 0 }).min(), minIndex > 0 {
            hasOlderMessages = true
        }
        messages = trimmed
    }

    private func cacheMessages(_ messages: [ChatMessage], conversationID: ConversationSummary.ID, total: Int) {
        MessageCacheStore.mergeAndSave(
            conversationID: conversationID,
            profile: profile,
            messages: messages,
            total: total
        )
        messageCacheStats = MessageCacheStore.stats()
    }

    private func refreshSelectedConversationPreview() {
        guard let selectedConversationID,
              let last = messages.max(by: { $0.index < $1.index }),
              let index = conversations.firstIndex(where: { $0.id == selectedConversationID })
        else {
            return
        }
        conversations[index].lastMessagePreview = last.body
        conversations[index].lastMessageID = last.id
        conversations[index].updatedAt = last.timestamp
        conversations[index].messageCount = max(conversations[index].messageCount, messages.count)
        conversations[index].isUnread = false
    }

    private func refreshSelectedConversationTotal(_ total: Int) {
        guard let selectedConversationID,
              let index = conversations.firstIndex(where: { $0.id == selectedConversationID })
        else {
            return
        }
        conversations[index].messageCount = max(conversations[index].messageCount, total)
    }

    private func appendSystemError(_ body: String) {
        messages.append(
            ChatMessage(
                id: "system-error-\(Date().timeIntervalSince1970)",
                index: (messages.map(\.index).max() ?? -1) + 1,
                role: .system,
                body: body,
                timestamp: Date(),
                userName: nil,
                isOptimistic: false,
                pending: false,
                error: body
            )
        )
    }

    static func mock() -> AppViewModel {
        AppViewModel(
            profile: ServerProfile(
                name: "Local Stellaclaw",
                connectionMode: .direct,
                baseURL: URL(string: "http://NAT-pl1:3011")!,
                token: "",
                username: "workspace-user"
            ),
            client: MockStellaAPIClient()
        )
    }

    static func localDevelopment() -> AppViewModel {
        let environment = ProcessInfo.processInfo.environment
        let hasEnvironmentProfile = environment.keys.contains { key in
            key.hasPrefix("STELLACODEX_")
        }
        let targetURL = environment["STELLACODEX_TARGET_URL"]
            .flatMap(URL.init(string:))
            ?? URL(string: "http://127.0.0.1:3011")!
        let directBaseURL = environment["STELLACODEX_SERVER_URL"]
            .flatMap(URL.init(string:))
            ?? URL(string: "http://NAT-pl1:3011")!
        let mode = ServerConnectionMode(rawValue: environment["STELLACODEX_CONNECTION_MODE"] ?? "ssh_proxy") ?? .sshProxy
        let sshUser = environment["STELLACODEX_SSH_USER"]
            ?? environment["USER"]
            ?? "workspace-user"
        let sshProxy = SSHProxyConfig(
            sshHost: environment["STELLACODEX_SSH_HOST"] ?? "NAT-pl1",
            sshPort: Int(environment["STELLACODEX_SSH_PORT"] ?? ""),
            sshUser: sshUser,
            targetURL: targetURL
        )
        let profile = ServerProfile(
            name: mode == .sshProxy ? "NAT-pl1 Stellaclaw" : "Direct Stellaclaw",
            connectionMode: mode,
            baseURL: directBaseURL,
            sshProxy: mode == .sshProxy ? sshProxy : nil,
            token: environment["STELLACODEX_SERVER_TOKEN"] ?? "local-web-token",
            username: environment["STELLACODEX_USER_NAME"] ?? "workspace-user"
        )
        let resolvedProfile = hasEnvironmentProfile ? profile : (ServerProfileStore.load() ?? profile)
        return AppViewModel(
            profile: resolvedProfile,
            client: StellaWebAPIClient(profile: resolvedProfile),
            makeClient: { StellaWebAPIClient(profile: $0) }
        )
    }
}

private struct ConversationDeletionContext {
    var conversation: ConversationSummary
    var index: Int
    var messages: [ChatMessage]
    var hasOlderMessages: Bool
    var wasSelected: Bool
    var pinnedConversationIDs: Set<ConversationSummary.ID>
}

private extension ChatAttachment {
    init(outgoingFile: OutgoingMessageFile, index: Int) {
        let name = outgoingFile.name?.nilIfEmpty ?? "attachment-\(index + 1)"
        let mediaType = outgoingFile.mediaType
        let isImage = mediaType?.hasPrefix("image/") == true
        self.init(
            id: "outgoing-\(index)-\(name)",
            index: index,
            source: "outgoing",
            kind: isImage ? "image" : "document",
            name: name,
            path: name,
            uri: outgoingFile.uri,
            mediaType: mediaType,
            width: outgoingFile.width,
            height: outgoingFile.height,
            sizeBytes: outgoingFile.sizeBytes,
            url: outgoingFile.uri,
            marker: nil,
            thumbnailDataURL: outgoingFile.thumbnailDataURL
        )
    }
}

private extension String {
    var nilIfEmpty: String? {
        isEmpty ? nil : self
    }

    var fileNameSafe: String {
        let unsafe = CharacterSet(charactersIn: "/\\:?%*|\"<>")
        return components(separatedBy: unsafe).joined(separator: "_")
    }
}
