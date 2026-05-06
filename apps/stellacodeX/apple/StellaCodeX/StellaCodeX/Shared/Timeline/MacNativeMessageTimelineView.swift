#if os(macOS)
import AppKit
import SwiftUI

struct MacNativeMessageTimelineView: NSViewRepresentable {
    let renderData: TimelineRenderData
    let hasOlderMessages: Bool
    let isLoadingMessages: Bool
    let isLoadingOlderMessages: Bool
    let activityStatus: String?
    let isConversationRunning: Bool
    let turnProgress: TurnProgressFeedback?
    let bottomScrollTrigger: Int
    let bottomScrollRequiresNearBottom: Bool
    let bottomLayoutChangeTrigger: Int
    var loadOlderAction: (() -> Void)?
    var inspectMessageAction: ((ChatMessage) -> Void)?
    var inspectToolAction: ((ChatMessage, ToolActivity) -> Void)?

    func makeCoordinator() -> Coordinator {
        Coordinator()
    }

    func makeNSView(context: Context) -> NSScrollView {
        let tableView = NSTableView(frame: .zero)
        tableView.headerView = nil
        tableView.intercellSpacing = .zero
        tableView.rowSizeStyle = .custom
        tableView.backgroundColor = .clear
        tableView.usesAlternatingRowBackgroundColors = false
        tableView.selectionHighlightStyle = .none
        tableView.allowsColumnSelection = false
        tableView.allowsMultipleSelection = false
        tableView.allowsEmptySelection = true
        tableView.delegate = context.coordinator
        tableView.dataSource = context.coordinator

        let column = NSTableColumn(identifier: Coordinator.columnIdentifier)
        column.resizingMask = .autoresizingMask
        tableView.addTableColumn(column)

        let scrollView = NSScrollView(frame: .zero)
        scrollView.drawsBackground = false
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = false
        scrollView.autohidesScrollers = true
        scrollView.documentView = tableView

        context.coordinator.tableView = tableView
        context.coordinator.scrollView = scrollView
        context.coordinator.installBoundsObserver()
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        context.coordinator.update(view: self, scrollView: scrollView)
    }

    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate {
        static let columnIdentifier = NSUserInterfaceItemIdentifier("TimelineColumn")
        private static let cellIdentifier = NSUserInterfaceItemIdentifier("TimelineCell")

        weak var tableView: NSTableView?
        weak var scrollView: NSScrollView?

        private var rows: [MacTimelineRow] = []
        private var previousSnapshot: TimelineSnapshot?
        private var rowHeightCache: [MacTimelineHeightKey: CGFloat] = [:]
        private var olderLoadAnchorID: TimelineEntry.ID?
        private var lastBottomScrollTrigger = 0
        private var lastBottomLayoutChangeTrigger = 0
        private var isLoadingMessages = false
        private var isLoadingOlderMessages = false
        private var hasOlderMessages = false
        private var loadOlderAction: (() -> Void)?
        private var inspectMessageAction: ((ChatMessage) -> Void)?
        private var inspectToolAction: ((ChatMessage, ToolActivity) -> Void)?
        private var bottomScrollRequiresNearBottom = false
        private var isProgrammaticScrollInFlight = false
        private var lastManualScrollAt = Date.distantPast
        private var lastMeasuredWidth: CGFloat = 0
        private var pendingReloadWorkItem: DispatchWorkItem?
        private var heightPrewarmToken = 0
        private var expandedRowIDs = Set<String>()

        func numberOfRows(in tableView: NSTableView) -> Int {
            rows.count
        }

        func tableView(_ tableView: NSTableView, heightOfRow row: Int) -> CGFloat {
            guard rows.indices.contains(row) else {
                return 1
            }
            let width = max(tableView.bounds.width, 320)
            let key = MacTimelineHeightKey(row: rows[row], width: roundedWidth(width))
            if let cached = rowHeightCache[key] {
                return cached
            }

            let height = rows[row].fastMeasuredHeight(width: width, expandedRowIDs: expandedRowIDs) ?? measuredHeight(for: rows[row], width: width)
            rowHeightCache[key] = height
            return height
        }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            guard rows.indices.contains(row) else {
                return nil
            }

            let cell = tableView.makeView(withIdentifier: Self.cellIdentifier, owner: self) as? MacTimelineHostingCell
                ?? MacTimelineHostingCell(identifier: Self.cellIdentifier)
            cell.update(rootView: rootView(for: rows[row], width: tableView.bounds.width))
            return cell
        }

