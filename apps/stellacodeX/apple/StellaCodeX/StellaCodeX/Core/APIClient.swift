import Foundation

protocol StellaAPIClient {
    func listModels() async throws -> [ModelSummary]
    func listConversations() async throws -> [ConversationSummary]
    func conversationStatus(conversationID: ConversationSummary.ID) async throws -> ConversationStatusSnapshot
    func conversationEvents() async throws -> AsyncThrowingStream<StellaConversationEvent, Error>
    func createConversation(nickname: String?, model: String?) async throws -> ConversationSummary.ID
    func renameConversation(id: ConversationSummary.ID, nickname: String) async throws -> ConversationSummary
    func deleteConversation(id: ConversationSummary.ID) async throws
    func markConversationSeen(conversationID: ConversationSummary.ID, lastSeenMessageID: String) async throws -> ConversationSeen
    func listMessagePage(conversationID: ConversationSummary.ID, offset: Int, limit: Int) async throws -> ChatMessagePage
    func messageDetail(conversationID: ConversationSummary.ID, messageID: ChatMessage.ID) async throws -> ChatMessageDetail
    func sendMessage(_ body: String, conversationID: ConversationSummary.ID, userName: String, remoteMessageID: String, files: [OutgoingMessageFile]) async throws
    func listWorkspace(conversationID: ConversationSummary.ID, path: String, limit: Int) async throws -> WorkspaceListing
    func workspaceFile(conversationID: ConversationSummary.ID, path: String, limitBytes: Int, full: Bool) async throws -> WorkspaceFile
    func downloadWorkspaceArchive(conversationID: ConversationSummary.ID, path: String) async throws -> Data
    func uploadWorkspaceArchive(conversationID: ConversationSummary.ID, path: String, archive: Data) async throws -> Int
    func deleteWorkspacePath(conversationID: ConversationSummary.ID, path: String) async throws
    func moveWorkspacePath(conversationID: ConversationSummary.ID, path: String, newPath: String) async throws
    func listTerminals(conversationID: ConversationSummary.ID) async throws -> [TerminalSummary]
    func createTerminal(conversationID: ConversationSummary.ID, options: TerminalCreateOptions) async throws -> TerminalSummary
    func terminateTerminal(conversationID: ConversationSummary.ID, terminalID: TerminalSummary.ID) async throws -> TerminalSummary
    func terminalSession(conversationID: ConversationSummary.ID, terminalID: TerminalSummary.ID, offset: UInt64) async throws -> TerminalWebSocketSession
    func foregroundEvents(conversationID: ConversationSummary.ID) async throws -> AsyncThrowingStream<StellaRealtimeEvent, Error>
}

struct ChatMessagePage: Hashable {
    var conversationID: String
    var offset: Int
    var limit: Int
    var total: Int
    var messages: [ChatMessage]

    var start: Int {
        messages.map(\.index).min() ?? offset
    }

    var end: Int {
        guard let maxIndex = messages.map(\.index).max() else {
            return offset
        }
        return maxIndex + 1
    }
}

struct MockStellaAPIClient: StellaAPIClient {
    private let now = Date()

    func listModels() async throws -> [ModelSummary] {
        [
            ModelSummary(
                alias: "main",
                modelName: "gpt-5.5",
                providerType: "openai",
                capabilities: ["text", "tool"],
                tokenMaxContext: 256_000,
                maxTokens: 16_384,
                effectiveMaxTokens: 16_384
            ),
            ModelSummary(
                alias: "fast",
                modelName: "gpt-5.4-mini",
                providerType: "openai",
                capabilities: ["text", "tool"],
                tokenMaxContext: 128_000,
                maxTokens: 8_192,
                effectiveMaxTokens: 8_192
            )
        ]
    }

    func listConversations() async throws -> [ConversationSummary] {
        [
            ConversationSummary(
                id: "mock-agent-server-workspace",
                title: "Agent server workspace",
                workspacePath: "~/Projects/ClawParty",
                lastMessagePreview: "Foreground session is ready.",
                status: .idle,
                updatedAt: now,
                model: "gpt-5.5",
                reasoning: "medium",
                sandbox: "subprocess",
                sandboxSource: "default",
                remote: "",
                messageCount: 4,
                lastMessageID: "3",
                lastSeenMessageID: "3",
                lastSeenAt: now,
                isUnread: false
            ),
            ConversationSummary(
                id: "mock-macos-client-skeleton",
                title: "macOS client skeleton",
                workspacePath: "apps/stellacodeX/macos",
                lastMessagePreview: "Build the first native shell.",
                status: .running,
                updatedAt: now.addingTimeInterval(-600),
                model: "codex subscription",
                reasoning: "high",
                sandbox: "bubblewrap",
                sandboxSource: "conversation",
                remote: "local",
                messageCount: 4,
                lastMessageID: "3",
                lastSeenMessageID: "1",
                lastSeenAt: now.addingTimeInterval(-900),
                isUnread: true
            )
        ]
    }

