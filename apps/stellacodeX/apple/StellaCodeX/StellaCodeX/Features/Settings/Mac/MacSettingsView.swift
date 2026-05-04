#if os(macOS)
import AppKit
import SwiftUI

struct MacSettingsView: View {
    @ObservedObject var viewModel: AppViewModel
    @AppStorage(AppAppearanceMode.storageKey) private var appearanceModeRaw = AppAppearanceMode.system.rawValue
    @AppStorage(AppLanguageMode.storageKey) private var languageModeRaw = AppLanguageMode.system.rawValue
    @State private var draft = ServerProfileDraft()
    @State private var errorMessage: String?

    var body: some View {
        TabView {
            appearancePane
                .tabItem {
                    Label("Appearance", systemImage: "paintbrush")
                }

            serverPane
                .tabItem {
                    Label("Server", systemImage: "server.rack")
                }

            modelsPane
                .tabItem {
                    Label("Models", systemImage: "cpu")
                }

            cachePane
                .tabItem {
                    Label("Cache", systemImage: "externaldrive")
                }
        }
        .frame(width: 620, height: 560)
        .padding(.top, 12)
        .onAppear {
            appearanceMode.applyMacAppearance()
            draft = ServerProfileDraft(profile: viewModel.profile)
            Task {
                await viewModel.loadModels()
            }
        }
        .onChange(of: appearanceModeRaw) { _, _ in
            appearanceMode.applyMacAppearance()
        }
    }

