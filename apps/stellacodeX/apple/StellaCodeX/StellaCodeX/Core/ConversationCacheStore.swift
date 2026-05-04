import Foundation

enum ConversationCacheStore {
    private static let storagePrefix = "StellaCodeX.cachedConversations.v1"

    static func load(profile: ServerProfile) -> [ConversationSummary] {
        guard let data = defaults.data(forKey: storageKey(profile: profile)),
              let cached = try? JSONDecoder().decode([ConversationSummary].self, from: data)
        else {
            return []
        }
        return cached
    }

    static func save(_ conversations: [ConversationSummary], profile: ServerProfile) {
        let trimmed = Array(conversations.prefix(200))
        guard let data = try? JSONEncoder().encode(trimmed) else {
            return
        }
        defaults.set(data, forKey: storageKey(profile: profile))
    }

    private static var defaults: UserDefaults {
        UserDefaults.standard
    }

    private static func storageKey(profile: ServerProfile) -> String {
        let scope = [
            profile.connectionMode.rawValue,
            profile.targetURL.absoluteString,
            profile.username
        ].joined(separator: "|")
        return "\(storagePrefix).\(scope)"
    }
}
