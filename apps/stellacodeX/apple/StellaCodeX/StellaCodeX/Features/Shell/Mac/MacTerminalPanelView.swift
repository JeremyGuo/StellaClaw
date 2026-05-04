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
                text: model.screenText,
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

    let text: String
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
        rendered.addAttributes(
            [.backgroundColor: NSColor.black],
            range: NSRange(location: 0, length: rendered.length)
        )
        applyDefaultTerminalAttributes(to: rendered)

        if textView.string != rendered.string || context.coordinator.lastDroppedBytes != droppedBytes {
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
        if let cached,
           cached.hasUsableContent,
           cached.nextOffset <= terminal.nextOffset {
            cachedRawOutput = cached.rawData
            updateRenderedOutput(fallback: cached.text)
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
            updateRenderedOutput(fallback: screenText)
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

    private func updateRenderedOutput(fallback: String) {
        let rendered = MacTerminalTextRenderer.render(cachedRawOutput, fallback: fallback)
        screenText = rendered
        screenAttributedText = MacTerminalTextRenderer.renderAttributed(cachedRawOutput, fallback: rendered)
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
