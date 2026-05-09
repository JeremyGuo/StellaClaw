import SwiftUI

struct MessageTimelineView: View {
    private static let bottomAnchorID = "message-timeline-bottom-anchor"

    let messages: [ChatMessage]
    var hasOlderMessages = false
    var isLoadingMessages = false
    var isLoadingOlderMessages = false
    var activityStatus: String?
    var isConversationRunning = false
    var turnProgress: TurnProgressFeedback?
    var bottomScrollTrigger = 0
    var bottomScrollRequiresNearBottom = false
    var bottomLayoutChangeTrigger = 0
    var loadOlderAction: (() -> Void)?
    var inspectMessageAction: ((ChatMessage) -> Void)?
    var inspectToolAction: ((ChatMessage, ToolActivity) -> Void)?
    var openAttachmentAction: ((ChatAttachment) -> Void)?

    @State private var olderLoadAnchorID: TimelineEntry.ID?
    @State private var previousTimelineSnapshot: TimelineSnapshot?
    @State private var didCompleteInitialBottomScroll = false
    @State private var isPreparingInitialBottomScroll = false
    @State private var isOlderLoadTriggerVisible = false
    @State private var isNearBottom = true
    @State private var bottomAnchorY: CGFloat?
    @State private var lastManualScrollAt = Date.distantPast
    @State private var isProgrammaticScrollInFlight = false

