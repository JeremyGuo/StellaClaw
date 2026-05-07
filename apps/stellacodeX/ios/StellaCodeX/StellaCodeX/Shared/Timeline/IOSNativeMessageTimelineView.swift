#if os(iOS)
import SwiftUI
import UIKit

struct IOSNativeMessageTimelineView: UIViewRepresentable {
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

    func makeUIView(context: Context) -> UITableView {
        let tableView = UITableView(frame: .zero, style: .plain)
        tableView.dataSource = context.coordinator
        tableView.delegate = context.coordinator
        tableView.separatorStyle = .none
        tableView.backgroundColor = .clear
        tableView.rowHeight = UITableView.automaticDimension
        tableView.estimatedRowHeight = 120
        tableView.keyboardDismissMode = .interactive
        tableView.showsVerticalScrollIndicator = true
        tableView.allowsSelection = false
        tableView.contentInsetAdjustmentBehavior = .never
        tableView.register(UITableViewCell.self, forCellReuseIdentifier: Coordinator.cellIdentifier)
        context.coordinator.tableView = tableView
        return tableView
    }

    func updateUIView(_ tableView: UITableView, context: Context) {
        context.coordinator.update(view: self, tableView: tableView)
    }

    final class Coordinator: NSObject, UITableViewDataSource, UITableViewDelegate {
        static let cellIdentifier = "IOSNativeMessageTimelineCell"

        weak var tableView: UITableView?

        private var rows: [IOSTimelineRow] = []
        private var previousSnapshot: TimelineSnapshot?
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
        private var rowHeightCache: [IOSTimelineHeightKey: CGFloat] = [:]
        private var lastMeasuredWidth: CGFloat = 0
        private var expandedToolProcessIDs = Set<String>()

        func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
            rows.count
        }

        func tableView(_ tableView: UITableView, estimatedHeightForRowAt indexPath: IndexPath) -> CGFloat {
            guard rows.indices.contains(indexPath.row) else {
                return 80
            }
            let width = max(tableView.bounds.width, 320)
            let key = IOSTimelineHeightKey(row: rows[indexPath.row], width: roundedWidth(width))
            if let cached = rowHeightCache[key] {
                return cached
            }
            return rows[indexPath.row].estimatedHeight(width: width)
        }

        func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
            let cell = tableView.dequeueReusableCell(withIdentifier: Self.cellIdentifier, for: indexPath)
            guard rows.indices.contains(indexPath.row) else {
                cell.contentConfiguration = nil
                return cell
            }

