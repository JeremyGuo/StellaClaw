import Foundation
import SwiftUI
#if os(macOS)
import AppKit
#endif

struct MarkdownContentView: View {
    let text: String
    var compact = false
    var fillsWidth = true
    @State private var isExpanded: Bool
    @State private var blocks: [MarkdownBlock]

    init(text: String, compact: Bool = false, fillsWidth: Bool = true) {
        self.text = text
        self.compact = compact
        self.fillsWidth = fillsWidth
        self._isExpanded = State(initialValue: text.count <= Self.collapseThreshold(compact: compact))
        self._blocks = State(initialValue: MarkdownBlock.cachedParse(Self.renderText(text, compact: compact, isExpanded: text.count <= Self.collapseThreshold(compact: compact))))
    }

    var body: some View {
        content
            .onChange(of: text) {
                let expanded = text.count <= Self.collapseThreshold(compact: compact)
                isExpanded = expanded
                blocks = MarkdownBlock.cachedParse(Self.renderText(text, compact: compact, isExpanded: expanded))
            }
            .onChange(of: isExpanded) {
                blocks = MarkdownBlock.cachedParse(Self.renderText(text, compact: compact, isExpanded: isExpanded))
            }
    }

    @ViewBuilder
    private var content: some View {
        VStack(alignment: .leading, spacing: compact ? 8 : 12) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                switch block {
                case .heading(let level, let value):
                    MarkdownHeadingView(level: level, text: value, compact: compact, fillsWidth: fillsWidth)
                case .paragraph(let value):
                    MarkdownParagraphView(text: value, compact: compact, fillsWidth: fillsWidth)
                case .quote(let value):
                    MarkdownQuoteView(text: value, compact: compact, fillsWidth: fillsWidth)
                case .unorderedList(let values):
                    MarkdownListView(items: values, ordered: false, compact: compact, fillsWidth: fillsWidth)
                case .orderedList(let values):
                    MarkdownListView(items: values, ordered: true, compact: compact, fillsWidth: fillsWidth)
                case .separator:
                    Divider()
                        .padding(.vertical, compact ? 2 : 4)
                case .code(let language, let code):
                    CodeBlockView(code: code, language: language, compact: compact)
                case .table(let table):
                    MarkdownTableView(table: table, compact: compact, fillsWidth: fillsWidth)
                }
            }

            if isCollapsible {
                Button {
                    withAnimation(.smooth(duration: 0.18)) {
                        isExpanded.toggle()
                    }
                } label: {
                    HStack(spacing: 7) {
                        Image(systemName: isExpanded ? "chevron.up" : "chevron.down")
                        Text(isExpanded ? "Collapse long message" : "Show full message")
                        Text("\(text.count) chars")
                            .foregroundStyle(.tertiary)
                    }
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 7)
                    .background(PlatformColor.secondaryBackground.opacity(0.58))
                    .clipShape(Capsule())
                }
                .buttonStyle(.plain)
            }
        }
        .textSelection(.enabled)
        .modifier(MarkdownWidthModifier(fillsWidth: fillsWidth))
    }

    private var isCollapsible: Bool {
        text.count > Self.collapseThreshold(compact: compact)
    }

    private static func collapseThreshold(compact: Bool) -> Int {
        compact ? 7_000 : 12_000
    }

    private static func renderText(_ text: String, compact: Bool, isExpanded: Bool) -> String {
        guard !isExpanded, text.count > collapseThreshold(compact: compact) else {
            return text
        }
        let threshold = collapseThreshold(compact: compact)
        let end = text.index(text.startIndex, offsetBy: threshold)
        return String(text[..<end]).trimmingCharacters(in: .whitespacesAndNewlines) + "\n\n..."
    }
}

private struct MarkdownWidthModifier: ViewModifier {
    let fillsWidth: Bool

    func body(content: Content) -> some View {
        if fillsWidth {
            content.frame(maxWidth: .infinity, alignment: .leading)
        } else {
            content.fixedSize(horizontal: false, vertical: true)
        }
    }
}