    var body: some View {
        let renderData = buildTimelineRenderData(from: messages)

        #if os(macOS)
        MacNativeMessageTimelineView(
            renderData: renderData,
            hasOlderMessages: hasOlderMessages,
            isLoadingMessages: isLoadingMessages,
            isLoadingOlderMessages: isLoadingOlderMessages,
            activityStatus: activityStatus,
            isConversationRunning: isConversationRunning,
            turnProgress: turnProgress,
            bottomScrollTrigger: bottomScrollTrigger,
            bottomScrollRequiresNearBottom: bottomScrollRequiresNearBottom,
            bottomLayoutChangeTrigger: bottomLayoutChangeTrigger,
            loadOlderAction: loadOlderAction,
            inspectMessageAction: inspectMessageAction,
            inspectToolAction: inspectToolAction,
            openAttachmentAction: openAttachmentAction
        )
        #elseif os(iOS)
        IOSNativeMessageTimelineView(
            renderData: renderData,
            hasOlderMessages: hasOlderMessages,
            isLoadingMessages: isLoadingMessages,
            isLoadingOlderMessages: isLoadingOlderMessages,
            activityStatus: activityStatus,
            isConversationRunning: isConversationRunning,
            turnProgress: turnProgress,
            bottomScrollTrigger: bottomScrollTrigger,
            bottomScrollRequiresNearBottom: bottomScrollRequiresNearBottom,
            bottomLayoutChangeTrigger: bottomLayoutChangeTrigger,
            loadOlderAction: loadOlderAction,
            inspectMessageAction: inspectMessageAction,
            inspectToolAction: inspectToolAction,
            openAttachmentAction: openAttachmentAction
        )
        #else
        ScrollViewReader { proxy in
            GeometryReader { viewportProxy in
                ScrollView {
                    timelineStack {
                        if isLoadingMessages && messages.isEmpty {
                            VStack(spacing: 10) {
                                ProgressView()
                                    .controlSize(.regular)

                                Text("Loading messages")
                                    .font(.caption.weight(.semibold))
                                    .foregroundStyle(.secondary)
                            }
                            .frame(maxWidth: .infinity)
                            .frame(minHeight: 280)
                        } else if hasOlderMessages {
                            Button {
                                requestLoadOlder()
                            } label: {
                                HStack(spacing: 8) {
                                    if isLoadingOlderMessages {
                                        ProgressView()
                                            .controlSize(.small)
                                    } else {
                                        Image(systemName: "clock.arrow.circlepath")
                                            .font(.caption.weight(.semibold))
                                    }

                                    Text(isLoadingOlderMessages ? "Loading earlier messages" : "Load earlier messages")
                                        .font(.caption.weight(.semibold))
                                }
                                .frame(maxWidth: .infinity)
                                .padding(.vertical, 10)
                            }
                            .buttonStyle(.plain)
                            .disabled(isLoadingOlderMessages)
                            .background(olderLoadVisibilityReader)
                        }

                        timelineEntryViews(entries: renderData.entries, proxy: proxy)

                        if turnProgress?.isActive == true || isConversationRunning || ActivityStatusView.shouldDisplay(activityStatus) {
                            ActivityStatusView(
                                status: activityStatus,
                                isRunning: isConversationRunning,
                                turnProgress: turnProgress
                            )
                                .padding(.horizontal, 24)
                                .padding(.vertical, 8)
                        }

                        if isLoadingMessages && !messages.isEmpty {
                            BottomLoadingMessagesView()
                                .padding(.horizontal, 24)
                                .padding(.vertical, 10)
                        }

                        Color.clear
                            .frame(height: 1)
                            .id(Self.bottomAnchorID)
                            .background(bottomVisibilityReader(viewportHeight: viewportProxy.size.height))
                    }
                    .padding(.vertical, 8)
                    .opacity(shouldHideTimelineForInitialPosition ? 0 : 1)
                }
                .interactiveKeyboardDismissOnIOS()
                .overlay {
                    if shouldHideTimelineForInitialPosition {
                        LoadingMessagesView()
                    }
                }
                .coordinateSpace(name: "MessageTimelineScroll")
                .onPreferenceChange(OlderLoadTriggerVisiblePreferenceKey.self) { visible in
                    isOlderLoadTriggerVisible = visible
                    if visible {
                        triggerLoadOlderIfNeeded()
                    }
                }
                .onPreferenceChange(BottomAnchorMetricsPreferenceKey.self) { metrics in
                    updateBottomAnchorState(metrics)
                }
                .onAppear {
                    let snapshot = renderData.snapshot
                    previousTimelineSnapshot = snapshot
                    guard let lastID = snapshot.lastMessageID else {
                        didCompleteInitialBottomScroll = false
                        isPreparingInitialBottomScroll = false
                        return
                    }
                    prepareInitialBottomPosition(proxy, lastID: lastID)
                }
                .onChange(of: renderData.snapshot) { _, snapshot in
                    let previousSnapshot = previousTimelineSnapshot
                    previousTimelineSnapshot = snapshot

                    guard let lastID = snapshot.lastMessageID else {
                        didCompleteInitialBottomScroll = false
                        isPreparingInitialBottomScroll = false
                        return
                    }

                    guard let previousSnapshot else {
                        prepareInitialBottomPosition(proxy, lastID: lastID)
                        return
                    }

                    let transaction = TimelineScrollTransaction(
                        previous: previousSnapshot,
                        current: snapshot,
                        olderLoadAnchorID: olderLoadAnchorID,
                        canAutoAlignToBottom: canAutoAlignToBottom,
                        didCompleteInitialBottomScroll: didCompleteInitialBottomScroll
                    )
                    applyScrollTransaction(transaction, proxy: proxy, lastID: lastID)
                }
                .onChange(of: isLoadingMessages) { _, isLoading in
                    guard !isLoading,
                          renderData.snapshot.lastMessageID != nil
                    else {
                        return
                    }
                    scrollToBottom(proxy, animated: false)
                    if isOlderLoadTriggerVisible {
                        triggerLoadOlderIfNeeded()
                    }
                }
                .onChange(of: isLoadingOlderMessages) { _, isLoading in
                    guard !isLoading else {
                        return
                    }
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) {
                        if olderLoadAnchorID != nil {
                            olderLoadAnchorID = nil
                        }
                    }
                }
                .onChange(of: bottomScrollTrigger) { _, _ in
                    guard didCompleteInitialBottomScroll else {
                        return
                    }
                    guard !bottomScrollRequiresNearBottom || canAutoAlignToBottom else {
                        return
                    }
                    scrollToBottom(proxy, animated: true)
                }
                .onChange(of: bottomLayoutChangeTrigger) { _, _ in
                    keepBottomAlignedAfterLayoutChange(proxy)
                }
            }
        }
        #endif
    }

    private var shouldHideTimelineForInitialPosition: Bool {
        isPreparingInitialBottomScroll && !messages.isEmpty
    }

    @ViewBuilder
    private func timelineStack<Content: View>(@ViewBuilder content: () -> Content) -> some View {
        #if os(macOS)
        VStack(alignment: .leading, spacing: 0, content: content)
        #else
        LazyVStack(alignment: .leading, spacing: 0, content: content)
        #endif
    }

    @ViewBuilder
    private func timelineEntryViews(entries: [TimelineEntry], proxy: ScrollViewProxy) -> some View {
        ForEach(entries) { entry in
            timelineEntryView(entry, proxy: proxy)
                .id(entry.id)
        }
    }

    @ViewBuilder
    private func timelineEntryView(_ entry: TimelineEntry, proxy: ScrollViewProxy) -> some View {
        TimelineEntryRowView(
            entry: entry,
            compact: isCompactTimeline,
            layoutChangeAction: {
                keepBottomAlignedAfterLayoutChange(proxy)
            },
            inspectMessageAction: inspectMessageAction,
            inspectToolAction: inspectToolAction,
            openAttachmentAction: openAttachmentAction
        )
    }

    private func prepareInitialBottomPosition(_ proxy: ScrollViewProxy, lastID: ChatMessage.ID) {
        isPreparingInitialBottomScroll = true
        didCompleteInitialBottomScroll = false
        DispatchQueue.main.async {
            proxy.scrollTo(Self.bottomAnchorID, anchor: .bottom)

            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                proxy.scrollTo(Self.bottomAnchorID, anchor: .bottom)

                DispatchQueue.main.asyncAfter(deadline: .now() + 0.08) {
                    proxy.scrollTo(Self.bottomAnchorID, anchor: .bottom)
                    isPreparingInitialBottomScroll = false
                    markInitialBottomScrollCompleted()
                }
            }
        }
    }

    private func scrollToBottom(_ proxy: ScrollViewProxy, lastID: ChatMessage.ID, animated: Bool) {
        scrollToBottom(proxy, animated: animated)
    }

    private func scrollToBottom(_ proxy: ScrollViewProxy, animated: Bool) {
        DispatchQueue.main.async {
            isProgrammaticScrollInFlight = true
            if animated {
                withAnimation(StellaCodeXMotion.scroll) {
                    proxy.scrollTo(Self.bottomAnchorID, anchor: .bottom)
                }
            } else {
                proxy.scrollTo(Self.bottomAnchorID, anchor: .bottom)
            }

            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                proxy.scrollTo(Self.bottomAnchorID, anchor: .bottom)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.16) {
                isProgrammaticScrollInFlight = false
            }
        }
    }

    private func keepBottomAlignedAfterLayoutChange(_ proxy: ScrollViewProxy) {
        guard didCompleteInitialBottomScroll,
              canAutoAlignToBottom
        else {
            return
        }
        DispatchQueue.main.async {
            scrollToBottom(proxy, animated: false)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            scrollToBottom(proxy, animated: false)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.12) {
            scrollToBottom(proxy, animated: false)
        }
    }

    private func markInitialBottomScrollCompleted() {
        DispatchQueue.main.async {
            didCompleteInitialBottomScroll = true
            if isOlderLoadTriggerVisible {
                triggerLoadOlderIfNeeded()
            }
        }
    }

    private var canAutoAlignToBottom: Bool {
        didCompleteInitialBottomScroll &&
        isNearBottom &&
        Date().timeIntervalSince(lastManualScrollAt) > 0.35
    }

    private func updateBottomAnchorState(_ metrics: BottomAnchorMetrics?) {
        guard let metrics else {
            return
        }
        let previousY = bottomAnchorY
        let nearBottom = metrics.minY <= metrics.viewportHeight + 24
        let movedMeaningfully = previousY.map { abs($0 - metrics.minY) > 2 } ?? false

        if didCompleteInitialBottomScroll,
           movedMeaningfully,
           !isProgrammaticScrollInFlight {
            lastManualScrollAt = Date()
        }

        bottomAnchorY = metrics.minY
        isNearBottom = nearBottom || shouldHideTimelineForInitialPosition
    }

    private func triggerLoadOlderIfNeeded() {
        guard hasOlderMessages,
              didCompleteInitialBottomScroll,
              !isLoadingMessages,
              !isLoadingOlderMessages,
              !messages.isEmpty
        else {
            return
        }
        requestLoadOlder()
    }

    private func requestLoadOlder() {
        guard hasOlderMessages,
              !isLoadingMessages,
              !isLoadingOlderMessages,
              !messages.isEmpty
        else {
            return
        }
        olderLoadAnchorID = previousTimelineSnapshot?.firstEntryID
        loadOlderAction?()
    }

    private func applyScrollTransaction(
        _ transaction: TimelineScrollTransaction,
        proxy: ScrollViewProxy,
        lastID: ChatMessage.ID
    ) {
        switch transaction.intent {
        case .none:
            return
        case .alignBottom(let animated):
            scrollToBottom(proxy, lastID: lastID, animated: animated)
        case .preserveAnchor(let anchorID):
            olderLoadAnchorID = nil
            restoreOlderLoadPosition(proxy, anchorID: anchorID)
        case .resetToBottom:
            olderLoadAnchorID = nil
            prepareInitialBottomPosition(proxy, lastID: lastID)
        }
    }

    private func restoreOlderLoadPosition(_ proxy: ScrollViewProxy, anchorID: TimelineEntry.ID) {
        isProgrammaticScrollInFlight = true

        DispatchQueue.main.async {
            proxy.scrollTo(anchorID, anchor: .top)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.04) {
            proxy.scrollTo(anchorID, anchor: .top)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.12) {
            proxy.scrollTo(anchorID, anchor: .top)
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.24) {
            isProgrammaticScrollInFlight = false
        }
    }

    private var olderLoadVisibilityReader: some View {
        GeometryReader { proxy in
            let frame = proxy.frame(in: .named("MessageTimelineScroll"))
            Color.clear.preference(
                key: OlderLoadTriggerVisiblePreferenceKey.self,
                value: frame.minY >= -24 && frame.minY <= 180
            )
        }
    }

    private func bottomVisibilityReader(viewportHeight: CGFloat) -> some View {
        GeometryReader { proxy in
            let frame = proxy.frame(in: .named("MessageTimelineScroll"))
            Color.clear.preference(
                key: BottomAnchorMetricsPreferenceKey.self,
                value: BottomAnchorMetrics(minY: frame.minY, viewportHeight: viewportHeight)
            )
        }
    }

    private var isCompactTimeline: Bool {
        #if os(macOS)
        false
        #else
        true
        #endif
    }
}