        func update(view: MacNativeMessageTimelineView, scrollView: NSScrollView) {
            guard let tableView else {
                return
            }

            let nextRows = MacTimelineRow.rows(
                renderData: view.renderData,
                hasOlderMessages: view.hasOlderMessages,
                isLoadingMessages: view.isLoadingMessages,
                isLoadingOlderMessages: view.isLoadingOlderMessages,
                activityStatus: view.activityStatus,
                isConversationRunning: view.isConversationRunning,
                turnProgress: view.turnProgress,
                shouldDisplayBottomLoading: view.isLoadingMessages && !view.renderData.entries.isEmpty
            )
            let nextSnapshot = view.renderData.snapshot
            let previousSnapshot = previousSnapshot
            let wasNearBottom = isNearBottom || !hasCompletedUserScroll
            let contentWidth = max(scrollView.contentView.bounds.width, 320)
            tableView.tableColumns.first?.width = contentWidth
            let width = contentWidth

            hasOlderMessages = view.hasOlderMessages
            isLoadingMessages = view.isLoadingMessages
            isLoadingOlderMessages = view.isLoadingOlderMessages
            loadOlderAction = view.loadOlderAction
            inspectMessageAction = view.inspectMessageAction
            inspectToolAction = view.inspectToolAction
            bottomScrollRequiresNearBottom = view.bottomScrollRequiresNearBottom

            let widthChanged = abs(lastMeasuredWidth - width) > 1
            if widthChanged {
                lastMeasuredWidth = width
                rowHeightCache.removeAll()
            }
            if abs(tableView.frame.width - contentWidth) > 1 {
                tableView.setFrameSize(NSSize(width: contentWidth, height: tableView.frame.height))
            }
            let rowsChanged = widthChanged || rowKeys(rows) != rowKeys(nextRows)

            rows = nextRows
            self.previousSnapshot = nextSnapshot
            if rowsChanged {
                reloadPreservingSelection(tableView)
            }

            if let previousSnapshot {
                let transaction = TimelineScrollTransaction(
                    previous: previousSnapshot,
                    current: nextSnapshot,
                    olderLoadAnchorID: olderLoadAnchorID,
                    canAutoAlignToBottom: wasNearBottom,
                    didCompleteInitialBottomScroll: true
                )
                apply(transaction, tableView: tableView)
            } else {
                scrollToBottom(tableView, animated: false)
            }

            if lastBottomScrollTrigger != view.bottomScrollTrigger {
                lastBottomScrollTrigger = view.bottomScrollTrigger
                if !view.bottomScrollRequiresNearBottom || isNearBottom {
                    scrollToBottom(tableView, animated: true)
                }
            }

            if lastBottomLayoutChangeTrigger != view.bottomLayoutChangeTrigger {
                lastBottomLayoutChangeTrigger = view.bottomLayoutChangeTrigger
                if isNearBottom {
                    invalidateHeightsAndScrollBottom(tableView)
                }
            }

            if !view.isLoadingMessages,
               nextSnapshot.lastEntryID != nil,
               !hasCompletedUserScroll {
                scrollToBottom(tableView, animated: false)
            }
        }

        func installBoundsObserver() {
            guard let scrollView else {
                return
            }
            scrollView.contentView.postsBoundsChangedNotifications = true
            NotificationCenter.default.addObserver(
                self,
                selector: #selector(boundsDidChange(_:)),
                name: NSView.boundsDidChangeNotification,
                object: scrollView.contentView
            )
        }

