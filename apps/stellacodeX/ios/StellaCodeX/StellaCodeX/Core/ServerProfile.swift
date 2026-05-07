import Foundation

enum ServerConnectionMode: String, CaseIterable, Identifiable, Hashable, Codable {
    case direct
    case sshProxy = "ssh_proxy"

    var id: String {
        rawValue
    }

    var displayName: String {
        switch self {
        case .direct:
            "Direct"
        case .sshProxy:
            "SSH Proxy"
        }
    }
}

struct SSHProxyConfig: Hashable, Codable {
    var sshHost: String
    var sshPort: Int?
    var sshUser: String
    var targetURL: URL

    init(
        sshHost: String,
        sshPort: Int? = nil,
        sshUser: String = "",
        targetURL: URL
    ) {
        self.sshHost = sshHost
        self.sshPort = sshPort
        self.sshUser = sshUser
        self.targetURL = targetURL
    }

    var resolvedSSHPort: Int {
        sshPort ?? 22
    }
}

struct ServerProfile: Identifiable, Hashable, Codable {
    let id: UUID
    var name: String
    var connectionMode: ServerConnectionMode
    var baseURL: URL
    var sshProxy: SSHProxyConfig?
    var token: String
    var username: String

    init(
        id: UUID = UUID(),
        name: String,
        connectionMode: ServerConnectionMode = .direct,
        baseURL: URL,
        sshProxy: SSHProxyConfig? = nil,
        token: String = "",
        username: String
    ) {
        self.id = id
        self.name = name
        self.connectionMode = connectionMode
        self.baseURL = baseURL
        self.sshProxy = sshProxy
        self.token = token
        self.username = username
    }

    var targetURL: URL {
        sshProxy?.targetURL ?? baseURL
    }

    var connectionSummary: String {
        switch connectionMode {
        case .direct:
            return baseURL.absoluteString
        case .sshProxy:
            let host = sshProxy?.sshHost.trimmingCharacters(in: .whitespacesAndNewlines)
            let displayHost = host?.isEmpty == false ? host! : "missing SSH host"
            let user = sshProxy?.sshUser.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            let port = sshProxy?.sshPort.map { ":\($0)" } ?? ""
            let prefix = user.isEmpty ? "\(displayHost)\(port)" : "\(user)@\(displayHost)\(port)"
            return "\(prefix) -> \(targetURL.absoluteString)"
        }
    }
}