private extension View {
    @ViewBuilder
    func interactiveKeyboardDismissOnIOS() -> some View {
        #if os(iOS)
        self.scrollDismissesKeyboard(.interactively)
        #else
        self
        #endif
    }
}

struct LoadingMessagesView: View {
    var body: some View {
        VStack(spacing: 10) {
            ProgressView()
                .controlSize(.regular)

            Text("Loading messages")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private struct BottomLoadingMessagesView: View {
    var body: some View {
        HStack(spacing: 8) {
            ProgressView()
                .controlSize(.small)

            Text("Loading messages")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 8)
        .accessibilityElement(children: .combine)
    }
}

private struct OlderLoadTriggerVisiblePreferenceKey: PreferenceKey {
    static var defaultValue = false

    static func reduce(value: inout Bool, nextValue: () -> Bool) {
        value = value || nextValue()
    }
}

private struct BottomAnchorMetrics: Equatable {
    var minY: CGFloat
    var viewportHeight: CGFloat
}

private struct BottomAnchorMetricsPreferenceKey: PreferenceKey {
    static var defaultValue: BottomAnchorMetrics?

    static func reduce(value: inout BottomAnchorMetrics?, nextValue: () -> BottomAnchorMetrics?) {
        value = nextValue() ?? value
    }
}

struct TimelineEntryRowView: View {
    let entry: TimelineEntry
    let compact: Bool
    var layoutChangeAction: (() -> Void)?
    var inspectMessageAction: ((ChatMessage) -> Void)?
    var inspectToolAction: ((ChatMessage, ToolActivity) -> Void)?
    var openAttachmentAction: ((ChatAttachment) -> Void)?

    var body: some View {
        switch entry {
        case .auxiliary(let messages):
            AuxiliaryStandaloneRow(
                messages: messages,
                compact: compact,
                layoutChangeAction: layoutChangeAction
            )
        case .message(let message, let auxiliaryMessages):
            #if os(macOS)
            MacMessageRow(
                message: message,
                auxiliaryMessages: auxiliaryMessages,
                layoutChangeAction: layoutChangeAction,
                inspectMessageAction: inspectMessageAction,
                inspectToolAction: inspectToolAction,
                openAttachmentAction: openAttachmentAction
            )
            #else
            IOSMessageBubble(
                message: message,
                auxiliaryMessages: auxiliaryMessages,
                layoutChangeAction: layoutChangeAction,
                inspectMessageAction: inspectMessageAction,
                inspectToolAction: inspectToolAction,
                openAttachmentAction: openAttachmentAction
            )
            #endif
        case .toolProcess(let group):
            ToolProcessGroupView(
                group: group,
                compact: compact,
            layoutChangeAction: layoutChangeAction,
            inspectMessageAction: inspectMessageAction,
            inspectToolAction: inspectToolAction,
            openAttachmentAction: openAttachmentAction
        )
        }
    }
}

private struct AuxiliaryDotsView: View {
    let messages: [AuxiliaryUserMessage]

    var body: some View {
        HStack(spacing: 4) {
            ForEach(Array(messages.enumerated()), id: \.element.id) { index, message in
                AuxiliaryPopoverButton(message: message, index: index)
            }
        }
        .accessibilityLabel("Auxiliary context")
    }
}

private struct AuxiliaryPopoverButton: View {
    let message: AuxiliaryUserMessage
    let index: Int
    @State private var isPresented = false

    var body: some View {
        Button {
            isPresented = true
        } label: {
            Circle()
                .fill(dotColor(index))
                .frame(width: 7, height: 7)
        }
        .buttonStyle(.plain)
        .help(message.title)
        .popover(isPresented: $isPresented) {
            AuxiliaryPopoverContent(message: message)
                .compactDetailPresentation(height: message.presentationHeight)
        }
    }

    private func dotColor(_ index: Int) -> Color {
        let colors: [Color] = [.secondary, .orange, .blue, .purple]
        return colors[index % colors.count].opacity(0.8)
    }
}

private struct AuxiliaryPopoverContent: View {
    let message: AuxiliaryUserMessage

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                Text(message.title)
                    .font(.caption.weight(.semibold))

                if message.fields.isEmpty {
                    Text(message.rawText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    ForEach(Array(message.fields.enumerated()), id: \.offset) { _, field in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(field.0)
                                .font(.caption2.weight(.semibold))
                                .foregroundStyle(.secondary)

                            Text(field.1.isEmpty ? "-" : field.1)
                                .font(.caption)
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
        }
        .frame(width: 320, alignment: .leading)
    }
}

private struct AuxiliaryStandaloneRow: View {
    let messages: [AuxiliaryUserMessage]
    let compact: Bool
    var layoutChangeAction: (() -> Void)?

    @State private var isExpanded = false

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Button {
                withAnimation(StellaCodeXMotion.quick) {
                    isExpanded.toggle()
                }
                notifyLayoutChange()
            } label: {
                HStack(spacing: 7) {
                    Image(systemName: isExpanded ? "chevron.down" : "chevron.right")
                        .font(.caption2.weight(.bold))
                        .foregroundStyle(.tertiary)
                        .frame(width: 10)

                    Image(systemName: "info.circle")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)

                    Text("Context")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)

                    Text(messages.map(\.title).joined(separator: ", "))
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)

                    Spacer(minLength: 0)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if isExpanded {
                VStack(alignment: .leading, spacing: 5) {
                    ForEach(messages) { message in
                        Text(message.rawText)
                            .font(.caption2.monospaced())
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                }
                .padding(.top, 1)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .background(PlatformColor.secondaryBackground.opacity(0.7))
        .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
        .frame(maxWidth: compact ? .infinity : 760, alignment: .leading)
        .padding(.horizontal, compact ? 12 : 24)
        .padding(.vertical, 4)
    }

    private func notifyLayoutChange() {
        layoutChangeAction?()
        DispatchQueue.main.async {
            layoutChangeAction?()
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.14) {
            layoutChangeAction?()
        }
    }
}

#if os(macOS)
private struct MacMessageRow: View {
    let message: ChatMessage
    let auxiliaryMessages: [AuxiliaryUserMessage]
    let layoutChangeAction: (() -> Void)?
    let inspectMessageAction: ((ChatMessage) -> Void)?
    let inspectToolAction: ((ChatMessage, ToolActivity) -> Void)?
    let openAttachmentAction: ((ChatAttachment) -> Void)?

    var body: some View {
        if message.role == .user {
            userRow
        } else {
            assistantRow
        }
    }

    private var userRow: some View {
        VStack(alignment: .trailing, spacing: 6) {
            if !auxiliaryMessages.isEmpty {
                AuxiliaryDotsView(messages: auxiliaryMessages)
            }

            VStack(alignment: .trailing, spacing: 8) {
                if !message.body.isEmpty {
                    if shouldUseLightweightUserText {
                        Text(message.body)
                            .font(.body)
                            .lineSpacing(2)
                            .multilineTextAlignment(.leading)
                            .textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    } else {
                        MarkdownContentView(text: message.body, compact: true, fillsWidth: true)
                    }
                }

                SelectionReferenceStripView(
                    selections: message.selectionReferences ?? [],
                    compact: true,
                    alignment: .trailing,
                    fillsWidth: true
                )

                AttachmentStripView(
                    attachments: message.attachments,
                    compact: true,
                    alignment: .trailing,
                    fillsWidth: true,
                    openAttachmentAction: openAttachmentAction
                )

                if !message.toolActivities.isEmpty {
                    ToolBatchSummaryView(
                        activities: message.toolActivities,
                        compact: true,
                        layoutChangeAction: layoutChangeAction,
                        toolAction: { activity in
                            inspectToolAction?(message, activity)
                        }
                    )
                }
            }
            .frame(maxWidth: .infinity, alignment: .trailing)
            .padding(.horizontal, userBubblePadding.horizontal)
            .padding(.vertical, userBubblePadding.vertical)
            .background(userBubbleBackgroundColor)
            .foregroundStyle(Color.white)
            .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))

            HStack(spacing: 7) {
                if let tokenUsage = message.tokenUsage, tokenUsage.hasUsage {
                    TokenUsagePill(usage: tokenUsage)
                }

                Text(message.timestamp, style: .time)
                if message.pending {
                    Text("正在发送")
                }
                if let error = message.error {
                    Text(error)
                        .foregroundStyle(.red)
                }
            }
            .font(.caption)
            .foregroundStyle(.tertiary)
        }
        .frame(maxWidth: 640, alignment: .trailing)
        .frame(maxWidth: .infinity, alignment: .trailing)
        .padding(.horizontal, 28)
        .padding(.vertical, 7)
        .contextMenu {
            Button {
                inspectMessageAction?(message)
            } label: {
                Label("Message Detail", systemImage: "doc.text.magnifyingglass")
            }
        }
    }

    private var assistantRow: some View {
        HStack(alignment: .top, spacing: 11) {
            avatar

            VStack(alignment: .leading, spacing: 10) {
                HStack(spacing: 8) {
                    Text(roleLabel)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)

                    if !auxiliaryMessages.isEmpty {
                        AuxiliaryDotsView(messages: auxiliaryMessages)
                    }

                    if message.pending {
                        Text("正在发送")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }

                    Spacer(minLength: 0)

                    if let tokenUsage = message.tokenUsage, tokenUsage.hasUsage {
                        TokenUsagePill(usage: tokenUsage)
                    }

                    Button {
                        inspectMessageAction?(message)
                    } label: {
                        Image(systemName: "doc.text.magnifyingglass")
                            .font(.caption.weight(.semibold))
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(.secondary)
                    .help("Message Detail")
                }

                if !message.body.isEmpty {
                    MarkdownContentView(text: message.body)
                }

                SelectionReferenceStripView(selections: message.selectionReferences ?? [])

                AttachmentStripView(
                    attachments: message.attachments,
                    openAttachmentAction: openAttachmentAction
                )

                if message.body.isEmpty && message.selectionReferences?.isEmpty != false && message.attachments.isEmpty && message.toolActivities.isEmpty,
                   let tokenUsage = message.tokenUsage,
                   tokenUsage.hasUsage {
                    TokenUsageSummaryView(usage: tokenUsage)
                }

                if !message.toolActivities.isEmpty {
                    ToolBatchSummaryView(
                        activities: message.toolActivities,
                        compact: false,
                        layoutChangeAction: layoutChangeAction,
                        toolAction: { activity in
                            inspectToolAction?(message, activity)
                        }
                    )
                }

                if let error = message.error {
                    Text(error)
                        .font(.caption)
                        .foregroundStyle(.red)
                }

                Text(message.timestamp, style: .time)
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
            .frame(maxWidth: 860, alignment: .leading)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 28)
        .padding(.vertical, 11)
        .contextMenu {
            Button {
                inspectMessageAction?(message)
            } label: {
                Label("Message Detail", systemImage: "doc.text.magnifyingglass")
            }
        }
    }

    private var avatar: some View {
        ZStack {
            Circle()
                .fill(roleAvatarBackground)
                .frame(width: 30, height: 30)

            Image(systemName: roleSymbol)
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(.white)
        }
        .padding(.top, 1)
    }

    private var roleLabel: String {
        message.userName ?? message.role.rawValue.capitalized
    }

    private var roleSymbol: String {
        switch message.role {
        case .user:
            "arrow.turn.down.left"
        case .assistant:
            "sparkles"
        case .tool:
            "terminal"
        case .system:
            "info.circle"
        }
    }

    private var roleAvatarBackground: some ShapeStyle {
        switch message.role {
        case .assistant:
            LinearGradient(colors: [.purple, .blue], startPoint: .topLeading, endPoint: .bottomTrailing)
        case .tool:
            LinearGradient(colors: [.orange, .yellow], startPoint: .topLeading, endPoint: .bottomTrailing)
        case .system:
            LinearGradient(colors: [.gray, .secondary], startPoint: .topLeading, endPoint: .bottomTrailing)
        case .user:
            LinearGradient(colors: [.blue, .cyan], startPoint: .topLeading, endPoint: .bottomTrailing)
        }
    }

    private var roleColor: Color {
        switch message.role {
        case .user:
            .accentColor
        case .assistant:
            .primary
        case .tool:
            .orange
        case .system:
            .secondary
        }
    }

    private var userBubbleBackgroundColor: Color {
        isUserImageOnlyMessage ? .clear : Color.accentColor
    }

    private var userBubblePadding: (horizontal: CGFloat, vertical: CGFloat) {
        isUserImageOnlyMessage ? (0, 0) : (14, 9)
    }

    private var isUserImageOnlyMessage: Bool {
        message.body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && message.selectionReferences?.isEmpty != false
            && !message.attachments.isEmpty
            && message.attachments.allSatisfy(\.isImage)
            && message.toolActivities.isEmpty
    }

    private var shouldUseLightweightUserText: Bool {
        let text = message.body
        guard text.count < 700,
              !text.contains("```"),
              !text.contains("|"),
              !text.contains("#"),
              !text.contains(">"),
              !text.contains("*"),
              !text.contains("- ")
        else {
            return false
        }
        return true
    }
}
#else
private struct IOSMessageBubble: View {
    let message: ChatMessage
    let auxiliaryMessages: [AuxiliaryUserMessage]
    let layoutChangeAction: (() -> Void)?
    let inspectMessageAction: ((ChatMessage) -> Void)?
    let inspectToolAction: ((ChatMessage, ToolActivity) -> Void)?
    let openAttachmentAction: ((ChatAttachment) -> Void)?

    var body: some View {
        if isUser {
            userBubble
        } else {
            assistantBlock
        }
    }

    private var userBubble: some View {
        HStack(alignment: .bottom) {
            Spacer(minLength: 36)
            VStack(alignment: isUser ? .trailing : .leading, spacing: 5) {
                HStack(spacing: 6) {
                    Text(roleLabel)
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)

                    if !auxiliaryMessages.isEmpty {
                        AuxiliaryDotsView(messages: auxiliaryMessages)
                    }
                }

                VStack(alignment: isUser ? .trailing : .leading, spacing: 8) {
                    if !message.body.isEmpty {
                        if shouldUseLightweightUserText {
                            Text(message.body)
                                .font(.body)
                                .lineSpacing(2)
                                .multilineTextAlignment(.leading)
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        } else {
                            MarkdownContentView(text: message.body, compact: true, fillsWidth: true)
                    }
                }

                    SelectionReferenceStripView(
                        selections: message.selectionReferences ?? [],
                        compact: true,
                        alignment: .trailing,
                        fillsWidth: true
                    )

                    AttachmentStripView(
                        attachments: message.attachments,
                        compact: true,
                        alignment: .trailing,
                        fillsWidth: true,
                        openAttachmentAction: openAttachmentAction
                    )

                    if !message.toolActivities.isEmpty {
                        ToolBatchSummaryView(
                            activities: message.toolActivities,
                            compact: true,
                            layoutChangeAction: layoutChangeAction,
                            toolAction: { activity in
                                inspectToolAction?(message, activity)
                            }
                        )
                    }
                }
                .padding(.horizontal, bubblePadding.horizontal)
                .padding(.vertical, bubblePadding.vertical)
                .background(userBubbleBackgroundColor)
                .foregroundStyle(isUser ? Color.white : Color.primary)
                .clipShape(RoundedRectangle(cornerRadius: 17, style: .continuous))

                HStack(spacing: 6) {
                    Text(message.timestamp, style: .time)
                    if let tokenUsage = message.tokenUsage, tokenUsage.hasUsage {
                        TokenUsagePill(usage: tokenUsage)
                    }
                    if message.pending {
                        Text("Sending")
                    }
                    if let error = message.error {
                        Text(error)
                            .foregroundStyle(.red)
                    }
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
            }
            .frame(maxWidth: 520, alignment: isUser ? .trailing : .leading)
        }
        .frame(maxWidth: .infinity, alignment: isUser ? .trailing : .leading)
        .padding(.horizontal, 12)
        .padding(.vertical, 5)
        .contextMenu {
            Button {
                inspectMessageAction?(message)
            } label: {
                Label("Message Detail", systemImage: "doc.text.magnifyingglass")
            }
        }
    }

    private var assistantBlock: some View {
        HStack(alignment: .top, spacing: 8) {
            assistantAvatar

            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 7) {
                    Text(roleLabel)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)

                    Spacer(minLength: 0)

                    if let tokenUsage = message.tokenUsage, tokenUsage.hasUsage {
                        TokenUsagePill(usage: tokenUsage)
                    }

                    Button {
                        inspectMessageAction?(message)
                    } label: {
                        Image(systemName: "doc.text.magnifyingglass")
                            .font(.caption.weight(.semibold))
                            .frame(width: 28, height: 24)
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(.secondary)
                }

                if !message.body.isEmpty {
                    MarkdownContentView(text: message.body, compact: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }

                SelectionReferenceStripView(selections: message.selectionReferences ?? [], compact: true)

                AttachmentStripView(
                    attachments: message.attachments,
                    compact: true,
                    openAttachmentAction: openAttachmentAction
                )

                if message.body.isEmpty && message.selectionReferences?.isEmpty != false && message.attachments.isEmpty && message.toolActivities.isEmpty,
                   let tokenUsage = message.tokenUsage,
                   tokenUsage.hasUsage {
                    TokenUsageSummaryView(usage: tokenUsage, compact: true)
                }

                if !message.toolActivities.isEmpty {
                    ToolBatchSummaryView(
                        activities: message.toolActivities,
                        compact: true,
                        layoutChangeAction: layoutChangeAction,
                        toolAction: { activity in
                            inspectToolAction?(message, activity)
                        }
                    )
                }

                HStack(spacing: 6) {
                    Text(message.timestamp, style: .time)
                    if let error = message.error {
                        Text(error)
                            .foregroundStyle(.red)
                    }
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.leading, 10)
        .padding(.trailing, 14)
        .padding(.vertical, 8)
        .contextMenu {
            Button {
                inspectMessageAction?(message)
            } label: {
                Label("Message Detail", systemImage: "doc.text.magnifyingglass")
            }
        }
    }

    private var assistantAvatar: some View {
        ZStack {
            Circle()
                .fill(LinearGradient(colors: avatarColors, startPoint: .topLeading, endPoint: .bottomTrailing))
                .frame(width: 32, height: 32)

            Image(systemName: roleSymbol)
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(.white)
        }
        .padding(.top, 1)
    }

    private var isUser: Bool {
        message.role == .user
    }

    private var roleLabel: String {
        message.userName ?? message.role.rawValue.capitalized
    }

    private var roleSymbol: String {
        switch message.role {
        case .assistant:
            "sparkles"
        case .tool:
            "terminal"
        case .system:
            "info.circle"
        case .user:
            "person"
        }
    }

    private var roleColor: Color {
        switch message.role {
        case .assistant:
            .primary
        case .tool:
            .orange
        case .system:
            .secondary
        case .user:
            .accentColor
        }
    }

    private var avatarColors: [Color] {
        switch message.role {
        case .assistant:
            [.purple, .blue]
        case .tool:
            [.orange, .yellow]
        case .system:
            [.gray, .secondary]
        case .user:
            [.blue, .cyan]
        }
    }

    private var bubbleColor: Color {
        switch message.role {
        case .user:
            .accentColor
        case .assistant:
            PlatformColor.secondaryBackground
        case .tool:
            .orange.opacity(0.16)
        case .system:
            PlatformColor.secondaryBackground
        }
    }

    private var userBubbleBackgroundColor: Color {
        isUserImageOnlyMessage ? .clear : bubbleColor
    }

    private var bubblePadding: (horizontal: CGFloat, vertical: CGFloat) {
        isUserImageOnlyMessage ? (0, 0) : (13, 10)
    }

    private var isUserImageOnlyMessage: Bool {
        isUser
            && message.body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && message.selectionReferences?.isEmpty != false
            && !message.attachments.isEmpty
            && message.attachments.allSatisfy(\.isImage)
            && message.toolActivities.isEmpty
    }

    private var shouldUseLightweightUserText: Bool {
        guard isUser else {
            return false
        }
        let text = message.body
        guard text.count < 700,
              !text.contains("```"),
              !text.contains("|"),
              !text.contains("#"),
              !text.contains(">"),
              !text.contains("*"),
              !text.contains("- ")
        else {
            return false
        }
        return true
    }
}
#endif

struct ToolProcessGroupView: View {
    let group: ToolProcessGroup
    let compact: Bool
    var externalExpanded: Binding<Bool>? = nil
    let layoutChangeAction: (() -> Void)?
    let inspectMessageAction: ((ChatMessage) -> Void)?
    let inspectToolAction: ((ChatMessage, ToolActivity) -> Void)?
    let openAttachmentAction: ((ChatAttachment) -> Void)?

    @State private var isExpanded = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            headerButton
                .transaction { transaction in
                    transaction.animation = nil
                }
                .animation(nil, value: isExpandedValue)

            if isExpandedValue {
                ToolProcessExpandedContentView(
                    group: group,
                    compact: compact,
                    inspectToolAction: inspectToolAction,
                    openAttachmentAction: openAttachmentAction
                )
                .transition(.identity)
            }
        }
        .animation(nil, value: isExpandedValue)
        .padding(.horizontal, compact ? 12 : 24)
        .padding(.vertical, compact ? 5 : 2)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contextMenu {
            if let firstMessage = group.messages.first {
                Button {
                    inspectMessageAction?(firstMessage)
                } label: {
                    Label("Message Detail", systemImage: "doc.text.magnifyingglass")
                }
            }
        }
    }

    private var headerButton: some View {
        Button {
            #if os(iOS)
            var transaction = Transaction()
            transaction.animation = nil
            withTransaction(transaction) {
                setExpanded(!isExpandedValue)
            }
            #else
            withAnimation(StellaCodeXMotion.quick) {
                setExpanded(!isExpandedValue)
            }
            #endif
            DispatchQueue.main.async {
                notifyLayoutChange()
            }
        } label: {
            HStack(spacing: 8) {
                Image(systemName: isExpandedValue ? "chevron.down" : "chevron.right")
                    .font(.caption.weight(.bold))
                    .foregroundStyle(.tertiary)
                    .frame(width: 12)
                    .contentTransition(.identity)

                Image(systemName: "wrench.and.screwdriver")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)

                Text(summaryTitle)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)

                Spacer(minLength: 8)

                Text(summaryMeta)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private var summaryTitle: String {
        let calls = group.activities.filter { $0.kind == .call }.count
        let results = group.activities.filter { $0.kind == .result }.count
        let firstName = group.activities.first?.name ?? "tool"
        if calls > 0 && results > 0 {
            return "Ran \(calls) \(calls == 1 ? "tool" : "tools") starting with \(firstName)"
        }
        if calls > 0 {
            return "\(calls == 1 ? "Tool call" : "\(calls) tool calls") starting with \(firstName)"
        }
        return "\(results == 1 ? "Tool result" : "\(results) tool results") for \(firstName)"
    }

    private var summaryMeta: String {
        let names = group.activities.map(\.name)
        let uniqueNames = names.reduce(into: [String]()) { partialResult, name in
            if !partialResult.contains(name) {
                partialResult.append(name)
            }
        }
        let visibleNames = uniqueNames.prefix(2).joined(separator: ", ")
        let suffix = uniqueNames.count > 2 ? " +\(uniqueNames.count - 2)" : ""
        return visibleNames.isEmpty ? "collapsed" : visibleNames + suffix
    }

    private var isExpandedValue: Bool {
        externalExpanded?.wrappedValue ?? isExpanded
    }

    private func setExpanded(_ value: Bool) {
        if let externalExpanded {
            externalExpanded.wrappedValue = value
        } else {
            isExpanded = value
        }
    }

    private func notifyLayoutChange() {
        layoutChangeAction?()
        DispatchQueue.main.async {
            layoutChangeAction?()
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.14) {
            layoutChangeAction?()
        }
    }
}

struct ToolProcessExpandedContentView: View {
    let group: ToolProcessGroup
    let compact: Bool
    let inspectToolAction: ((ChatMessage, ToolActivity) -> Void)?
    let openAttachmentAction: ((ChatAttachment) -> Void)?

    @State private var isContentVisible = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(group.messages) { message in
                if let tokenUsage = message.tokenUsage, tokenUsage.hasUsage {
                    TokenUsagePill(usage: tokenUsage)
                        .padding(.horizontal, compact ? 10 : 0)
                        .padding(.top, 2)
                }

                if !message.body.isEmpty || message.selectionReferences?.isEmpty == false || !message.attachments.isEmpty {
                    VStack(alignment: .leading, spacing: 7) {
                        if !message.body.isEmpty {
                            MarkdownContentView(text: message.body, compact: compact)
                        }

                        SelectionReferenceStripView(selections: message.selectionReferences ?? [], compact: compact)

                        AttachmentStripView(
                            attachments: message.attachments,
                            compact: compact,
                            openAttachmentAction: openAttachmentAction
                        )
                    }
                    .padding(.horizontal, compact ? 10 : 0)
                    .padding(.top, 3)
                }

                ForEach(message.toolActivities) { activity in
                    Button {
                        inspectToolAction?(message, activity)
                    } label: {
                        ToolActivityRow(activity: activity)
                    }
                    .buttonStyle(.plain)
                }
            }
        }
        .padding(.top, 8)
        .padding(.bottom, 10)
        .opacity(isContentVisible ? 1 : 0)
        .offset(y: isContentVisible ? 0 : -4)
        .clipped()
        .onAppear {
            guard compact else {
                isContentVisible = true
                return
            }
            isContentVisible = false
            DispatchQueue.main.async {
                isContentVisible = true
            }
        }
        .onDisappear {
            isContentVisible = false
        }
    }
}

private struct ToolBatchSummaryView: View {
    let activities: [ToolActivity]
    let compact: Bool
    let layoutChangeAction: (() -> Void)?
    let toolAction: (ToolActivity) -> Void