private enum MarkdownBlock: Hashable {
    case heading(level: Int, String)
    case paragraph(String)
    case quote(String)
    case unorderedList([String])
    case orderedList([String])
    case separator
    case code(language: String?, code: String)
    case table(MarkdownTable)

    private static let cacheLock = NSLock()
    private static var parseCache: [String: [MarkdownBlock]] = [:]

    static func cachedParse(_ text: String) -> [MarkdownBlock] {
        cacheLock.lock()
        if let cached = parseCache[text] {
            cacheLock.unlock()
            return cached
        }
        cacheLock.unlock()

        let blocks = parse(text)

        cacheLock.lock()
        if parseCache.count > 512 {
            parseCache.removeAll(keepingCapacity: true)
        }
        parseCache[text] = blocks
        cacheLock.unlock()
        return blocks
    }

    static func parse(_ text: String) -> [MarkdownBlock] {
        var blocks: [MarkdownBlock] = []
        var paragraphLines: [String] = []
        var quoteLines: [String] = []
        var unorderedItems: [String] = []
        var orderedItems: [String] = []
        var codeLines: [String] = []
        var codeLanguage: String?
        var inCode = false

        func flushListsAndQuote() {
            if !quoteLines.isEmpty {
                blocks.append(.quote(quoteLines.joined(separator: "\n")))
                quoteLines.removeAll()
            }
            if !unorderedItems.isEmpty {
                blocks.append(.unorderedList(unorderedItems))
                unorderedItems.removeAll()
            }
            if !orderedItems.isEmpty {
                blocks.append(.orderedList(orderedItems))
                orderedItems.removeAll()
            }
        }

        func flushParagraph() {
            let value = paragraphLines.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines)
            if !value.isEmpty {
                blocks.append(.paragraph(value))
            }
            paragraphLines.removeAll()
        }

        func flushCode() {
            blocks.append(.code(language: codeLanguage, code: codeLines.joined(separator: "\n")))
            codeLines.removeAll()
            codeLanguage = nil
        }

        let lines = text.components(separatedBy: .newlines)
        var index = 0
        while index < lines.count {
            let line = lines[index]
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.hasPrefix("```") {
                if inCode {
                    flushCode()
                    inCode = false
                } else {
                    flushParagraph()
                    let language = String(trimmed.dropFirst(3)).trimmingCharacters(in: .whitespacesAndNewlines)
                    codeLanguage = language.isEmpty ? nil : language
                    inCode = true
                }
                index += 1
                continue
            }

            if inCode {
                codeLines.append(line)
                index += 1
                continue
            }

            if let table = parseTable(lines: lines, startIndex: index) {
                flushParagraph()
                flushListsAndQuote()
                blocks.append(.table(table.table))
                index = table.nextIndex
                continue
            }

            if trimmed.isEmpty {
                flushParagraph()
                flushListsAndQuote()
                index += 1
                continue
            }

            if let heading = parseHeading(trimmed) {
                flushParagraph()
                flushListsAndQuote()
                blocks.append(.heading(level: heading.level, heading.text))
                index += 1
                continue
            }

            if isSeparator(trimmed) {
                flushParagraph()
                flushListsAndQuote()
                blocks.append(.separator)
                index += 1
                continue
            }

            if let quote = parseQuote(trimmed) {
                flushParagraph()
                if !unorderedItems.isEmpty || !orderedItems.isEmpty {
                    flushListsAndQuote()
                }
                quoteLines.append(quote)
                index += 1
                continue
            }

            if let item = parseUnorderedItem(trimmed) {
                flushParagraph()
                if !quoteLines.isEmpty || !orderedItems.isEmpty {
                    flushListsAndQuote()
                }
                unorderedItems.append(item)
                index += 1
                continue
            }

            if let item = parseOrderedItem(trimmed) {
                flushParagraph()
                if !quoteLines.isEmpty || !unorderedItems.isEmpty {
                    flushListsAndQuote()
                }
                orderedItems.append(item)
                index += 1
                continue
            }

            flushListsAndQuote()
            paragraphLines.append(line)
            index += 1
        }

        if inCode {
            flushCode()
        }
        flushParagraph()
        flushListsAndQuote()