        @objc private func boundsDidChange(_ notification: Notification) {
            guard !isProgrammaticScrollInFlight else {
                return
            }
            lastManualScrollAt = Date()
            triggerLoadOlderIfNeeded()
        }

        private var hasCompletedUserScroll: Bool {
            lastManualScrollAt > .distantPast
        }

        private var isNearBottom: Bool {
            guard let scrollView,
                  let documentView = scrollView.documentView
            else {
                return true
            }
            let visibleMaxY = scrollView.contentView.bounds.maxY
            return documentView.bounds.height - visibleMaxY < 48
        }

        private func triggerLoadOlderIfNeeded() {
            guard hasOlderMessages,
                  !isLoadingMessages,
                  !isLoadingOlderMessages,
                  let scrollView,
                  scrollView.contentView.bounds.minY <= 80
            else {
                return
            }
            olderLoadAnchorID = previousSnapshot?.firstEntryID
            loadOlderAction?()
        }

        private func apply(_ transaction: TimelineScrollTransaction, tableView: NSTableView) {
            switch transaction.intent {
            case .none:
                return
            case .alignBottom(let animated):
                scrollToBottom(tableView, animated: animated)
            case .preserveAnchor(let anchorID):
                olderLoadAnchorID = nil
                scrollToRow(withID: anchorID, tableView: tableView, anchor: .top)
            case .resetToBottom:
                olderLoadAnchorID = nil
                scrollToBottom(tableView, animated: false)
            }
        }

        private func invalidateHeightsAndScrollBottom(_ tableView: NSTableView) {
            rowHeightCache.removeAll()
            scheduleHeightPrewarm(tableView: tableView)
            if tableView.numberOfRows > 0 {
                tableView.noteHeightOfRows(withIndexesChanged: IndexSet(integersIn: 0..<tableView.numberOfRows))
            }
            scrollToBottom(tableView, animated: false)
        }

        private func reloadPreservingSelection(_ tableView: NSTableView) {
            pendingReloadWorkItem?.cancel()
            tableView.reloadData()
            let workItem = DispatchWorkItem { [weak tableView] in
                guard let tableView else {
                    return
                }
                self.scheduleHeightPrewarm(tableView: tableView)
            }
            pendingReloadWorkItem = workItem
            DispatchQueue.main.async(execute: workItem)
        }

        private func scrollToBottom(_ tableView: NSTableView, animated: Bool) {
            guard tableView.numberOfRows > 0 else {
                return
            }
            isProgrammaticScrollInFlight = true
            let row = tableView.numberOfRows - 1
            let action = {
                tableView.scrollRowToVisible(row)
                if let scrollView = tableView.enclosingScrollView,
                   let documentView = scrollView.documentView {
                    let bottomY = max(0, documentView.bounds.height - scrollView.contentView.bounds.height)
                    scrollView.contentView.scroll(to: NSPoint(x: 0, y: bottomY))
                    scrollView.reflectScrolledClipView(scrollView.contentView)
                }
            }

            if animated {
                NSAnimationContext.runAnimationGroup { context in
                    context.duration = 0.16
                    action()
                }
            } else {
                action()
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.18) { [weak self] in
                self?.isProgrammaticScrollInFlight = false
            }
        }

        private enum RowAnchor {
            case top
        }

