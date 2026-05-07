#if os(macOS)
import AppKit
import Combine
import SwiftUI

struct MacTerminalPanelView: View {
    @ObservedObject var viewModel: AppViewModel
    @StateObject private var model = MacTerminalSessionsModel()

    var body: some View {
        VStack(spacing: 0) {
            toolbar

            Divider()

            terminalBody
        }
        .background(Color.black)
        .task {
            await model.configure(viewModel: viewModel)
        }
        .onDisappear {
            model.close()
        }
        .alert("Terminal Error", isPresented: Binding(
            get: { model.errorMessage != nil },
            set: { if !$0 { model.errorMessage = nil } }
        )) {
            Button("OK", role: .cancel) {
                model.errorMessage = nil
            }
        } message: {
            Text(model.errorMessage ?? "")
        }
    }

    private var toolbar: some View {
        HStack(spacing: 12) {
            Menu {
                ForEach(model.terminals) { terminal in
                    Button {
                        Task {
                            await model.selectTerminal(terminal)
                        }
                    } label: {
                        Label(
                            displayName(for: terminal),
                            systemImage: terminal.id == model.activeTerminal?.id ? "checkmark.circle.fill" : "terminal"
                        )
                    }
                }
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: "terminal")
                    Text(model.activeTerminal.map(displayName(for:)) ?? "Terminal")
                        .lineLimit(1)
                    Image(systemName: "chevron.down")
                        .font(.caption.weight(.semibold))
                }
            }
            .menuStyle(.borderlessButton)
            .disabled(model.terminals.isEmpty)

            statusPill

            Spacer()

            Button {
                Task {
                    await model.refresh()
                }
            } label: {
                Image(systemName: "arrow.clockwise")
            }
            .help("Refresh Terminal Sessions")
            .disabled(model.isLoading)

            Button {
                Task {
                    await model.createTerminal()
                }
            } label: {
                Image(systemName: "plus")
            }
            .help("New Terminal")

            Button(role: .destructive) {
                Task {
                    await model.deleteActiveTerminal()
                }
            } label: {
                Image(systemName: "trash")
            }
            .help("Delete Terminal")
            .disabled(model.activeTerminal == nil)
        }
        .font(.system(size: 13))
        .padding(.horizontal, 14)
        .frame(height: 44)
        .background(PlatformColor.appBackground)
    }

    private var statusPill: some View {
        HStack(spacing: 6) {
            Circle()
                .fill(model.isConnected ? Color.green : Color.orange)
                .frame(width: 7, height: 7)
            Text(model.statusText.isEmpty ? "idle" : model.statusText)
        }
        .font(.caption.monospaced())
        .foregroundStyle(.secondary)
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(PlatformColor.controlBackground)
        .clipShape(Capsule())
    }

    @ViewBuilder
    private var terminalBody: some View {
        if model.isLoading && model.activeTerminal == nil {
            ProgressView("Opening terminal...")
                .foregroundStyle(.white)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if model.terminals.isEmpty {
            VStack(spacing: 14) {
                Image(systemName: "terminal")
                    .font(.system(size: 36, weight: .semibold))
                    .foregroundStyle(.white.opacity(0.72))
                Text("No Terminal Sessions")
                    .font(.headline)
                    .foregroundStyle(.white)
                Text("Create a session connected to this conversation workspace.")
                    .font(.subheadline)
                    .foregroundStyle(.white.opacity(0.62))
                Button("New Terminal") {
                    Task {
                        await model.createTerminal()
                    }
                }
                .buttonStyle(.borderedProminent)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color.black)
        } else {
            MacTerminalScreenView(
                attributedText: model.screenAttributedText,
                statusText: model.statusText,
                droppedBytes: model.droppedBytes,
                onInput: { input in
                    model.sendInput(input)
                },
                onViewportChange: { size in
                    model.updateViewport(size)
                }
            )
        }
    }

    private func displayName(for terminal: TerminalSummary) -> String {
        let shortID = terminal.terminalID.split(separator: "-").last.map(String.init) ?? terminal.terminalID
        let shellName = terminal.shell.split(separator: "/").last.map(String.init) ?? terminal.shell
        return "\(shellName) \(shortID.prefix(6))"
    }
}

private struct MacTerminalScreenView: View {
    private let terminalFontSize: CGFloat = 15
    private let terminalCellWidth: CGFloat = 9.0
    private let terminalLineHeight: CGFloat = 20
    private let terminalContentInset: CGFloat = 18

    let attributedText: NSAttributedString
    let statusText: String
    let droppedBytes: UInt64
    let onInput: (String) -> Void
    let onViewportChange: (MacTerminalViewportSize) -> Void

    var body: some View {
        MacTerminalNativeTextView(
            attributedText: attributedText,
            droppedBytes: droppedBytes,
            contentInset: terminalContentInset,
            onInput: onInput
        )
        .background(Color.black)
        .background {
            GeometryReader { geometry in
                Color.clear.preference(
                    key: MacTerminalViewportSizePreferenceKey.self,
                    value: viewportSize(for: geometry.size)
                )
            }
        }
        .onPreferenceChange(MacTerminalViewportSizePreferenceKey.self) { size in
            onViewportChange(size)
        }
    }

    private func viewportSize(for size: CGSize) -> MacTerminalViewportSize {
        let horizontalInsets = terminalContentInset * 2
        let verticalInsets = terminalContentInset * 2 + (droppedBytes > 0 ? 24 : 0)
        let cols = Int(((size.width - horizontalInsets) / terminalCellWidth).rounded(.down))
        let rows = Int(((size.height - verticalInsets) / terminalLineHeight).rounded(.down))
        return MacTerminalViewportSize(cols: min(max(cols, 24), 180), rows: min(max(rows, 8), 120))
    }
}

private struct MacTerminalNativeTextView: NSViewRepresentable {
    let attributedText: NSAttributedString
    let droppedBytes: UInt64
    let contentInset: CGFloat
    let onInput: (String) -> Void

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.drawsBackground = true
        scrollView.backgroundColor = .black
        scrollView.borderType = .noBorder
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = false
        scrollView.autohidesScrollers = true
        scrollView.contentView.postsBoundsChangedNotifications = true