            cell.selectionStyle = .none
            cell.backgroundColor = .clear
            cell.contentView.backgroundColor = .clear
            cell.contentConfiguration = UIHostingConfiguration {
                rootView(for: rows[indexPath.row], width: max(tableView.bounds.width, 320))
            }
            .margins(.all, 0)
            .background(.clear)
            return cell
        }

        func tableView(_ tableView: UITableView, willDisplay cell: UITableViewCell, forRowAt indexPath: IndexPath) {
            guard rows.indices.contains(indexPath.row), cell.bounds.height > 1 else {
                return
            }
            let width = max(tableView.bounds.width, 320)
            let key = IOSTimelineHeightKey(row: rows[indexPath.row], width: roundedWidth(width))
            rowHeightCache[key] = ceil(cell.bounds.height)
        }

        func update(view: IOSNativeMessageTimelineView, tableView: UITableView) {
            let nextRows = IOSTimelineRow.rows(
                renderData: view.renderData,
                hasOlderMessages: view.hasOlderMessages,
                isLoadingMessages: view.isLoadingMessages,
                isLoadingOlderMessages: view.isLoadingOlderMessages,
                activityStatus: view.activityStatus,
                isConversationRunning: view.isConversationRunning,
                turnProgress: view.turnProgress,
                expandedToolProcessIDs: expandedToolProcessIDs,
                shouldDisplayBottomLoading: view.isLoadingMessages && !view.renderData.entries.isEmpty
            )
            let nextSnapshot = view.renderData.snapshot
            let previousSnapshot = previousSnapshot
            let wasNearBottom = isNearBottom || !hasCompletedUserScroll

            hasOlderMessages = view.hasOlderMessages
            isLoadingMessages = view.isLoadingMessages
            isLoadingOlderMessages = view.isLoadingOlderMessages
            loadOlderAction = view.loadOlderAction
            inspectMessageAction = view.inspectMessageAction
            inspectToolAction = view.inspectToolAction
            bottomScrollRequiresNearBottom = view.bottomScrollRequiresNearBottom
            let width = max(tableView.bounds.width, 320)
            if abs(lastMeasuredWidth - width) > 1 {
                lastMeasuredWidth = width
                rowHeightCache.removeAll()
            }

            let previousRows = rows
            let previousIDs = previousRows.map(\.id)
            let nextIDs = nextRows.map(\.id)
            rows = nextRows
            self.previousSnapshot = nextSnapshot

            if previousIDs == nextIDs {
                reloadVisibleChangedRows(previousRows: previousRows, tableView: tableView)
            } else {
                reload(tableView)
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
                    invalidateVisibleHeights(tableView, keepBottomAligned: true)
                }
            }

            if !view.isLoadingMessages,
               nextSnapshot.lastEntryID != nil,
               !hasCompletedUserScroll {
                scrollToBottom(tableView, animated: false)
            }
        }

        func scrollViewDidScroll(_ scrollView: UIScrollView) {
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
            guard let tableView else {
                return true
            }
            let visibleBottom = tableView.contentOffset.y + tableView.bounds.height - tableView.adjustedContentInset.bottom
            return tableView.contentSize.height - visibleBottom < 56
        }

        private func triggerLoadOlderIfNeeded() {
            guard hasOlderMessages,
                  !isLoadingMessages,
                  !isLoadingOlderMessages,
                  let tableView,
                  tableView.contentOffset.y <= 80
            else {
                return
            }
            olderLoadAnchorID = previousSnapshot?.firstEntryID
            loadOlderAction?()
        }

        private func reload(_ tableView: UITableView) {
            UIView.performWithoutAnimation {
                tableView.reloadData()
                tableView.layoutIfNeeded()
            }
        }

        private func reloadVisibleChangedRows(previousRows: [IOSTimelineRow], tableView: UITableView) {
            let visibleRows = Set((tableView.indexPathsForVisibleRows ?? []).map(\.row))
            let changedRows = rows.indices.compactMap { index -> IndexPath? in
                guard visibleRows.contains(index),
                      previousRows.indices.contains(index),
                      previousRows[index].signature != rows[index].signature
                else {
                    return nil
                }
                return IndexPath(row: index, section: 0)
            }
            guard !changedRows.isEmpty else {
                return
            }

            UIView.performWithoutAnimation {
                tableView.reloadRows(at: changedRows, with: .none)
                tableView.layoutIfNeeded()
            }
        }

        private func apply(_ transaction: TimelineScrollTransaction, tableView: UITableView) {
            switch transaction.intent {
            case .none:
                return
            case .alignBottom(let animated):
                scrollToBottom(tableView, animated: animated)
            case .preserveAnchor(let anchorID):
                olderLoadAnchorID = nil
                scrollToRow(withID: anchorID, tableView: tableView, position: .top, animated: false)
            case .resetToBottom:
                olderLoadAnchorID = nil
                scrollToBottom(tableView, animated: false)
            }
        }

        private func scrollToBottom(_ tableView: UITableView, animated: Bool) {
            guard !rows.isEmpty else {
                return
            }
            isProgrammaticScrollInFlight = true
            let indexPath = IndexPath(row: rows.count - 1, section: 0)
            tableView.scrollToRow(at: indexPath, at: .bottom, animated: animated)
            DispatchQueue.main.asyncAfter(deadline: .now() + (animated ? 0.22 : 0.08)) { [weak self] in
                self?.isProgrammaticScrollInFlight = false
            }
        }

        private func scrollToRow(
            withID id: TimelineEntry.ID,
            tableView: UITableView,
            position: UITableView.ScrollPosition,
            animated: Bool
        ) {
            guard let row = rows.firstIndex(where: { $0.id == id }) else {
                return
            }
            isProgrammaticScrollInFlight = true
            tableView.scrollToRow(at: IndexPath(row: row, section: 0), at: position, animated: animated)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.12) { [weak self] in
                self?.isProgrammaticScrollInFlight = false
            }
        }

        private func scheduleHeightInvalidation(for rowID: String, reloadRow: Bool = true, keepBottomAligned: Bool) {
            DispatchQueue.main.async { [weak self] in
                self?.invalidateHeight(for: rowID, reloadRow: reloadRow, keepBottomAligned: keepBottomAligned)
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.08) { [weak self] in
                self?.invalidateHeight(for: rowID, reloadRow: false, keepBottomAligned: keepBottomAligned)
            }
        }

        private func invalidateHeight(for rowID: String, reloadRow: Bool, keepBottomAligned: Bool) {
            guard let tableView,
                  let row = rows.firstIndex(where: { $0.id == rowID })
            else {
                return
            }

            let indexPath = IndexPath(row: row, section: 0)
            let anchor = keepBottomAligned && isNearBottom ? nil : currentTopAnchor(in: tableView)
            rowHeightCache = rowHeightCache.filter { $0.key.id != rowID }
            let wereAnimationsEnabled = UIView.areAnimationsEnabled
            UIView.setAnimationsEnabled(false)
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            UIView.performWithoutAnimation {
                if reloadRow {
                    tableView.reloadRows(at: [indexPath], with: .none)
                }
                tableView.beginUpdates()
                tableView.endUpdates()
                if let anchor {
                    restoreTopAnchor(anchor, in: tableView)
                }
            }
            CATransaction.commit()
            UIView.setAnimationsEnabled(wereAnimationsEnabled)
            if keepBottomAligned, isNearBottom {
                scrollToBottom(tableView, animated: false)
            }
        }

        private func invalidateVisibleHeights(_ tableView: UITableView, keepBottomAligned: Bool) {
            rowHeightCache.removeAll()
            UIView.performWithoutAnimation {
                tableView.beginUpdates()
                tableView.endUpdates()
            }
            if keepBottomAligned {
                scrollToBottom(tableView, animated: false)
            }
        }

        @ViewBuilder
        private func rootView(for row: IOSTimelineRow, width: CGFloat) -> some View {
            let rowID = row.id
            switch row {
            case .toolProcessHeader(let group):
                IOSToolProcessHeaderRow(
                    group: group,
                    isExpanded: expandedToolProcessIDs.contains(group.id),
                    toggleAction: { [weak self] isExpanded in
                        self?.setToolProcessGroup(group, expanded: isExpanded)
                    },
                    inspectMessageAction: inspectMessageAction,
                )
                .frame(width: width, alignment: .leading)
            case .toolProcessContent(let group):
                ToolProcessExpandedContentView(
                    group: group,
                    compact: true,
                    inspectToolAction: inspectToolAction
                )
                .padding(.horizontal, 12)
                .frame(width: width, alignment: .leading)
            default:
                row.view(
                    width: width,
                    loadOlderAction: loadOlderAction,
                    layoutChangeAction: { [weak self] in
                        self?.scheduleHeightInvalidation(for: rowID, keepBottomAligned: true)
                    },
                    inspectMessageAction: inspectMessageAction,
                    inspectToolAction: inspectToolAction
                )
                .frame(width: width, alignment: .leading)
            }
        }

        private func rowKeys(_ rows: [IOSTimelineRow]) -> [IOSTimelineRowKey] {
            rows.map { IOSTimelineRowKey(id: $0.id, signature: $0.signature) }
        }

        private func setToolProcessGroup(_ group: ToolProcessGroup, expanded: Bool) {
            guard let tableView else {
                if expanded {
                    expandedToolProcessIDs.insert(group.id)
                } else {
                    expandedToolProcessIDs.remove(group.id)
                }
                return
            }

            let headerID = IOSTimelineRow.toolHeaderID(group.id)
            let contentID = IOSTimelineRow.toolContentID(group.id)
            let wasNearBottom = isNearBottom
            let anchor = wasNearBottom ? nil : currentTopAnchor(in: tableView)

            if expanded {
                guard !expandedToolProcessIDs.contains(group.id),
                      let headerIndex = rows.firstIndex(where: { $0.id == headerID })
                else {
                    return
                }
                expandedToolProcessIDs.insert(group.id)
                let insertionIndex = headerIndex + 1
                rows.insert(.toolProcessContent(group), at: insertionIndex)
                rowHeightCache = rowHeightCache.filter { $0.key.id != contentID }
                tableView.performBatchUpdates {
                    tableView.insertRows(at: [IndexPath(row: insertionIndex, section: 0)], with: .fade)
                } completion: { [weak self] _ in
                    guard let self else { return }
                    if let anchor {
                        self.restoreTopAnchor(anchor, in: tableView)
                    } else if wasNearBottom {
                        self.scrollToBottom(tableView, animated: false)
                    }
                }
            } else {
                guard expandedToolProcessIDs.contains(group.id),
                      let contentIndex = rows.firstIndex(where: { $0.id == contentID })
                else {
                    return
                }
                expandedToolProcessIDs.remove(group.id)
                rows.remove(at: contentIndex)
                rowHeightCache = rowHeightCache.filter { $0.key.id != contentID }
                tableView.performBatchUpdates {
                    tableView.deleteRows(at: [IndexPath(row: contentIndex, section: 0)], with: .fade)
                } completion: { [weak self] _ in
                    guard let self else { return }
                    if let anchor {
                        self.restoreTopAnchor(anchor, in: tableView)
                    } else if wasNearBottom {
                        self.scrollToBottom(tableView, animated: false)
                    }
                }
            }
        }

        private func roundedWidth(_ width: CGFloat) -> Int {
            Int(width.rounded(.toNearestOrAwayFromZero))
        }

        private func currentTopAnchor(in tableView: UITableView) -> (row: Int, offset: CGFloat)? {
            guard let indexPath = tableView.indexPathsForVisibleRows?.min(),
                  rows.indices.contains(indexPath.row)
            else {
                return nil
            }
            let rect = tableView.rectForRow(at: indexPath)
            return (indexPath.row, tableView.contentOffset.y - rect.minY)
        }

        private func restoreTopAnchor(_ anchor: (row: Int, offset: CGFloat), in tableView: UITableView) {
            guard rows.indices.contains(anchor.row) else {
                return
            }
            let rect = tableView.rectForRow(at: IndexPath(row: anchor.row, section: 0))
            let minOffset = -tableView.adjustedContentInset.top
            let maxOffset = max(minOffset, tableView.contentSize.height - tableView.bounds.height + tableView.adjustedContentInset.bottom)
            let nextOffset = min(max(rect.minY + anchor.offset, minOffset), maxOffset)
            tableView.setContentOffset(CGPoint(x: 0, y: nextOffset), animated: false)
        }
    }
}