        private func scrollToRow(withID id: TimelineEntry.ID, tableView: NSTableView, anchor: RowAnchor) {
            guard let row = rows.firstIndex(where: { $0.id == id }) else {
                return
            }
            isProgrammaticScrollInFlight = true
            tableView.scrollRowToVisible(row)
            if let scrollView = tableView.enclosingScrollView {
                let rowRect = tableView.rect(ofRow: row)
                scrollView.contentView.scroll(to: NSPoint(x: 0, y: max(0, rowRect.minY)))
                scrollView.reflectScrolledClipView(scrollView.contentView)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { [weak self] in
                self?.isProgrammaticScrollInFlight = false
            }
        }

        private func measuredHeight(for row: MacTimelineRow, width: CGFloat) -> CGFloat {
            let measuringView = NSHostingView(rootView: rootView(for: row, width: width))
            measuringView.frame = NSRect(x: 0, y: 0, width: width, height: 10)
            let height = measuringView.fittingSize.height
            return max(1, ceil(height))
        }

        private func scheduleHeightPrewarm(tableView: NSTableView) {
            heightPrewarmToken += 1
            let token = heightPrewarmToken
            let width = max(tableView.bounds.width, 320)
            let rowCount = rows.count
            guard rowCount > 0 else {
                return
            }

            func prewarmBatch(start: Int) {
                guard token == heightPrewarmToken else {
                    return
                }
                let end = min(start + 6, rowCount)
                guard start < end else {
                    return
                }

                for index in start..<end where rows.indices.contains(index) {
                    let row = rows[index]
                    let key = MacTimelineHeightKey(row: row, width: roundedWidth(width))
                    guard rowHeightCache[key] == nil else {
                        continue
                    }
                    let height = row.fastMeasuredHeight(width: width, expandedRowIDs: expandedRowIDs) ?? measuredHeight(for: row, width: width)
                    rowHeightCache[key] = height
                }

                if end < rowCount {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.02) {
                        prewarmBatch(start: end)
                    }
                }
            }

            DispatchQueue.main.async {
                prewarmBatch(start: 0)
            }
        }

        private func rootView(for row: MacTimelineRow, width: CGFloat) -> AnyView {
            let rowID = row.id
            if case .entry(.toolProcess(let group)) = row {
                return AnyView(
                    ToolProcessGroupView(
                        group: group,
                        compact: false,
                        externalExpanded: Binding(
                            get: { [weak self] in
                                self?.expandedRowIDs.contains(rowID) == true
                            },
                            set: { [weak self] isExpanded in
                                guard let self else {
                                    return
                                }
                                if isExpanded {
                                    expandedRowIDs.insert(rowID)
                                } else {
                                    expandedRowIDs.remove(rowID)
                                }
                                scheduleHeightInvalidation(for: rowID, keepBottomAligned: true)
                            }
                        ),
                        layoutChangeAction: { [weak self] in
                            self?.scheduleHeightInvalidation(for: rowID, keepBottomAligned: true)
                        },
                        inspectMessageAction: inspectMessageAction,
                        inspectToolAction: inspectToolAction
                    )
                    .frame(width: width, alignment: .leading)
                )
            }

            return AnyView(
                row.view(
                    compact: false,
                    width: width,
                    loadOlderAction: loadOlderAction,
                    layoutChangeAction: { [weak self] in
                        self?.scheduleHeightInvalidation(for: rowID, keepBottomAligned: true)
                    },
                    inspectMessageAction: inspectMessageAction,
                    inspectToolAction: inspectToolAction
                )
                .frame(width: width, alignment: .leading)
            )
        }

        private func scheduleHeightInvalidation(for rowID: String, keepBottomAligned: Bool) {
            DispatchQueue.main.async { [weak self] in
                self?.invalidateHeight(for: rowID, reloadRow: true, keepBottomAligned: keepBottomAligned)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.08) { [weak self] in
                self?.invalidateHeight(for: rowID, reloadRow: false, keepBottomAligned: keepBottomAligned)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.18) { [weak self] in
                self?.invalidateHeight(for: rowID, reloadRow: false, keepBottomAligned: keepBottomAligned)
            }
        }

        private func invalidateHeight(for rowID: String, reloadRow: Bool, keepBottomAligned: Bool) {
            guard let tableView else {
                return
            }

            rowHeightCache = rowHeightCache.filter { $0.key.id != rowID }
            if let index = rows.firstIndex(where: { $0.id == rowID }) {
                if reloadRow {
                    tableView.reloadData(forRowIndexes: IndexSet(integer: index), columnIndexes: IndexSet(integer: 0))
                }
                tableView.noteHeightOfRows(withIndexesChanged: IndexSet(integer: index))
            }
            if keepBottomAligned, isNearBottom {
                scrollToBottom(tableView, animated: false)
            }
        }

