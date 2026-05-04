import Foundation

enum ConversationPinStore {
    private static let storagePrefix = "StellaCodeX.pinnedConversations.v1"

    static func load(profile: ServerProfile) -> Set<ConversationSummary.ID> {
        guard let data = UserDefaults.standard.data(forKey: storageKey(profile: profile)),
              let values = try? JSONDecoder().decode([ConversationSummary.ID].self, from: data)
        else {
            return []
        }
        return Set(values)
    }

    static func save(_ pinnedIDs: Set<ConversationSummary.ID>, profile: ServerProfile) {
        guard let data = try? JSONEncoder().encode(Array(pinnedIDs).sorted()) else {
            return
        }
        UserDefaults.standard.set(data, forKey: storageKey(profile: profile))
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
