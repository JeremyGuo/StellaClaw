import Foundation

#if canImport(Citadel) && canImport(NIO) && canImport(NIOSSH)
import Citadel
import Crypto
import NIO
import NIOSSH
#endif

nonisolated private func sshProxyDebugLog(_ message: @autoclosure () -> String) {
    _ = message
}

actor AppleSSHTunnelManager {
    static let shared = AppleSSHTunnelManager()

    private var activeTunnel: ActiveTunnel?
    #if canImport(Citadel) && canImport(NIO) && canImport(NIOSSH)
    private var pendingTunnel: PendingTunnel?
    #endif

    func resolveBaseURL(for profile: ServerProfile) async throws -> URL {
        guard profile.connectionMode == .sshProxy else {
            return profile.baseURL
        }

        guard let config = profile.sshProxy else {
            throw SSHProxyError.missingConfig
        }

        #if canImport(Citadel) && canImport(NIO) && canImport(NIOSSH)
        let signature = TunnelSignature(config: config)
        if let activeTunnel, activeTunnel.signature == signature, activeTunnel.isOpen {
            return activeTunnel.baseURL
        }

        if let pendingTunnel, pendingTunnel.signature == signature {
            let tunnel = try await pendingTunnel.task.value
            activeTunnel = tunnel
            return tunnel.baseURL
        }

        try await activeTunnel?.close()
        activeTunnel = nil

        let task = Task { try await openCitadelTunnel(config: config, signature: signature) }
        pendingTunnel = PendingTunnel(signature: signature, task: task)
        do {
            let tunnel = try await task.value
            if pendingTunnel?.signature == signature {
                pendingTunnel = nil
            }
            activeTunnel = tunnel
            return tunnel.baseURL
        } catch {
            if pendingTunnel?.signature == signature {
                pendingTunnel = nil
            }
            throw error
        }
        #else
        throw SSHProxyError.clientUnavailable
        #endif
    }

    func close() async {
        #if canImport(Citadel) && canImport(NIO) && canImport(NIOSSH)
        pendingTunnel?.task.cancel()
        pendingTunnel = nil
        #endif
        try? await activeTunnel?.close()
        activeTunnel = nil
    }

    #if canImport(Citadel) && canImport(NIO) && canImport(NIOSSH)
    private func openCitadelTunnel(config: SSHProxyConfig, signature: TunnelSignature) async throws -> ActiveTunnel {
        let host = config.sshHost.trimmingCharacters(in: .whitespacesAndNewlines)
        let user = config.sshUser.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !host.isEmpty else {
            throw SSHProxyError.missingHost
        }
        guard !user.isEmpty else {
            throw SSHProxyError.missingUser
        }

        let identity = try AppleSSHIdentityStore.loadOrCreate()
        let authentication = SSHAuthenticationMethod.ed25519(
            username: user,
            privateKey: identity.privateKey
        )
        let resolvedPort = config.resolvedSSHPort
        var settings = SSHClientSettings(
            host: host,
            port: resolvedPort,
            authenticationMethod: { authentication },
            hostKeyValidator: .acceptAnything()
        )
        settings.connectTimeout = .seconds(10)

        sshProxyDebugLog("Opening SSH proxy to \(user)@\(host):\(resolvedPort), target \(config.targetURL.absoluteString)")
        let client: SSHClient
        do {
            client = try await SSHClient.connect(to: settings)
        } catch SSHClientError.allAuthenticationOptionsFailed {
            throw SSHProxyError.authenticationFailed(user: user, host: host)
        }
        sshProxyDebugLog("SSH proxy connected; opening local forwarder")
        let forwarder: LocalDirectTCPIPForwarder
        do {
            forwarder = try await LocalDirectTCPIPForwarder.start(client: client, targetURL: config.targetURL)
        } catch SSHClientError.channelCreationFailed {
            try? await client.close()
            throw SSHProxyError.targetConnectionFailed(target: config.targetURL.absoluteString)
        }
        sshProxyDebugLog("SSH proxy ready at \(forwarder.baseURL.absoluteString)")
        return ActiveTunnel(client: client, forwarder: forwarder, signature: signature)
    }
    #endif
}

enum SSHProxyError: Error, LocalizedError {
    case clientUnavailable
    case missingConfig
    case missingHost
    case missingUser
    case invalidTargetURL
    case localPortUnavailable
    case authenticationFailed(user: String, host: String)
    case targetConnectionFailed(target: String)

    var errorDescription: String? {
        switch self {
        case .clientUnavailable:
            "SSH client package is unavailable in this build."
        case .missingConfig:
            "SSH proxy configuration is missing."
        case .missingHost:
            "SSH proxy host is missing."
        case .missingUser:
            "SSH proxy user is missing."
        case .invalidTargetURL:
            "SSH proxy target URL is invalid."
        case .localPortUnavailable:
            "SSH proxy local port is unavailable."
        case .authenticationFailed(let user, let host):
            "SSH authentication failed for \(user)@\(host). Add the generated public key to that account's authorized_keys, or check SSH Host/User/Port."
        case .targetConnectionFailed(let target):
            "SSH connected, but the remote target \(target) could not be opened. Check the Target URL from the SSH host."
        }
    }
}

#if canImport(Citadel) && canImport(NIO) && canImport(NIOSSH)
private struct TunnelSignature: Equatable {
    var sshHost: String
    var sshPort: Int
    var sshUser: String
    var targetURL: URL

    init(config: SSHProxyConfig) {
        self.sshHost = config.sshHost.trimmingCharacters(in: .whitespacesAndNewlines)
        self.sshPort = config.sshPort
            ?? 22
        self.sshUser = config.sshUser.trimmingCharacters(in: .whitespacesAndNewlines)
        self.targetURL = config.targetURL
    }
}