        if blocks.isEmpty && !text.isEmpty {
            blocks.append(.paragraph(text))
        }
        return blocks
    }

    private static func parseHeading(_ line: String) -> (level: Int, text: String)? {
        let markerCount = line.prefix { $0 == "#" }.count
        guard (1...4).contains(markerCount) else {
            return nil
        }
        let remaining = line.dropFirst(markerCount)
        guard remaining.first == " " else {
            return nil
        }
        let text = remaining.trimmingCharacters(in: .whitespacesAndNewlines)
        return text.isEmpty ? nil : (markerCount, text)
    }

    private static func parseQuote(_ line: String) -> String? {
        guard line.hasPrefix(">") else {
            return nil
        }
        return line.dropFirst().trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private static func parseUnorderedItem(_ line: String) -> String? {
        for marker in ["- ", "* ", "+ "] where line.hasPrefix(marker) {
            let item = String(line.dropFirst(marker.count)).trimmingCharacters(in: .whitespacesAndNewlines)
            return item.isEmpty ? nil : item
        }
        return nil
    }

    private static func parseOrderedItem(_ line: String) -> String? {
        guard let dot = line.firstIndex(of: ".") else {
            return nil
        }
        let number = line[..<dot]
        guard !number.isEmpty, number.allSatisfy(\.isNumber) else {
            return nil
        }
        let afterDot = line[line.index(after: dot)...]
        guard afterDot.first == " " else {
            return nil
        }
        let item = afterDot.trimmingCharacters(in: .whitespacesAndNewlines)
        return item.isEmpty ? nil : item
    }

    private static func isSeparator(_ line: String) -> Bool {
        let normalized = line.replacingOccurrences(of: " ", with: "")
        return normalized.count >= 3
            && (normalized.allSatisfy { $0 == "-" }
                || normalized.allSatisfy { $0 == "*" }
                || normalized.allSatisfy { $0 == "_" })
    }

    private static func parseTable(lines: [String], startIndex: Int) -> (table: MarkdownTable, nextIndex: Int)? {
        guard startIndex + 1 < lines.count else {
            return nil
        }
        let headerLine = lines[startIndex].trimmingCharacters(in: .whitespaces)
        let separatorLine = lines[startIndex + 1].trimmingCharacters(in: .whitespaces)
        guard headerLine.contains("|"),
              let headers = parseTableRow(headerLine),
              headers.count >= 2,
              let alignments = parseTableSeparator(separatorLine, columnCount: headers.count) else {
            return nil
        }

        var rows: [[String]] = []
        var nextIndex = startIndex + 2
        while nextIndex < lines.count {
            let line = lines[nextIndex].trimmingCharacters(in: .whitespaces)
            guard line.contains("|"),
                  let row = parseTableRow(line),
                  !row.allSatisfy(\.isEmpty) else {
                break
            }
            rows.append(normalizeTableRow(row, columnCount: headers.count))
            nextIndex += 1
        }

        return (
            MarkdownTable(
                headers: normalizeTableRow(headers, columnCount: headers.count),
                alignments: normalizeTableAlignments(alignments, columnCount: headers.count),
                rows: rows
            ),
            nextIndex
        )
    }

    private static func parseTableRow(_ line: String) -> [String]? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        let hasLeadingPipe = trimmed.hasPrefix("|")
        let hasTrailingPipe = trimmed.hasSuffix("|")
        let dropStart = hasLeadingPipe ? 1 : 0
        let dropEnd = hasTrailingPipe ? 1 : 0
        let content = String(trimmed.dropFirst(dropStart).dropLast(dropEnd))
        var cells: [String] = []
        var current = ""
        var isEscaped = false

        for character in content {
            if isEscaped {
                current.append(character)
                isEscaped = false
                continue
            }
            if character == "\\" {
                isEscaped = true
                continue
            }
            if character == "|" {
                cells.append(current.trimmingCharacters(in: .whitespacesAndNewlines))
                current.removeAll()
            } else {
                current.append(character)
            }
        }
        cells.append(current.trimmingCharacters(in: .whitespacesAndNewlines))
        return cells.count >= 2 ? cells : nil
    }

    private static func parseTableSeparator(_ line: String, columnCount: Int) -> [MarkdownTable.Alignment]? {
        guard let cells = parseTableRow(line), cells.count >= columnCount else {
            return nil
        }
        let alignments = cells.prefix(columnCount).map { cell -> MarkdownTable.Alignment? in
            let value = cell.trimmingCharacters(in: .whitespaces)
            let stripped = value.trimmingCharacters(in: CharacterSet(charactersIn: ":"))
            guard stripped.count >= 1, stripped.allSatisfy({ $0 == "-" }) else {
                return nil
            }
            if value.hasPrefix(":"), value.hasSuffix(":") {
                return .center
            }
            if value.hasSuffix(":") {
                return .trailing
            }
            return .leading
        }
        guard alignments.allSatisfy({ $0 != nil }) else {
            return nil
        }
        return alignments.map { $0 ?? .leading }
    }

    private static func normalizeTableRow(_ row: [String], columnCount: Int) -> [String] {
        if row.count == columnCount {
            return row
        }
        if row.count > columnCount {
            return Array(row.prefix(columnCount))
        }
        return row + Array(repeating: "", count: columnCount - row.count)
    }

    private static func normalizeTableAlignments(_ alignments: [MarkdownTable.Alignment], columnCount: Int) -> [MarkdownTable.Alignment] {
        if alignments.count >= columnCount {
            return Array(alignments.prefix(columnCount))
        }
        return alignments + Array(repeating: .leading, count: columnCount - alignments.count)
    }
}