    @Environment(\.colorScheme) private var colorScheme
    @State private var isExpanded = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Button {
                withAnimation(StellaCodeXMotion.quick) {
                    isExpanded.toggle()
                }
                notifyLayoutChange()
            } label: {
                HStack(spacing: 8) {
                    Image(systemName: isExpanded ? "chevron.down" : "chevron.right")
                        .font(.caption2.weight(.bold))
                        .foregroundStyle(.tertiary)
                        .frame(width: 10)

                    Image(systemName: "wrench.and.screwdriver")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)

                    VStack(alignment: .leading, spacing: 2) {
                        Text(batchTitle)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.primary)
                            .lineLimit(1)

                        Text(batchSubtitle)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }

                    Spacer(minLength: 8)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if isExpanded {
                VStack(alignment: .leading, spacing: 6) {
                    ForEach(activities) { activity in
                        Button {
                            toolAction(activity)
                        } label: {
                            ToolActivityRow(activity: activity)
                        }
                        .buttonStyle(.plain)
                    }
                }
                .padding(.top, 7)
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .padding(.horizontal, compact ? 10 : 11)
        .padding(.vertical, compact ? 8 : 9)
        .background(toolBackground)
        .clipShape(RoundedRectangle(cornerRadius: compact ? 13 : 8, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: compact ? 13 : 8, style: .continuous)
                .strokeBorder(PlatformColor.separator.opacity(0.35))
        }
    }