        let textView = TerminalTextView()
        textView.onInput = onInput
        textView.isEditable = false
        textView.isSelectable = true
        textView.allowsUndo = false
        textView.drawsBackground = true
        textView.backgroundColor = .black
        textView.textColor = .white
        textView.insertionPointColor = .white
        textView.font = .monospacedSystemFont(ofSize: 15, weight: .regular)
        textView.textContainerInset = NSSize(width: contentInset, height: contentInset)
        textView.textContainer?.lineFragmentPadding = 0
        textView.minSize = NSSize(width: 0, height: 0)
        textView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width, .height]
        textView.textContainer?.containerSize = NSSize(
            width: scrollView.contentView.bounds.width,
            height: CGFloat.greatestFiniteMagnitude
        )
        textView.textContainer?.widthTracksTextView = true
        textView.frame = scrollView.contentView.bounds

        scrollView.documentView = textView
        context.coordinator.textView = textView
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        guard let textView = context.coordinator.textView else {
            return
        }

        textView.onInput = onInput
        let contentWidth = max(1, scrollView.contentView.bounds.width)
        textView.frame.size.width = contentWidth
        textView.textContainer?.containerSize = NSSize(
            width: contentWidth,
            height: CGFloat.greatestFiniteMagnitude
        )
        textView.textContainer?.widthTracksTextView = true

        let rendered = NSMutableAttributedString(attributedString: attributedText)
        if droppedBytes > 0 {
            let gap = NSAttributedString(
                string: "replay gap: dropped \(droppedBytes) bytes\n\n",
                attributes: [
                    .font: NSFont.monospacedSystemFont(ofSize: 12, weight: .regular),
                    .foregroundColor: NSColor.systemOrange
                ]
            )
            rendered.insert(gap, at: 0)
        }
        applyDefaultTerminalAttributes(to: rendered)

        if !textView.attributedString().isEqual(to: rendered) || context.coordinator.lastDroppedBytes != droppedBytes {
            textView.textStorage?.setAttributedString(rendered)
            context.coordinator.lastDroppedBytes = droppedBytes
            textView.scrollRangeToVisible(NSRange(location: max(0, rendered.length - 1), length: 1))
        }

        DispatchQueue.main.async {
            if let window = scrollView.window, window.firstResponder !== textView {
                window.makeFirstResponder(textView)
            }
        }
    }

    func makeCoordinator() -> Coordinator {
        Coordinator()
    }

    private func applyDefaultTerminalAttributes(to rendered: NSMutableAttributedString) {
        let fullRange = NSRange(location: 0, length: rendered.length)
        let defaultFont = NSFont.monospacedSystemFont(ofSize: 15, weight: .regular)

        rendered.enumerateAttributes(in: fullRange) { attributes, range, _ in
            var patch: [NSAttributedString.Key: Any] = [:]
            if attributes[.foregroundColor] == nil {
                patch[.foregroundColor] = NSColor.white
            }
            if attributes[.font] == nil {
                patch[.font] = defaultFont
            }
            if !patch.isEmpty {
                rendered.addAttributes(patch, range: range)
            }
        }
    }

    final class Coordinator {
        weak var textView: TerminalTextView?
        var lastDroppedBytes: UInt64 = 0
    }

    final class TerminalTextView: NSTextView {
        var onInput: ((String) -> Void)?

        override var acceptsFirstResponder: Bool {
            true
        }

        override func keyDown(with event: NSEvent) {
            if let input = specialTerminalInput(for: event) {
                onInput?(input)
                return
            }
            let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            if modifiers.contains(.command) {
                super.keyDown(with: event)
                return
            }
            interpretKeyEvents([event])
        }

        override func doCommand(by selector: Selector) {
            if let input = terminalInput(forCommand: selector) {
                onInput?(input)
                return
            }
            super.doCommand(by: selector)
        }

        override func insertText(_ insertString: Any, replacementRange: NSRange) {
            if let value = insertString as? String, !value.isEmpty {
                onInput?(value)
            } else if let value = insertString as? NSAttributedString, !value.string.isEmpty {
                onInput?(value.string)
            }
        }

        private func specialTerminalInput(for event: NSEvent) -> String? {
            let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            if modifiers.contains(.command) {
                return nil
            }

            if modifiers.contains(.control),
               let controlInput = controlInput(for: event) {
                return controlInput
            }

            return terminalInput(forKeyCode: event.keyCode)
        }

        private func terminalInput(forKeyCode keyCode: UInt16) -> String? {
            switch keyCode {
            case 36, 76: "\r"
            case 48: "\t"
            case 51: "\u{7f}"
            case 53: "\u{1B}"
            case 115: "\u{1B}[H"
            case 117: "\u{1B}[3~"
            case 119: "\u{1B}[F"
            case 123: "\u{1B}[D"
            case 124: "\u{1B}[C"
            case 125: "\u{1B}[B"
            case 126: "\u{1B}[A"
            default: nil
            }
        }

        private func terminalInput(forCommand selector: Selector) -> String? {
            switch selector {
            case #selector(insertNewline(_:)): "\r"
            case #selector(insertTab(_:)): "\t"
            case #selector(deleteBackward(_:)): "\u{7f}"
            case #selector(cancelOperation(_:)): "\u{1B}"
            case #selector(moveLeft(_:)): "\u{1B}[D"
            case #selector(moveRight(_:)): "\u{1B}[C"
            case #selector(moveDown(_:)): "\u{1B}[B"
            case #selector(moveUp(_:)): "\u{1B}[A"
            case #selector(moveToBeginningOfLine(_:)): "\u{1B}[H"
            case #selector(moveToEndOfLine(_:)): "\u{1B}[F"
            case #selector(deleteForward(_:)): "\u{1B}[3~"
            default: nil
            }
        }

        private func controlInput(for event: NSEvent) -> String? {
            guard let scalar = event.charactersIgnoringModifiers?.lowercased().unicodeScalars.first else {
                return nil
            }
            if scalar.value >= 97 && scalar.value <= 122 {
                return String(UnicodeScalar(scalar.value - 96)!)
            }
            switch scalar {
            case "[":
                return "\u{1B}"
            case "\\":
                return "\u{1C}"
            case "]":
                return "\u{1D}"
            case "^":
                return "\u{1E}"
            case "_":
                return "\u{1F}"
            default:
                return nil
            }
        }
    }
}

private struct MacTerminalViewportSize: Equatable, Codable {
    var cols: Int
    var rows: Int
}

private struct MacTerminalViewportSizePreferenceKey: PreferenceKey {
    static var defaultValue = MacTerminalViewportSize(cols: 100, rows: 32)