private struct PendingTunnel {
    let signature: TunnelSignature
    let task: Task<ActiveTunnel, Error>
}

private final class ActiveTunnel: @unchecked Sendable {
    let client: SSHClient
    let forwarder: LocalDirectTCPIPForwarder
    let signature: TunnelSignature

    init(client: SSHClient, forwarder: LocalDirectTCPIPForwarder, signature: TunnelSignature) {
        self.client = client
        self.forwarder = forwarder
        self.signature = signature
    }

    var baseURL: URL {
        forwarder.baseURL
    }

    var isOpen: Bool {
        client.isConnected && forwarder.isOpen
    }

    func close() async throws {
        try await forwarder.close()
        try await client.close()
    }
}

private final class LocalDirectTCPIPForwarder: @unchecked Sendable {
    let serverChannel: Channel
    let baseURL: URL
    let targetHost: String
    let targetPort: Int

    init(serverChannel: Channel, baseURL: URL, targetHost: String, targetPort: Int) {
        self.serverChannel = serverChannel
        self.baseURL = baseURL
        self.targetHost = targetHost
        self.targetPort = targetPort
    }

    var isOpen: Bool {
        serverChannel.isActive
    }

    static func start(client: SSHClient, targetURL: URL) async throws -> LocalDirectTCPIPForwarder {
        guard let targetHost = targetURL.host(percentEncoded: false) else {
            throw SSHProxyError.invalidTargetURL
        }
        let targetPort = targetURL.port ?? (targetURL.scheme == "https" ? 443 : 80)
        let group = client.eventLoop
        let bootstrap = ServerBootstrap(group: group)
            .serverChannelOption(ChannelOptions.socketOption(.so_reuseaddr), value: 1)
            .childChannelOption(ChannelOptions.socketOption(.so_reuseaddr), value: 1)
            .childChannelOption(ChannelOptions.allowRemoteHalfClosure, value: true)
            .childChannelInitializer { inboundChannel in
                let inboundForwarder = ByteBufferForwarder()
                let sshForwarder = ByteBufferForwarder(peer: inboundChannel)

                return inboundChannel.pipeline.addHandler(inboundForwarder).flatMap {
                    inboundChannel.eventLoop.makeFutureWithTask {
                        do {
                            let originator = try SocketAddress(ipAddress: "127.0.0.1", port: 0)
                            sshProxyDebugLog("Opening direct-tcpip channel to \(targetHost):\(targetPort)")
                            let sshChannel = try await client.createDirectTCPIPChannel(
                                using: SSHChannelType.DirectTCPIP(
                                    targetHost: targetHost,
                                    targetPort: targetPort,
                                    originatorAddress: originator
                                )
                            ) { channel in
                                channel.pipeline.addHandler(sshForwarder)
                            }
                            inboundForwarder.setPeer(sshChannel)
                            sshProxyDebugLog("direct-tcpip channel ready")
                        } catch {
                            sshProxyDebugLog("direct-tcpip channel failed: \(error.localizedDescription)")
                            inboundChannel.close(promise: nil)
                            throw error
                        }
                    }
                }
            }

        let serverChannel = try await bootstrap.bind(host: "127.0.0.1", port: 0).get()
        guard let localPort = serverChannel.localAddress?.port else {
            throw SSHProxyError.localPortUnavailable
        }

        var components = URLComponents(url: targetURL, resolvingAgainstBaseURL: false)
        components?.host = "127.0.0.1"
        components?.port = localPort
        guard let baseURL = components?.url else {
            throw SSHProxyError.invalidTargetURL
        }
        return LocalDirectTCPIPForwarder(serverChannel: serverChannel, baseURL: baseURL, targetHost: targetHost, targetPort: targetPort)
    }

    func close() async throws {
        try await serverChannel.close().get()
    }
}

private final class ByteBufferForwarder: ChannelInboundHandler, @unchecked Sendable {
    typealias InboundIn = ByteBuffer
    typealias OutboundOut = ByteBuffer

    private var peer: Channel?
    private var pendingReads: [ByteBuffer] = []
    private var didClosePeerOutput = false

    init(peer: Channel? = nil) {
        self.peer = peer
    }

    func setPeer(_ peer: Channel) {
        self.peer = peer
        flushPendingReads()
    }

    func channelRead(context: ChannelHandlerContext, data: NIOAny) {
        let buffer = unwrapInboundIn(data)
        guard let peer else {
            pendingReads.append(buffer)
            return
        }
        peer.writeAndFlush(wrapOutboundOut(buffer), promise: nil)
    }

    func channelInactive(context: ChannelHandlerContext) {
        closePeerOutput()
        peer = nil
    }

    func errorCaught(context: ChannelHandlerContext, error: Error) {
        if !isExpectedClose(error) {
            sshProxyDebugLog("forwarder error: \(error.localizedDescription)")
        }
        context.close(promise: nil)
        closePeerOutput()
        peer = nil
    }

    private func flushPendingReads() {
        guard let peer else {
            return
        }
        for buffer in pendingReads {
            peer.writeAndFlush(wrapOutboundOut(buffer), promise: nil)
        }
        pendingReads.removeAll()
    }

    private func closePeerOutput() {
        guard !didClosePeerOutput else {
            return
        }
        didClosePeerOutput = true
        peer?.close(mode: .output, promise: nil)
    }

    private func isExpectedClose(_ error: Error) -> Bool {
        let description = error.localizedDescription.lowercased()
        return description.contains("socket is not connected")
            || description.contains("connection reset")
            || description.contains("connection closed")
            || description.contains("operation canceled")
            || description.contains("cancelled")
    }
}
#endif