    private func notifyLayoutChange() {
        layoutChangeAction?()
        DispatchQueue.main.async {
            layoutChangeAction?()
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.14) {
            layoutChangeAction?()
        }
    }

    private var batchTitle: String {
        let calls = activities.filter { $0.kind == .call }.count
        let results = activities.filter { $0.kind == .result }.count
        if calls > 0 && results > 0 {
            return "\(activities.count) tool events"
        }
        if calls > 0 {
            return calls == 1 ? "1 tool call" : "\(calls) tool calls"
        }
        return results == 1 ? "1 tool result" : "\(results) tool results"
    }

    private var batchSubtitle: String {
        let names = activities.map(\.name)
        let uniqueNames = names.reduce(into: [String]()) { partialResult, name in
            if !partialResult.contains(name) {
                partialResult.append(name)
            }
        }
        let visibleNames = uniqueNames.prefix(3).joined(separator: ", ")
        let suffix = uniqueNames.count > 3 ? " +" + String(uniqueNames.count - 3) : ""
        return visibleNames.isEmpty ? "Tap to inspect" : visibleNames + suffix
    }

    private var toolBackground: Color {
        #if os(macOS)
        colorScheme == .light ? Color.black.opacity(0.035) : PlatformColor.controlBackground.opacity(0.75)
        #else
        PlatformColor.appBackground.opacity(0.72)
        #endif
    }
}

private struct ToolActivityRow: View {
    let activity: ToolActivity
    private let detailPreview: String

