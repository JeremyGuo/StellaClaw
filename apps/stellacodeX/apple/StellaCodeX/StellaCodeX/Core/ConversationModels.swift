import Foundation

enum ConversationStatus: String, CaseIterable, Hashable, Codable {
    case idle = "Idle"
    case running = "Running"
    case failed = "Failed"
}

struct ConversationSummary: Identifiable, Hashable, Codable {
    let id: String
    var title: String
    var workspacePath: String
    var lastMessagePreview: String
    var status: ConversationStatus
    var updatedAt: Date
    var model: String
    var modelSelectionPending: Bool? = nil
    var reasoning: String
    var sandbox: String
    var sandboxSource: String? = nil
    var remote: String
    var messageCount: Int
    var lastMessageID: String?
    var lastSeenMessageID: String? = nil
    var lastSeenAt: Date? = nil
    var isUnread: Bool
    var isPinned: Bool = false
}

struct ConversationSeen: Hashable {
    var lastSeenMessageID: String
    var updatedAt: Date
}

struct TurnProgressFeedback: Identifiable, Hashable {
    var id: String
    var phase: String
    var model: String
    var activity: String
    var hint: String?
    var error: String?
    var finalState: String?
    var plan: TurnProgressPlan?

    var isActive: Bool {
        let normalizedPhase = phase.lowercased()
        let normalizedFinal = finalState?.lowercased()
        return normalizedFinal != "done"
            && normalizedFinal != "failed"
            && normalizedPhase != "done"
            && normalizedPhase != "failed"
    }

    var title: String {
        let normalizedPhase = phase.lowercased()
        if finalState?.lowercased() == "failed" || normalizedPhase == "failed" {
            return "Failed"
        }
        if finalState?.lowercased() == "done" || normalizedPhase == "done" {
            return "Completed"
        }
        if normalizedPhase.contains("work") {
            return "Working"
        }
        if normalizedPhase.contains("think") || normalizedPhase.contains("reason") {
            return "Thinking"
        }
        return "Running"
    }

    var subtitle: String {
        if !model.isEmpty, !activity.isEmpty {
            return "\(model) · \(activity)"
        }
        if !model.isEmpty {
            return model
        }
        return activity
    }
}

struct TurnProgressPlan: Hashable {
    var explanation: String?
    var items: [TurnProgressPlanItem]
}

struct TurnProgressPlanItem: Identifiable, Hashable {
    var id: String { step }
    var step: String
    var status: String
}

struct PendingConversationDeletion: Identifiable, Hashable {
    let id: ConversationSummary.ID
    var title: String
}

enum ChatRole: String, Hashable, Codable {
    case user = "User"
    case assistant = "Assistant"
    case tool = "Tool"
    case system = "System"
}

enum ToolActivityKind: String, Hashable, Codable {
    case call = "Call"
    case result = "Result"
}

struct ToolActivity: Identifiable, Hashable, Codable {
    let id: String
    var kind: ToolActivityKind
    var name: String
    var summary: String
    var detail: String
}

struct ChatAttachment: Identifiable, Hashable, Codable {
    let id: String
    var index: Int
    var source: String
    var kind: String
    var name: String
    var path: String
    var uri: String
    var mediaType: String?
    var width: Int?
    var height: Int?
    var sizeBytes: Int?
    var url: String
    var marker: String?
    var thumbnailDataURL: String?

    var isImage: Bool {
        kind == "image" || mediaType?.hasPrefix("image/") == true || thumbnailDataURL != nil
    }
}

struct ChatMessage: Identifiable, Hashable, Codable {
    let id: String
    var index: Int
    var role: ChatRole
    var body: String
    var toolActivities: [ToolActivity] = []
    var attachments: [ChatAttachment] = []
    var tokenUsage: TokenUsage? = nil
    var timestamp: Date
    var userName: String?
    var isOptimistic: Bool
    var pending: Bool
    var error: String?
}

struct TokenUsage: Hashable, Codable {
    var cacheRead: Int
    var cacheWrite: Int
    var input: Int
    var output: Int
    var total: Int
    var costUSD: Double?

    var hasUsage: Bool {
        total > 0 || cacheRead > 0 || cacheWrite > 0 || input > 0 || output > 0 || (costUSD ?? 0) > 0
    }
}

struct ConversationUsageCost: Hashable {
    var cacheRead: Double
    var cacheWrite: Double
    var input: Double
    var output: Double

    var total: Double {
        cacheRead + cacheWrite + input + output
    }
}

struct ConversationUsageTotals: Hashable {
    var cacheRead: Int
    var cacheWrite: Int
    var input: Int
    var output: Int
    var cost: ConversationUsageCost

    static let empty = ConversationUsageTotals(
        cacheRead: 0,
        cacheWrite: 0,
        input: 0,
        output: 0,
        cost: ConversationUsageCost(cacheRead: 0, cacheWrite: 0, input: 0, output: 0)
    )

    var totalTokens: Int {
        cacheRead + cacheWrite + input + output
    }

    var cacheHitRate: Double {
        guard totalTokens > 0 else {
            return 0
        }
        return Double(cacheRead) / Double(totalTokens)
    }

