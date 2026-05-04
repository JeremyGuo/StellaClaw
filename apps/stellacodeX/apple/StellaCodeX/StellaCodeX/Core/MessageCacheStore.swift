import Foundation

struct MessageCacheSnapshot: Hashable {
    var conversationID: ConversationSummary.ID
    var total: Int
    var messages: [ChatMessage]
    var accessedAt: Date
    var updatedAt: Date

    var hasOlderMessages: Bool {
        messages.map(\.index).min().map { $0 > 0 } ?? false
    }

    func latestMessages(limit: Int) -> [ChatMessage] {
        let sorted = messages.sorted(by: messageSort)
        guard sorted.count > limit else {
            return sorted
        }
        return Array(sorted.suffix(limit))
    }

    func page(offset: Int, limit: Int) -> [ChatMessage] {
        let end = offset + limit
        return messages
            .filter { $0.index >= offset && $0.index < end }
            .sorted(by: messageSort)
    }
}

enum MessageCacheStore {
    private static let version = 1
    private static let maxMessagesPerConversation = 2_000
    private static let expirationInterval: TimeInterval = 30 * 24 * 60 * 60

    static func load(conversationID: ConversationSummary.ID, profile: ServerProfile) -> MessageCacheSnapshot? {
        guard var record = readRecord(conversationID: conversationID, profile: profile) else {
            return nil
        }
        record.accessedAt = Date()
        writeRecord(record, profile: profile)
        return record.snapshot
    }

    static func loadPage(conversationID: ConversationSummary.ID, profile: ServerProfile, offset: Int, limit: Int) -> [ChatMessage] {
        guard let snapshot = load(conversationID: conversationID, profile: profile) else {
            return []
        }
        let page = snapshot.page(offset: offset, limit: limit)
        guard page.count == limit else {
            return []
        }
        return page
    }

    static func mergeAndSave(conversationID: ConversationSummary.ID, profile: ServerProfile, messages newMessages: [ChatMessage], total: Int) {
        let cleanMessages = newMessages.filter(isCacheable)
        guard !cleanMessages.isEmpty || total == 0 else {
            return
        }

        let existing = readRecord(conversationID: conversationID, profile: profile)
        var byID = Dictionary(uniqueKeysWithValues: (existing?.messages ?? []).map { ($0.id, $0) })
        for message in cleanMessages {
            byID[message.id] = message
        }

        let sorted = byID.values.sorted(by: messageSort)
        let capped = sorted.count > maxMessagesPerConversation ? Array(sorted.suffix(maxMessagesPerConversation)) : sorted
        let record = MessageCacheRecord(
            version: version,
            conversationID: conversationID,
            total: max(total, existing?.total ?? 0, capped.count),
            messages: capped,
            accessedAt: Date(),
            updatedAt: Date()
        )
        writeRecord(record, profile: profile)
    }

    static func remove(conversationID: ConversationSummary.ID, profile: ServerProfile) {
        try? FileManager.default.removeItem(at: fileURL(conversationID: conversationID, profile: profile))
    }

    @discardableResult
    static func clearAll() -> Int {
        let root = rootDirectory
        let removed = allCacheFileURLs().count
        let urls = (try? FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)) ?? []
        for url in urls {
            try? FileManager.default.removeItem(at: url)
        }
        return removed
    }

    @discardableResult
    static func removeExpired(now: Date = Date()) -> Int {
        var removed = 0
        for url in allCacheFileURLs() {
            guard let data = try? Data(contentsOf: url),
                  let record = try? decoder.decode(MessageCacheRecord.self, from: data),
                  now.timeIntervalSince(record.accessedAt) > expirationInterval
            else {
                continue
            }
            try? FileManager.default.removeItem(at: url)
            removed += 1
        }
        return removed
    }

    static func stats() -> MessageCacheStats {
        let urls = allCacheFileURLs()
        var bytes: Int64 = 0
        for url in urls {
            let values = try? url.resourceValues(forKeys: [.fileSizeKey])
            bytes += Int64(values?.fileSize ?? 0)
        }
        return MessageCacheStats(conversationCount: urls.count, bytes: bytes)
    }

    private static func readRecord(conversationID: ConversationSummary.ID, profile: ServerProfile) -> MessageCacheRecord? {
        let url = fileURL(conversationID: conversationID, profile: profile)
        guard let data = try? Data(contentsOf: url),
              let record = try? decoder.decode(MessageCacheRecord.self, from: data),
              record.version == version
        else {
            return nil
        }
        return record
    }

    private static func writeRecord(_ record: MessageCacheRecord, profile: ServerProfile) {
        let url = fileURL(conversationID: record.conversationID, profile: profile)
        do {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            let data = try encoder.encode(record)
            try data.write(to: url, options: [.atomic])
        } catch {
            // Message cache is a performance optimization; ignore persistence failures.
        }
    }

    private static func isCacheable(_ message: ChatMessage) -> Bool {
        message.index >= 0
            && !message.pending
            && message.error == nil
            && !message.isOptimistic
            && !message.id.hasPrefix("apple-")
            && !message.id.hasPrefix("local-")
            && !message.id.hasPrefix("system-error-")
            && !message.id.hasPrefix("load-messages-error-")
    }

    private static func fileURL(conversationID: ConversationSummary.ID, profile: ServerProfile) -> URL {
        rootDirectory
            .appendingPathComponent(profileScope(profile), isDirectory: true)
            .appendingPathComponent("\(conversationID.fileNameSafe).json")
    }

    private static var rootDirectory: URL {
        let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
            ?? FileManager.default.temporaryDirectory
        return base
            .appendingPathComponent("StellaCodeX", isDirectory: true)
            .appendingPathComponent("MessageCache", isDirectory: true)
    }

    private static func profileScope(_ profile: ServerProfile) -> String {
        [
            profile.connectionMode.rawValue,
            profile.targetURL.absoluteString,
            profile.username
        ]
        .joined(separator: "|")
        .fileNameSafe
    }

    private static func allCacheFileURLs() -> [URL] {
        guard let enumerator = FileManager.default.enumerator(
            at: rootDirectory,
            includingPropertiesForKeys: [.fileSizeKey],
            options: [.skipsHiddenFiles]
        ) else {
            return []
        }
        return enumerator.compactMap { item in
            guard let url = item as? URL, url.pathExtension == "json" else {
                return nil
            }
            return url
        }
    }

    private static var encoder: JSONEncoder {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        return encoder
    }

    private static var decoder: JSONDecoder {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return decoder
    }
}

struct MessageCacheStats: Hashable {
    var conversationCount: Int
    var bytes: Int64
}

private struct MessageCacheRecord: Codable {
    var version: Int
    var conversationID: ConversationSummary.ID
    var total: Int
    var messages: [ChatMessage]
    var accessedAt: Date
    var updatedAt: Date

    var snapshot: MessageCacheSnapshot {
        MessageCacheSnapshot(
            conversationID: conversationID,
            total: total,
            messages: messages.sorted(by: messageSort),
            accessedAt: accessedAt,
            updatedAt: updatedAt
        )
    }
}

private func messageSort(_ left: ChatMessage, _ right: ChatMessage) -> Bool {
    if left.index == right.index {
        return left.timestamp < right.timestamp
    }
    return left.index < right.index
}

private extension String {
    var fileNameSafe: String {
        let unsafe = CharacterSet(charactersIn: "/\\:?%*|\"<>")
        return components(separatedBy: unsafe).joined(separator: "_")
    }
}