private struct MarkdownTable: Hashable {
    enum Alignment: Hashable {
        case leading
        case center
        case trailing
    }

    let headers: [String]
    let alignments: [Alignment]
    let rows: [[String]]
}

private struct MarkdownHeadingView: View {
    let level: Int
    let text: String
    let compact: Bool
    let fillsWidth: Bool

    var body: some View {
        Text(attributedText)
            .font(font)
            .textSelection(.enabled)
            .modifier(MarkdownWidthModifier(fillsWidth: fillsWidth))
            .padding(.top, level == 1 ? 4 : 2)
    }

    private var font: Font {
        switch level {
        case 1:
            compact ? .title3.weight(.bold) : .title2.weight(.bold)
        case 2:
            compact ? .headline.weight(.bold) : .title3.weight(.bold)
        default:
            .body.weight(.semibold)
        }
    }

    private var attributedText: AttributedString {
        MarkdownInline.parse(text)
    }
}

private struct MarkdownParagraphView: View {
    let text: String
    let compact: Bool
    let fillsWidth: Bool

    var body: some View {
        #if os(macOS)
        if !compact {
            MacSelectableAttributedText(
                attributedText: attributedText,
                font: .systemFont(ofSize: NSFont.systemFontSize),
                lineSpacing: 3
            )
            .modifier(MarkdownWidthModifier(fillsWidth: fillsWidth))
        } else {
            swiftUIText
        }
        #else
        swiftUIText
        #endif
    }

    private var swiftUIText: some View {
        Text(attributedText)
            .font(compact ? .body : .body)
            .lineSpacing(compact ? 2 : 3)
            .textSelection(.enabled)
            .modifier(MarkdownWidthModifier(fillsWidth: fillsWidth))
    }

    private var attributedText: AttributedString {
        MarkdownInline.parse(text)
    }
}

#if os(macOS)
private struct MacSelectableAttributedText: View {
    let attributedText: AttributedString
    let font: NSFont
    let lineSpacing: CGFloat
    @State private var measuredHeight: CGFloat = 24

    var body: some View {
        GeometryReader { proxy in
            MacSelectableAttributedTextRepresentable(
                attributedText: attributedText,
                font: font,
                lineSpacing: lineSpacing,
                width: max(1, proxy.size.width.rounded(.down)),
                measuredHeight: $measuredHeight
            )
        }
        .frame(height: measuredHeight)
    }
}

private struct MacSelectableAttributedTextRepresentable: NSViewRepresentable {
    let attributedText: AttributedString
    let font: NSFont
    let lineSpacing: CGFloat
    let width: CGFloat
    @Binding var measuredHeight: CGFloat

