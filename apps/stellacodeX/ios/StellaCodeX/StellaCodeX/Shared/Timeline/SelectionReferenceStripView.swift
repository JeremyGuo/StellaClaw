import SwiftUI

struct SelectionReferenceStripView: View {
    let selections: [SelectionReference]
    var compact = false
    var alignment: HorizontalAlignment = .leading
    var fillsWidth = true

    var body: some View {
        if !selections.isEmpty {
            VStack(alignment: alignment, spacing: 7) {
                ForEach(selections) { selection in
                    SelectionReferenceCard(selection: selection, compact: compact)
                }
            }
            .frame(maxWidth: fillsWidth ? .infinity : nil, alignment: frameAlignment)
        }
    }

    private var frameAlignment: Alignment {
        alignment == .trailing ? .trailing : .leading
    }
}

private struct SelectionReferenceCard: View {
    let selection: SelectionReference
    let compact: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: compact ? 5 : 7) {
            HStack(spacing: 7) {
                Image(systemName: iconName)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(Color.accentColor)
                    .frame(width: 18)

                Text(selection.fileName?.selectionNilIfBlank ?? selection.filePath)
                    .font(.caption.weight(.semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)

                Spacer(minLength: 6)

                if !selection.sourceKind.isEmpty {
                    Text(selection.sourceKind.uppercased())
                        .font(.caption2.weight(.bold))
                        .foregroundStyle(.secondary)
                }
            }

            Text(selection.selectedText.trimmingCharacters(in: .whitespacesAndNewlines))
                .font(compact ? .caption : .callout)
                .lineLimit(compact ? 3 : 5)
                .textSelection(.enabled)
                .foregroundStyle(.primary)

            if let location = locationText {
                Text(location)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .padding(.horizontal, compact ? 10 : 12)
        .padding(.vertical, compact ? 8 : 10)
        .background(PlatformColor.secondaryBackground.opacity(0.78))
        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(Color.accentColor.opacity(0.22))
        }
        .contextMenu {
            Button {
                Pasteboard.copy(selection.selectedText)
            } label: {
                Label("Copy Selection", systemImage: "doc.on.doc")
            }

            Button {
                Pasteboard.copy(selection.filePath)
            } label: {
                Label("Copy File Path", systemImage: "folder")
            }
        }
    }

    private var iconName: String {
        switch selection.sourceKind.lowercased() {
        case "pdf":
            return "doc.richtext"
        case "markdown":
            return "text.alignleft"
        case "html":
            return "chevron.left.forwardslash.chevron.right"
        case "word", "docx":
            return "doc.text"
        case "file":
            return "doc"
        default:
            return "quote.opening"
        }
    }

    private var locationText: String? {
        guard let locator = selection.locator else {
            return selection.filePath
        }
        if let start = locator.startLine, let end = locator.endLine {
            return start == end ? "\(selection.filePath):\(start)" : "\(selection.filePath):\(start)-\(end)"
        }
        if let page = locator.page {
            return "\(selection.filePath) · page \(page)"
        }
        return selection.filePath
    }
}

private extension String {
    var selectionNilIfBlank: String? {
        let trimmed = trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
