import Foundation
import SwiftUI

struct ToolDetailContentView: View {
    let activity: ToolActivity

    private var analysis: ToolDetailAnalysis {
        ToolDetailAnalysis(activity: activity)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 8) {
                Label(analysis.kindLabel, systemImage: analysis.systemImage)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)

                if !analysis.fileReferences.isEmpty {
                    Text("\(analysis.fileReferences.count) file reference\(analysis.fileReferences.count == 1 ? "" : "s")")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)
                }

                Spacer()

                Button {
                    Pasteboard.copy(analysis.renderedText)
                } label: {
                    Label("Copy", systemImage: "doc.on.doc")
                }
                .buttonStyle(.borderless)
            }

            if !analysis.fileReferences.isEmpty {
                fileReferences
            }

            switch analysis.kind {
            case .json:
                CodeBlockView(code: analysis.renderedText, language: "json")
            case .diff:
                DiffContentView(diff: analysis.renderedText)
            case .longLog:
                LongLogContentView(text: analysis.renderedText)
            case .plain:
                CodeBlockView(code: analysis.renderedText, language: analysis.languageHint)
            }
        }
    }

    private var fileReferences: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                ForEach(analysis.fileReferences, id: \.self) { reference in
                    Text(reference)
                        .font(.caption.monospaced())
                        .lineLimit(1)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 5)
                        .background(PlatformColor.secondaryBackground.opacity(0.7))
                        .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
                }
            }
        }
    }
}

private struct ToolDetailAnalysis {
    enum Kind {
        case json
        case diff
        case longLog
        case plain
    }

    let activity: ToolActivity
    let renderedText: String
    let kind: Kind
    let fileReferences: [String]

    init(activity: ToolActivity) {
        self.activity = activity
        let raw = (activity.detail.isEmpty ? activity.summary : activity.detail)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let prettyJSON = Self.prettyJSON(raw)
        let rendered = prettyJSON ?? raw
        self.renderedText = rendered
        self.fileReferences = Self.extractFileReferences(from: rendered)

        if prettyJSON != nil {
            self.kind = .json
        } else if Self.looksLikeDiff(rendered) {
            self.kind = .diff
        } else if rendered.components(separatedBy: .newlines).count > 180 || rendered.count > 18_000 {
            self.kind = .longLog
        } else {
            self.kind = .plain
        }
    }

    var kindLabel: String {
        switch kind {
        case .json:
            "JSON"
        case .diff:
            "Diff"
        case .longLog:
            "Long log"
        case .plain:
            activity.kind == .call ? "Arguments" : "Output"
        }
    }

    var systemImage: String {
        switch kind {
        case .json:
            "curlybraces"
        case .diff:
            "plusminus"
        case .longLog:
            "text.alignleft"
        case .plain:
            activity.kind == .call ? "terminal" : "doc.plaintext"
        }
    }

    var languageHint: String {
        let name = activity.name.lowercased()
        if name.contains("shell") || name == "bash" || name == "exec" {
            return "shell"
        }
        if name.contains("python") {
            return "python"
        }
        return activity.kind == .call ? "input" : "text"
    }

    private static func prettyJSON(_ raw: String) -> String? {
        guard let data = raw.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data),
              JSONSerialization.isValidJSONObject(object),
              let pretty = try? JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys]),
              let output = String(data: pretty, encoding: .utf8)
        else {
            return nil
        }
        return output
    }

    private static func looksLikeDiff(_ text: String) -> Bool {
        let lines = text.components(separatedBy: .newlines).prefix(40)
        return lines.contains { line in
            line.hasPrefix("diff --git")
                || line.hasPrefix("@@")
                || line.hasPrefix("+++")
                || line.hasPrefix("---")
        }
    }

    private static func extractFileReferences(from text: String) -> [String] {
        let tokens = text
            .replacingOccurrences(of: "\"", with: " ")
            .replacingOccurrences(of: "'", with: " ")
            .replacingOccurrences(of: "(", with: " ")
            .replacingOccurrences(of: ")", with: " ")
            .components(separatedBy: .whitespacesAndNewlines)

        let candidates = tokens.compactMap { token -> String? in
            let value = token.trimmingCharacters(in: CharacterSet(charactersIn: ",;:"))
            guard value.count > 2 else {
                return nil
            }
            if value.hasPrefix("file://") || value.hasPrefix("/") || value.hasPrefix("./") || value.hasPrefix("../") {
                return value
            }
            if value.contains("/") && value.contains(".") {
                return value
            }
            return nil
        }

        var seen = Set<String>()
        return candidates.filter { candidate in
            guard !seen.contains(candidate) else {
                return false
            }
            seen.insert(candidate)
            return true
        }
        .prefix(12)
        .map(\.self)
    }
}

private struct DiffContentView: View {
    let diff: String

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(diff.components(separatedBy: .newlines).enumerated()), id: \.offset) { _, line in
                Text(line.isEmpty ? " " : line)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(foreground(for: line))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 2)
                    .background(background(for: line))
            }
        }
        .padding(.vertical, 8)
        .background(PlatformColor.secondaryBackground.opacity(0.72))
        .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .strokeBorder(PlatformColor.separator.opacity(0.38))
        }
    }

    private func foreground(for line: String) -> Color {
        if line.hasPrefix("+") && !line.hasPrefix("+++") {
            return .green
        }
        if line.hasPrefix("-") && !line.hasPrefix("---") {
            return .red
        }
        if line.hasPrefix("@@") {
            return .accentColor
        }
        return .primary
    }

    private func background(for line: String) -> Color {
        if line.hasPrefix("+") && !line.hasPrefix("+++") {
            return Color.green.opacity(0.09)
        }
        if line.hasPrefix("-") && !line.hasPrefix("---") {
            return Color.red.opacity(0.08)
        }
        if line.hasPrefix("@@") {
            return Color.accentColor.opacity(0.08)
        }
        return Color.clear
    }
}

private struct LongLogContentView: View {
    let text: String

    @State private var visibleLineLimit = 160

    private var lines: [String] {
        text.components(separatedBy: .newlines)
    }

    private var visibleText: String {
        lines.prefix(visibleLineLimit).joined(separator: "\n")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Text("\(lines.count) lines")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)

                Spacer()

                Button("Show 160 more") {
                    withAnimation(.smooth(duration: 0.16)) {
                        visibleLineLimit = min(lines.count, visibleLineLimit + 160)
                    }
                }
                .disabled(visibleLineLimit >= lines.count)

                Button("Show all") {
                    withAnimation(.smooth(duration: 0.16)) {
                        visibleLineLimit = lines.count
                    }
                }
                .disabled(visibleLineLimit >= lines.count)
            }
            .font(.caption)

            CodeBlockView(code: visibleText, language: "log")
        }
    }
}