        private func roundedWidth(_ width: CGFloat) -> Int {
            Int(width.rounded(.toNearestOrAwayFromZero))
        }

        private func rowKeys(_ rows: [MacTimelineRow]) -> [MacTimelineRowKey] {
            rows.map { MacTimelineRowKey(id: $0.id, signature: $0.signature) }
        }
    }
}

private extension MacTimelineRow {
    func fastMeasuredHeight(width: CGFloat, expandedRowIDs: Set<String>) -> CGFloat? {
        switch self {
        case .emptyLoading:
            return 280
        case .older:
            return 38
        case .bottomLoading:
            return 46
        case .activity:
            return 72
        case .entry(let entry):
            return entry.fastMeasuredHeight(width: width, isExpanded: expandedRowIDs.contains(id))
        }
    }
}

private extension TimelineEntry {
    func fastMeasuredHeight(width: CGFloat, isExpanded: Bool) -> CGFloat? {
        switch self {
        case .message(let message, let auxiliaryMessages):
            return message.fastMacTimelineHeight(width: width, auxiliaryCount: auxiliaryMessages.count)
        case .auxiliary:
            return nil
        case .toolProcess:
            return isExpanded ? nil : 36
        }
    }
}

private extension ChatMessage {
    func fastMacTimelineHeight(width: CGFloat, auxiliaryCount: Int) -> CGFloat? {
        guard role == .user,
              attachments.isEmpty,
              toolActivities.isEmpty,
              !pending,
              error == nil,
              tokenUsage?.hasUsage != true
        else {
            return nil
        }

        let text = body.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, text.count < 700 else {
            return nil
        }

        let availableWidth = min(max(width - 56, 180), 640)
        let textWidth = max(80, availableWidth - 28)
        let font = NSFont.systemFont(ofSize: 14)
        let paragraphStyle = NSMutableParagraphStyle()
        paragraphStyle.lineBreakMode = .byWordWrapping
        let rect = (text as NSString).boundingRect(
            with: NSSize(width: textWidth, height: .greatestFiniteMagnitude),
            options: [.usesLineFragmentOrigin, .usesFontLeading],
            attributes: [
                .font: font,
                .paragraphStyle: paragraphStyle
            ]
        )
        let auxiliaryHeight = auxiliaryCount > 0 ? CGFloat(18 + min(auxiliaryCount, 4) * 3) : 0
        let bubbleHeight = ceil(rect.height) + 18
        let metadataHeight: CGFloat = 18
        return max(50, auxiliaryHeight + bubbleHeight + metadataHeight + 14)
    }
}

private enum MacTimelineRow: Identifiable {
    case emptyLoading
    case older(isLoading: Bool)
    case entry(TimelineEntry)
    case activity(status: String?, isRunning: Bool, progress: TurnProgressFeedback?)
    case bottomLoading

    var id: String {
        switch self {
        case .emptyLoading:
            "loading"
        case .older:
            "older"
        case .entry(let entry):
            entry.id
        case .activity:
            "activity"
        case .bottomLoading:
            "bottom-loading"
        }
    }

    var signature: Int {
        switch self {
        case .emptyLoading:
            return 1
        case .older(let isLoading):
            return isLoading ? 21 : 20
        case .entry(let entry):
            return entry.renderSignature
        case .activity(let status, let isRunning, let progress):
            var hasher = Hasher()
            hasher.combine(status)
            hasher.combine(isRunning)
            hasher.combine(progress?.isActive)
            hasher.combine(progress?.title)
            hasher.combine(progress?.subtitle)
            return hasher.finalize()
        case .bottomLoading:
            return 3
        }
    }

