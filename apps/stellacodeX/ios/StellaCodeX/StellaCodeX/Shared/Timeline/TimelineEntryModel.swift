import CoreGraphics
import Foundation

enum TimelineEntry: Identifiable {
    case auxiliary([AuxiliaryUserMessage])
    case message(ChatMessage, [AuxiliaryUserMessage])
    case toolProcess(ToolProcessGroup)

    var id: String {
        switch self {
        case .auxiliary(let messages):
            messages.map(\.id).joined(separator: "-")
        case .message(let message, _):
            message.id
        case .toolProcess(let group):
            group.id
        }
    }

    var renderSignature: Int {
        var hasher = Hasher()
        hasher.combine(id)
        switch self {
        case .auxiliary(let messages):
            for message in messages {
                hasher.combine(message.id)
                hasher.combine(message.rawText)
                hasher.combine(message.fields.count)
            }
        case .message(let message, let auxiliaryMessages):
            hasher.combine(message.renderSignature)
            for auxiliaryMessage in auxiliaryMessages {
                hasher.combine(auxiliaryMessage.id)
                hasher.combine(auxiliaryMessage.rawText)
            }
        case .toolProcess(let group):
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
        }
        return hasher.finalize()
    }
}

struct TimelineSnapshot: Equatable {
    let messageIDs: [ChatMessage.ID]
    let entryIDs: [TimelineEntry.ID]
    let contentSignature: Int

    init(messageIDs: [ChatMessage.ID], entryIDs: [TimelineEntry.ID], contentSignature: Int = 0) {
        self.messageIDs = messageIDs
        self.entryIDs = entryIDs
        self.contentSignature = contentSignature
    }

    var firstMessageID: ChatMessage.ID? {
        messageIDs.first
    }

    var lastMessageID: ChatMessage.ID? {
        messageIDs.last
    }

    var firstEntryID: TimelineEntry.ID? {
        entryIDs.first
    }

    var lastEntryID: TimelineEntry.ID? {
        entryIDs.last
    }

    var isEmpty: Bool {
        entryIDs.isEmpty
    }
}

struct TimelineRenderData {
    let entries: [TimelineEntry]
    let snapshot: TimelineSnapshot

    static let empty = TimelineRenderData(
        entries: [],
        snapshot: TimelineSnapshot(messageIDs: [], entryIDs: [])
    )
}

enum TimelineMutation: Equatable {
    case unchanged
    case updated
    case prepended
    case appended
    case expanded
    case replaced

    init(previous: TimelineSnapshot, current: TimelineSnapshot) {
        guard previous.entryIDs != current.entryIDs else {
            self = previous.contentSignature == current.contentSignature ? .unchanged : .updated
            return
        }

        guard !previous.entryIDs.isEmpty,
              current.entryIDs.count > previous.entryIDs.count
        else {
            self = .replaced
            return
        }

        let previousCount = previous.entryIDs.count
        if current.entryIDs.suffix(previousCount).elementsEqual(previous.entryIDs) {
            self = .prepended
        } else if current.entryIDs.prefix(previousCount).elementsEqual(previous.entryIDs) {
            self = .appended
        } else if current.entryIDs.containsContiguousSubsequence(previous.entryIDs) {
            self = .expanded
        } else {
            self = .replaced
        }
    }
}

private extension Array where Element: Equatable {
    func containsContiguousSubsequence(_ subsequence: [Element]) -> Bool {
        guard !subsequence.isEmpty,
              subsequence.count <= count
        else {
            return false
        }

        for startIndex in 0...(count - subsequence.count) {
            let endIndex = startIndex + subsequence.count
            if Array(self[startIndex..<endIndex]) == subsequence {
                return true
            }
        }
        return false
    }
}

struct AuxiliaryUserMessage: Identifiable {
    let id: String
    var title: String
    var summary: String
    var fields: [(String, String)]
    var rawText: String

    init(message: ChatMessage) {
        id = message.id
        rawText = message.body.trimmingCharacters(in: .whitespacesAndNewlines)
        title = AuxiliaryUserMessage.title(from: rawText)
        fields = AuxiliaryUserMessage.fields(from: rawText)

        let fieldMap = Dictionary(uniqueKeysWithValues: fields)
        if title == "Incoming User Metadata" {
            let speaker = fieldMap["Speaker"]?.nilIfBlank ?? "unknown speaker"
            if let messageTime = fieldMap["Message time"]?.nilIfBlank {
                summary = "\(speaker) - \(messageTime)"
            } else {
                summary = speaker
            }
        } else {
            summary = fields.first.map { "\($0.0): \($0.1)" } ?? title
        }
    }

    private static func title(from text: String) -> String {
        guard text.hasPrefix("["),
              let end = text.firstIndex(of: "]")
        else {
            return "Context"
        }
        return String(text[text.index(after: text.startIndex)..<end])
    }