    @Environment(\.colorScheme) private var colorScheme

    init(activity: ToolActivity) {
        self.activity = activity
        detailPreview = Self.makeDetailPreview(activity.detail)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 7) {
                Image(systemName: activity.kind == .call ? "arrow.up.right.circle" : "checkmark.circle")
                    .foregroundStyle(activity.kind == .call ? Color.orange : Color.green)

                Text(activity.kind.rawValue)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)

                Text(activity.name)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.primary)
                    .lineLimit(1)

                Spacer(minLength: 0)
            }

            Text(activity.summary)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(2)

            if !activity.detail.isEmpty && activity.detail != activity.summary {
                Text(detailPreview)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(4)
                    .textSelection(.enabled)
            }
        }
        .padding(8)
        .background(rowBackground)
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(rowBorder)
        }
    }

    private static func makeDetailPreview(_ detail: String) -> String {
        let trimmed = detail.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.count > 360 else {
            return trimmed
        }
        return String(trimmed.prefix(360)) + "..."
    }

    private var rowBackground: Color {
        #if os(macOS)
        colorScheme == .light ? Color.black.opacity(0.035) : PlatformColor.secondaryBackground.opacity(0.65)
        #else
        PlatformColor.secondaryBackground.opacity(0.65)
        #endif
    }

    private var rowBorder: Color {
        #if os(macOS)
        colorScheme == .light ? PlatformColor.separator.opacity(0.22) : Color.clear
        #else
        Color.clear
        #endif
    }
}