private struct IOSTimelineRowKey: Hashable {
    let id: String
    let signature: Int
}

private struct IOSTimelineHeightKey: Hashable {
    let id: String
    let signature: Int
    let width: Int

    init(row: IOSTimelineRow, width: Int) {
        id = row.id
        signature = row.signature
        self.width = width
    }
}

private enum IOSTimelineRow: Identifiable {
    case emptyLoading
    case older(isLoading: Bool)
    case entry(TimelineEntry)
    case toolProcessHeader(ToolProcessGroup)
    case toolProcessContent(ToolProcessGroup)
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
        case .toolProcessHeader(let group):
            Self.toolHeaderID(group.id)
        case .toolProcessContent(let group):
            Self.toolContentID(group.id)
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
        case .toolProcessHeader(let group):
            var hasher = Hasher()
            hasher.combine(group.id)
            hasher.combine(group.activities.count)
            hasher.combine(group.activities.first?.name)
            return hasher.finalize()
        case .toolProcessContent(let group):
            var hasher = Hasher()
            hasher.combine(group.id)
            for message in group.messages {
                hasher.combine(message.renderSignature)
            }
            for activity in group.activities {
                hasher.combine(activity.id)
                hasher.combine(activity.kind.rawValue)
                hasher.combine(activity.name)
                hasher.combine(activity.summary)
                hasher.combine(activity.detail)
            }
            return hasher.finalize()
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