    func conversationEvents() async throws -> AsyncThrowingStream<StellaConversationEvent, Error> {
        AsyncThrowingStream { continuation in
            Task {
                do {
                    continuation.yield(.snapshot(try await listConversations()))
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
        }
    }

    func conversationStatus(conversationID: ConversationSummary.ID) async throws -> ConversationStatusSnapshot {
        ConversationStatusSnapshot(
            conversationID: conversationID,
            model: "gpt-5.5",
            reasoning: "medium",
            sandbox: "subprocess",
            sandboxSource: "default",
            remote: "local",
            workspace: "~/Projects/ClawParty",
            runningBackground: 1,
            totalBackground: 2,
            runningSubagents: 0,
            totalSubagents: 1,
            usage: ConversationUsageSummary(
                foreground: ConversationUsageTotals(
                    cacheRead: 64_200,
                    cacheWrite: 5_400,
                    input: 18_900,
                    output: 7_100,
                    cost: ConversationUsageCost(cacheRead: 0.021, cacheWrite: 0.034, input: 0.081, output: 0.16)
                ),
                background: ConversationUsageTotals(
                    cacheRead: 8_500,
                    cacheWrite: 1_100,
                    input: 4_200,
                    output: 1_900,
                    cost: ConversationUsageCost(cacheRead: 0.004, cacheWrite: 0.008, input: 0.017, output: 0.041)
                ),
                subagents: .empty,
                mediaTools: .empty
            )
        )
    }

    func createConversation(nickname: String?, model: String?) async throws -> ConversationSummary.ID {
        "mock-new-\(UUID().uuidString)"
    }

    func renameConversation(id: ConversationSummary.ID, nickname: String) async throws -> ConversationSummary {
        let existing = try await listConversations().first { $0.id == id }
        return ConversationSummary(
            id: id,
            title: nickname.trimmingCharacters(in: .whitespacesAndNewlines),
            workspacePath: existing?.workspacePath ?? "~/Projects/ClawParty",
            lastMessagePreview: existing?.lastMessagePreview ?? "Renamed mock conversation.",
            status: existing?.status ?? .idle,
            updatedAt: existing?.updatedAt ?? now,
            model: existing?.model ?? "gpt-5.5",
            reasoning: existing?.reasoning ?? "medium",
            sandbox: existing?.sandbox ?? "subprocess",
            sandboxSource: existing?.sandboxSource,
            remote: existing?.remote ?? "",
            messageCount: existing?.messageCount ?? 0,
            lastMessageID: existing?.lastMessageID,
            lastSeenMessageID: existing?.lastSeenMessageID,
            lastSeenAt: existing?.lastSeenAt,
            isUnread: existing?.isUnread ?? false
        )
    }

    func deleteConversation(id: ConversationSummary.ID) async throws {
    }

    func markConversationSeen(conversationID: ConversationSummary.ID, lastSeenMessageID: String) async throws -> ConversationSeen {
        ConversationSeen(lastSeenMessageID: lastSeenMessageID, updatedAt: now)
    }

    func listMessagePage(conversationID: ConversationSummary.ID, offset: Int = 0, limit: Int = 80) async throws -> ChatMessagePage {
        let messages = [
            ChatMessage(
                id: "0",
                index: 0,
                role: .system,
                body: "Connected to the mock Stellaclaw Web channel.",
                timestamp: now.addingTimeInterval(-240),
                userName: nil,
                isOptimistic: false,
                pending: false,
                error: nil
            ),
            ChatMessage(
                id: "1",
                index: 1,
                role: .user,
                body: "Start the macOS native client.",
                timestamp: now.addingTimeInterval(-180),
                userName: "workspace-user",
                isOptimistic: false,
                pending: false,
                error: nil
            ),
            ChatMessage(
                id: "2",
                index: 2,
                role: .assistant,
                body: "I checked the Apple client structure and prepared the first native shell.",
                toolActivities: [
                    ToolActivity(
                        id: "2-tool-0",
                        kind: .call,
                        name: "rg",
                        summary: "Locate Stellacode2 chat surfaces",
                        detail: "rg --files apps/stellacode2/src"
                    ),
                    ToolActivity(
                        id: "2-tool-1",
                        kind: .call,
                        name: "sed",
                        summary: "Read App and workspace components",
                        detail: "Read App.jsx and ChatWorkspace.jsx for feature parity."
                    ),
                    ToolActivity(
                        id: "2-tool-2",
                        kind: .result,
                        name: "xcodebuild",
                        summary: "macOS build succeeded",
                        detail: "The native SwiftUI target built successfully for macOS."
                    )
                ],
                timestamp: now.addingTimeInterval(-120),
                userName: nil,
                isOptimistic: false,
                pending: false,
                error: nil
            ),
            ChatMessage(
                id: "3",
                index: 3,
                role: .tool,
                body: "Mock realtime stream: waiting for Web channel integration.",
                timestamp: now.addingTimeInterval(-90),
                userName: nil,
                isOptimistic: false,
                pending: false,
                error: nil
            )
        ]
        return ChatMessagePage(
            conversationID: conversationID,
            offset: offset,
            limit: limit,
            total: messages.count,
            messages: messages
        )
    }

    func messageDetail(conversationID: ConversationSummary.ID, messageID: ChatMessage.ID) async throws -> ChatMessageDetail {
        let page = try await listMessagePage(conversationID: conversationID, offset: 0, limit: 80)
        let message = page.messages.first { $0.id == messageID } ?? page.messages.first!
        return ChatMessageDetail(
            id: "\(conversationID)-\(message.id)",
            conversationID: conversationID,
            message: message,
            renderedText: message.body,
            toolActivities: message.toolActivities,
            attachments: message.attachments,
            attachmentCount: 0,
            attachmentErrors: []
        )
    }

    func sendMessage(_ body: String, conversationID: ConversationSummary.ID, userName: String, remoteMessageID: String, files: [OutgoingMessageFile]) async throws {
    }

    func listWorkspace(conversationID: ConversationSummary.ID, path: String, limit: Int) async throws -> WorkspaceListing {
        WorkspaceListing(
            conversationID: conversationID,
            mode: "local",
            remote: nil,
            workspaceRoot: "~/Projects/ClawParty",
            path: path,
            parent: nil,
            totalEntries: 3,
            returnedEntries: 3,
            truncated: false,
            entries: [
                WorkspaceEntry(name: "README.md", path: "README.md", kind: "file", sizeBytes: 2048, modifiedMS: nil, hidden: false, readonly: false),
                WorkspaceEntry(name: "apps", path: "apps", kind: "directory", sizeBytes: nil, modifiedMS: nil, hidden: false, readonly: false),
                WorkspaceEntry(name: "core", path: "core", kind: "directory", sizeBytes: nil, modifiedMS: nil, hidden: false, readonly: false)
            ]
        )
    }

    func workspaceFile(conversationID: ConversationSummary.ID, path: String, limitBytes: Int, full: Bool) async throws -> WorkspaceFile {
        WorkspaceFile(
            conversationID: conversationID,
            mode: "local",
            remote: nil,
            workspaceRoot: "~/Projects/ClawParty",
            path: path,
            name: path.split(separator: "/").last.map(String.init) ?? "README.md",
            sizeBytes: 128,
            modifiedMS: nil,
            offset: 0,
            returnedBytes: 128,
            truncated: false,
            encoding: "utf8",
            data: "# Mock file\n\nWorkspace preview is connected."
        )
    }

    func downloadWorkspaceArchive(conversationID: ConversationSummary.ID, path: String) async throws -> Data {
        try TarGzipArchive.singleFile(name: "mock.txt", data: Data("mock download".utf8))
    }

    func uploadWorkspaceArchive(conversationID: ConversationSummary.ID, path: String, archive: Data) async throws -> Int {
        1
    }

    func deleteWorkspacePath(conversationID: ConversationSummary.ID, path: String) async throws {
    }

    func moveWorkspacePath(conversationID: ConversationSummary.ID, path: String, newPath: String) async throws {
    }

    func listTerminals(conversationID: ConversationSummary.ID) async throws -> [TerminalSummary] {
        []
    }

    func createTerminal(conversationID: ConversationSummary.ID, options: TerminalCreateOptions) async throws -> TerminalSummary {
        TerminalSummary(
            terminalID: "terminal_0001",
            conversationID: conversationID,
            mode: "local",
            remote: nil,
            shell: "/bin/zsh",
            cwd: "~/Projects/ClawParty",
            cols: options.cols ?? 90,
            rows: options.rows ?? 28,
            running: true,
            createdMS: 0,
            updatedMS: 0,
            nextOffset: 0
        )
    }

    func terminateTerminal(conversationID: ConversationSummary.ID, terminalID: TerminalSummary.ID) async throws -> TerminalSummary {
        TerminalSummary(
            terminalID: terminalID,
            conversationID: conversationID,
            mode: "local",
            remote: nil,
            shell: "/bin/zsh",
            cwd: "~/Projects/ClawParty",
            cols: 90,
            rows: 28,
            running: false,
            createdMS: 0,
            updatedMS: 0,
            nextOffset: 0
        )
    }

    func terminalSession(conversationID: ConversationSummary.ID, terminalID: TerminalSummary.ID, offset: UInt64) async throws -> TerminalWebSocketSession {
        TerminalWebSocketSession.mock()
    }

    func foregroundEvents(conversationID: ConversationSummary.ID) async throws -> AsyncThrowingStream<StellaRealtimeEvent, Error> {
        AsyncThrowingStream { continuation in
            continuation.yield(
                .subscriptionAck(
                    conversationID: conversationID,
                    total: 4,
                    currentMessageID: "3",
                    nextMessageID: "4",
                    reason: "subscribed"
                )
            )
        }
    }
}

struct StellaWebAPIClient: StellaAPIClient {
    let profile: ServerProfile
    var session: URLSession = StellaWebAPIClient.makeSession()
    private let tunnelManager = AppleSSHTunnelManager.shared

    private static func makeSession() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.httpMaximumConnectionsPerHost = 1
        configuration.httpShouldUsePipelining = false
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.timeoutIntervalForRequest = 30
        configuration.timeoutIntervalForResource = 300
        return URLSession(configuration: configuration)
    }

    func listModels() async throws -> [ModelSummary] {
        let payload: ModelListResponse = try await request("/api/models")
        return payload.models.map(ModelSummary.init(web:))
    }

    func listConversations() async throws -> [ConversationSummary] {
        let payload: ConversationListResponse = try await request("/api/conversations?limit=80")
        return payload.conversations.map(ConversationSummary.init(web:))
    }

    func conversationStatus(conversationID: ConversationSummary.ID) async throws -> ConversationStatusSnapshot {
        let path = "/api/conversations/\(conversationID.urlPathEncoded)/status"
        let payload: WebConversationStatusSnapshot = try await request(path)
        return ConversationStatusSnapshot(web: payload)
    }

    func conversationEvents() async throws -> AsyncThrowingStream<StellaConversationEvent, Error> {
        let url = try await websocketURL("/api/conversations/stream")
        var request = URLRequest(url: url)
        if !profile.token.isEmpty {
            request.setValue("Bearer \(profile.token)", forHTTPHeaderField: "Authorization")
        }

        return AsyncThrowingStream { continuation in
            let task = session.webSocketTask(with: request)
            let receiveTask = Task {
                do {
                    task.resume()
                    while !Task.isCancelled {
                        let message = try await task.receive()
                        guard let data = message.dataValue else {
                            continue
                        }
                        let envelope = try JSONDecoder.stella.decode(ConversationStreamEnvelope.self, from: data)
                        if let event = envelope.event {
                            continuation.yield(event)
                        }
                    }
                } catch is CancellationError {
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }

            continuation.onTermination = { _ in
                receiveTask.cancel()
                task.cancel(with: .normalClosure, reason: nil)
            }
        }
    }

    func createConversation(nickname: String?, model: String?) async throws -> ConversationSummary.ID {
        let requestBody = CreateConversationBody(
            nickname: nickname?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty,
            model: model?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
        )
        let payload: CreateConversationResponse = try await request("/api/conversations", method: "POST", body: requestBody)
        return payload.conversation_id
    }

    func renameConversation(id: ConversationSummary.ID, nickname: String) async throws -> ConversationSummary {
        let trimmed = nickname.trimmingCharacters(in: .whitespacesAndNewlines)
        let requestBody = UpdateConversationBody(nickname: trimmed)
        let path = "/api/conversations/\(id.urlPathEncoded)"
        let payload: UpdateConversationResponse = try await request(path, method: "PATCH", body: requestBody)
        return ConversationSummary(web: payload.conversation)
    }

    func deleteConversation(id: ConversationSummary.ID) async throws {
        let path = "/api/conversations/\(id.urlPathEncoded)"
        let _: DeleteConversationResponse = try await request(path, method: "DELETE")
    }

    func markConversationSeen(conversationID: ConversationSummary.ID, lastSeenMessageID: String) async throws -> ConversationSeen {
        let path = "/api/conversations/\(conversationID.urlPathEncoded)/seen"
        let body = MarkConversationSeenBody(last_seen_message_id: lastSeenMessageID)
        let payload: MarkConversationSeenResponse = try await request(path, method: "POST", body: body)
        return ConversationSeen(web: payload.seen)
    }

    func listMessagePage(conversationID: ConversationSummary.ID, offset: Int = 0, limit: Int = 80) async throws -> ChatMessagePage {
        let path = "/api/conversations/\(conversationID.urlPathEncoded)/messages?offset=\(max(0, offset))&limit=\(max(1, min(200, limit)))"
        let payload: MessageListResponse = try await request(path)
        return ChatMessagePage(
            conversationID: payload.conversation_id ?? conversationID,
            offset: payload.offset ?? offset,
            limit: payload.limit ?? limit,
            total: payload.total ?? payload.messages.count,
            messages: payload.messages.map(ChatMessage.init(web:))
        )
    }

    func messageDetail(conversationID: ConversationSummary.ID, messageID: ChatMessage.ID) async throws -> ChatMessageDetail {
        let path = "/api/conversations/\(conversationID.urlPathEncoded)/messages/\(messageID.urlPathEncoded)"
        let payload: MessageDetailResponse = try await request(path)
        let message = payload.webMessage
        return ChatMessageDetail(
            id: "\(conversationID)-\(payload.id)",
            conversationID: payload.conversation_id,
            message: ChatMessage(web: message),
            renderedText: payload.rendered_text ?? message.text ?? "",
            toolActivities: message.items?.enumerated().compactMap { index, item in
                item.toolActivity(messageID: payload.id, itemIndex: index)
            } ?? [],
            attachments: payload.attachments?.map(ChatAttachment.init(web:)) ?? [],
            attachmentCount: payload.attachments?.count ?? 0,
            attachmentErrors: payload.attachment_errors ?? []
        )
    }

    func sendMessage(_ body: String, conversationID: ConversationSummary.ID, userName: String, remoteMessageID: String, files: [OutgoingMessageFile]) async throws {
        let path = "/api/conversations/\(conversationID.urlPathEncoded)/messages"
        let requestBody = SendMessageBody(
            user_name: userName,
            text: body,
            remote_message_id: remoteMessageID,
            files: files.isEmpty ? nil : files
        )
        let _: SendMessageResponse = try await request(path, method: "POST", body: requestBody)
    }

    func listWorkspace(conversationID: ConversationSummary.ID, path: String = "", limit: Int = 500) async throws -> WorkspaceListing {
        let requestPath = "/api/conversations/\(conversationID.urlPathEncoded)/workspace?path=\(path.urlQueryEncoded)&limit=\(max(1, min(1000, limit)))"
        let payload: WebWorkspaceListing = try await request(requestPath)
        return WorkspaceListing(web: payload)
    }

    func workspaceFile(conversationID: ConversationSummary.ID, path: String, limitBytes: Int = 2_000_000, full: Bool = false) async throws -> WorkspaceFile {
        var requestPath = "/api/conversations/\(conversationID.urlPathEncoded)/workspace/file?path=\(path.urlQueryEncoded)&offset=0&limit_bytes=\(max(1, limitBytes))"
        if full {
            requestPath += "&full=true"
        }
        let payload: WebWorkspaceFile = try await request(requestPath)
        return WorkspaceFile(web: payload)
    }

    func downloadWorkspaceArchive(conversationID: ConversationSummary.ID, path: String) async throws -> Data {
        let requestPath = "/api/conversations/\(conversationID.urlPathEncoded)/workspace/download?path=\(path.urlQueryEncoded)"
        return try await requestData(requestPath)
    }

    func uploadWorkspaceArchive(conversationID: ConversationSummary.ID, path: String, archive: Data) async throws -> Int {
        let requestPath = "/api/conversations/\(conversationID.urlPathEncoded)/workspace/upload?path=\(path.urlQueryEncoded)"
        let payload: WorkspaceUploadResponse = try await requestDataResponse(requestPath, method: "POST", body: archive, contentType: "application/gzip")
        return payload.entries_extracted
    }

    func deleteWorkspacePath(conversationID: ConversationSummary.ID, path: String) async throws {
        let requestPath = "/api/conversations/\(conversationID.urlPathEncoded)/workspace?path=\(path.urlQueryEncoded)"
        let _: WorkspaceDeleteResponse = try await request(requestPath, method: "DELETE")
    }

    func moveWorkspacePath(conversationID: ConversationSummary.ID, path: String, newPath: String) async throws {
        let requestPath = "/api/conversations/\(conversationID.urlPathEncoded)/workspace"
        let body = WorkspaceMoveBody(path: path, new_path: newPath)
        let _: WorkspaceMoveResponse = try await request(requestPath, method: "PATCH", body: body)
    }

    func listTerminals(conversationID: ConversationSummary.ID) async throws -> [TerminalSummary] {
        let requestPath = "/api/conversations/\(conversationID.urlPathEncoded)/terminals"
        let payload: TerminalListResponse = try await request(requestPath)
        return payload.terminals.map(TerminalSummary.init(web:))
    }

    func createTerminal(conversationID: ConversationSummary.ID, options: TerminalCreateOptions) async throws -> TerminalSummary {
        let requestPath = "/api/conversations/\(conversationID.urlPathEncoded)/terminals"
        let payload: WebTerminalSummary = try await request(requestPath, method: "POST", body: options)
        return TerminalSummary(web: payload)
    }

    func terminateTerminal(conversationID: ConversationSummary.ID, terminalID: TerminalSummary.ID) async throws -> TerminalSummary {
        let requestPath = "/api/conversations/\(conversationID.urlPathEncoded)/terminals/\(terminalID.urlPathEncoded)"
        let payload: WebTerminalSummary = try await request(requestPath, method: "DELETE")
        return TerminalSummary(web: payload)
    }

    func terminalSession(conversationID: ConversationSummary.ID, terminalID: TerminalSummary.ID, offset: UInt64) async throws -> TerminalWebSocketSession {
        var url = try await websocketURL("/api/conversations/\(conversationID.urlPathEncoded)/terminals/\(terminalID.urlPathEncoded)/stream")
        if var components = URLComponents(url: url, resolvingAgainstBaseURL: false) {
            var queryItems = components.queryItems ?? []
            queryItems.append(URLQueryItem(name: "offset", value: "\(offset)"))
            components.queryItems = queryItems
            url = components.url ?? url
        }
        var request = URLRequest(url: url)
        if !profile.token.isEmpty {
            request.setValue("Bearer \(profile.token)", forHTTPHeaderField: "Authorization")
        }
        let task = session.webSocketTask(with: request)
        return TerminalWebSocketSession(task: task)
    }

    func foregroundEvents(conversationID: ConversationSummary.ID) async throws -> AsyncThrowingStream<StellaRealtimeEvent, Error> {
        let url = try await websocketURL("/api/conversations/\(conversationID.urlPathEncoded)/foreground/ws")
        var request = URLRequest(url: url)
        if !profile.token.isEmpty {
            request.setValue("Bearer \(profile.token)", forHTTPHeaderField: "Authorization")
        }

        return AsyncThrowingStream { continuation in
            let task = session.webSocketTask(with: request)
            let receiveTask = Task {
                do {
                    task.resume()
                    while !Task.isCancelled {
                        let message = try await task.receive()
                        guard let data = message.dataValue else {
                            continue
                        }
                        let envelope = try JSONDecoder.stella.decode(RealtimeEnvelope.self, from: data)
                        for event in envelope.events {
                            continuation.yield(event)
                        }
                    }
                } catch is CancellationError {
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }

            continuation.onTermination = { _ in
                receiveTask.cancel()
                task.cancel(with: .normalClosure, reason: nil)
            }
        }
    }

    private func request<Response: Decodable>(
        _ path: String,
        method: String = "GET",
        body: (some Encodable)? = Optional<Data>.none
    ) async throws -> Response {
        let baseURL = try await tunnelManager.resolveBaseURL(for: profile)
        let url = URL(string: path, relativeTo: baseURL)?.absoluteURL ?? baseURL

        var request = URLRequest(url: url)
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if !profile.token.isEmpty {
            request.setValue("Bearer \(profile.token)", forHTTPHeaderField: "Authorization")
        }

        if let body {
            request.httpBody = try JSONEncoder().encode(AnyEncodable(body))
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }

        let (data, response) = try await session.data(for: request)
        guard let httpResponse = response as? HTTPURLResponse else {
            throw StellaAPIError.invalidResponse
        }
        guard (200..<300).contains(httpResponse.statusCode) else {
            let payload = try? JSONDecoder().decode(APIErrorResponse.self, from: data)
            throw StellaAPIError.http(status: httpResponse.statusCode, message: payload?.error ?? HTTPURLResponse.localizedString(forStatusCode: httpResponse.statusCode))
        }

        return try JSONDecoder.stella.decode(Response.self, from: data)
    }

    private func requestData(_ path: String, method: String = "GET", body: Data? = nil, contentType: String? = nil) async throws -> Data {
        let (data, _) = try await performRequest(path, method: method, body: body, contentType: contentType, accept: "*/*")
        return data
    }

    private func requestDataResponse<Response: Decodable>(_ path: String, method: String, body: Data, contentType: String) async throws -> Response {
        let (data, _) = try await performRequest(path, method: method, body: body, contentType: contentType, accept: "application/json")
        return try JSONDecoder.stella.decode(Response.self, from: data)
    }

    private func performRequest(_ path: String, method: String, body: Data?, contentType: String?, accept: String) async throws -> (Data, HTTPURLResponse) {
        let baseURL = try await tunnelManager.resolveBaseURL(for: profile)
        let url = URL(string: path, relativeTo: baseURL)?.absoluteURL ?? baseURL

        var request = URLRequest(url: url)
        request.httpMethod = method
        request.setValue(accept, forHTTPHeaderField: "Accept")
        if !profile.token.isEmpty {
            request.setValue("Bearer \(profile.token)", forHTTPHeaderField: "Authorization")
        }
        if let body {
            request.httpBody = body
        }
        if let contentType {
            request.setValue(contentType, forHTTPHeaderField: "Content-Type")
        }

        let (data, response) = try await session.data(for: request)
        guard let httpResponse = response as? HTTPURLResponse else {
            throw StellaAPIError.invalidResponse
        }
        guard (200..<300).contains(httpResponse.statusCode) else {
            let payload = try? JSONDecoder().decode(APIErrorResponse.self, from: data)
            throw StellaAPIError.http(status: httpResponse.statusCode, message: payload?.error ?? HTTPURLResponse.localizedString(forStatusCode: httpResponse.statusCode))
        }
        return (data, httpResponse)
    }

    private func websocketURL(_ path: String) async throws -> URL {
        let baseURL = try await tunnelManager.resolveBaseURL(for: profile)
        let httpURL = URL(string: path, relativeTo: baseURL)?.absoluteURL ?? baseURL
        guard var components = URLComponents(url: httpURL, resolvingAgainstBaseURL: false) else {
            throw StellaAPIError.invalidResponse
        }
        components.scheme = components.scheme == "https" ? "wss" : "ws"
        if !profile.token.isEmpty {
            var queryItems = components.queryItems ?? []
            queryItems.append(URLQueryItem(name: "token", value: profile.token))
            components.queryItems = queryItems
        }
        guard let url = components.url else {
            throw StellaAPIError.invalidResponse
        }
        return url
    }
}

enum StellaRealtimeEvent {
    case subscriptionAck(conversationID: String, total: Int, currentMessageID: String?, nextMessageID: String?, reason: String)
    case messages(conversationID: String, messages: [ChatMessage], total: Int)
    case conversationDeleted(conversationID: String)
    case turnProgress(TurnProgressFeedback)
    case progress(String)
    case error(String)
}

enum StellaAPIError: Error, LocalizedError {
    case invalidResponse
    case http(status: Int, message: String)

    var errorDescription: String? {
        switch self {
        case .invalidResponse:
            "Invalid server response"
        case .http(let status, let message):
            "HTTP \(status): \(message)"
        }
    }
}

private struct ConversationListResponse: Decodable {
    var conversations: [WebConversationSummary]
}

private struct ModelListResponse: Decodable {
    var models: [WebModelSummary]
}

private struct WebModelSummary: Decodable {
    var alias: String
    var model_name: String
    var provider_type: String
    var capabilities: [String]?
    var token_max_context: Int?
    var max_tokens: Int?
    var effective_max_tokens: Int?
}

private struct MessageListResponse: Decodable {
    var conversation_id: String?
    var offset: Int?
    var limit: Int?
    var total: Int?
    var messages: [WebMessage]
}

private struct MessageDetailResponse: Decodable {
    var conversation_id: String
    var id: String
    var index: Int
    var rendered_text: String?
    var items: [WebMessageItem]?
    var attachments: [WebAttachment]?
    var attachment_errors: [String]?

    var webMessage: WebMessage {
        WebMessage(
            id: id,
            index: index,
            role: decodedRole,
            text: rendered_text,
            preview: rendered_text,
            items: items,
            attachments: attachments,
            user_name: nil,
            message_time: nil,
            has_token_usage: token_usage != nil,
            token_usage: token_usage
        )
    }

    var token_usage: WebTokenUsage?

    private var decodedRole: String {
        "assistant"
    }
}

private struct WebAttachment: Decodable {
    var index: Int?
    var source: String?
    var kind: String?
    var name: String?
    var path: String?
    var uri: String?
    var media_type: String?
    var width: Int?
    var height: Int?
    var size_bytes: Int?
    var url: String?
    var marker: String?
    var thumbnail: WebThumbnail?
}

private struct WebThumbnail: Decodable {
    var media_type: String?
    var data_base64: String?
    var data_url: String?
    var width: Int?
    var height: Int?
    var size_bytes: Int?
}

private struct CreateConversationBody: Encodable {
    var nickname: String?
    var model: String?
}

private struct CreateConversationResponse: Decodable {
    var conversation_id: String
}

private struct UpdateConversationBody: Encodable {
    var nickname: String
}

private struct UpdateConversationResponse: Decodable {
    var conversation: WebConversationSummary
}

private struct DeleteConversationResponse: Decodable {
    var deleted: Bool
}

private struct SendMessageBody: Encodable {
    var user_name: String
    var text: String
    var remote_message_id: String
    var files: [OutgoingMessageFile]?
}

private struct SendMessageResponse: Decodable {
    var accepted: Bool
}

private struct MarkConversationSeenBody: Encodable {
    var last_seen_message_id: String
}

private struct MarkConversationSeenResponse: Decodable {
    var conversation_id: String
    var seen: WebConversationSeen
}

private struct WebConversationSeen: Decodable {
    var last_seen_message_id: String
    var updated_at: String
}

private struct WorkspaceUploadResponse: Decodable {
    var conversation_id: String
    var path: String
    var entries_extracted: Int
}

private struct WorkspaceDeleteResponse: Decodable {
    var deleted: Bool
}

private struct WorkspaceMoveBody: Encodable {
    var path: String
    var new_path: String
}

private struct WorkspaceMoveResponse: Decodable {
    var moved: Bool
}

private struct TerminalListResponse: Decodable {
    var conversation_id: String
    var terminals: [WebTerminalSummary]
}

private struct WebTerminalSummary: Decodable {
    var terminal_id: String
    var conversation_id: String
    var mode: String
    var remote: WebWorkspaceRemote?
    var shell: String
    var cwd: String
    var cols: Int
    var rows: Int
    var running: Bool
    var created_ms: UInt64
    var updated_ms: UInt64
    var next_offset: UInt64
}

private struct TerminalStreamControl: Decodable {
    var type: String
    var terminal_id: String?
    var next_offset: UInt64?
    var running: Bool?
    var dropped_bytes: UInt64?
    var reason: String?
    var error: String?
    var message: String?
}

private struct APIErrorResponse: Decodable {
    var error: String
}

private struct WebConversationSummary: Decodable {
    var conversation_id: String
    var nickname: String?
    var platform_chat_id: String?
    var model: String?
    var model_selection_pending: Bool?
    var reasoning: String?
    var sandbox: String?
    var sandbox_source: String?
    var remote: String?
    var workspace: String?
    var processing_state: String?
    var running: Bool?
    var message_count: Int?
    var last_message_id: String?
    var last_message_time: String?
    var last_seen_message_id: String?
    var last_seen_at: String?
}

private struct WebConversationStatusSnapshot: Decodable {
    var conversation_id: String
    var model: String
    var reasoning: String
    var sandbox: String
    var sandbox_source: String
    var remote: String
    var workspace: String
    var running_background: Int
    var total_background: Int
    var running_subagents: Int
    var total_subagents: Int
    var usage: WebConversationUsageSummary
}

private struct WebConversationUsageSummary: Decodable {
    var foreground: WebConversationUsageTotals
    var background: WebConversationUsageTotals
    var subagents: WebConversationUsageTotals
    var media_tools: WebConversationUsageTotals
}

private struct WebConversationUsageTotals: Decodable {
    var cache_read: Int?
    var cache_write: Int?
    var uncache_input: Int?
    var input: Int?
    var output: Int?
    var cost: WebConversationUsageCost?
}

private struct WebConversationUsageCost: Decodable {
    var cache_read: Double?
    var cache_write: Double?
    var uncache_input: Double?
    var input: Double?
    var output: Double?
}

private struct WebMessage: Decodable {
    var id: String
    var index: Int?
    var role: String
    var text: String?
    var preview: String?
    var items: [WebMessageItem]?
    var attachments: [WebAttachment]?
    var user_name: String?
    var message_time: String?
    var has_token_usage: Bool?
    var token_usage: WebTokenUsage?
}

private struct WebTokenUsage: Decodable {
    var cache_read: Int?
    var cache_write: Int?
    var uncache_input: Int?
    var input: Int?
    var output: Int?
    var total: Int?
    var cost_usd: WebTokenUsageCost?
    var cost: WebTokenUsageCost?
}

private struct WebTokenUsageCost: Decodable {
    var cache_read: Double?
    var cache_write: Double?
    var uncache_input: Double?
    var input: Double?
    var output: Double?
    var total: Double?
}

private struct WebMessageItem: Decodable {
    var type: String
    var text: String?
    var tool_name: String?
    var arguments: JSONValue?
    var context: String?
    var context_with_attachment_markers: String?
    var call_id: String?
    var tool_call_id: String?
    var item_id: String?
    var id: String?
}

private struct WebWorkspaceListing: Decodable {
    var conversation_id: String
    var mode: String
    var remote: WebWorkspaceRemote?
    var workspace_root: String
    var path: String
    var parent: String?
    var total_entries: Int
    var returned_entries: Int
    var truncated: Bool
    var entries: [WebWorkspaceEntry]
}

private struct WebWorkspaceRemote: Decodable {
    var host: String
    var cwd: String?
}

private struct WebWorkspaceEntry: Decodable {
    var name: String
    var path: String
    var kind: String
    var size_bytes: Int64?
    var modified_ms: UInt64?
    var hidden: Bool
    var readonly: Bool
}

private struct WebWorkspaceFile: Decodable {
    var conversation_id: String
    var mode: String
    var remote: WebWorkspaceRemote?
    var workspace_root: String
    var path: String
    var name: String
    var size_bytes: Int64
    var modified_ms: UInt64?
    var offset: Int64
    var returned_bytes: Int
    var truncated: Bool
    var encoding: String
    var data: String
}

private struct RealtimeEnvelope: Decodable {
    var type: String?
    var reason: String?
    var conversation_id: String?
    var total: Int?
    var current_message_id: String?
    var next_message_id: String?
    var messages: [WebMessage]?
    var message: String?
    var error: String?
    var phase: String?
    var final_state: String?
    var turn_id: String?
    var model: String?
    var activity: String?
    var hint: String?
    var plan: WebTurnProgressPlan?
    var progress: WebTurnProgress?
    var turn_progress: WebTurnProgress?
    var important: Bool?

    var events: [StellaRealtimeEvent] {
        switch type {
        case "subscription_ack":
            var events: [StellaRealtimeEvent] = [
                .subscriptionAck(
                    conversationID: conversation_id ?? "",
                    total: total ?? 0,
                    currentMessageID: current_message_id,
                    nextMessageID: next_message_id,
                    reason: reason ?? "subscribed"
                )
            ]
            if let progress = turn_progress?.feedback {
                events.append(.turnProgress(progress))
            }
            return events
        case "messages":
            return [
                .messages(
                    conversationID: conversation_id ?? "",
                    messages: messages?.map(ChatMessage.init(web:)) ?? [],
                    total: total ?? 0
                )
            ]
        case "conversation_deleted":
            return [.conversationDeleted(conversationID: conversation_id ?? "")]
        case "turn_progress":
            return [.turnProgress(turnProgressFeedback)]
        case "error":
            return [.error(message ?? error ?? "Realtime error")]
        default:
            if type == nil,
               phase != nil || final_state != nil || progress != nil {
                return [.turnProgress(turnProgressFeedback)]
            }
            if let phase {
                return [.progress(final_state.map { "\(phase): \($0)" } ?? phase)]
            }
            return []
        }
    }

    private var turnProgressFeedback: TurnProgressFeedback {
        let progress = progress
        let id = turn_id ?? progress?.turn_id ?? "current"
        let phase = phase ?? progress?.phase ?? "running"
        return TurnProgressFeedback(
            id: id,
            phase: phase,
            model: model ?? progress?.model ?? "",
            activity: activity ?? progress?.activity ?? message ?? "",
            hint: hint ?? progress?.hint,
            error: error ?? progress?.error,
            finalState: final_state ?? progress?.final_state,
            plan: (plan ?? progress?.plan)?.feedback
        )
    }
}

private struct WebTurnProgress: Decodable {
    var type: String?
    var turn_id: String?
    var phase: String?
    var model: String?
    var activity: String?
    var hint: String?
    var plan: WebTurnProgressPlan?
    var error: String?
    var final_state: String?

    var feedback: TurnProgressFeedback {
        TurnProgressFeedback(
            id: turn_id ?? "current",
            phase: phase ?? "running",
            model: model ?? "",
            activity: activity ?? "",
            hint: hint,
            error: error,
            finalState: final_state,
            plan: plan?.feedback
        )
    }
}

private struct WebTurnProgressPlan: Decodable {
    var explanation: String?
    var items: [WebTurnProgressPlanItem]?

    var feedback: TurnProgressPlan {
        TurnProgressPlan(
            explanation: explanation,
            items: (items ?? []).map { TurnProgressPlanItem(step: $0.step, status: $0.status) }
        )
    }
}

private struct WebTurnProgressPlanItem: Decodable {
    var step: String
    var status: String
}

final class TerminalWebSocketSession {
    let events: AsyncThrowingStream<TerminalStreamEvent, Error>

    private let task: URLSessionWebSocketTask?

    init(task: URLSessionWebSocketTask) {
        self.task = task
        let taskRef = task
        self.events = AsyncThrowingStream { continuation in
            let receiveTask = Task {
                do {
                    taskRef.resume()
                    while !Task.isCancelled {
                        let message = try await taskRef.receive()
                        switch message {
                        case .data(let data):
                            continuation.yield(.output(data))
                        case .string(let raw):
                            guard let data = raw.data(using: .utf8) else {
                                continue
                            }
                            let control = try JSONDecoder.stella.decode(TerminalStreamControl.self, from: data)
                            switch control.type {
                            case "attached":
                                continuation.yield(.attached(nextOffset: control.next_offset ?? 0, running: control.running ?? true))
                            case "dropped":
                                continuation.yield(.dropped(control.dropped_bytes ?? 0))
                            case "exit":
                                continuation.yield(.exit)
                            case "detached":
                                continuation.yield(.detached(control.reason ?? "detached"))
                            case "error":
                                continuation.yield(.error(control.message ?? control.error ?? "terminal error"))
                            case "pong":
                                break
                            default:
                                break
                            }
                        @unknown default:
                            break
                        }
                    }
                } catch is CancellationError {
                    continuation.finish()
                } catch {
                    continuation.finish(throwing: error)
                }
            }
            continuation.onTermination = { _ in
                receiveTask.cancel()
                taskRef.cancel(with: .normalClosure, reason: nil)
            }
        }
    }

    private init(events: AsyncThrowingStream<TerminalStreamEvent, Error>) {
        self.task = nil
        self.events = events
    }

    func sendInput(_ text: String) {
        guard let data = text.data(using: .utf8) else {
            return
        }
        send(data)
    }

    func send(_ data: Data) {
        task?.send(.data(data)) { _ in }
    }

    func resize(cols: Int, rows: Int) {
        let payload: [String: Any] = [
            "type": "resize",
            "cols": cols,
            "rows": rows
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload),
              let text = String(data: data, encoding: .utf8)
        else {
            return
        }
        task?.send(.string(text)) { _ in }
    }

    func close() {
        task?.cancel(with: .normalClosure, reason: nil)
    }

    static func mock() -> TerminalWebSocketSession {
        TerminalWebSocketSession(
            events: AsyncThrowingStream { continuation in
                continuation.yield(.attached(nextOffset: 0, running: true))
                continuation.yield(.output(Data("StellaCodeX mock terminal\n$ ".utf8)))
            }
        )
    }
}

enum StellaConversationEvent {
    case snapshot([ConversationSummary])
    case upsert(ConversationSummary)
    case deleted(conversationID: String)
    case processing(conversationID: String, status: ConversationStatus, running: Bool)
    case turnCompleted(
        conversationID: String,
        conversation: ConversationSummary?,
        messageCount: Int?,
        lastMessageID: String?,
        lastMessageTime: Date?,
        unread: Bool?
    )
    case seen(conversationID: String, seen: ConversationSeen)
    case error(String)
}

private struct ConversationStreamEnvelope: Decodable {
    var type: String?
    var conversation_id: String?
    var conversations: [WebConversationSummary]?
    var conversation: WebConversationSummary?
    var processing_state: String?
    var running: Bool?
    var message_count: Int?
    var last_message_id: String?
    var last_message_time: String?
    var unread: Bool?
    var seen: WebConversationSeen?
    var message: String?
    var error: String?

    var event: StellaConversationEvent? {
        switch type {
        case "conversation_snapshot":
            return .snapshot(conversations?.map(ConversationSummary.init(web:)) ?? [])
        case "conversation_upserted":
            guard let conversation else {
                return nil
            }
            return .upsert(ConversationSummary(web: conversation))
        case "conversation_deleted":
            guard let conversation_id else {
                return nil
            }
            return .deleted(conversationID: conversation_id)
        case "conversation_processing":
            guard let conversation_id else {
                return nil
            }
            return .processing(
                conversationID: conversation_id,
                status: Self.status(processingState: processing_state, running: running),
                running: running ?? false
            )
        case "conversation_turn_completed":
            guard let conversation_id else {
                return nil
            }
            return .turnCompleted(
                conversationID: conversation_id,
                conversation: conversation.map(ConversationSummary.init(web:)),
                messageCount: message_count,
                lastMessageID: last_message_id,
                lastMessageTime: last_message_time.flatMap(DateFormatter.stellaISO.date(from:)),
                unread: unread
            )
        case "conversation_seen":
            guard let conversation_id, let seen else {
                return nil
            }
            return .seen(conversationID: conversation_id, seen: ConversationSeen(web: seen))
        case "error":
            return .error(message ?? error ?? "Conversation stream error")
        default:
            return nil
        }
    }

    private static func status(processingState: String?, running: Bool?) -> ConversationStatus {
        if running == true || processingState == "running" {
            return .running
        }
        if processingState == "failed" {
            return .failed
        }
        return .idle
    }
}

private enum JSONValue: Decodable {
    case string(String)
    case number(Double)
    case bool(Bool)
    case object([String: JSONValue])
    case array([JSONValue])
    case null

    init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
        } else {
            self = .object(try container.decode([String: JSONValue].self))
        }
    }

    var displayString: String {
        switch self {
        case .string(let value):
            return value
        case .number(let value):
            return value.rounded() == value ? String(Int(value)) : String(value)
        case .bool(let value):
            return String(value)
        case .object(let value):
            let pairs = value.map { "\"\($0)\": \($1.displayString)" }.sorted()
            return "{\(pairs.joined(separator: ", "))}"
        case .array(let value):
            return "[\(value.map(\.displayString).joined(separator: ", "))]"
        case .null:
            return "null"
        }
    }
}

extension ConversationSummary {
    nonisolated fileprivate init(web: WebConversationSummary) {
        let title = [web.nickname, web.platform_chat_id, web.conversation_id]
            .compactMap { $0?.trimmingCharacters(in: .whitespacesAndNewlines) }
            .first { !$0.isEmpty } ?? web.conversation_id
        let lastTime = web.last_message_time.flatMap(DateFormatter.stellaISO.date(from:)) ?? .distantPast
        let status: ConversationStatus
        if web.running == true || web.processing_state == "running" {
            status = .running
        } else if web.processing_state == "failed" {
            status = .failed
        } else {
            status = .idle
        }

        self.init(
            id: web.conversation_id,
            title: title,
            workspacePath: web.workspace ?? "",
            lastMessagePreview: web.model_selection_pending == true ? "Model selection pending" : (web.model ?? ""),
            status: status,
            updatedAt: lastTime,
            model: web.model ?? "",
            modelSelectionPending: web.model_selection_pending == true || (web.model ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
            reasoning: web.reasoning ?? "",
            sandbox: web.sandbox ?? "",
            sandboxSource: web.sandbox_source,
            remote: web.remote ?? "",
            messageCount: web.message_count ?? 0,
            lastMessageID: web.last_message_id,
            lastSeenMessageID: web.last_seen_message_id,
            lastSeenAt: web.last_seen_at.flatMap(DateFormatter.stellaISO.date(from:)),
            isUnread: Self.isUnread(lastMessageID: web.last_message_id, lastSeenMessageID: web.last_seen_message_id)
        )
    }

    private static func isUnread(lastMessageID: String?, lastSeenMessageID: String?) -> Bool {
        guard let last = Int(lastMessageID ?? "") else {
            return false
        }
        return last > (Int(lastSeenMessageID ?? "") ?? -1)
    }
}

extension ConversationSeen {
    nonisolated fileprivate init(web: WebConversationSeen) {
        self.init(
            lastSeenMessageID: web.last_seen_message_id,
            updatedAt: DateFormatter.stellaISO.date(from: web.updated_at) ?? Date()
        )
    }
}

extension ConversationStatusSnapshot {
    nonisolated fileprivate init(web: WebConversationStatusSnapshot) {
        self.init(
            conversationID: web.conversation_id,
            model: web.model,
            reasoning: web.reasoning,
            sandbox: web.sandbox,
            sandboxSource: web.sandbox_source,
            remote: web.remote,
            workspace: web.workspace,
            runningBackground: web.running_background,
            totalBackground: web.total_background,
            runningSubagents: web.running_subagents,
            totalSubagents: web.total_subagents,
            usage: ConversationUsageSummary(web: web.usage)
        )
    }
}

extension ConversationUsageSummary {
    nonisolated fileprivate init(web: WebConversationUsageSummary) {
        self.init(
            foreground: ConversationUsageTotals(web: web.foreground),
            background: ConversationUsageTotals(web: web.background),
            subagents: ConversationUsageTotals(web: web.subagents),
            mediaTools: ConversationUsageTotals(web: web.media_tools)
        )
    }
}

extension ConversationUsageTotals {
    nonisolated fileprivate init(web: WebConversationUsageTotals) {
        let cost = web.cost
        self.init(
            cacheRead: web.cache_read ?? 0,
            cacheWrite: web.cache_write ?? 0,
            input: web.input ?? web.uncache_input ?? 0,
            output: web.output ?? 0,
            cost: ConversationUsageCost(
                cacheRead: cost?.cache_read ?? 0,
                cacheWrite: cost?.cache_write ?? 0,
                input: cost?.input ?? cost?.uncache_input ?? 0,
                output: cost?.output ?? 0
            )
        )
    }
}

extension ChatMessage {
    nonisolated fileprivate init(web: WebMessage) {
        let text = web.text?.trimmingCharacters(in: .whitespacesAndNewlines)
        let toolActivities = web.items?.enumerated().compactMap { index, item -> ToolActivity? in
            item.toolActivity(messageID: web.id, itemIndex: index)
        } ?? []
        let itemText = web.items?
            .compactMap { item -> String? in
                switch item.type {
                case "text":
                    return item.text
                case "tool_call":
                    return nil
                case "tool_result":
                    return nil
                default:
                    return nil
                }
            }
            .joined(separator: "\n\n")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let body = [text, itemText, web.preview]
            .compactMap { $0 }
            .first { !$0.isEmpty } ?? ""

        self.init(
            id: web.id,
            index: web.index ?? (Int(web.id) ?? 0),
            role: ChatRole(webRole: web.role),
            body: body,
            toolActivities: toolActivities,
            attachments: web.attachments?.map(ChatAttachment.init(web:)) ?? [],
            tokenUsage: web.token_usage.map(TokenUsage.init(web:)),
            timestamp: web.message_time.flatMap(DateFormatter.stellaISO.date(from:)) ?? Date(),
            userName: web.user_name,
            isOptimistic: false,
            pending: false,
            error: nil
        )
    }
}

extension TokenUsage {
    nonisolated fileprivate init(web: WebTokenUsage) {
        let cacheRead = web.cache_read ?? 0
        let cacheWrite = web.cache_write ?? 0
        let input = web.input ?? web.uncache_input ?? 0
        let output = web.output ?? 0
        let total = web.total ?? cacheRead + cacheWrite + input + output
        let cost = web.cost_usd ?? web.cost
        let costTotal = cost?.total
            ?? ((cost?.cache_read ?? 0)
                + (cost?.cache_write ?? 0)
                + (cost?.uncache_input ?? cost?.input ?? 0)
                + (cost?.output ?? 0))
        self.init(
            cacheRead: cacheRead,
            cacheWrite: cacheWrite,
            input: input,
            output: output,
            total: total,
            costUSD: costTotal > 0 ? costTotal : nil
        )
    }
}

extension WebMessageItem {
    nonisolated fileprivate func toolActivity(messageID: String, itemIndex: Int) -> ToolActivity? {
        let kind: ToolActivityKind
        switch type {
        case "tool_call":
            kind = .call
        case "tool_result":
            kind = .result
        default:
            return nil
        }

        let toolName = (tool_name ?? "tool").trimmingCharacters(in: .whitespacesAndNewlines)
        let detail = [
            text,
            arguments?.displayString,
            context_with_attachment_markers,
            context
        ]
            .compactMap { $0?.trimmingCharacters(in: .whitespacesAndNewlines) }
            .first { !$0.isEmpty } ?? ""
        let summary: String
        if detail.isEmpty {
            summary = kind == .call ? "Queued tool call" : "Tool result received"
        } else {
            summary = detail.replacingOccurrences(of: "\n", with: " ")
        }

        return ToolActivity(
            id: item_id ?? id ?? call_id ?? tool_call_id ?? "\(messageID)-tool-\(itemIndex)",
            kind: kind,
            name: toolName.isEmpty ? "tool" : toolName,
            summary: String(summary.prefix(120)),
            detail: detail
        )
    }
}

extension ChatRole {
    nonisolated fileprivate init(webRole: String) {
        switch webRole.lowercased() {
        case "user":
            self = .user
        case "assistant":
            self = .assistant
        case "tool":
            self = .tool
        default:
            self = .system
        }
    }
}

extension ModelSummary {
    nonisolated fileprivate init(web: WebModelSummary) {
        self.init(
            alias: web.alias,
            modelName: web.model_name,
            providerType: web.provider_type,
            capabilities: web.capabilities ?? [],
            tokenMaxContext: web.token_max_context ?? 0,
            maxTokens: web.max_tokens ?? 0,
            effectiveMaxTokens: web.effective_max_tokens ?? 0
        )
    }
}

extension WorkspaceListing {
    nonisolated fileprivate init(web: WebWorkspaceListing) {
        self.init(
            conversationID: web.conversation_id,
            mode: web.mode,
            remote: web.remote.map(WorkspaceRemote.init(web:)),
            workspaceRoot: web.workspace_root,
            path: web.path,
            parent: web.parent,
            totalEntries: web.total_entries,
            returnedEntries: web.returned_entries,
            truncated: web.truncated,
            entries: web.entries.map(WorkspaceEntry.init(web:))
        )
    }
}

extension WorkspaceRemote {
    nonisolated fileprivate init(web: WebWorkspaceRemote) {
        self.init(host: web.host, cwd: web.cwd)
    }
}

extension WorkspaceEntry {
    nonisolated fileprivate init(web: WebWorkspaceEntry) {
        self.init(
            name: web.name,
            path: web.path,
            kind: web.kind,
            sizeBytes: web.size_bytes,
            modifiedMS: web.modified_ms,
            hidden: web.hidden,
            readonly: web.readonly
        )
    }
}

extension WorkspaceFile {
    nonisolated fileprivate init(web: WebWorkspaceFile) {
        self.init(
            conversationID: web.conversation_id,
            mode: web.mode,
            remote: web.remote.map(WorkspaceRemote.init(web:)),
            workspaceRoot: web.workspace_root,
            path: web.path,
            name: web.name,
            sizeBytes: web.size_bytes,
            modifiedMS: web.modified_ms,
            offset: web.offset,
            returnedBytes: web.returned_bytes,
            truncated: web.truncated,
            encoding: web.encoding,
            data: web.data
        )
    }
}

extension TerminalSummary {
    nonisolated fileprivate init(web: WebTerminalSummary) {
        self.init(
            terminalID: web.terminal_id,
            conversationID: web.conversation_id,
            mode: web.mode,
            remote: web.remote.map(WorkspaceRemote.init(web:)),
            shell: web.shell,
            cwd: web.cwd,
            cols: web.cols,
            rows: web.rows,
            running: web.running,
            createdMS: web.created_ms,
            updatedMS: web.updated_ms,
            nextOffset: web.next_offset
        )
    }
}

extension ChatAttachment {
    nonisolated fileprivate init(web: WebAttachment) {
        let index = web.index ?? 0
        self.init(
            id: "\(index)-\(web.path ?? web.uri ?? web.name ?? UUID().uuidString)",
            index: index,
            source: web.source ?? "",
            kind: web.kind ?? "",
            name: web.name ?? web.path ?? "attachment",
            path: web.path ?? "",
            uri: web.uri ?? "",
            mediaType: web.media_type,
            width: web.width,
            height: web.height,
            sizeBytes: web.size_bytes,
            url: web.url ?? "",
            marker: web.marker,
            thumbnailDataURL: web.thumbnail?.data_url
        )
    }
}

private struct AnyEncodable: Encodable {
    private let encodeClosure: (Encoder) throws -> Void

    init(_ value: some Encodable) {
        self.encodeClosure = value.encode
    }

    func encode(to encoder: Encoder) throws {
        try encodeClosure(encoder)
    }
}

private extension JSONDecoder {
    static var stella: JSONDecoder {
        JSONDecoder()
    }
}

private extension DateFormatter {
    static let stellaISO: ISO8601DateFormatter = {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter
    }()
}

private extension String {
    var urlPathEncoded: String {
        addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? self
    }

    var urlQueryEncoded: String {
        var allowed = CharacterSet.urlQueryAllowed
        allowed.remove(charactersIn: "&+=?")
        return addingPercentEncoding(withAllowedCharacters: allowed) ?? self
    }

    var nilIfEmpty: String? {
        isEmpty ? nil : self
    }
}

private extension URLSessionWebSocketTask.Message {
    var dataValue: Data? {
        switch self {
        case .data(let data):
            data
        case .string(let string):
            string.data(using: .utf8)
        @unknown default:
            nil
        }
    }
}
