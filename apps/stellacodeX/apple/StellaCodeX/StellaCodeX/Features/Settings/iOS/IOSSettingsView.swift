#if os(iOS)
import SwiftUI

struct IOSSettingsView: View {
    @ObservedObject var viewModel: AppViewModel
    @AppStorage(AppAppearanceMode.storageKey) private var appearanceModeRaw = AppAppearanceMode.system.rawValue
    @AppStorage(AppLanguageMode.storageKey) private var languageModeRaw = AppLanguageMode.system.rawValue
    @State private var draft = ServerProfileDraft()
    @State private var errorMessage: String?

    var body: some View {
        NavigationStack {
            Form {
                Section("Appearance") {
                    Picker("Theme", selection: $appearanceModeRaw) {
                        ForEach(AppAppearanceMode.allCases) { mode in
                            Text(LocalizedStringKey(mode.displayName)).tag(mode.rawValue)
                        }
                    }
                    .pickerStyle(.segmented)

                    Text(LocalizedStringKey(appearanceMode.detail))
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }

                Section("Language") {
                    Picker("Language", selection: $languageModeRaw) {
                        ForEach(AppLanguageMode.allCases) { mode in
                            Text(mode.titleKey).tag(mode.rawValue)
                        }
                    }

                    Text(languageMode.detailKey)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                }

                Section("Profile") {
                    TextField("Name", text: $draft.name)
                        .textInputAutocapitalization(.words)

                    TextField("User", text: $draft.username)
                        .textInputAutocapitalization(.never)
                        .disableAutocorrection(true)

                    SecureField("Web Token", text: $draft.token)
                        .textInputAutocapitalization(.never)
                        .disableAutocorrection(true)
                }

                Section("Connection") {
                    Picker("Mode", selection: $draft.mode) {
                        ForEach(ServerConnectionMode.allCases) { mode in
                            Text(LocalizedStringKey(mode.displayName)).tag(mode)
                        }
                    }

                    if draft.mode == .direct {
                        TextField("Direct URL", text: $draft.directURL)
                            .textInputAutocapitalization(.never)
                            .disableAutocorrection(true)
                            .keyboardType(.URL)
                    } else {
                        TextField("Target URL", text: $draft.targetURL)
                            .textInputAutocapitalization(.never)
                            .disableAutocorrection(true)
                            .keyboardType(.URL)
                    }
                }

                if draft.mode == .sshProxy {
                    Section {
                        TextField("SSH Host", text: $draft.sshHost)
                            .textInputAutocapitalization(.never)
                            .disableAutocorrection(true)

                        TextField("SSH Port", text: $draft.sshPort)
                            .keyboardType(.numberPad)

                        TextField("SSH User", text: $draft.sshUser)
                            .textInputAutocapitalization(.never)
                            .disableAutocorrection(true)
                    } header: {
                        Text("SSH Proxy")
                    } footer: {
                        Text("Leave SSH Port empty to use 22. Authentication uses the generated public key below.")
                    }

                    Section {
                        if let identityError = viewModel.sshIdentityError {
                            Label(identityError, systemImage: "exclamationmark.triangle.fill")
                                .foregroundStyle(.red)
                        } else {
                            Text(viewModel.sshPublicKey)
                                .font(.footnote.monospaced())
                                .textSelection(.enabled)

                            Button {
                                UIPasteboard.general.string = viewModel.sshPublicKey
                            } label: {
                                Label("Copy Public Key", systemImage: "doc.on.doc")
                            }
                            .disabled(viewModel.sshPublicKey.isEmpty)
                        }
                    } header: {
                        Text("Generated Public Key")
                    } footer: {
                        Text("Add this key to the remote account's authorized_keys before connecting.")
                    }
                }

                Section("Current Endpoint") {
                    LabeledContent("Active", value: viewModel.profile.connectionSummary)
                    LabeledContent("Edited", value: draft.previewSummary)
                }

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
                                Text("\(model.providerType) · \(model.modelName)")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                                if model.effectiveMaxTokens > 0 {
                                    Text("context \(model.tokenMaxContext) · output \(model.effectiveMaxTokens)")
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

                Section {
                    LabeledContent("Conversations", value: "\(viewModel.messageCacheStats.conversationCount)")
                    LabeledContent("Storage", value: formatCacheBytes(viewModel.messageCacheStats.bytes))

                    Button {
                        viewModel.pruneExpiredMessageCache()
                    } label: {
                        Label("Remove Expired Cache", systemImage: "clock.arrow.circlepath")
                    }

                    Button(role: .destructive) {
                        viewModel.clearMessageCache()
                    } label: {
                        Label("Clear Message Cache", systemImage: "trash")
                    }
                } header: {
                    Text("Message Cache")
                } footer: {
                    Text("Message cache is reused when opening conversations and is automatically cleaned after 30 days without access.")
                }

                if let errorMessage {
                    Section {
                        Label(errorMessage, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.red)
                    }
                }

                Section {
                    Button {
                        applyChanges()
                    } label: {
                        Label("Apply and Reload", systemImage: "arrow.clockwise")
                    }
                    .disabled(!draft.hasMinimumFields)

                    Button {
                        draft.useNATPL1Defaults()
                        errorMessage = nil
                    } label: {
                        Label("Use NAT-pl1 Defaults", systemImage: "point.3.connected.trianglepath.dotted")
                    }
                }
            }
            .safeAreaPadding(.bottom, 88)
            .navigationTitle("Settings")
            .onAppear {
                draft = ServerProfileDraft(profile: viewModel.profile)
                Task {
                    await viewModel.loadModels()
                }
            }
        }
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
    IOSSettingsView(viewModel: .mock())
}
#endif