    private var appearancePane: some View {
        ScrollView {
            MacSettingsCard {
                MacSettingsRow(
                    title: "Theme",
                    detail: LocalizedStringKey(appearanceMode.detail)
                ) {
                    Picker("Theme", selection: $appearanceModeRaw) {
                        ForEach(AppAppearanceMode.allCases) { mode in
                            Text(LocalizedStringKey(mode.displayName)).tag(mode.rawValue)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.segmented)
                    .frame(width: 170)
                }

                Divider()
                    .padding(.horizontal, 10)

                MacSettingsRow(
                    title: "Language",
                    detail: languageMode.detailKey
                ) {
                    Picker("Language", selection: $languageModeRaw) {
                        ForEach(AppLanguageMode.allCases) { mode in
                            Text(mode.titleKey).tag(mode.rawValue)
                        }
                    }
                    .labelsHidden()
                    .frame(width: 128)
                }
            }
            .padding(22)
        }
    }

    private var serverPane: some View {
        Form {
            Section("Profile") {
                TextField("Name", text: $draft.name)
                TextField("User", text: $draft.username)
                SecureField("Web Token", text: $draft.token)
            }

            Section("Connection") {
                Picker("Mode", selection: $draft.mode) {
                    ForEach(ServerConnectionMode.allCases) { mode in
                        Text(LocalizedStringKey(mode.displayName)).tag(mode)
                    }
                }
                .pickerStyle(.segmented)

                if draft.mode == .direct {
                    TextField("Direct URL", text: $draft.directURL)
                } else {
                    TextField("Target URL", text: $draft.targetURL)
                }
            }

            if draft.mode == .sshProxy {
                Section("SSH Proxy") {
                    TextField("SSH Host / Alias", text: $draft.sshHost)
                    TextField("SSH Port", text: $draft.sshPort)
                    TextField("SSH User", text: $draft.sshUser)
                }

                Section("Generated Public Key") {
                    if let identityError = viewModel.sshIdentityError {
                        Label(identityError, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.red)
                    } else {
                        Text(viewModel.sshPublicKey)
                            .font(.footnote.monospaced())
                            .textSelection(.enabled)

                        Button {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(viewModel.sshPublicKey, forType: .string)
                        } label: {
                            Label("Copy Public Key", systemImage: "doc.on.doc")
                        }
                        .disabled(viewModel.sshPublicKey.isEmpty)
                    }
                }
            }

            Section("Current Endpoint") {
                LabeledContent("Active", value: viewModel.profile.connectionSummary)
                LabeledContent("Edited", value: draft.previewSummary)
            }

            if let errorMessage {
                Section {
                    Label(errorMessage, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                }
            }

            Section {
                HStack {
                    Button {
                        draft.useNATPL1Defaults()
                        errorMessage = nil
                    } label: {
                        Label("Use NAT-pl1 Defaults", systemImage: "point.3.connected.trianglepath.dotted")
                    }

                    Spacer()

                    Button {
                        applyChanges()
                    } label: {
                        Label("Apply and Reload", systemImage: "arrow.clockwise")
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(!draft.hasMinimumFields)
                }
            }
        }
        .formStyle(.grouped)
        .padding(22)
    }

    private var modelsPane: some View {
        Form {
            Section("Models") {
                if let modelsError = viewModel.modelsError {
                    Label(modelsError, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                } else if viewModel.availableModels.isEmpty {
                    Text("No models loaded")
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(viewModel.availableModels) { model in
                        VStack(alignment: .leading, spacing: 3) {
                            Text(model.alias)
                                .font(.body.weight(.semibold))
                            Text("\(model.providerType) - \(model.modelName)")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            if model.effectiveMaxTokens > 0 {
                                Text("context \(model.tokenMaxContext) - output \(model.effectiveMaxTokens)")
                                    .font(.caption2)
                                    .foregroundStyle(.tertiary)
                            }
                        }
                        .padding(.vertical, 2)
                    }
                }

                Button {
                    Task {
                        await viewModel.loadModels()
                    }
                } label: {
                    Label("Reload Models", systemImage: "arrow.clockwise")
                }
            }
        }
        .formStyle(.grouped)
        .padding(22)
    }

    private var cachePane: some View {
        Form {
            Section("Message Cache") {
                LabeledContent("Conversations", value: "\(viewModel.messageCacheStats.conversationCount)")
                LabeledContent("Storage", value: formatCacheBytes(viewModel.messageCacheStats.bytes))

                HStack {
                    Button {
                        viewModel.pruneExpiredMessageCache()
                    } label: {
                        Label("Remove Expired", systemImage: "clock.arrow.circlepath")
                    }

                    Spacer()

                    Button(role: .destructive) {
                        viewModel.clearMessageCache()
                    } label: {
                        Label("Clear Cache", systemImage: "trash")
                    }
                }
            }

            Section {
                Text("Conversation messages are cached locally after REST or WebSocket loads. Opening a cached conversation reads the latest page locally first, then uses realtime ack events to repair gaps. Cache entries are automatically removed after 30 days without access.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .padding(22)
    }

    private var appearanceMode: AppAppearanceMode {
        AppAppearanceMode(rawValue: appearanceModeRaw) ?? .system
    }

    private var languageMode: AppLanguageMode {
        AppLanguageMode(rawValue: languageModeRaw) ?? .system
    }

    private func applyChanges() {
        do {
            let profile = try draft.makeProfile(current: viewModel.profile)
            errorMessage = nil
            viewModel.updateProfile(profile)

            Task {
                await viewModel.loadInitialData()
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

private func formatCacheBytes(_ bytes: Int64) -> String {
    ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
}

private struct MacSettingsCard<Content: View>: View {
    let content: Content

    init(@ViewBuilder content: () -> Content) {
        self.content = content()
    }

    var body: some View {
        VStack(spacing: 0) {
            content
        }
        .padding(.vertical, 10)
        .background(PlatformColor.controlBackground.opacity(0.62))
        .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
    }
}

private struct MacSettingsRow<Control: View>: View {
    let title: LocalizedStringKey
    let detail: LocalizedStringKey
    let control: Control

    init(
        title: LocalizedStringKey,
        detail: LocalizedStringKey,
        @ViewBuilder control: () -> Control
    ) {
        self.title = title
        self.detail = detail
        self.control = control()
    }

    var body: some View {
        HStack(alignment: .center, spacing: 18) {
            VStack(alignment: .leading, spacing: 8) {
                Text(title)
                    .font(.headline)

                Text(detail)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }

            Spacer(minLength: 18)

            control
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 12)
        .frame(minHeight: 76)
    }
}

private struct ServerProfileDraft {
    var name = "NAT-pl1 Stellaclaw"
    var username = "workspace-user"
    var token = "local-web-token"
    var mode: ServerConnectionMode = .sshProxy
    var directURL = "http://NAT-pl1:3011"
    var targetURL = "http://127.0.0.1:3011"
    var sshHost = "NAT-pl1"
    var sshPort = ""
    var sshUser = ProcessInfo.processInfo.environment["USER"] ?? "workspace-user"

    init() {
    }

    init(profile: ServerProfile) {
        self.name = profile.name
        self.username = profile.username
        self.token = profile.token
        self.mode = profile.connectionMode
        self.directURL = profile.baseURL.absoluteString
        self.targetURL = profile.targetURL.absoluteString

        if let sshProxy = profile.sshProxy {
            self.sshHost = sshProxy.sshHost
            self.sshPort = sshProxy.sshPort.map { String($0) } ?? ""
            self.sshUser = sshProxy.sshUser
        }
    }

    var hasMinimumFields: Bool {
        !name.trimmed.isEmpty
            && !username.trimmed.isEmpty
            && (mode == .direct ? URL(string: directURL.trimmed) != nil : sshProxyHasMinimumFields)
    }

    var previewSummary: String {
        switch mode {
        case .direct:
            return directURL.trimmed
        case .sshProxy:
            let userPrefix = sshUser.trimmed.isEmpty ? "" : "\(sshUser.trimmed)@"
            let portSuffix = sshPort.trimmed.isEmpty ? "" : ":\(sshPort.trimmed)"
            return "\(userPrefix)\(sshHost.trimmed)\(portSuffix) -> \(targetURL.trimmed)"
        }
    }

    mutating func useNATPL1Defaults() {
        name = "NAT-pl1 Stellaclaw"
        mode = .sshProxy
        directURL = "http://NAT-pl1:3011"
        targetURL = "http://127.0.0.1:3011"
        sshHost = "NAT-pl1"
        sshPort = ""
        if sshUser.trimmed.isEmpty {
            sshUser = ProcessInfo.processInfo.environment["USER"] ?? "workspace-user"
        }
        if token.trimmed.isEmpty {
            token = "local-web-token"
        }
    }

    func makeProfile(current: ServerProfile) throws -> ServerProfile {
        let baseURL: URL
        let sshProxy: SSHProxyConfig?
        if mode == .sshProxy {
            guard let target = URL(string: targetURL.trimmed) else {
                throw ServerProfileDraftError.invalidTargetURL
            }
            baseURL = target
            let port: Int?
            if sshPort.trimmed.isEmpty {
                port = nil
            } else if let parsedPort = Int(sshPort.trimmed), (1...65535).contains(parsedPort) {
                port = parsedPort
            } else {
                throw ServerProfileDraftError.invalidSSHPort
            }
            guard !sshHost.trimmed.isEmpty else {
                throw ServerProfileDraftError.missingSSHHost
            }
            guard !sshUser.trimmed.isEmpty else {
                throw ServerProfileDraftError.missingSSHUser
            }

            sshProxy = SSHProxyConfig(
                sshHost: sshHost.trimmed,
                sshPort: port,
                sshUser: sshUser.trimmed,
                targetURL: target
            )
        } else {
            guard let direct = URL(string: directURL.trimmed) else {
                throw ServerProfileDraftError.invalidDirectURL
            }
            baseURL = direct
            sshProxy = nil
        }

        return ServerProfile(
            id: current.id,
            name: name.trimmed,
            connectionMode: mode,
            baseURL: baseURL,
            sshProxy: sshProxy,
            token: token,
            username: username.trimmed
        )
    }

    private var sshProxyHasMinimumFields: Bool {
        !sshHost.trimmed.isEmpty
            && !sshUser.trimmed.isEmpty
            && (sshPort.trimmed.isEmpty || Int(sshPort.trimmed) != nil)
            && URL(string: targetURL.trimmed) != nil
    }
}

private enum ServerProfileDraftError: Error, LocalizedError {
    case invalidDirectURL
    case invalidTargetURL
    case invalidSSHPort
    case missingSSHHost
    case missingSSHUser

    var errorDescription: String? {
        switch self {
        case .invalidDirectURL:
            "Direct URL is invalid."
        case .invalidTargetURL:
            "Target URL is invalid."
        case .invalidSSHPort:
            "SSH port must be between 1 and 65535."
        case .missingSSHHost:
            "SSH host is required."
        case .missingSSHUser:
            "SSH user is required."
        }
    }
}

private extension String {
    var trimmed: String {
        trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

#Preview {
    MacSettingsView(viewModel: .mock())
}
#endif