    private static func fields(from text: String) -> [(String, String)] {
        let lines = text
            .components(separatedBy: .newlines)
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
        var result: [(String, String)] = []
        var currentKey: String?

        func appendField(_ key: String, _ value: String) {
            let cleanKey = key.trimmingCharacters(in: .whitespacesAndNewlines)
            let cleanValue = value.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !cleanKey.isEmpty else {
                return
            }
            if let index = result.firstIndex(where: { $0.0 == cleanKey }) {
                result[index].1 = [result[index].1, cleanValue]
                    .filter { !$0.isEmpty }
                    .joined(separator: " ")
            } else {
                result.append((cleanKey, cleanValue))
            }
        }

        for line in lines.dropFirst() {
            if let delimiter = line.firstIndex(of: ":") {
                let key = String(line[..<delimiter])
                let value = String(line[line.index(after: delimiter)...])
                appendField(key, value)
                currentKey = value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? key : nil
            } else if let key = currentKey {
                appendField(key, line)
                currentKey = nil
            } else {
                appendField("Note", line)
            }
        }
        return result
    }
}

extension AuxiliaryUserMessage {
    var presentationHeight: CGFloat {
        let base: CGFloat = 104
        let fieldHeight = CGFloat(max(fields.count, 1)) * 44
        return min(max(base + fieldHeight, 190), 360)
    }
}

extension ChatMessage {
    var shouldRenderInTimeline: Bool {
        !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || !toolActivities.isEmpty
            || !attachments.isEmpty
            || tokenUsage?.hasUsage == true
            || pending
            || error != nil
    }

    var isToolProcessMessage: Bool {
        !toolActivities.isEmpty
    }

    var auxiliaryUserMessage: AuxiliaryUserMessage? {
        guard role == .user else {
            return nil
        }
        let trimmed = body.trimmingCharacters(in: .whitespacesAndNewlines)
        let lowercased = trimmed.lowercased()
        let prefixes = [
            "[incoming user metadata]",
            "[runtime prompt updates]",
            "[runtime skill updates]",
            "[system context]",
            "[developer context]",
            "[tool context]"
        ]
        guard prefixes.contains(where: { lowercased.hasPrefix($0) }) else {
            return nil
        }
        return AuxiliaryUserMessage(message: self)
    }

    var renderSignature: Int {
        var hasher = Hasher()
        hasher.combine(id)
        hasher.combine(index)
        hasher.combine(role.rawValue)
        hasher.combine(body)
        hasher.combine(userName)
        hasher.combine(isOptimistic)
        hasher.combine(pending)
        hasher.combine(error)
        hasher.combine(tokenUsage)
        for activity in toolActivities {
            hasher.combine(activity.id)
            hasher.combine(activity.kind.rawValue)
            hasher.combine(activity.name)
            hasher.combine(activity.summary)
            hasher.combine(activity.detail)
        }
        for attachment in attachments {
            hasher.combine(attachment.id)
            hasher.combine(attachment.name)
            hasher.combine(attachment.mediaType)
            hasher.combine(attachment.width)
            hasher.combine(attachment.height)
            hasher.combine(attachment.sizeBytes)
            hasher.combine(attachment.thumbnailDataURL)
        }
        return hasher.finalize()
    }
}

struct ToolProcessGroup: Identifiable {
    let id: String
    let messages: [ChatMessage]
    let activities: [ToolActivity]

    init(messages: [ChatMessage]) {
        self.messages = messages
        id = messages.first?.id ?? UUID().uuidString
        activities = messages.flatMap(\.toolActivities)
    }
}

func buildTimelineEntries(from messages: [ChatMessage]) -> [TimelineEntry] {
    var entries: [TimelineEntry] = []
    var pendingToolMessages: [ChatMessage] = []
    var pendingAuxiliaryMessages: [AuxiliaryUserMessage] = []

    func flushToolMessages() {
        guard !pendingToolMessages.isEmpty else {
            return
        }
        entries.append(.toolProcess(ToolProcessGroup(messages: pendingToolMessages)))
        pendingToolMessages.removeAll()
    }

    for message in messages where message.shouldRenderInTimeline {
        if let auxiliaryMessage = message.auxiliaryUserMessage {
            pendingAuxiliaryMessages.append(auxiliaryMessage)
        } else if message.isToolProcessMessage {
            pendingToolMessages.append(message)
        } else {
            flushToolMessages()
            let auxiliaryMessages = message.role == .user ? pendingAuxiliaryMessages : []
            if message.role == .user {
                pendingAuxiliaryMessages.removeAll()
            }
            entries.append(.message(message, auxiliaryMessages))
        }
    }

    flushToolMessages()
    if !pendingAuxiliaryMessages.isEmpty {
        entries.append(.auxiliary(pendingAuxiliaryMessages))
    }
    return entries
}

func buildTimelineRenderData(from messages: [ChatMessage]) -> TimelineRenderData {
    let renderableMessages = messages.filter(\.shouldRenderInTimeline)
    let entries = buildTimelineEntries(from: renderableMessages)
    return TimelineRenderData(
        entries: entries,
        snapshot: TimelineSnapshot(
            messageIDs: renderableMessages.map(\.id),
            entryIDs: entries.map(\.id),
            contentSignature: timelineContentSignature(for: renderableMessages)
        )
    )
}

private func timelineContentSignature(for messages: [ChatMessage]) -> Int {
    var hasher = Hasher()
    for message in messages {
        hasher.combine(message.renderSignature)
    }
    return hasher.finalize()
}

private extension String {
    var nilIfBlank: String? {
        let trimmed = trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }
}