    static func rows(
        renderData: TimelineRenderData,
        hasOlderMessages: Bool,
        isLoadingMessages: Bool,
        isLoadingOlderMessages: Bool,
        activityStatus: String?,
        isConversationRunning: Bool,
        turnProgress: TurnProgressFeedback?,
        shouldDisplayBottomLoading: Bool
    ) -> [MacTimelineRow] {
        if isLoadingMessages && renderData.entries.isEmpty {
            return [.emptyLoading]
        }

        var rows: [MacTimelineRow] = []
        if hasOlderMessages {
            rows.append(.older(isLoading: isLoadingOlderMessages))
        }
        rows.append(contentsOf: renderData.entries.map(MacTimelineRow.entry))
        if turnProgress?.isActive == true || isConversationRunning || ActivityStatusView.shouldDisplay(activityStatus) {
            rows.append(.activity(status: activityStatus, isRunning: isConversationRunning, progress: turnProgress))
        }
        if shouldDisplayBottomLoading {
            rows.append(.bottomLoading)
        }
        return rows
    }

    @ViewBuilder
    func view(
        compact: Bool,
        width: CGFloat,
        loadOlderAction: (() -> Void)?,
        layoutChangeAction: (() -> Void)?,
        inspectMessageAction: ((ChatMessage) -> Void)?,
        inspectToolAction: ((ChatMessage, ToolActivity) -> Void)?
    ) -> some View {
        switch self {
        case .emptyLoading:
            LoadingMessagesView()
                .frame(width: width)
                .frame(minHeight: 280)
        case .older(let isLoadingOlderMessages):
            Button {
                loadOlderAction?()
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
            .frame(width: width)
        case .entry(let entry):
            TimelineEntryRowView(
                entry: entry,
                compact: compact,
                layoutChangeAction: layoutChangeAction,
                inspectMessageAction: inspectMessageAction,
                inspectToolAction: inspectToolAction
            )
            .frame(width: width, alignment: .leading)
        case .activity(let status, let isRunning, let progress):
            ActivityStatusView(status: status, isRunning: isRunning, turnProgress: progress)
                .padding(.horizontal, 24)
                .padding(.vertical, 8)
                .frame(width: width, alignment: .leading)
        case .bottomLoading:
            MacTimelineBottomLoadingMessagesView()
                .padding(.horizontal, 24)
                .padding(.vertical, 10)
                .frame(width: width)
        }
    }
}

private struct MacTimelineHeightKey: Hashable {
    let id: String
    let signature: Int
    let width: Int

    init(row: MacTimelineRow, width: Int) {
        id = row.id
        signature = row.signature
        self.width = width
    }
}

private struct MacTimelineRowKey: Hashable {
    let id: String
    let signature: Int
}

private final class MacTimelineHostingCell: NSTableCellView {
    private var hostingView: NSHostingView<AnyView>?

    convenience init(identifier: NSUserInterfaceItemIdentifier) {
        self.init(frame: .zero)
        self.identifier = identifier
        wantsLayer = true
        layer?.backgroundColor = NSColor.clear.cgColor
    }

    func update(rootView: AnyView) {
        if let hostingView {
            hostingView.rootView = rootView
            return
        }

        let hostingView = NSHostingView(rootView: rootView)
        hostingView.translatesAutoresizingMaskIntoConstraints = false
        hostingView.setContentHuggingPriority(.defaultLow, for: .horizontal)
        hostingView.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        addSubview(hostingView)
        NSLayoutConstraint.activate([
            hostingView.leadingAnchor.constraint(equalTo: leadingAnchor),
            hostingView.trailingAnchor.constraint(equalTo: trailingAnchor),
            hostingView.topAnchor.constraint(equalTo: topAnchor),
            hostingView.bottomAnchor.constraint(equalTo: bottomAnchor)
        ])
        self.hostingView = hostingView
    }
}

private struct MacTimelineBottomLoadingMessagesView: View {
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
    }
}
#endif