private struct TokenUsagePill: View {
    let usage: TokenUsage
    @State private var isPresented = false
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        Button {
            isPresented = true
        } label: {
            HStack(spacing: 5) {
                Circle()
                    .fill(statusColor.opacity(0.9))
                    .frame(width: 6, height: 6)

                Text(formatTokens(usage.total))
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(pillBackground)
            .clipShape(Capsule())
            .overlay {
                Capsule()
                    .strokeBorder(PlatformColor.separator.opacity(0.35))
            }
        }
        .buttonStyle(.plain)
        .help("Token Usage")
        .popover(isPresented: $isPresented) {
            TokenUsagePopover(usage: usage)
                .compactDetailPresentation(height: 210)
        }
    }

    private var statusColor: Color {
        guard usage.total > 0 else {
            return .red
        }
        return Double(usage.cacheRead) / Double(usage.total) >= 0.8 ? .green : .red
    }

    private var pillBackground: Color {
        #if os(macOS)
        colorScheme == .light ? Color.black.opacity(0.055) : PlatformColor.secondaryBackground.opacity(0.72)
        #else
        PlatformColor.secondaryBackground.opacity(0.72)
        #endif
    }
}

private struct TokenUsageSummaryView: View {
    let usage: TokenUsage
    var compact = false

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(spacing: 7) {
                Image(systemName: "chart.bar")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)

                Text("Token Usage")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)

                TokenUsagePill(usage: usage)
            }

            TokenUsageGrid(usage: usage)
        }
        .padding(.horizontal, compact ? 10 : 11)
        .padding(.vertical, compact ? 8 : 9)
        .background(PlatformColor.secondaryBackground.opacity(0.58))
        .clipShape(RoundedRectangle(cornerRadius: compact ? 13 : 8, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: compact ? 13 : 8, style: .continuous)
                .strokeBorder(PlatformColor.separator.opacity(0.32))
        }
    }
}

private struct TokenUsagePopover: View {
    let usage: TokenUsage

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            Text("Token Usage")
                .font(.caption.weight(.semibold))

