import Foundation

enum ServerProfileStore {
    private static let storageKey = "StellaCodeX.serverProfile.v1"

    static func load() -> ServerProfile? {
        guard let data = UserDefaults.standard.data(forKey: storageKey) else {
            return nil
        }
        return try? JSONDecoder().decode(ServerProfile.self, from: data)
    }

    static func save(_ profile: ServerProfile) {
        guard let data = try? JSONEncoder().encode(profile) else {
            return
        }
        UserDefaults.standard.set(data, forKey: storageKey)
    }
}
