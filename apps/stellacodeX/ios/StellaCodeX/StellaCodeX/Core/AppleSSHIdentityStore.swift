import Foundation
import Security

#if canImport(Crypto)
import Crypto
#endif

struct AppleSSHIdentity {
    #if canImport(Crypto)
    let privateKey: Curve25519.Signing.PrivateKey
    #endif
    let publicKey: String
}

enum AppleSSHIdentityStore {
    private static let service = "com.jeremyguo.StellaCodeX.ssh-identity"
    private static let account = "default-ed25519"

    static func loadOrCreate() throws -> AppleSSHIdentity {
        #if canImport(Crypto)
        if let data = try loadPrivateKeyData() {
            let privateKey = try Curve25519.Signing.PrivateKey(rawRepresentation: data)
            return AppleSSHIdentity(
                privateKey: privateKey,
                publicKey: openSSHPublicKey(for: privateKey.publicKey)
            )
        }

        let privateKey = Curve25519.Signing.PrivateKey()
        try savePrivateKeyData(privateKey.rawRepresentation)
        return AppleSSHIdentity(
            privateKey: privateKey,
            publicKey: openSSHPublicKey(for: privateKey.publicKey)
        )
        #else
        throw SSHIdentityError.cryptoUnavailable
        #endif
    }

    static func publicKey() throws -> String {
        try loadOrCreate().publicKey
    }

    #if canImport(Crypto)
    private static func openSSHPublicKey(for publicKey: Curve25519.Signing.PublicKey) -> String {
        var payload = Data()
        appendSSHString("ssh-ed25519", to: &payload)
        appendSSHData(publicKey.rawRepresentation, to: &payload)
        return "ssh-ed25519 \(payload.base64EncodedString()) stellacodex"
    }
    #endif

    private static func loadPrivateKeyData() throws -> Data? {
        var result: CFTypeRef?
        let status = SecItemCopyMatching(loadQuery as CFDictionary, &result)

        if status == errSecItemNotFound {
            return nil
        }
        guard status == errSecSuccess else {
            throw SSHIdentityError.keychain(status)
        }
        guard let data = result as? Data else {
            throw SSHIdentityError.invalidStoredKey
        }
        return data
    }

    private static func savePrivateKeyData(_ data: Data) throws {
        SecItemDelete(baseQuery as CFDictionary)

        var query = baseQuery
        query[kSecValueData as String] = data
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly

        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw SSHIdentityError.keychain(status)
        }
    }

    private static var baseQuery: [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account
        ]
    }

    private static var loadQuery: [String: Any] {
        var query = baseQuery
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        return query
    }

    private static func appendSSHString(_ string: String, to data: inout Data) {
        appendSSHData(Data(string.utf8), to: &data)
    }

    private static func appendSSHData(_ value: Data, to data: inout Data) {
        var length = UInt32(value.count).bigEndian
        withUnsafeBytes(of: &length) { bytes in
            data.append(contentsOf: bytes)
        }
        data.append(value)
    }
}

enum SSHIdentityError: Error, LocalizedError {
    case cryptoUnavailable
    case invalidStoredKey
    case keychain(OSStatus)

    var errorDescription: String? {
        switch self {
        case .cryptoUnavailable:
            "Swift Crypto is unavailable in this build."
        case .invalidStoredKey:
            "Stored SSH identity is invalid."
        case .keychain(let status):
            "Keychain error \(status)."
        }
    }
}