    static func reduce(value: inout MacTerminalViewportSize, nextValue: () -> MacTerminalViewportSize) {
        value = nextValue()
    }
}

@MainActor
private final class MacTerminalSessionsModel: ObservableObject {
    @Published var terminals: [TerminalSummary] = []
    @Published var activeTerminal: TerminalSummary?
    @Published var screenText = ""
    @Published var screenAttributedText = MacTerminalTextRenderer.attributedPlain(" ")
    @Published var statusText = ""
    @Published var isConnected = false
    @Published var isLoading = false
    @Published var droppedBytes: UInt64 = 0
    @Published var errorMessage: String?

    private weak var viewModel: AppViewModel?
    private var conversationID: ConversationSummary.ID?
    private var websocket: TerminalWebSocketSession?
    private var streamTask: Task<Void, Never>?
    private var screen = MacXtermScreenBuffer(cols: 100, rows: 32)
    private var viewportSize = MacTerminalViewportSize(cols: 100, rows: 32)
    private var nextOffset: UInt64 = 0
    private var cachedRawOutput = Data()

    func configure(viewModel: AppViewModel) async {
        guard self.viewModel !== viewModel || conversationID != viewModel.selectedConversationID else {
            return
        }
        self.viewModel = viewModel
        conversationID = viewModel.selectedConversationID
        await refresh(selectExisting: true)
    }