    func makeNSView(context: Context) -> NSTextView {
        let textView = NSTextView()
        textView.isEditable = false
        textView.isSelectable = true
        textView.drawsBackground = false
        textView.backgroundColor = .clear
        textView.textColor = .labelColor
        textView.font = font
        textView.textContainerInset = .zero
        textView.textContainer?.lineFragmentPadding = 0
        textView.textContainer?.widthTracksTextView = true
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width]
        textView.isRichText = false
        textView.importsGraphics = false
        textView.allowsUndo = false
        textView.usesFindBar = false
        textView.usesFontPanel = false
        textView.usesRuler = false
        textView.isContinuousSpellCheckingEnabled = false
        textView.isGrammarCheckingEnabled = false
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticDashSubstitutionEnabled = false
        textView.isAutomaticTextReplacementEnabled = false
        textView.isAutomaticSpellingCorrectionEnabled = false
        textView.isAutomaticLinkDetectionEnabled = false
        textView.isAutomaticDataDetectionEnabled = false
        textView.isAutomaticTextCompletionEnabled = false
        textView.enabledTextCheckingTypes = 0
        return textView
    }

    func updateNSView(_ textView: NSTextView, context: Context) {
        let rendered = NSMutableAttributedString(attributedString: NSAttributedString(attributedText))
        let fullRange = NSRange(location: 0, length: rendered.length)
        if fullRange.length > 0 {
            let paragraph = NSMutableParagraphStyle()
            paragraph.lineSpacing = lineSpacing
            rendered.addAttributes(
                [
                    .font: font,
                    .foregroundColor: NSColor.labelColor,
                    .paragraphStyle: paragraph
                ],
                range: fullRange
            )
        }

        if textView.attributedString() != rendered {
            textView.textStorage?.setAttributedString(rendered)
        }

        let resolvedWidth = max(width, 1)
        if abs(textView.frame.size.width - resolvedWidth) > 0.5 {
            textView.textContainer?.containerSize = NSSize(width: resolvedWidth, height: CGFloat.greatestFiniteMagnitude)
            textView.frame.size.width = resolvedWidth
        }

        DispatchQueue.main.async {
            guard let layoutManager = textView.layoutManager,
                  let textContainer = textView.textContainer
            else {
                return
            }
            layoutManager.ensureLayout(for: textContainer)
            let usedHeight = ceil(layoutManager.usedRect(for: textContainer).height)
            let nextHeight = max(24, usedHeight)
            if abs(measuredHeight - nextHeight) > 0.5 {
                measuredHeight = nextHeight
            }
        }
    }
}
#endif

private struct MarkdownQuoteView: View {
    let text: String
    let compact: Bool
    let fillsWidth: Bool

    var body: some View {
        HStack(alignment: .top, spacing: 9) {
            RoundedRectangle(cornerRadius: 2, style: .continuous)
                .fill(Color.accentColor.opacity(0.65))
                .frame(width: 3)

            MarkdownParagraphView(text: text, compact: compact, fillsWidth: fillsWidth)
                .foregroundStyle(.secondary)
        }
        .padding(.vertical, 6)
        .padding(.horizontal, 9)
        .background(PlatformColor.secondaryBackground.opacity(0.58))
        .clipShape(RoundedRectangle(cornerRadius: compact ? 10 : 8, style: .continuous))
    }
}

private struct MarkdownListView: View {
    let items: [String]
    let ordered: Bool
    let compact: Bool
    let fillsWidth: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: compact ? 5 : 7) {
            ForEach(Array(items.enumerated()), id: \.offset) { index, item in
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    Text(marker(index: index))
                        .font(.body.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .frame(width: ordered ? 24 : 13, alignment: .trailing)

                    Text(MarkdownInline.parse(item))
                        .font(.body)
                        .lineSpacing(compact ? 2 : 3)
                        .textSelection(.enabled)
                        .modifier(MarkdownWidthModifier(fillsWidth: fillsWidth))
                }
            }
        }
        .modifier(MarkdownWidthModifier(fillsWidth: fillsWidth))
    }

    private func marker(index: Int) -> String {
        ordered ? "\(index + 1)." : "•"
    }
}

