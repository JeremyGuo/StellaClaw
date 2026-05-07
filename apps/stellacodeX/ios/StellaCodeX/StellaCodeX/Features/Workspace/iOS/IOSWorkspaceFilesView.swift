#if os(iOS)
import SwiftUI
import UniformTypeIdentifiers

struct IOSWorkspaceFilesView: View {
    @ObservedObject var viewModel: AppViewModel
    @State private var listing: WorkspaceListing?
    @State private var previewEntry: WorkspaceEntry?
    @State private var currentPath = ""
    @State private var isLoading = false
    @State private var isFileImporterPresented = false
    @State private var errorMessage: String?
    @State private var statusMessage: String?
    @State private var shareItem: WorkspaceShareItem?
    @State private var moveDraft = WorkspaceMoveDraft()
    @State private var deleteCandidate: WorkspaceEntry?

    var body: some View {
        List {
            if let conversation = viewModel.selectedConversation {
                Section {
                    ConversationInfoRows(conversation: conversation)
                }
            }

            Section {
                if isLoading && listing == nil {
                    ProgressView("Loading workspace")
                } else if let listing {
                    workspaceHeader(listing)

                    if let parent = listing.parent, !currentPath.isEmpty {
                        Button {
                            currentPath = parent
                            Task { await loadCurrentPath(force: true) }
                        } label: {
                            Label("Parent directory", systemImage: "arrowshape.turn.up.left")
                        }
                    }

                    ForEach(listing.entries) { entry in
                        workspaceRow(entry)
                    }

                    if listing.entries.isEmpty {
                        ContentUnavailableView("Empty Folder", systemImage: "folder")
                    }
                }

                if let errorMessage {
                    Label(errorMessage, systemImage: "exclamationmark.triangle.fill")
                        .foregroundStyle(.red)
                }

                if let statusMessage {
                    Label(statusMessage, systemImage: "checkmark.circle.fill")
                        .foregroundStyle(.green)
                }
            } header: {
                Text(currentPath.isEmpty ? "Workspace" : currentPath)
            }
        }
        .navigationTitle("Files")
        .navigationBarTitleDisplayMode(.inline)
        .navigationDestination(item: $previewEntry) { entry in
            WorkspaceFilePreviewScreen(
                viewModel: viewModel,
                entry: entry,
                onWorkspaceChanged: {
                    Task { await loadCurrentPath(force: true) }
                }
            )
        }
        .toolbar {
            ToolbarItemGroup(placement: .topBarTrailing) {
                Button {
                    Task { await loadCurrentPath(force: true) }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .disabled(isLoading)

                Button {
                    isFileImporterPresented = true
                } label: {
                    Image(systemName: "square.and.arrow.up")
                }
            }
        }
        .task {
            await loadCurrentPath(force: true)
        }
        .fileImporter(isPresented: $isFileImporterPresented, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            Task {
                await handleImport(result)
            }
        }
        .sheet(item: $shareItem) { item in
            ActivityShareSheet(items: [item.url])
        }
        .alert("Rename or Move", isPresented: $moveDraft.isPresented) {
            TextField("Destination path", text: $moveDraft.newPath)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()

            Button("Cancel", role: .cancel) {
                moveDraft = WorkspaceMoveDraft()
            }

            Button("Apply") {
                Task {
                    await movePath(from: moveDraft.path, to: moveDraft.newPath)
                    moveDraft = WorkspaceMoveDraft()
                }
            }
            .disabled(moveDraft.newPath.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        } message: {
            Text("Use a path relative to the workspace root.")
        }
        .alert("Delete Workspace Item", isPresented: Binding(
            get: { deleteCandidate != nil },
            set: { if !$0 { deleteCandidate = nil } }
        )) {
            Button("Cancel", role: .cancel) {
                deleteCandidate = nil
            }

            Button("Delete", role: .destructive) {
                guard let entry = deleteCandidate else {
                    return
                }
                Task {
                    await delete(entry)
                    deleteCandidate = nil
                }
            }
        } message: {
            Text("This permanently deletes \(deleteCandidate?.path ?? "this item").")
        }
    }

    private func workspaceHeader(_ listing: WorkspaceListing) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Label(listing.workspaceRoot.isEmpty ? "workspace" : listing.workspaceRoot, systemImage: "folder")
                .font(.headline)
            Text(listing.locationLabel)
                .font(.caption)
                .foregroundStyle(.secondary)
            if listing.truncated {
                Text("Showing \(listing.returnedEntries) of \(listing.totalEntries) entries")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 4)
    }

    private func workspaceRow(_ entry: WorkspaceEntry) -> some View {
        Button {
            if entry.isDirectory {
                currentPath = entry.path
                Task { await loadCurrentPath(force: true) }
            } else {
                previewEntry = entry
            }
        } label: {
            HStack(spacing: 12) {
                Image(systemName: icon(for: entry))
                    .foregroundStyle(entry.isDirectory ? Color.accentColor : .secondary)
                    .frame(width: 24)

                VStack(alignment: .leading, spacing: 2) {
                    Text(entry.name)
                        .foregroundStyle(.primary)
                    if !entry.isDirectory {
                        Text(formatBytes(entry.sizeBytes))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }

                Spacer()

                if entry.isDirectory {
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.tertiary)
                }
            }
        }
        .swipeActions(edge: .trailing, allowsFullSwipe: false) {
            Button(role: .destructive) {
                deleteCandidate = entry
            } label: {
                Label("Delete", systemImage: "trash")
            }
            .tint(.red)

            Button {
                moveDraft = WorkspaceMoveDraft(path: entry.path, name: entry.name)
            } label: {
                Label("Move", systemImage: "arrow.triangle.2.circlepath")
            }
            .tint(.orange)

            Button {
                Task { await download(path: entry.path, name: entry.name) }
            } label: {
                Label("Share", systemImage: "square.and.arrow.up")
            }
            .tint(.blue)
        }
    }

    private func loadCurrentPath(force: Bool) async {
        isLoading = true
        errorMessage = nil
        do {
            listing = try await viewModel.loadWorkspaceListing(path: currentPath)
        } catch {
            errorMessage = error.localizedDescription
        }
        isLoading = false
    }

    private func handleImport(_ result: Result<[URL], Error>) async {
        do {
            let urls = try result.get()
            guard !urls.isEmpty else {
                return
            }
            let count = try await viewModel.uploadWorkspaceFiles(fileURLs: urls, targetPath: currentPath)
            statusMessage = "Uploaded \(count) item\(count == 1 ? "" : "s")"
            await loadCurrentPath(force: true)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func download(path: String, name: String) async {
        do {
            shareItem = WorkspaceShareItem(url: try await viewModel.downloadWorkspaceArchive(path: path, suggestedName: name))
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func delete(_ entry: WorkspaceEntry) async {
        do {
            try await viewModel.deleteWorkspacePath(entry.path)
            statusMessage = "Deleted \(entry.name)"
            await loadCurrentPath(force: true)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func movePath(from path: String, to newPath: String) async {
        do {
            let trimmed = newPath.trimmingCharacters(in: .whitespacesAndNewlines)
            try await viewModel.moveWorkspacePath(path, to: trimmed)
            statusMessage = "Moved to \(trimmed)"
            await loadCurrentPath(force: true)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func icon(for entry: WorkspaceEntry) -> String {
        if entry.isDirectory {
            return "folder"
        }
        if isImagePath(entry.path) {
            return "photo"
        }
        return "doc.text"
    }

    private func formatBytes(_ bytes: Int64?) -> String {
        guard let bytes else {
            return ""
        }
        return ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
    }
}

private struct WorkspaceFilePreviewScreen: View {
    @ObservedObject var viewModel: AppViewModel
    let entry: WorkspaceEntry
    let onWorkspaceChanged: () -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var file: WorkspaceFile?
    @State private var isLoading = false
    @State private var errorMessage: String?
    @State private var shareItem: WorkspaceShareItem?
    @State private var moveDraft = WorkspaceMoveDraft()
    @State private var isDeletePresented = false
    @State private var markdownMode: MarkdownPreviewMode = .preview

    var body: some View {
        Group {
            if isLoading && file == nil {
                ProgressView("Loading \(entry.name)")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if let file {
                WorkspaceFilePreviewContent(file: file, markdownMode: $markdownMode)
            } else {
                ContentUnavailableView(
                    "Preview Unavailable",
                    systemImage: "doc.questionmark",
                    description: Text(errorMessage ?? "This file cannot be previewed.")
                )
            }
        }
        .navigationTitle(entry.name)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItemGroup(placement: .topBarTrailing) {
                Button {
                    Task { await loadFile(force: true) }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .disabled(isLoading)

                Button {
                    Task { await download(path: entry.path, name: entry.name) }
                } label: {
                    Image(systemName: "square.and.arrow.up")
                }

                Menu {
                    Button {
                        moveDraft = WorkspaceMoveDraft(path: entry.path, name: entry.name)
                    } label: {
                        Label("Rename or Move", systemImage: "arrow.triangle.2.circlepath")
                    }

                    Button(role: .destructive) {
                        isDeletePresented = true
                    } label: {
                        Label("Delete", systemImage: "trash")
                    }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
            }
        }
        .task {
            await loadFile(force: false)
        }
        .sheet(item: $shareItem) { item in
            ActivityShareSheet(items: [item.url])
        }
        .alert("Rename or Move", isPresented: $moveDraft.isPresented) {
            TextField("Destination path", text: $moveDraft.newPath)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()

            Button("Cancel", role: .cancel) {
                moveDraft = WorkspaceMoveDraft()
            }

            Button("Apply") {
                Task {
                    await movePath(from: moveDraft.path, to: moveDraft.newPath)
                    moveDraft = WorkspaceMoveDraft()
                }
            }
            .disabled(moveDraft.newPath.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        } message: {
            Text("Use a path relative to the workspace root.")
        }
        .alert("Delete Workspace Item", isPresented: $isDeletePresented) {
            Button("Cancel", role: .cancel) {
                isDeletePresented = false
            }

            Button("Delete", role: .destructive) {
                Task {
                    await deleteFile()
                }
            }
        } message: {
            Text("This permanently deletes \(entry.path).")
        }
    }

    private func loadFile(force: Bool) async {
        if isLoading {
            return
        }
        errorMessage = nil
        let image = isImagePath(entry.path)
        if !image, let sizeBytes = entry.sizeBytes, sizeBytes > 25_000_000 {
            file = nil
            errorMessage = "Preview is limited to 25 MB for this file type. Use Share / Open In... to download it."
            return
        }

        isLoading = true
        defer {
            isLoading = false
        }
        do {
            file = try await viewModel.loadWorkspaceFile(
                path: entry.path,
                previewLimitBytes: image ? 1 : 2_000_000,
                full: image
            )
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func download(path: String, name: String) async {
        do {
            shareItem = WorkspaceShareItem(url: try await viewModel.downloadWorkspaceArchive(path: path, suggestedName: name))
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func movePath(from path: String, to newPath: String) async {
        do {
            let trimmed = newPath.trimmingCharacters(in: .whitespacesAndNewlines)
            try await viewModel.moveWorkspacePath(path, to: trimmed)
            onWorkspaceChanged()
            dismiss()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func deleteFile() async {
        do {
            try await viewModel.deleteWorkspacePath(entry.path)
            onWorkspaceChanged()
            dismiss()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

private enum MarkdownPreviewMode: String, CaseIterable {
    case preview = "Preview"
    case source = "Source"
}

private struct WorkspaceFilePreviewContent: View {
    let file: WorkspaceFile
    @Binding var markdownMode: MarkdownPreviewMode

    var body: some View {
        VStack(spacing: 0) {
            WorkspaceFileMetadataBar(file: file)

            Divider()

            if isMarkdownPath(file.path) {
                Picker("Markdown Mode", selection: $markdownMode) {
                    ForEach(MarkdownPreviewMode.allCases, id: \.self) { mode in
                        Text(mode.rawValue).tag(mode)
                    }
                }
                .pickerStyle(.segmented)
                .padding(.horizontal, 16)
                .padding(.vertical, 10)

                Divider()

                if markdownMode == .preview {
                    ScrollView {
                        MarkdownContentView(text: file.data, compact: false)
                            .padding(16)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                } else {
                    WorkspaceSourceCodeView(text: file.data, language: languageForPath(file.path))
                }
            } else if isImagePath(file.path), let data = file.decodedData, let image = UIImage(data: data) {
                ScrollView([.vertical, .horizontal]) {
                    Image(uiImage: image)
                        .resizable()
                        .scaledToFit()
                        .padding(16)
                        .frame(maxWidth: .infinity)
                }
                .background(Color.black.opacity(0.04))
            } else if file.isText {
                WorkspaceSourceCodeView(text: file.data, language: languageForPath(file.path))
            } else {
                ContentUnavailableView("Binary Preview Unavailable", systemImage: "doc.zipper")
            }

            if file.truncated {
                Divider()
                Label(
                    "Preview truncated at \(ByteCountFormatter.string(fromByteCount: Int64(file.returnedBytes), countStyle: .file))",
                    systemImage: "scissors"
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 16)
                .padding(.vertical, 8)
            }
        }
    }
}

private struct WorkspaceFileMetadataBar: View {
    let file: WorkspaceFile

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: isImagePath(file.path) ? "photo" : (isMarkdownPath(file.path) ? "doc.richtext" : "doc.text"))
                .font(.headline)
                .foregroundStyle(Color.accentColor)
                .frame(width: 24)

            VStack(alignment: .leading, spacing: 2) {
                Text(file.path)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(1)
                Text("\(ByteCountFormatter.string(fromByteCount: file.sizeBytes, countStyle: .file)) · \(file.encoding)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Spacer(minLength: 0)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(PlatformColor.secondaryBackground.opacity(0.55))
    }
}

private struct WorkspaceSourceCodeView: View {
    let text: String
    let language: String?

    var body: some View {
        ScrollView([.vertical, .horizontal]) {
            Text(SyntaxHighlighter.highlight(text, language: language))
                .font(.system(.footnote, design: .monospaced))
                .textSelection(.enabled)
                .padding(14)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(PlatformColor.controlBackground)
    }
}

private struct WorkspaceMoveDraft {
    var path = ""
    var newPath = ""

    init() {
    }

    init(path: String, name: String) {
        self.path = path
        self.newPath = path
    }

    var isPresented: Bool {
        get {
            !path.isEmpty
        }
        set {
            if !newValue {
                path = ""
                newPath = ""
            }
        }
    }
}

private struct ConversationInfoRows: View {
    let conversation: ConversationSummary

    var body: some View {
        LabeledContent("Conversation", value: conversation.title)
        LabeledContent("Model", value: conversation.model.isEmpty ? "pending" : conversation.model)
        LabeledContent("Workspace", value: conversation.workspacePath.isEmpty ? "No workspace" : conversation.workspacePath)
        LabeledContent("Remote", value: conversation.remote.isEmpty ? "local" : conversation.remote)
    }
}

private struct WorkspaceShareItem: Identifiable {
    let url: URL
    var id: String { url.absoluteString }
}

private struct ActivityShareSheet: UIViewControllerRepresentable {
    let items: [Any]

    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: items, applicationActivities: nil)
    }

    func updateUIViewController(_ uiViewController: UIActivityViewController, context: Context) {
    }
}

private func isImagePath(_ path: String) -> Bool {
    let ext = path.split(separator: ".").last?.lowercased() ?? ""
    return ["png", "jpg", "jpeg", "gif", "webp", "heic"].contains(String(ext))
}

private func isMarkdownPath(_ path: String) -> Bool {
    let ext = fileExtension(path)
    return ["md", "markdown", "mdown", "mkd"].contains(ext)
}

private func languageForPath(_ path: String) -> String? {
    switch fileExtension(path) {
    case "swift": "swift"
    case "rs": "rust"
    case "js", "mjs", "cjs": "javascript"
    case "ts", "tsx": "typescript"
    case "jsx": "javascript"
    case "json", "jsonl": "json"
    case "py": "python"
    case "sh", "bash", "zsh": "shell"
    case "html", "htm": "html"
    case "css": "css"
    case "md", "markdown", "mdown", "mkd": "markdown"
    case "toml": "toml"
    case "yaml", "yml": "yaml"
    case "xml": "xml"
    case "c", "h": "c"
    case "cpp", "cc", "cxx", "hpp": "cpp"
    case "java": "java"
    case "go": "go"
    case "rb": "ruby"
    default: nil
    }
}

private func fileExtension(_ path: String) -> String {
    path.split(separator: ".").last.map { String($0).lowercased() } ?? ""
}

private enum SyntaxHighlighter {
    static func highlight(_ text: String, language: String?) -> AttributedString {
        var attributed = AttributedString(text.isEmpty ? " " : text)
        attributed.foregroundColor = .primary

        applyRegex(#""(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'"#, color: .orange, to: &attributed, source: text)
        applyRegex(#"//.*|#.*|/\*[\s\S]*?\*/"#, color: .secondary, to: &attributed, source: text)
        applyRegex(#"\b\d+(?:\.\d+)?\b"#, color: .purple, to: &attributed, source: text)

        let keywords = keywords(for: language)
        if !keywords.isEmpty {
            let escaped = keywords.map(NSRegularExpression.escapedPattern(for:)).joined(separator: "|")
            applyRegex(#"\b("# + escaped + #")\b"#, color: .blue, weight: .semibold, to: &attributed, source: text)
        }

        return attributed
    }

    private static func keywords(for language: String?) -> [String] {
        switch language {
        case "swift":
            ["actor", "as", "async", "await", "case", "catch", "class", "enum", "extension", "false", "for", "func", "guard", "if", "import", "in", "let", "nil", "private", "protocol", "public", "return", "self", "static", "struct", "switch", "throw", "throws", "true", "try", "var", "while"]
        case "rust":
            ["async", "await", "const", "crate", "enum", "false", "fn", "for", "if", "impl", "let", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "static", "struct", "trait", "true", "type", "use", "where", "while"]
        case "javascript", "typescript":
            ["async", "await", "break", "case", "catch", "class", "const", "continue", "default", "else", "export", "false", "for", "from", "function", "if", "import", "in", "interface", "let", "new", "null", "return", "switch", "this", "throw", "true", "try", "type", "undefined", "var", "while"]
        case "python":
            ["and", "as", "async", "await", "class", "def", "elif", "else", "except", "False", "finally", "for", "from", "if", "import", "in", "is", "lambda", "None", "not", "or", "pass", "return", "True", "try", "while", "with", "yield"]
        case "shell":
            ["case", "do", "done", "elif", "else", "esac", "export", "fi", "for", "function", "if", "in", "local", "then", "while"]
        case "go":
            ["break", "case", "chan", "const", "continue", "defer", "else", "fallthrough", "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range", "return", "select", "struct", "switch", "type", "var"]
        default:
            ["false", "null", "true"]
        }
    }

    private static func applyRegex(
        _ pattern: String,
        color: Color,
        weight: Font.Weight? = nil,
        to attributed: inout AttributedString,
        source: String
    ) {
        guard let regex = try? NSRegularExpression(pattern: pattern, options: []) else {
            return
        }
        let nsRange = NSRange(source.startIndex..<source.endIndex, in: source)
        for match in regex.matches(in: source, range: nsRange) {
            guard let range = Range(match.range, in: source),
                  let attributedRange = Range(range, in: attributed)
            else {
                continue
            }
            attributed[attributedRange].foregroundColor = color
            if let weight {
                attributed[attributedRange].font = .system(.footnote, design: .monospaced).weight(weight)
            }
        }
    }
}

#endif