    func refresh(selectExisting: Bool = false) async {
        guard let viewModel else {
            return
        }
        isLoading = true
        defer {
            isLoading = false
        }

        do {
            let updated = try await viewModel.listTerminals()
            terminals = updated.sorted { $0.updatedMS > $1.updatedMS }

            if let current = activeTerminal,
               let replacement = terminals.first(where: { $0.id == current.id }) {
                activeTerminal = replacement
            } else if selectExisting || activeTerminal == nil {
                activeTerminal = terminals.first
            }

            if let activeTerminal {
                await selectTerminal(activeTerminal)
            } else {
                close()
                screen = MacXtermScreenBuffer(cols: viewportSize.cols, rows: viewportSize.rows)
                screenText = ""
                statusText = ""
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func createTerminal() async {
        guard let viewModel else {
            return
        }
        isLoading = true
        defer {
            isLoading = false
        }

        do {
            let terminal = try await viewModel.createTerminal(
                options: TerminalCreateOptions(shell: nil, cwd: nil, cols: viewportSize.cols, rows: viewportSize.rows)
            )
            terminals.insert(terminal, at: 0)
            await selectTerminal(terminal)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func deleteActiveTerminal() async {
        guard let viewModel,
              let activeTerminal
        else {
            return
        }

        do {
            _ = try await viewModel.terminateTerminal(id: activeTerminal.id)
            MacTerminalOutputCacheStore.remove(conversationID: activeTerminal.conversationID, terminalID: activeTerminal.id)
            terminals.removeAll { $0.id == activeTerminal.id }
            close()
            self.activeTerminal = terminals.first
            if let next = self.activeTerminal {
                await selectTerminal(next)
            } else {
                screen = MacXtermScreenBuffer(cols: viewportSize.cols, rows: viewportSize.rows)
                screenText = ""
                screenAttributedText = MacTerminalTextRenderer.attributedPlain(" ")
                statusText = ""
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func selectTerminal(_ terminal: TerminalSummary) async {
        guard let viewModel else {
            return
        }

        close()
        activeTerminal = terminal
        isConnected = false
        statusText = "connecting..."
        droppedBytes = 0

        let cached = MacTerminalOutputCacheStore.load(conversationID: terminal.conversationID, terminalID: terminal.id)
        screen = MacXtermScreenBuffer(cols: viewportSize.cols, rows: viewportSize.rows)
        if let cached,
           cached.hasUsableContent,
           cached.nextOffset <= terminal.nextOffset {
            cachedRawOutput = cached.rawData
            if cachedRawOutput.isEmpty {
                screen.replace(with: cached.text)
            } else {
                screen.feed(cachedRawOutput)
            }
            updatePublishedScreen()
            nextOffset = cached.nextOffset
        } else {
            if cached != nil {
                MacTerminalOutputCacheStore.remove(conversationID: terminal.conversationID, terminalID: terminal.id)
            }
            cachedRawOutput = Data()
            screenText = ""
            screenAttributedText = MacTerminalTextRenderer.attributedPlain(" ")
            nextOffset = 0
        }

        do {
            let session = try await viewModel.openTerminalSession(id: terminal.id, offset: nextOffset)
            websocket = session
            streamTask = Task { [weak self, session] in
                do {
                    for try await event in session.events {
                        await MainActor.run {
                            self?.handle(event, terminal: terminal)
                        }
                    }
                } catch {
                    await MainActor.run {
                        self?.isConnected = false
                        self?.statusText = "disconnected"
                        self?.errorMessage = error.localizedDescription
                    }
                }
            }
            session.resize(cols: viewportSize.cols, rows: viewportSize.rows)
        } catch {
            isConnected = false
            statusText = "disconnected"
            errorMessage = error.localizedDescription
        }
    }

    func updateViewport(_ size: MacTerminalViewportSize) {
        guard viewportSize != size else {
            return
        }
        viewportSize = size
        websocket?.resize(cols: size.cols, rows: size.rows)

        guard activeTerminal != nil else {
            return
        }

        if cachedRawOutput.isEmpty {
            screen.resize(cols: size.cols, rows: size.rows)
        } else {
            screen = MacXtermScreenBuffer(cols: size.cols, rows: size.rows)
            screen.feed(cachedRawOutput)
        }
        updatePublishedScreen()
    }

    func sendInput(_ input: String) {
        guard isConnected, !input.isEmpty else {
            return
        }
        websocket?.sendInput(input)
    }

    func close() {
        streamTask?.cancel()
        streamTask = nil
        websocket?.close()
        websocket = nil
        isConnected = false
    }

    private func handle(_ event: TerminalStreamEvent, terminal: TerminalSummary) {
        switch event {
        case let .attached(offset, running):
            nextOffset = max(nextOffset, offset)
            isConnected = running
            statusText = running ? "connected" : "exited"
        case let .output(data):
            nextOffset += UInt64(data.count)
            cachedRawOutput.append(data)
            if cachedRawOutput.count > 500_000 {
                cachedRawOutput.removeFirst(cachedRawOutput.count - 500_000)
            }
            screen.feed(data)
            updatePublishedScreen()
            MacTerminalOutputCacheStore.save(
                MacTerminalCacheSnapshot(text: screenText, nextOffset: nextOffset, rawData: cachedRawOutput),
                conversationID: terminal.conversationID,
                terminalID: terminal.id
            )
        case let .dropped(bytes):
            droppedBytes += bytes
            statusText = "replay gap"
        case .exit:
            isConnected = false
            statusText = "exited"
        case let .detached(reason):
            isConnected = false
            statusText = reason
        case let .error(message):
            isConnected = false
            statusText = "error"
            errorMessage = message
        }
    }

    private func updatePublishedScreen() {
        screenText = screen.renderedText
        screenAttributedText = screen.renderedAttributedText
    }
}

private struct MacTerminalCacheSnapshot: Codable {
    var text: String
    var nextOffset: UInt64
    var rawBase64: String?

    var hasUsableContent: Bool {
        !rawData.isEmpty || !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var rawData: Data {
        guard let rawBase64,
              let data = Data(base64Encoded: rawBase64)
        else {
            return Data()
        }
        return data
    }

    init(text: String, nextOffset: UInt64, rawData: Data) {
        self.text = text
        self.nextOffset = nextOffset
        self.rawBase64 = rawData.base64EncodedString()
    }
}

private enum MacTerminalOutputCacheStore {
    static func load(conversationID: String, terminalID: String) -> MacTerminalCacheSnapshot? {
        guard let data = try? Data(contentsOf: cacheURL(conversationID: conversationID, terminalID: terminalID)) else {
            return nil
        }
        return try? JSONDecoder().decode(MacTerminalCacheSnapshot.self, from: data)
    }

    static func save(_ snapshot: MacTerminalCacheSnapshot, conversationID: String, terminalID: String) {
        do {
            let raw = snapshot.rawData.suffix(500_000)
            let payload = MacTerminalCacheSnapshot(text: snapshot.text, nextOffset: snapshot.nextOffset, rawData: raw)
            let data = try JSONEncoder().encode(payload)
            let url = cacheURL(conversationID: conversationID, terminalID: terminalID)
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try data.write(to: url, options: [.atomic])
        } catch {
            // Terminal cache is a performance hint; connection state remains authoritative.
        }
    }

    static func remove(conversationID: String, terminalID: String) {
        try? FileManager.default.removeItem(at: cacheURL(conversationID: conversationID, terminalID: terminalID))
    }

    private static func cacheURL(conversationID: String, terminalID: String) -> URL {
        baseURL
            .appendingPathComponent(safePathComponent(conversationID), isDirectory: true)
            .appendingPathComponent("\(safePathComponent(terminalID)).json")
    }

    private static var baseURL: URL {
        FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("StellaCodeXMacTerminalCache", isDirectory: true)
    }

    private static func safePathComponent(_ value: String) -> String {
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-_"))
        let scalars = value.unicodeScalars.map { scalar in
            allowed.contains(scalar) ? Character(scalar) : "_"
        }
        let result = String(scalars)
        return result.isEmpty ? "terminal" : result
    }
}

private final class MacXtermScreenBuffer {
    private enum ParserState {
        case normal
        case escape
        case csi(String)
        case osc
        case oscEscape
    }

    private let scrollbackLimit = 2_000
    private var cols: Int
    private var rows: Int
    private var lines: [[MacTerminalCell]]
    private var alternateLines: [[MacTerminalCell]]?
    private var primaryLines: [[MacTerminalCell]]?
    private var cursorRow = 0
    private var cursorCol = 0
    private var savedCursorRow = 0
    private var savedCursorCol = 0
    private var parserState: ParserState = .normal
    private var currentStyle = MacTerminalStyle()

    init(cols: Int, rows: Int) {
        self.cols = max(cols, 20)
        self.rows = max(rows, 8)
        self.lines = [Array(repeating: MacTerminalCell.blank, count: max(cols, 20))]
    }

    var renderedText: String {
        lines
            .map { String($0.map(\.character)).trimmingCharacters(in: .whitespaces) }
            .joined(separator: "\n")
    }

    var renderedAttributedText: NSAttributedString {
        let output = NSMutableAttributedString()
        for (lineIndex, line) in lines.enumerated() {
            let cells = trimmedCells(line)
            if cells.isEmpty {
                if lineIndex < lines.count - 1 {
                    output.append(MacTerminalTextRenderer.attributedPlain("\n"))
                }
                continue
            }

            var runStart = 0
            while runStart < cells.count {
                let style = cells[runStart].style
                var runEnd = runStart + 1
                while runEnd < cells.count, cells[runEnd].style == style {
                    runEnd += 1
                }

                output.append(NSAttributedString(
                    string: String(cells[runStart..<runEnd].map(\.character)),
                    attributes: style.attributes
                ))
                runStart = runEnd
            }

            if lineIndex < lines.count - 1 {
                output.append(MacTerminalTextRenderer.attributedPlain("\n"))
            }
        }

        return output.length == 0 ? MacTerminalTextRenderer.attributedPlain(" ") : output
    }

    func replace(with text: String) {
        let split = text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        lines = split.isEmpty ? [blankLine()] : split.map { paddedLine($0) }
        cursorRow = max(lines.count - 1, 0)
        cursorCol = lines.last.map { min(visibleLength($0), cols - 1) } ?? 0
        trimScrollback()
    }

    func resize(cols: Int, rows: Int) {
        let nextCols = max(cols, 20)
        let nextRows = max(rows, 8)
        guard nextCols != self.cols || nextRows != self.rows else {
            return
        }

        self.cols = nextCols
        self.rows = nextRows
        lines = lines.map { resizeLine($0, cols: nextCols) }
        primaryLines = primaryLines?.map { resizeLine($0, cols: nextCols) }
        alternateLines = alternateLines?.map { resizeLine($0, cols: nextCols) }
        if lines.isEmpty {
            lines = [blankLine()]
        }
        cursorRow = min(cursorRow, max(lines.count - 1, 0))
        cursorCol = min(cursorCol, nextCols - 1)
        trimScrollback()
    }

    func feed(_ data: Data) {
        let string = String(decoding: data, as: UTF8.self)
        for scalar in string.unicodeScalars {
            feed(scalar)
        }
    }

    private func feed(_ scalar: UnicodeScalar) {
        switch parserState {
        case .normal:
            handleNormal(scalar)
        case .escape:
            handleEscape(scalar)
        case let .csi(buffer):
            let value = scalar.value
            if (0x40...0x7E).contains(value) {
                handleCSI(buffer, final: Character(scalar))
                parserState = .normal
            } else {
                parserState = .csi(buffer + String(scalar))
            }
        case .osc:
            if scalar == "\u{07}" {
                parserState = .normal
            } else if scalar == "\u{1B}" {
                parserState = .oscEscape
            }
        case .oscEscape:
            parserState = .normal
        }
    }

    private func handleNormal(_ scalar: UnicodeScalar) {
        switch scalar {
        case "\u{1B}":
            parserState = .escape
        case "\r":
            cursorCol = 0
        case "\n":
            newLine()
        case "\u{08}":
            cursorCol = max(cursorCol - 1, 0)
        case "\t":
            let spaces = max(1, 8 - (cursorCol % 8))
            for _ in 0..<spaces {
                write(" ")
            }
        default:
            guard !CharacterSet.controlCharacters.contains(scalar) else {
                return
            }
            write(Character(scalar))
        }
    }

    private func handleEscape(_ scalar: UnicodeScalar) {
        switch scalar {
        case "[":
            parserState = .csi("")
        case "]":
            parserState = .osc
        case "7":
            savedCursorRow = cursorRow
            savedCursorCol = cursorCol
            parserState = .normal
        case "8":
            restoreCursor()
            parserState = .normal
        case "D":
            newLine()
            parserState = .normal
        case "E":
            cursorCol = 0
            newLine()
            parserState = .normal
        case "M":
            reverseIndex()
            parserState = .normal
        case "c":
            reset()
            parserState = .normal
        default:
            parserState = .normal
        }
    }

    private func handleCSI(_ rawParameters: String, final: Character) {
        let parameters = csiParameters(rawParameters)
        let first = parameters.first ?? 0

        switch final {
        case "m":
            applySGR(rawParameters)
        case "A":
            cursorRow = max(cursorRow - max(first, 1), 0)
        case "B":
            cursorRow = min(cursorRow + max(first, 1), max(lines.count - 1, 0))
        case "C":
            cursorCol = min(cursorCol + max(first, 1), cols - 1)
        case "D":
            cursorCol = max(cursorCol - max(first, 1), 0)
        case "E":
            cursorRow = min(cursorRow + max(first, 1), max(lines.count - 1, 0))
            cursorCol = 0
        case "F":
            cursorRow = max(cursorRow - max(first, 1), 0)
            cursorCol = 0
        case "G":
            cursorCol = min(max(first, 1) - 1, cols - 1)
        case "d":
            cursorRow = max(first - 1, 0)
            ensureCursorLine()
        case "H", "f":
            cursorRow = max((parameters.first ?? 1) - 1, 0)
            cursorCol = min(max((parameters.dropFirst().first ?? 1) - 1, 0), cols - 1)
            ensureCursorLine()
        case "J":
            if first == 0 {
                clearDisplayFromCursor()
            } else if first == 1 {
                clearDisplayToCursor()
            } else if first == 2 || first == 3 {
                lines = [blankLine()]
                cursorRow = 0
                cursorCol = 0
            }
        case "K":
            if first == 1 {
                clearLineToCursor()
            } else if first == 2 {
                lines[cursorRow] = blankLine()
            } else {
                clearLineFromCursor()
            }
        case "P":
            deleteCharacters(max(first, 1))
        case "@":
            insertCharacters(max(first, 1))
        case "X":
            eraseCharacters(max(first, 1))
        case "L":
            insertLines(max(first, 1))
        case "M":
            deleteLines(max(first, 1))
        case "S":
            scrollUp(max(first, 1))
        case "T":
            scrollDown(max(first, 1))
        case "s":
            savedCursorRow = cursorRow
            savedCursorCol = cursorCol
        case "u":
            restoreCursor()
        case "h", "l":
            handleMode(rawParameters, enabled: final == "h")
        default:
            break
        }
    }

    private func handleMode(_ rawParameters: String, enabled: Bool) {
        let values = Set(csiParameters(rawParameters))
        guard values.contains(1049) || values.contains(47) || values.contains(1047) else {
            return
        }
        if enabled {
            guard primaryLines == nil else {
                return
            }
            primaryLines = lines
            savedCursorRow = cursorRow
            savedCursorCol = cursorCol
            alternateLines = Array(repeating: blankLine(), count: rows)
            lines = alternateLines ?? [blankLine()]
            cursorRow = 0
            cursorCol = 0
        } else if let primaryLines {
            alternateLines = lines
            lines = primaryLines
            self.primaryLines = nil
            restoreCursor()
            ensureCursorLine()
        }
    }

    private func reset() {
        lines = [blankLine()]
        primaryLines = nil
        alternateLines = nil
        cursorRow = 0
        cursorCol = 0
        savedCursorRow = 0
        savedCursorCol = 0
        parserState = .normal
        currentStyle = MacTerminalStyle()
    }

    private func write(_ character: Character) {
        ensureCursorLine()
        lines[cursorRow][cursorCol] = MacTerminalCell(character: character, style: currentStyle)
        cursorCol += 1
        if cursorCol >= cols {
            cursorCol = 0
            newLine()
        }
    }

    private func newLine() {
        cursorRow += 1
        cursorCol = 0
        ensureCursorLine()
        trimScrollback()
    }

    private func reverseIndex() {
        if cursorRow > 0 {
            cursorRow -= 1
        } else {
            lines.insert(blankLine(), at: 0)
            trimScrollback()
        }
    }

    private func restoreCursor() {
        cursorRow = min(max(savedCursorRow, 0), max(lines.count - 1, 0))
        cursorCol = min(max(savedCursorCol, 0), cols - 1)
    }

    private func ensureCursorLine() {
        while cursorRow >= lines.count {
            lines.append(blankLine())
        }
    }

    private func blankLine() -> [MacTerminalCell] {
        Array(repeating: MacTerminalCell(character: " ", style: currentStyle.backgroundOnly), count: cols)
    }

    private func paddedLine(_ text: String) -> [MacTerminalCell] {
        var cells = text.prefix(cols).map { MacTerminalCell(character: $0, style: MacTerminalStyle()) }
        if cells.count < cols {
            cells.append(contentsOf: Array(repeating: MacTerminalCell.blank, count: cols - cells.count))
        }
        return cells
    }

    private func resizeLine(_ line: [MacTerminalCell], cols: Int) -> [MacTerminalCell] {
        var resized = Array(line.prefix(cols))
        if resized.count < cols {
            resized.append(contentsOf: Array(repeating: MacTerminalCell.blank, count: cols - resized.count))
        }
        return resized
    }

    private func visibleLength(_ line: [MacTerminalCell]) -> Int {
        String(line.map(\.character)).trimmingCharacters(in: .whitespaces).count
    }

    private func trimScrollback() {
        let limit = max(scrollbackLimit, rows)
        if lines.count > limit {
            let overflow = lines.count - limit
            lines.removeFirst(overflow)
            cursorRow = max(cursorRow - overflow, 0)
        }
    }

    private func clearDisplayFromCursor() {
        ensureCursorLine()
        clearLineFromCursor()
        if cursorRow + 1 < lines.count {
            for row in (cursorRow + 1)..<lines.count {
                lines[row] = blankLine()
            }
        }
    }

    private func clearDisplayToCursor() {
        ensureCursorLine()
        if cursorRow > 0 {
            for row in 0..<cursorRow {
                lines[row] = blankLine()
            }
        }
        clearLineToCursor()
    }

    private func clearLineFromCursor() {
        ensureCursorLine()
        guard cursorCol < cols else {
            return
        }
        for index in cursorCol..<cols {
            lines[cursorRow][index] = MacTerminalCell(character: " ", style: currentStyle.backgroundOnly)
        }
    }

    private func clearLineToCursor() {
        ensureCursorLine()
        for index in 0...min(cursorCol, cols - 1) {
            lines[cursorRow][index] = MacTerminalCell(character: " ", style: currentStyle.backgroundOnly)
        }
    }

    private func deleteCharacters(_ count: Int) {
        ensureCursorLine()
        let end = min(cols, cursorCol + count)
        lines[cursorRow].removeSubrange(cursorCol..<end)
        lines[cursorRow].append(contentsOf: Array(repeating: MacTerminalCell.blank, count: end - cursorCol))
    }

    private func insertCharacters(_ count: Int) {
        ensureCursorLine()
        let insertCount = min(max(count, 0), cols - cursorCol)
        lines[cursorRow].insert(contentsOf: Array(repeating: MacTerminalCell.blank, count: insertCount), at: cursorCol)
        lines[cursorRow] = Array(lines[cursorRow].prefix(cols))
    }

    private func eraseCharacters(_ count: Int) {
        ensureCursorLine()
        let end = min(cols, cursorCol + count)
        for index in cursorCol..<end {
            lines[cursorRow][index] = MacTerminalCell(character: " ", style: currentStyle.backgroundOnly)
        }
    }

    private func insertLines(_ count: Int) {
        ensureCursorLine()
        let insertCount = min(max(count, 0), rows)
        for _ in 0..<insertCount {
            lines.insert(blankLine(), at: cursorRow)
        }
        trimScrollback()
    }

    private func deleteLines(_ count: Int) {
        ensureCursorLine()
        let deleteCount = min(max(count, 0), max(lines.count - cursorRow, 0))
        if deleteCount > 0 {
            lines.removeSubrange(cursorRow..<(cursorRow + deleteCount))
        }
        if lines.isEmpty {
            lines = [blankLine()]
        }
        ensureCursorLine()
    }

    private func scrollUp(_ count: Int) {
        for _ in 0..<min(max(count, 0), max(lines.count, 1)) {
            if !lines.isEmpty {
                lines.removeFirst()
            }
            lines.append(blankLine())
        }
        cursorRow = min(cursorRow, max(lines.count - 1, 0))
    }

    private func scrollDown(_ count: Int) {
        for _ in 0..<min(max(count, 0), rows) {
            lines.insert(blankLine(), at: 0)
        }
        trimScrollback()
        cursorRow = min(cursorRow + count, max(lines.count - 1, 0))
    }

    private func trimmedCells(_ line: [MacTerminalCell]) -> [MacTerminalCell] {
        var end = line.count
        while end > 0, line[end - 1].character == " " {
            end -= 1
        }
        return Array(line.prefix(end))
    }

    private func csiParameters(_ raw: String) -> [Int] {
        raw.split(separator: ";", omittingEmptySubsequences: false).map { part in
            let digits = part.filter { $0.isNumber || $0 == "-" }
            return Int(digits) ?? 0
        }
    }

    private func applySGR(_ raw: String) {
        var parameters = raw
            .replacingOccurrences(of: ":", with: ";")
            .split(separator: ";", omittingEmptySubsequences: false)
            .map { Int($0) ?? 0 }
        if parameters.isEmpty {
            parameters = [0]
        }

        var index = 0
        while index < parameters.count {
            let value = parameters[index]
            switch value {
            case 0:
                currentStyle = MacTerminalStyle()
            case 1:
                currentStyle.bold = true
            case 2:
                currentStyle.dim = true
            case 22:
                currentStyle.bold = false
                currentStyle.dim = false
            case 3:
                currentStyle.italic = true
            case 23:
                currentStyle.italic = false
            case 4:
                currentStyle.underline = true
            case 24:
                currentStyle.underline = false
            case 7:
                currentStyle.inverse = true
            case 27:
                currentStyle.inverse = false
            case 30...37:
                currentStyle.foreground = MacTerminalColor.palette(value - 30)
            case 39:
                currentStyle.foreground = nil
            case 40...47:
                currentStyle.background = MacTerminalColor.palette(value - 40)
            case 49:
                currentStyle.background = nil
            case 90...97:
                currentStyle.foreground = MacTerminalColor.palette(value - 90 + 8)
            case 100...107:
                currentStyle.background = MacTerminalColor.palette(value - 100 + 8)
            case 38, 48:
                let isForeground = value == 38
                if index + 2 < parameters.count, parameters[index + 1] == 5 {
                    setColor(MacTerminalColor.ansi256(parameters[index + 2]), foreground: isForeground)
                    index += 2
                } else if index + 4 < parameters.count, parameters[index + 1] == 2 {
                    setColor(
                        MacTerminalColor.rgb(
                            red: parameters[index + 2],
                            green: parameters[index + 3],
                            blue: parameters[index + 4]
                        ),
                        foreground: isForeground
                    )
                    index += 4
                }
            default:
                break
            }
            index += 1
        }
    }

    private func setColor(_ color: MacTerminalColor, foreground: Bool) {
        if foreground {
            currentStyle.foreground = color
        } else {
            currentStyle.background = color
        }
    }
}

private struct MacTerminalCell: Equatable {
    var character: Character
    var style: MacTerminalStyle

    static let blank = MacTerminalCell(character: " ", style: MacTerminalStyle())
}

private struct MacTerminalStyle: Equatable {
    var foreground: MacTerminalColor?
    var background: MacTerminalColor?
    var bold = false
    var dim = false
    var italic = false
    var underline = false
    var inverse = false

    var backgroundOnly: MacTerminalStyle {
        MacTerminalStyle(foreground: nil, background: background)
    }

    var attributes: [NSAttributedString.Key: Any] {
        var attributes: [NSAttributedString.Key: Any] = [
            .font: NSFont.monospacedSystemFont(ofSize: 15, weight: bold ? .bold : .regular),
            .foregroundColor: foregroundColor,
            .backgroundColor: backgroundColor
        ]
        if underline {
            attributes[.underlineStyle] = NSUnderlineStyle.single.rawValue
        }
        if italic {
            attributes[.obliqueness] = 0.18
        }
        return attributes
    }

    private var foregroundColor: NSColor {
        if inverse {
            return background?.nsColor ?? .black
        }
        let color = foreground?.nsColor ?? (bold ? .white : NSColor(calibratedWhite: 0.92, alpha: 1))
        return dim ? color.withAlphaComponent(0.68) : color
    }

    private var backgroundColor: NSColor {
        if inverse {
            return foreground?.nsColor ?? NSColor(calibratedWhite: 0.92, alpha: 1)
        }
        return background?.nsColor ?? .black
    }
}

private struct MacTerminalColor: Equatable {
    var red: Double
    var green: Double
    var blue: Double

    var nsColor: NSColor {
        NSColor(calibratedRed: red, green: green, blue: blue, alpha: 1)
    }

    static func palette(_ index: Int) -> MacTerminalColor {
        let palette: [MacTerminalColor] = [
            rgb(red: 0, green: 0, blue: 0),
            rgb(red: 205, green: 49, blue: 49),
            rgb(red: 13, green: 188, blue: 121),
            rgb(red: 229, green: 229, blue: 16),
            rgb(red: 36, green: 114, blue: 200),
            rgb(red: 188, green: 63, blue: 188),
            rgb(red: 17, green: 168, blue: 205),
            rgb(red: 229, green: 229, blue: 229),
            rgb(red: 102, green: 102, blue: 102),
            rgb(red: 241, green: 76, blue: 76),
            rgb(red: 35, green: 209, blue: 139),
            rgb(red: 245, green: 245, blue: 67),
            rgb(red: 59, green: 142, blue: 234),
            rgb(red: 214, green: 112, blue: 214),
            rgb(red: 41, green: 184, blue: 219),
            rgb(red: 255, green: 255, blue: 255)
        ]
        return palette[max(0, min(index, palette.count - 1))]
    }

    static func ansi256(_ index: Int) -> MacTerminalColor {
        let clamped = max(0, min(index, 255))
        if clamped < 16 {
            return palette(clamped)
        }
        if clamped < 232 {
            let value = clamped - 16
            let red = value / 36
            let green = (value % 36) / 6
            let blue = value % 6
            return rgb(red: cubeComponent(red), green: cubeComponent(green), blue: cubeComponent(blue))
        }
        let gray = 8 + (clamped - 232) * 10
        return rgb(red: gray, green: gray, blue: gray)
    }

    static func rgb(red: Int, green: Int, blue: Int) -> MacTerminalColor {
        MacTerminalColor(
            red: Double(max(0, min(red, 255))) / 255.0,
            green: Double(max(0, min(green, 255))) / 255.0,
            blue: Double(max(0, min(blue, 255))) / 255.0
        )
    }

    private static func cubeComponent(_ value: Int) -> Int {
        value == 0 ? 0 : 55 + value * 40
    }
}

private enum MacTerminalTextRenderer {
    static func render(_ data: Data, fallback: String) -> String {
        guard let raw = String(data: data, encoding: .utf8) else {
            return fallback
        }
        let cleaned = stripANSI(raw)
        if cleaned.count > 60_000 {
            return String(cleaned.suffix(60_000))
        }
        return cleaned
    }

    static func renderAttributed(_ data: Data, fallback: String) -> NSAttributedString {
        guard let raw = String(data: data, encoding: .utf8) else {
            return attributedPlain(fallback)
        }
        var renderer = ANSIAttributedRenderer(raw: raw)
        return renderer.render()
    }

    static func attributedPlain(_ value: String) -> NSAttributedString {
        NSAttributedString(
            string: value.isEmpty ? " " : value,
            attributes: [
                .font: NSFont.monospacedSystemFont(ofSize: 15, weight: .regular),
                .foregroundColor: NSColor.white,
                .backgroundColor: NSColor.black
            ]
        )
    }

    private static func stripANSI(_ raw: String) -> String {
        var result = ""
        var index = raw.startIndex

        while index < raw.endIndex {
            let character = raw[index]
            if character == "\u{001B}" {
                raw.formIndex(after: &index)
                guard index < raw.endIndex else {
                    break
                }

                if raw[index] == "[" {
                    raw.formIndex(after: &index)
                    while index < raw.endIndex {
                        let scalar = raw[index].unicodeScalars.first?.value ?? 0
                        raw.formIndex(after: &index)
                        if scalar >= 0x40 && scalar <= 0x7E {
                            break
                        }
                    }
                    continue
                }

                raw.formIndex(after: &index)
                continue
            }

            if character == "\r" {
                result.append("\n")
            } else {
                result.append(character)
            }
            raw.formIndex(after: &index)
        }

        return result
    }
}

private struct ANSIAttributedRenderer {
    private let raw: String
    private var index: String.Index
    private var output = NSMutableAttributedString()
    private var buffer = ""
    private var foreground: NSColor = .white
    private var background: NSColor = .black
    private var isBold = false

    init(raw: String) {
        if raw.count > 90_000 {
            self.raw = String(raw.suffix(90_000))
        } else {
            self.raw = raw
        }
        self.index = self.raw.startIndex
    }

    mutating func render() -> NSAttributedString {
        while index < raw.endIndex {
            let character = raw[index]
            if character == "\u{001B}" {
                flush()
                handleEscape()
                continue
            }
            if character == "\r" {
                buffer.append("\n")
            } else {
                buffer.append(character)
            }
            raw.formIndex(after: &index)
        }
        flush()
        if output.length == 0 {
            output = NSMutableAttributedString(attributedString: MacTerminalTextRenderer.attributedPlain(" "))
        }
        return output
    }

    private mutating func handleEscape() {
        raw.formIndex(after: &index)
        guard index < raw.endIndex else {
            return
        }

        guard raw[index] == "[" else {
            raw.formIndex(after: &index)
            return
        }

        raw.formIndex(after: &index)
        let parametersStart = index
        while index < raw.endIndex {
            let scalar = raw[index].unicodeScalars.first?.value ?? 0
            if scalar >= 0x40 && scalar <= 0x7E {
                let command = raw[index]
                let parameters = String(raw[parametersStart..<index])
                raw.formIndex(after: &index)
                if command == "m" {
                    applySGR(parameters)
                }
                return
            }
            raw.formIndex(after: &index)
        }
    }

    private mutating func applySGR(_ parameters: String) {
        let normalized = parameters.replacingOccurrences(of: ":", with: ";")
        let codes = normalized.isEmpty ? [0] : normalized
            .split(separator: ";", omittingEmptySubsequences: false)
            .map { Int($0) ?? 0 }
        var offset = 0
        while offset < codes.count {
            let code = codes[offset]
            switch code {
            case 0:
                foreground = .white
                background = .black
                isBold = false
            case 1:
                isBold = true
            case 22:
                isBold = false
            case 30...37, 90...97:
                foreground = ansiColor(code)
            case 40...47, 100...107:
                background = ansiColor(code - 10)
            case 39:
                foreground = .white
            case 49:
                background = .black
            case 38:
                if offset + 2 < codes.count, codes[offset + 1] == 5 {
                    foreground = ansi256Color(codes[offset + 2])
                    offset += 2
                } else if offset + 4 < codes.count, codes[offset + 1] == 2 {
                    let rgbStart = offset + (offset + 5 < codes.count && codes[offset + 2] == 0 ? 3 : 2)
                    if rgbStart + 2 < codes.count {
                        foreground = rgbColor(codes[rgbStart], codes[rgbStart + 1], codes[rgbStart + 2])
                        offset = rgbStart + 2
                    }
                }
            case 48:
                if offset + 2 < codes.count, codes[offset + 1] == 5 {
                    background = ansi256Color(codes[offset + 2])
                    offset += 2
                } else if offset + 4 < codes.count, codes[offset + 1] == 2 {
                    let rgbStart = offset + (offset + 5 < codes.count && codes[offset + 2] == 0 ? 3 : 2)
                    if rgbStart + 2 < codes.count {
                        background = rgbColor(codes[rgbStart], codes[rgbStart + 1], codes[rgbStart + 2])
                        offset = rgbStart + 2
                    }
                }
            default:
                break
            }
            offset += 1
        }
    }

    private mutating func flush() {
        guard !buffer.isEmpty else {
            return
        }
        output.append(NSAttributedString(
            string: buffer,
            attributes: [
                .font: NSFont.monospacedSystemFont(ofSize: 15, weight: isBold ? .bold : .regular),
                .foregroundColor: foreground,
                .backgroundColor: background
            ]
        ))
        buffer.removeAll(keepingCapacity: true)
    }

    private func ansiColor(_ code: Int) -> NSColor {
        switch code {
        case 30: NSColor(calibratedWhite: 0.08, alpha: 1)
        case 31: NSColor(calibratedRed: 0.85, green: 0.16, blue: 0.16, alpha: 1)
        case 32: NSColor(calibratedRed: 0.12, green: 0.74, blue: 0.22, alpha: 1)
        case 33: NSColor(calibratedRed: 0.86, green: 0.65, blue: 0.12, alpha: 1)
        case 34: NSColor(calibratedRed: 0.16, green: 0.42, blue: 0.92, alpha: 1)
        case 35: NSColor(calibratedRed: 0.72, green: 0.27, blue: 0.88, alpha: 1)
        case 36: NSColor(calibratedRed: 0.10, green: 0.72, blue: 0.86, alpha: 1)
        case 37: NSColor(calibratedWhite: 0.86, alpha: 1)
        case 90: NSColor(calibratedWhite: 0.46, alpha: 1)
        case 91: NSColor(calibratedRed: 1.0, green: 0.36, blue: 0.36, alpha: 1)
        case 92: NSColor(calibratedRed: 0.35, green: 0.95, blue: 0.48, alpha: 1)
        case 93: NSColor(calibratedRed: 1.0, green: 0.85, blue: 0.35, alpha: 1)
        case 94: NSColor(calibratedRed: 0.35, green: 0.62, blue: 1.0, alpha: 1)
        case 95: NSColor(calibratedRed: 0.86, green: 0.45, blue: 1.0, alpha: 1)
        case 96: NSColor(calibratedRed: 0.35, green: 0.9, blue: 1.0, alpha: 1)
        case 97: .white
        default: .white
        }
    }

    private func ansi256Color(_ value: Int) -> NSColor {
        if value < 16 {
            return ansiColor(value < 8 ? value + 30 : value + 82)
        }
        if value >= 232 {
            let channel = Double(8 + (value - 232) * 10) / 255.0
            return NSColor(calibratedRed: channel, green: channel, blue: channel, alpha: 1)
        }
        let color = max(16, min(231, value)) - 16
        let red = color / 36
        let green = (color % 36) / 6
        let blue = color % 6
        func component(_ index: Int) -> Double {
            index == 0 ? 0 : Double(55 + index * 40) / 255.0
        }
        return NSColor(calibratedRed: component(red), green: component(green), blue: component(blue), alpha: 1)
    }

    private func rgbColor(_ red: Int, _ green: Int, _ blue: Int) -> NSColor {
        NSColor(
            calibratedRed: Double(max(0, min(255, red))) / 255.0,
            green: Double(max(0, min(255, green))) / 255.0,
            blue: Double(max(0, min(255, blue))) / 255.0,
            alpha: 1
        )
    }
}

#Preview {
    MacTerminalPanelView(viewModel: .mock())
}
#endif