    static func toolHeaderID(_ id: String) -> String {
        "tool-header-\(id)"
    }

    static func toolContentID(_ id: String) -> String {
        "tool-content-\(id)"
    }

    static func rows(
        renderData: TimelineRenderData,
        hasOlderMessages: Bool,
        isLoadingMessages: Bool,
        isLoadingOlderMessages: Bool,
        activityStatus: String?,
        isConversationRunning: Bool,
        turnProgress: TurnProgressFeedback?,
        expandedToolProcessIDs: Set<String>,
        shouldDisplayBottomLoading: Bool
    ) -> [IOSTimelineRow] {
        if isLoadingMessages && renderData.entries.isEmpty {
            return [.emptyLoading]
        }

        var rows: [IOSTimelineRow] = []
        if hasOlderMessages {
            rows.append(.older(isLoading: isLoadingOlderMessages))
        }
        for entry in renderData.entries {
            if case .toolProcess(let group) = entry {
                rows.append(.toolProcessHeader(group))
                if expandedToolProcessIDs.contains(group.id) {
                    rows.append(.toolProcessContent(group))
                }
            } else {
                rows.append(.entry(entry))
            }
        }
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
                compact: true,
                layoutChangeAction: layoutChangeAction,
                inspectMessageAction: inspectMessageAction,
                inspectToolAction: inspectToolAction
            )
            .frame(width: width, alignment: .leading)
        case .toolProcessHeader, .toolProcessContent:
            EmptyView()
        case .activity(let status, let isRunning, let progress):
            ActivityStatusView(status: status, isRunning: isRunning, turnProgress: progress)
                .padding(.horizontal, 24)
                .padding(.vertical, 8)
                .frame(width: width, alignment: .leading)
        case .bottomLoading:
            IOSNativeBottomLoadingMessagesView()
                .padding(.horizontal, 24)
                .padding(.vertical, 10)
                .frame(width: width)
        }
    }

    func estimatedHeight(width: CGFloat) -> CGFloat {
        switch self {
        case .emptyLoading:
            return 280
        case .older:
            return 42
        case .entry(let entry):
            return entry.estimatedIOSHeight(width: width)
        case .toolProcessHeader:
            return 48
        case .toolProcessContent:
            return 220
        case .activity:
            return 76
        case .bottomLoading:
            return 48
        }
    }
}