private struct MarkdownTableView: View {
    let table: MarkdownTable
    let compact: Bool
    let fillsWidth: Bool

    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        ScrollView(.horizontal, showsIndicators: true) {
            VStack(alignment: .leading, spacing: 0) {
                tableRow(cells: table.headers, isHeader: true)

                Divider()

                ForEach(Array(table.rows.enumerated()), id: \.offset) { index, row in
                    tableRow(cells: row, isHeader: false)
                        .background(index.isMultiple(of: 2) ? Color.clear : alternatingRowBackground)
                }
            }
            .fixedSize(horizontal: true, vertical: false)
            .background(tableBackground)
            .clipShape(RoundedRectangle(cornerRadius: compact ? 9 : 10, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: compact ? 9 : 10, style: .continuous)
                    .strokeBorder(PlatformColor.separator.opacity(0.45))
            }
        }
        .modifier(MarkdownWidthModifier(fillsWidth: fillsWidth))
    }

    private func tableRow(cells: [String], isHeader: Bool) -> some View {
        HStack(alignment: .top, spacing: 0) {
            ForEach(Array(cells.enumerated()), id: \.offset) { index, cell in
                Text(MarkdownInline.parse(cell.isEmpty ? " " : cell))
                    .font(isHeader ? .callout.weight(.semibold) : .callout)
                    .lineSpacing(compact ? 1 : 2)
                    .multilineTextAlignment(textAlignment(for: index))
                    .textSelection(.enabled)
                    .frame(width: columnWidth(for: index), alignment: cellFrameAlignment(for: index))
                    .padding(.horizontal, compact ? 9 : 11)
                    .padding(.vertical, compact ? 7 : 9)

                if index < cells.count - 1 {
                    Rectangle()
                        .fill(PlatformColor.separator.opacity(0.36))
                        .frame(width: 1)
                }
            }
        }
    }

    private var tableBackground: Color {
        #if os(macOS)
        colorScheme == .light ? Color.black.opacity(0.025) : PlatformColor.secondaryBackground.opacity(0.52)
        #else
        PlatformColor.secondaryBackground.opacity(0.52)
        #endif
    }

    private var alternatingRowBackground: Color {
        #if os(macOS)
        colorScheme == .light ? Color.black.opacity(0.025) : PlatformColor.secondaryBackground.opacity(0.35)
        #else
        PlatformColor.secondaryBackground.opacity(0.35)
        #endif
    }

    private func columnWidth(for index: Int) -> CGFloat {
        let values = [table.headers[safe: index] ?? ""] + table.rows.map { $0[safe: index] ?? "" }
        let longest = values.map(estimatedTextWidth).max() ?? 0
        let estimated = longest + CGFloat(compact ? 30 : 38)
        let minimum = minColumnWidth(for: index)
        let maximum = maxColumnWidth(for: index)
        return min(max(estimated, minimum), maximum)
    }

    private func cellFrameAlignment(for index: Int) -> SwiftUI.Alignment {
        switch effectiveAlignment(for: index) {
        case .leading:
            return .leading
        case .center:
            return .center
        case .trailing:
            return .trailing
        }
    }

    private func textAlignment(for index: Int) -> TextAlignment {
        switch effectiveAlignment(for: index) {
        case .leading:
            return .leading
        case .center:
            return .center
        case .trailing:
            return .trailing
        }
    }

    private func effectiveAlignment(for index: Int) -> MarkdownTable.Alignment {
        let alignment = table.alignments[safe: index] ?? .leading

        // In chat transcripts, right-aligned numeric columns make narrow tables look
        // ragged beside prose. Preserve explicit center alignment, but keep regular
        // and trailing columns leading for stable scanning across iOS and macOS.
        if alignment == .center {
            return .center
        }
        return .leading
    }

    private func minColumnWidth(for index: Int) -> CGFloat {
        if index == 0 {
            return compact ? 72 : 84
        }
        return compact ? 112 : 132
    }

    private func maxColumnWidth(for index: Int) -> CGFloat {
        let isLastColumn = index == max(0, table.headers.count - 1)
        if isLastColumn {
            return compact ? 420 : 560
        }
        return compact ? 300 : 380
    }

    private func estimatedTextWidth(_ value: String) -> CGFloat {
        value.reduce(CGFloat.zero) { width, character in
            if character.isNewline {
                return width
            }
            if character.isASCII {
                return width + (character.isWhitespace ? 4.5 : 8.1)
            }
            if character.isEmojiPresentation {
                return width + 18
            }
            return width + 15.8
        }
    }
}

