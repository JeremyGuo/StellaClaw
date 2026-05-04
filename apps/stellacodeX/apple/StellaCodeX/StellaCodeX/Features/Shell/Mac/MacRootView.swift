#if os(macOS)
import AppKit
import SwiftUI

struct MacRootView: View {
    private static let sidebarWidthKey = "stellacodex.mac.sidebar.width"
    private static let filesWidthKey = "stellacodex.mac.files.width"
    private static let terminalHeightKey = "stellacodex.mac.terminal.height"

    @ObservedObject var viewModel: AppViewModel
    @AppStorage(Self.sidebarWidthKey) private var persistedSidebarWidth = 300.0
    @AppStorage(Self.filesWidthKey) private var persistedFilesWidth = 680.0
    @AppStorage(Self.terminalHeightKey) private var persistedTerminalHeight = 360.0
    @State private var sidebarWidth = Self.storedDouble(forKey: Self.sidebarWidthKey, defaultValue: 300)
    @State private var filesWidth = Self.storedDouble(forKey: Self.filesWidthKey, defaultValue: 680)
    @State private var terminalHeight = Self.storedDouble(forKey: Self.terminalHeightKey, defaultValue: 360)
    @State private var isSidebarVisible = true
    @State private var isInspectorVisible = false
    @State private var isTerminalVisible = false
    @State private var isFilesVisible = false
    @State private var isActionsPresented = false
    @State private var isResizingLayout = false

    private let minSidebarWidth: Double = 220
    private let maxSidebarWidth: Double = 520
    private let minFilesWidth: Double = 420
    private let absoluteMinFilesWidth: Double = 300
    private let maxFilesWidth: Double = 940
    private let minChatWidth: Double = 560
    private let resizeHandleWidth: Double = 7
    private let inspectorWidth: CGFloat = 300