private extension TimelineEntry {
    func estimatedIOSHeight(width: CGFloat) -> CGFloat {
        switch self {
        case .message(let message, let auxiliaryMessages):
            return message.fastIOSTimelineHeight(width: width, auxiliaryCount: auxiliaryMessages.count) ?? 160
        case .auxiliary:
            return 54
        case .toolProcess:
            return 44
        }
    }
}

private extension ChatMessage {
    func fastIOSTimelineHeight(width: CGFloat, auxiliaryCount: Int) -> CGFloat? {
        guard role == .user,
              attachments.isEmpty,
              toolActivities.isEmpty,
              !pending,
              error == nil
        else {
            return nil
        }

        let text = body.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, text.count < 700 else {
            return nil
        }

        let bubbleWidth = min(max(width - 24 - 36, 170), 520)
        let textWidth = max(80, bubbleWidth - 26)
        let font = UIFont.preferredFont(forTextStyle: .body)
        let paragraphStyle = NSMutableParagraphStyle()
        paragraphStyle.lineBreakMode = .byWordWrapping
        let rect = (text as NSString).boundingRect(
            with: CGSize(width: textWidth, height: .greatestFiniteMagnitude),
            options: [.usesLineFragmentOrigin, .usesFontLeading],
            attributes: [
                .font: font,
                .paragraphStyle: paragraphStyle
            ],
            context: nil
        )
        let roleHeight: CGFloat = 16
        let auxiliaryHeight = auxiliaryCount > 0 ? CGFloat(12 + min(auxiliaryCount, 4) * 3) : 0
        let bubbleHeight = ceil(rect.height) + 20
        let metadataHeight: CGFloat = 16
        return max(52, auxiliaryHeight + roleHeight + bubbleHeight + metadataHeight + 10)
    }
}

private struct IOSNativeBottomLoadingMessagesView: View {
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

private struct IOSToolProcessHeaderRow: View {
    let group: ToolProcessGroup
    let toggleAction: (Bool) -> Void
    let inspectMessageAction: ((ChatMessage) -> Void)?

    @State private var isExpanded: Bool

    init(
        group: ToolProcessGroup,
        isExpanded: Bool,
        toggleAction: @escaping (Bool) -> Void,
        inspectMessageAction: ((ChatMessage) -> Void)?
    ) {
        self.group = group
        self.toggleAction = toggleAction
        self.inspectMessageAction = inspectMessageAction
        _isExpanded = State(initialValue: isExpanded)
    }

    var body: some View {
        Button {
            var transaction = Transaction()
            transaction.animation = nil
            withTransaction(transaction) {
                isExpanded.toggle()
            }
            toggleAction(isExpanded)
        } label: {
            HStack(spacing: 8) {
                Image(systemName: isExpanded ? "chevron.down" : "chevron.right")
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
            .padding(.horizontal, 12)
            .padding(.vertical, 5)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .transaction { transaction in
            transaction.animation = nil
        }
        .animation(nil, value: isExpanded)
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
}
#endif