private extension Character {
    var isASCII: Bool {
        unicodeScalars.allSatisfy(\.isASCII)
    }

    var isEmojiPresentation: Bool {
        unicodeScalars.contains { scalar in
            scalar.properties.isEmojiPresentation
        }
    }
}

private enum MarkdownInline {
    private static let cacheLock = NSLock()
    private static var parseCache: [String: AttributedString] = [:]

    static func parse(_ text: String) -> AttributedString {
        cacheLock.lock()
        if let cached = parseCache[text] {
            cacheLock.unlock()
            return cached
        }
        cacheLock.unlock()

        let attributed: AttributedString
        if let parsed = try? AttributedString(
            markdown: text,
            options: AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        ) {
            attributed = parsed
        } else {
            attributed = AttributedString(text)
        }

        cacheLock.lock()
        if parseCache.count > 2_048 {
            parseCache.removeAll(keepingCapacity: true)
        }
        parseCache[text] = attributed
        cacheLock.unlock()
        return attributed
    }
}

private extension Array {
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}

struct CodeBlockView: View {
    let code: String
    var language: String?
    var compact = false

    @State private var isCollapsed = false
    @State private var copied = false
    @Environment(\.colorScheme) private var colorScheme

    private var lineCount: Int {
        max(1, code.components(separatedBy: .newlines).count)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 8) {
                Label(languageLabel, systemImage: "chevron.left.forwardslash.chevron.right")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)

                Text("\(lineCount) line\(lineCount == 1 ? "" : "s")")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)

                Spacer(minLength: 8)

                Button {
                    withAnimation(.smooth(duration: 0.18)) {
                        isCollapsed.toggle()
                    }
                } label: {
                    Image(systemName: isCollapsed ? "chevron.down" : "chevron.up")
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .accessibilityLabel(isCollapsed ? "Expand Code" : "Collapse Code")

                Button {
                    Pasteboard.copy(code)
                    copied = true
                    Task {
                        try? await Task.sleep(for: .seconds(1.2))
                        copied = false
                    }
                } label: {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                }
                .buttonStyle(.plain)
                .foregroundStyle(copied ? .green : .secondary)
                .accessibilityLabel("Copy Code")
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .background(codeHeaderBackground)

            if !isCollapsed {
                ScrollView(.horizontal) {
                    Text(code.isEmpty ? " " : code)
                        .font(.system(compact ? .caption : .callout, design: .monospaced))
                        .textSelection(.enabled)
                        .padding(10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .background(codeBodyBackground)
                .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: compact ? 9 : 10, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: compact ? 9 : 10, style: .continuous)
                .strokeBorder(PlatformColor.separator.opacity(0.38))
        }
    }

    private var languageLabel: String {
        language?.isEmpty == false ? language! : "code"
    }

    private var codeHeaderBackground: Color {
        #if os(macOS)
        colorScheme == .light ? Color.black.opacity(0.04) : PlatformColor.controlBackground.opacity(0.9)
        #else
        PlatformColor.controlBackground.opacity(0.9)
        #endif
    }

    private var codeBodyBackground: Color {
        #if os(macOS)
        colorScheme == .light ? Color.black.opacity(0.025) : PlatformColor.secondaryBackground.opacity(0.72)
        #else
        PlatformColor.secondaryBackground.opacity(0.72)
        #endif
    }
}

enum Pasteboard {
    static func copy(_ text: String) {
        #if os(macOS)
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        #else
        UIPasteboard.general.string = text
        #endif
    }
}