            TokenUsageGrid(usage: usage)
        }
        .frame(width: 230, alignment: .leading)
        .padding(12)
    }
}

private struct TokenUsageGrid: View {
    let usage: TokenUsage

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            usageRow("Input", usage.input)
            usageRow("Output", usage.output)
            usageRow("Cache Read", usage.cacheRead)
            usageRow("Cache Write", usage.cacheWrite)
            Divider()
            usageRow("Total", usage.total)
            if let costUSD = usage.costUSD {
                usageRow("Cost", formatCost(costUSD))
            }
        }
    }

    private func usageRow(_ label: String, _ value: Int) -> some View {
        usageRow(label, value.formatted())
    }

    private func usageRow(_ label: String, _ value: String) -> some View {
        HStack {
            Text(label)
                .font(.caption)
                .foregroundStyle(.secondary)

            Spacer(minLength: 12)

            Text(value)
                .font(.caption.monospacedDigit().weight(.semibold))
                .foregroundStyle(.primary)
        }
    }
}

private func formatTokens(_ value: Int) -> String {
    if value >= 1_000_000 {
        return String(format: "%.1fM tokens", Double(value) / 1_000_000)
    }
    if value >= 1_000 {
        return "\(Int(round(Double(value) / 1_000)))K tokens"
    }
    return "\(value) tokens"
}

private func formatCost(_ value: Double) -> String {
    String(format: "$%.3f", value)
}

private extension View {
    @ViewBuilder
    func compactDetailPresentation(height: CGFloat) -> some View {
        #if os(iOS)
        self
            .presentationCompactAdaptation(.sheet)
            .presentationDetents([.height(height)])
            .presentationDragIndicator(.visible)
        #else
        self
        #endif
    }
}

struct ActivityStatusView: View {
    let status: String?
    let isRunning: Bool
    let turnProgress: TurnProgressFeedback?

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 10) {
                if isRunning || statusKind.isActive {
                    ProgressView()
                        .controlSize(.small)
                } else {
                    Image(systemName: statusKind.icon)
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(statusKind.color)
                }

                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 7) {
                        Text(statusKind.title)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.primary)

                        if let planSummary {
                            Text(planSummary)
                                .font(.caption2.monospacedDigit())
                                .foregroundStyle(.tertiary)
                        }
                    }

                    Text(detailText)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }

                Spacer(minLength: 8)
            }

            if let plan = turnProgress?.plan,
               !plan.items.isEmpty || plan.explanation?.isEmpty == false {
                ActivityPlanView(plan: plan)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(PlatformColor.controlBackground.opacity(0.72))
        .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(PlatformColor.separator.opacity(0.32))
        }
    }

    static func shouldDisplay(_ status: String?) -> Bool {
        let normalized = status?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() ?? ""
        return !normalized.isEmpty
            && normalized != "connected"
            && normalized != "subscribed"
            && normalized != "disconnected"
            && normalized != "done"
            && normalized != "done: done"
            && normalized != "completed"
    }

    private var cleanStatus: String {
        status?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    }

    private var detailText: String {
        if let turnProgress {
            let detail = turnProgress.subtitle
            return detail.isEmpty ? turnProgress.activity : detail
        }
        return cleanStatus
    }

    private var statusKind: StatusKind {
        StatusKind(status: cleanStatus, isRunning: isRunning, turnProgress: turnProgress)
    }

    private var planSummary: String? {
        guard let plan = turnProgress?.plan,
              !plan.items.isEmpty
        else {
            return nil
        }
        let completed = plan.items.filter { ActivityPlanView.normalizedStatus($0.status) == "completed" }.count
        return "\(completed)/\(plan.items.count)"
    }

    private struct StatusKind {
        let title: String
        let icon: String
        let color: Color
        let isActive: Bool

        init(status: String, isRunning: Bool, turnProgress: TurnProgressFeedback?) {
            if let turnProgress {
                self.title = turnProgress.title
                self.icon = turnProgress.isActive ? "arrow.triangle.2.circlepath" : "checkmark.circle"
                self.color = turnProgress.finalState == "failed" ? .red : .accentColor
                self.isActive = turnProgress.isActive
                return
            }
            let normalized = status.lowercased()
            if normalized.contains("tool") {
                self.title = "Tool running"
                self.icon = "wrench.and.screwdriver"
                self.color = .orange
                self.isActive = true
            } else if normalized.contains("think") || normalized.contains("reason") {
                self.title = "Thinking"
                self.icon = "brain"
                self.color = .accentColor
                self.isActive = true
            } else if normalized.contains("error") || normalized.contains("failed") {
                self.title = "Interrupted"
                self.icon = "exclamationmark.triangle"
                self.color = .red
                self.isActive = false
            } else if isRunning || normalized.contains("running") || normalized.contains("progress") {
                self.title = "Running"
                self.icon = "arrow.triangle.2.circlepath"
                self.color = .accentColor
                self.isActive = true
            } else {
                self.title = "Status"
                self.icon = "info.circle"
                self.color = .secondary
                self.isActive = false
            }
        }
    }
}

private struct ActivityPlanView: View {
    let plan: TurnProgressPlan

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            if let explanation = plan.explanation?.trimmingCharacters(in: .whitespacesAndNewlines),
               !explanation.isEmpty {
                Text(explanation)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }

            VStack(alignment: .leading, spacing: 5) {
                ForEach(Array(plan.items.enumerated()), id: \.offset) { _, item in
                    HStack(alignment: .firstTextBaseline, spacing: 7) {
                        Image(systemName: iconName(for: item.status))
                            .font(.caption2.weight(.bold))
                            .foregroundStyle(color(for: item.status))
                            .frame(width: 13)

                        Text(item.step)
                            .font(.caption2)
                            .foregroundStyle(foreground(for: item.status))
                            .lineLimit(2)

                        Spacer(minLength: 0)
                    }
                    .padding(.vertical, 1)
                }
            }
        }
        .padding(.leading, 28)
    }

    static func normalizedStatus(_ status: String) -> String {
        switch status.lowercased() {
        case "completed", "done", "success":
            return "completed"
        case "in_progress", "running", "active":
            return "in_progress"
        default:
            return "pending"
        }
    }

    private func iconName(for status: String) -> String {
        switch Self.normalizedStatus(status) {
        case "completed":
            return "checkmark.circle.fill"
        case "in_progress":
            return "circle.dotted"
        default:
            return "circle"
        }
    }

    private func color(for status: String) -> Color {
        switch Self.normalizedStatus(status) {
        case "completed":
            return .green
        case "in_progress":
            return .accentColor
        default:
            return .secondary.opacity(0.6)
        }
    }

    private func foreground(for status: String) -> Color {
        switch Self.normalizedStatus(status) {
        case "completed":
            return .secondary
        case "in_progress":
            return .primary
        default:
            return .secondary
        }
    }
}