    var body: some View {
        GeometryReader { proxy in
            let metrics = layoutMetrics(for: proxy.size.width)

            HStack(spacing: 0) {
                if isSidebarVisible {
                    MacConversationListView(viewModel: viewModel)
                        .frame(width: CGFloat(metrics.sidebarWidth))
                        .layoutPriority(4)

                    MacSidebarResizeHandle(
                        width: $sidebarWidth,
                        isResizing: $isResizingLayout,
                        minWidth: minSidebarWidth,
                        maxWidth: maxSidebarWidth,
                        onResizeEnded: {
                            persistedSidebarWidth = sidebarWidth
                        }
                    )
                    .layoutPriority(4)
                }

                VStack(spacing: 0) {
                    MacChatWorkspaceView(viewModel: viewModel, isResizingLayout: isResizingLayout)
                        .frame(minWidth: minChatWidth, maxWidth: .infinity, maxHeight: .infinity)

                    if isTerminalVisible {
                        MacTerminalResizeHandle(
                            height: $terminalHeight,
                            isResizing: $isResizingLayout,
                            minHeight: 260,
                            maxHeight: 620,
                            onResizeEnded: {
                                persistedTerminalHeight = terminalHeight
                            }
                        )

                        MacTerminalPanelView(viewModel: viewModel)
                            .frame(height: CGFloat(terminalHeight))
                            .transition(.move(edge: .bottom).combined(with: .opacity))
                    }
                }
                .frame(minWidth: minChatWidth, maxWidth: .infinity, maxHeight: .infinity)
                .layoutPriority(3)

                if isFilesVisible {
                    MacTrailingSidebarResizeHandle(
                        width: $filesWidth,
                        isResizing: $isResizingLayout,
                        minWidth: metrics.filesResizeMinWidth,
                        maxWidth: metrics.filesResizeMaxWidth,
                        onResizeEnded: {
                            persistedFilesWidth = filesWidth
                        }
                    )
                    .layoutPriority(1)

                    MacWorkspaceFilesView(viewModel: viewModel)
                        .frame(width: CGFloat(metrics.filesWidth))
                        .clipped()
                        .layoutPriority(1)
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                }

                if isInspectorVisible {
                    Rectangle()
                        .fill(PlatformColor.separator)
                        .frame(width: 1)
                        .layoutPriority(2)

                    MacInspectorView(viewModel: viewModel)
                        .frame(width: inspectorWidth)
                        .layoutPriority(2)
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                }
            }
        }
        .frame(minWidth: 980, minHeight: 620)
        .background(PlatformColor.appBackground)
        .transaction { transaction in
            if isResizingLayout {
                transaction.disablesAnimations = true
                transaction.animation = nil
            }
        }
        .animation(nil, value: sidebarWidth)
        .animation(nil, value: filesWidth)
        .animation(nil, value: terminalHeight)
        .animation(.easeInOut(duration: 0.16), value: isFilesVisible)
        .animation(.easeInOut(duration: 0.16), value: isInspectorVisible)
        .toolbar {
            ToolbarItem(placement: .navigation) {
                Button {
                    var transaction = Transaction()
                    transaction.disablesAnimations = true
                    withTransaction(transaction) {
                        isSidebarVisible.toggle()
                    }
                } label: {
                    Image(systemName: "sidebar.left")
                }
                .help(isSidebarVisible ? "Hide Sidebar" : "Show Sidebar")
                .accessibilityLabel(isSidebarVisible ? "Hide Sidebar" : "Show Sidebar")
            }

            ToolbarItemGroup(placement: .primaryAction) {
                Button {
                    Task {
                        await viewModel.loadSelectedConversation()
                    }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .help("Refresh")

                Button {
                    withAnimation(.easeInOut(duration: 0.18)) {
                        isTerminalVisible.toggle()
                    }
                } label: {
                    Image(systemName: "terminal")
                }
                .help(isTerminalVisible ? "Hide Terminal" : "Show Terminal")
                .disabled(viewModel.selectedConversationID == nil)

                Button {
                    withAnimation(.easeInOut(duration: 0.16)) {
                        isFilesVisible.toggle()
                    }
                } label: {
                    Image(systemName: "folder")
                }
                .help(isFilesVisible ? "Hide Files" : "Show Files")
                .disabled(viewModel.selectedConversationID == nil)

                Button {
                    isActionsPresented = true
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .help("Conversation Actions")
                .disabled(viewModel.selectedConversationID == nil)

                Button {
                    withAnimation(.easeInOut(duration: 0.16)) {
                        isInspectorVisible.toggle()
                    }
                } label: {
                    Image(systemName: "sidebar.right")
                }
                .help(isInspectorVisible ? "Hide Overview" : "Show Overview")
            }
        }
        .sheet(isPresented: $isActionsPresented) {
            MacConversationActionsView(viewModel: viewModel)
                .frame(minWidth: 760, idealWidth: 880, minHeight: 560, idealHeight: 680)
        }
        .onChange(of: viewModel.selectedConversationID) { _, selectedConversationID in
            if selectedConversationID == nil {
                isFilesVisible = false
            }
        }
    }

    private static func storedDouble(forKey key: String, defaultValue: Double) -> Double {
        let value = UserDefaults.standard.double(forKey: key)
        return value > 0 ? value : defaultValue
    }

    private func layoutMetrics(for totalWidth: CGFloat) -> MacRootLayoutMetrics {
        let visibleSidebarWidth = isSidebarVisible ? min(max(sidebarWidth, minSidebarWidth), maxSidebarWidth) : 0
        let sidebarHandleWidth = isSidebarVisible ? resizeHandleWidth : 0
        let filesHandleWidth = isFilesVisible ? resizeHandleWidth : 0
        let visibleInspectorWidth = isInspectorVisible ? Double(inspectorWidth) + 1 : 0
        let reservedForPrimaryContent = visibleSidebarWidth
            + sidebarHandleWidth
            + minChatWidth
            + filesHandleWidth
            + visibleInspectorWidth
        let availableForFiles = max(0, Double(totalWidth) - reservedForPrimaryContent)
        let filesMaxWidth = min(maxFilesWidth, availableForFiles)
        let filesEffectiveWidth = isFilesVisible
            ? min(filesWidth, filesMaxWidth)
            : 0
        let filesResizeMinWidth = min(minFilesWidth, max(absoluteMinFilesWidth, filesMaxWidth))

        return MacRootLayoutMetrics(
            sidebarWidth: visibleSidebarWidth,
            filesWidth: filesEffectiveWidth,
            filesResizeMinWidth: min(filesResizeMinWidth, filesMaxWidth),
            filesResizeMaxWidth: max(0, filesMaxWidth)
        )
    }
}

private struct MacRootLayoutMetrics {
    var sidebarWidth: Double
    var filesWidth: Double
    var filesResizeMinWidth: Double
    var filesResizeMaxWidth: Double
}

private struct MacTrailingSidebarResizeHandle: View {
    @Binding var width: Double
    @Binding var isResizing: Bool
    let minWidth: Double
    let maxWidth: Double
    let onResizeEnded: () -> Void
    @State private var dragStartWidth: Double?

    var body: some View {
        ZStack {
            Rectangle()
                .fill(Color.clear)
                .frame(width: 7)

            Rectangle()
                .fill(PlatformColor.separator)
                .frame(width: 1)
        }
        .contentShape(Rectangle())
        .gesture(
            DragGesture(minimumDistance: 0, coordinateSpace: .global)
                .onChanged { value in
                    let start = dragStartWidth ?? width
                    dragStartWidth = start
                    isResizing = true
                    let proposed = min(max(start - value.translation.width, minWidth), maxWidth)
                    var transaction = Transaction()
                    transaction.disablesAnimations = true
                    withTransaction(transaction) {
                        if abs(width - proposed) >= 0.5 {
                            width = proposed
                        }
                    }
                }
                .onEnded { _ in
                    dragStartWidth = nil
                    isResizing = false
                    onResizeEnded()
                }
        )
        .onHover { isHovering in
            if isHovering {
                NSCursor.resizeLeftRight.push()
            } else {
                NSCursor.pop()
            }
        }
        .accessibilityLabel("Resize Files Sidebar")
    }
}

private struct MacTerminalResizeHandle: View {
    @Binding var height: Double
    @Binding var isResizing: Bool
    let minHeight: Double
    let maxHeight: Double
    let onResizeEnded: () -> Void
    @State private var dragStartHeight: Double?

    var body: some View {
        ZStack {
            Rectangle()
                .fill(PlatformColor.separator.opacity(0.55))
                .frame(height: 1)

            Rectangle()
                .fill(Color.clear)
                .frame(height: 7)
        }
        .contentShape(Rectangle())
        .gesture(
            DragGesture(minimumDistance: 0, coordinateSpace: .global)
                .onChanged { value in
                    let start = dragStartHeight ?? height
                    dragStartHeight = start
                    isResizing = true
                    let proposed = min(max(start - value.translation.height, minHeight), maxHeight)
                    var transaction = Transaction()
                    transaction.disablesAnimations = true
                    withTransaction(transaction) {
                        if abs(height - proposed) >= 0.5 {
                            height = proposed
                        }
                    }
                }
                .onEnded { _ in
                    dragStartHeight = nil
                    isResizing = false
                    onResizeEnded()
                }
        )
        .onHover { isHovering in
            if isHovering {
                NSCursor.resizeUpDown.push()
            } else {
                NSCursor.pop()
            }
        }
        .accessibilityLabel("Resize Terminal")
    }
}

private struct MacSidebarResizeHandle: View {
    @Binding var width: Double
    @Binding var isResizing: Bool
    let minWidth: Double
    let maxWidth: Double
    let onResizeEnded: () -> Void
    @State private var dragStartWidth: Double?

    var body: some View {
        ZStack {
            Rectangle()
                .fill(Color.clear)
                .frame(width: 7)

            Rectangle()
                .fill(PlatformColor.separator)
                .frame(width: 1)
        }
        .contentShape(Rectangle())
        .gesture(
            DragGesture(minimumDistance: 0, coordinateSpace: .global)
                .onChanged { value in
                    let start = dragStartWidth ?? width
                    dragStartWidth = start
                    isResizing = true
                    let proposed = min(max(start + value.translation.width, minWidth), maxWidth)
                    var transaction = Transaction()
                    transaction.disablesAnimations = true
                    withTransaction(transaction) {
                        if abs(width - proposed) >= 0.5 {
                            width = proposed
                        }
                    }
                }
                .onEnded { _ in
                    dragStartWidth = nil
                    isResizing = false
                    onResizeEnded()
                }
        )
        .onHover { isHovering in
            if isHovering {
                NSCursor.resizeLeftRight.push()
            } else {
                NSCursor.pop()
            }
        }
        .accessibilityLabel("Resize Sidebar")
    }
}

private struct MacInspectorView: View {
    @ObservedObject var viewModel: AppViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header

            ScrollView {
                if let conversation = viewModel.selectedConversation {
                    VStack(alignment: .leading, spacing: 16) {
                        overviewHero(conversation)
                        metrics
                        runtimeCard(conversation)
                        usageCard
                    }
                    .padding(18)
                } else {
                    VStack(spacing: 10) {
                        Image(systemName: "info.circle")
                            .font(.system(size: 28, weight: .medium))
                            .foregroundStyle(.tertiary)
                        Text("选择一个 Conversation 查看简介")
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity, minHeight: 220)
                    .padding(18)
                }
            }
        }
        .background(PlatformColor.inspectorBackground)
        .task(id: viewModel.selectedConversationID) {
            await viewModel.loadSelectedConversationStatus()
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Conversation 概览")
                .font(.headline)
            Text(viewModel.selectedConversation?.id ?? "未选择 Conversation")
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 18)
        .padding(.vertical, 14)
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(PlatformColor.separator.opacity(0.45))
                .frame(height: 1)
        }
    }

    private func overviewHero(_ conversation: ConversationSummary) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(conversation.id)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)

            Text(conversation.title)
                .font(.system(size: 20, weight: .semibold))
                .lineLimit(1)
                .truncationMode(.tail)

            HStack(spacing: 7) {
                Circle()
                    .fill(remoteIsActive ? Color.accentColor : Color.secondary.opacity(0.55))
                    .frame(width: 8, height: 8)

                Text(remoteIsActive ? remoteLabel : "local workspace")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(PlatformColor.secondaryBackground)
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }

    private var metrics: some View {
        HStack(spacing: 8) {
            metric(title: "Cache", value: "\(Int((usage.cacheHitRate * 100).rounded()))%")
            metric(title: "Tokens", value: compactNumber(usage.totalTokens))
            metric(title: "Cost", value: costString(usage.cost.total))
        }
    }

    private func metric(title: String, value: String) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(title)
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.system(size: 15, weight: .semibold))
                .lineLimit(1)
                .minimumScaleFactor(0.75)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 10)
        .padding(.vertical, 10)
        .background(PlatformColor.secondaryBackground)
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }

    private func runtimeCard(_ conversation: ConversationSummary) -> some View {
        overviewCard("运行状态") {
            VStack(spacing: 10) {
                keyValue("model", value: modelLabel(conversation))
                keyValue("sandbox", value: sandboxLabel(conversation))
                keyValue("background", value: "\(status?.runningBackground ?? 0) / \(status?.totalBackground ?? 0)")
                keyValue("subagents", value: "\(status?.runningSubagents ?? 0) / \(status?.totalSubagents ?? 0)")
            }
        }
    }

    private var usageCard: some View {
        overviewCard("Usage") {
            VStack(spacing: 12) {
                usageRow("Cache Read", value: usage.cacheRead)
                usageRow("Cache Write", value: usage.cacheWrite)
                usageRow("Input", value: usage.input)
                usageRow("Output", value: usage.output)
            }
        }
    }

    private func overviewCard<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title)
                .font(.subheadline.weight(.semibold))

            content()
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(PlatformColor.secondaryBackground)
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }

    private func keyValue(_ key: String, value: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(key)
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(width: 82, alignment: .leading)

            Spacer(minLength: 8)

            Text(value)
                .font(.caption)
                .foregroundStyle(.primary)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .trailing)
        }
    }

    private func usageRow(_ title: String, value: Int) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack {
                Text(title)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Text(compactNumber(value))
                    .font(.caption.weight(.semibold))
            }

            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule()
                        .fill(PlatformColor.separator.opacity(0.55))

                    Capsule()
                        .fill(Color.accentColor.opacity(0.7))
                        .frame(width: usageBarWidth(value: value, totalWidth: proxy.size.width))
                }
            }
            .frame(height: 5)
        }
    }

    private var status: ConversationStatusSnapshot? {
        guard viewModel.selectedConversationStatus?.conversationID == viewModel.selectedConversationID else {
            return nil
        }
        return viewModel.selectedConversationStatus
    }

    private var usage: ConversationUsageTotals {
        status?.usage.total ?? .empty
    }

    private var remoteLabel: String {
        let raw = viewModel.selectedConversation?.remote.nilIfEmpty ?? status?.remote ?? ""
        return raw
    }

    private var remoteIsActive: Bool {
        let normalized = remoteLabel.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        return !normalized.isEmpty && !["selectable", "disabled", "local", "none"].contains(normalized)
    }

    private func modelLabel(_ conversation: ConversationSummary) -> String {
        status?.model.nilIfEmpty ?? conversation.model.nilIfEmpty ?? "pending"
    }

    private func sandboxLabel(_ conversation: ConversationSummary) -> String {
        status?.sandbox.nilIfEmpty ?? conversation.sandbox.nilIfEmpty ?? "pending"
    }

    private func usageBarWidth(value: Int, totalWidth: CGFloat) -> CGFloat {
        guard usage.totalTokens > 0, totalWidth > 0 else {
            return 0
        }
        let ratio = CGFloat(value) / CGFloat(usage.totalTokens)
        return max(3, min(totalWidth, totalWidth * ratio))
    }

    private func compactNumber(_ value: Int) -> String {
        let absolute = abs(value)
        if absolute >= 1_000_000 {
            return String(format: "%.1fM", Double(value) / 1_000_000)
        }
        if absolute >= 1_000 {
            return String(format: "%.1fK", Double(value) / 1_000)
        }
        return "\(value)"
    }

    private func costString(_ value: Double) -> String {
        if value <= 0 {
            return "$0"
        }
        if value < 0.01 {
            return String(format: "$%.4f", value)
        }
        return String(format: "$%.2f", value)
    }
}

private extension String {
    var nilIfEmpty: String? {
        let trimmed = trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}

#Preview {
    MacRootView(viewModel: .mock())
}
#endif