    func adding(_ other: ConversationUsageTotals) -> ConversationUsageTotals {
        ConversationUsageTotals(
            cacheRead: cacheRead + other.cacheRead,
            cacheWrite: cacheWrite + other.cacheWrite,
            input: input + other.input,
            output: output + other.output,
            cost: ConversationUsageCost(
                cacheRead: cost.cacheRead + other.cost.cacheRead,
                cacheWrite: cost.cacheWrite + other.cost.cacheWrite,
                input: cost.input + other.cost.input,
                output: cost.output + other.cost.output
            )
        )
    }
}

struct ConversationUsageSummary: Hashable {
    var foreground: ConversationUsageTotals
    var background: ConversationUsageTotals
    var subagents: ConversationUsageTotals
    var mediaTools: ConversationUsageTotals

    var total: ConversationUsageTotals {
        foreground
            .adding(background)
            .adding(subagents)
            .adding(mediaTools)
    }
}

struct ConversationStatusSnapshot: Hashable {
    var conversationID: String
    var model: String
    var reasoning: String
    var sandbox: String
    var sandboxSource: String
    var remote: String
    var workspace: String
    var runningBackground: Int
    var totalBackground: Int
    var runningSubagents: Int
    var totalSubagents: Int
    var usage: ConversationUsageSummary
}

struct ChatMessageDetail: Identifiable, Hashable {
    let id: String
    var conversationID: String
    var message: ChatMessage
    var renderedText: String
    var toolActivities: [ToolActivity]
    var attachments: [ChatAttachment] = []
    var attachmentCount: Int
    var attachmentErrors: [String]

    var displayText: String {
        let trimmedRendered = renderedText.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmedRendered.isEmpty {
            return trimmedRendered
        }
        return message.body
    }
}

struct OutgoingMessageFile: Hashable, Encodable {
    var uri: String
    var mediaType: String?
    var name: String?
    var sizeBytes: Int?
    var width: Int?
    var height: Int?
    var thumbnailDataURL: String?

    enum CodingKeys: String, CodingKey {
        case uri
        case mediaType = "media_type"
        case name
    }
}

struct ChatDetailPresentation: Identifiable, Hashable {
    let id: String
    var detail: ChatMessageDetail
    var selectedToolID: ToolActivity.ID?

    var selectedTool: ToolActivity? {
        guard let selectedToolID else {
            return nil
        }
        return detail.toolActivities.first { $0.id == selectedToolID }
    }
}

struct ModelSummary: Identifiable, Hashable {
    var id: String { alias }
    var alias: String
    var modelName: String
    var providerType: String
    var capabilities: [String]
    var tokenMaxContext: Int
    var maxTokens: Int
    var effectiveMaxTokens: Int
}

struct WorkspaceListing: Hashable {
    var conversationID: String
    var mode: String
    var remote: WorkspaceRemote?
    var workspaceRoot: String
    var path: String
    var parent: String?
    var totalEntries: Int
    var returnedEntries: Int
    var truncated: Bool
    var entries: [WorkspaceEntry]

    var locationLabel: String {
        if let remote {
            return [remote.host, remote.cwd].compactMap(\.self).filter { !$0.isEmpty }.joined(separator: " - ")
        }
        return mode.isEmpty ? "local workspace" : mode
    }
}

struct WorkspaceRemote: Hashable {
    var host: String
    var cwd: String?
}

struct WorkspaceEntry: Identifiable, Hashable {
    var id: String { path.isEmpty ? name : path }
    var name: String
    var path: String
    var kind: String
    var sizeBytes: Int64?
    var modifiedMS: UInt64?
    var hidden: Bool
    var readonly: Bool

    var isDirectory: Bool {
        kind == "directory"
    }
}

struct WorkspaceFile: Hashable {
    var conversationID: String
    var mode: String
    var remote: WorkspaceRemote?
    var workspaceRoot: String
    var path: String
    var name: String
    var sizeBytes: Int64
    var modifiedMS: UInt64?
    var offset: Int64
    var returnedBytes: Int
    var truncated: Bool
    var encoding: String
    var data: String

    var isText: Bool {
        encoding == "utf8"
    }

    var decodedData: Data? {
        Data(base64Encoded: data)
    }
}

struct TerminalSummary: Identifiable, Hashable {
    var id: String { terminalID }
    var terminalID: String
    var conversationID: String
    var mode: String
    var remote: WorkspaceRemote?
    var shell: String
    var cwd: String
    var cols: Int
    var rows: Int
    var running: Bool
    var createdMS: UInt64
    var updatedMS: UInt64
    var nextOffset: UInt64
}

struct TerminalCreateOptions: Encodable, Hashable {
    var shell: String?
    var cwd: String?
    var cols: Int?
    var rows: Int?

    init(shell: String? = nil, cwd: String? = nil, cols: Int? = nil, rows: Int? = nil) {
        self.shell = shell
        self.cwd = cwd
        self.cols = cols
        self.rows = rows
    }
}

enum TerminalStreamEvent: Hashable {
    case attached(nextOffset: UInt64, running: Bool)
    case output(Data)
    case dropped(UInt64)
    case exit
    case detached(String)
    case error(String)
}
