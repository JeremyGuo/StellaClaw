#if os(macOS)
import AppKit
import SwiftUI
import UniformTypeIdentifiers

struct MacWorkspaceFilesView: View {
    @ObservedObject var viewModel: AppViewModel
    @State private var rootListing: WorkspaceListing?
    @State private var listingsByPath: [String: WorkspaceListing] = [:]
    @State private var expandedDirectoryPaths: Set<String> = []
    @State private var loadingDirectoryPaths: Set<String> = []
    @State private var selectedEntry: WorkspaceEntry?
    @State private var selectedDirectoryPath = ""
    @State private var isLoading = false
    @State private var isFileImporterPresented = false
    @State private var errorMessage: String?
    @State private var statusMessage: String?
    @State private var moveDraft = MacWorkspaceMoveDraft()
    @State private var deleteCandidate: WorkspaceEntry?

    var body: some View {
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                header

                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        if let rootListing {
                            ForEach(sortedEntries(rootListing.entries)) { entry in
                                explorerRow(entry: entry, depth: 0)
                            }
                        } else if isLoading {
                            ProgressView("Loading workspace")
                                .frame(maxWidth: .infinity)
                                .padding(.vertical, 28)
                        } else {
                            ContentUnavailableView("No Workspace", systemImage: "folder.badge.questionmark")
                                .padding(.vertical, 32)
                        }
                    }
                    .padding(.vertical, 8)
                }
                .background(PlatformColor.sidebarBackground)

                footer
            }
            .frame(width: 320)
            .background(PlatformColor.sidebarBackground)

            Rectangle()
                .fill(PlatformColor.separator)
                .frame(width: 1)

            MacWorkspaceFilePreviewPane(
                viewModel: viewModel,
                entry: selectedEntry,
                onWorkspaceChanged: {
                    Task { await reloadCurrentExplorerScope() }
                }
            )
        }
        .background(PlatformColor.appBackground)
        .task(id: viewModel.selectedConversationID) {
            selectedDirectoryPath = ""
            selectedEntry = nil
            expandedDirectoryPaths = []
            listingsByPath = [:]
            await loadWorkspaceRoot()
        }
        .fileImporter(isPresented: $isFileImporterPresented, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            Task { await handleImport(result) }
        }
        .alert("Rename or Move", isPresented: $moveDraft.isPresented) {
            TextField("Destination path", text: $moveDraft.newPath)
            Button("Cancel", role: .cancel) {
                moveDraft = MacWorkspaceMoveDraft()
            }
            Button("Apply") {
                Task {
                    await movePath(from: moveDraft.path, to: moveDraft.newPath)
                    moveDraft = MacWorkspaceMoveDraft()
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

    private var header: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Label("Explorer", systemImage: "folder")
                    .font(.headline)
                Spacer()
                if isLoading {
                    ProgressView()
                        .controlSize(.small)
                }
                Button {
                    Task { await reloadCurrentExplorerScope() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                        .frame(width: 26, height: 24)
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .disabled(isLoading)
                .help("Reload")

                Button {
                    isFileImporterPresented = true
                } label: {
                    Image(systemName: "square.and.arrow.up")
                        .frame(width: 26, height: 24)
                }
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
                .help("Upload Files")
            }

            if let conversation = viewModel.selectedConversation {
                Text(conversation.title)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(1)
                Text(conversation.workspacePath.isEmpty ? "No workspace selected" : conversation.workspacePath)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }

            if let rootListing {
                Text(rootListing.path.isEmpty ? rootListing.locationLabel : rootListing.path)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }

            HStack(spacing: 6) {
                Image(systemName: "scope")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                Text(selectedDirectoryPath.isEmpty ? "/" : selectedDirectoryPath)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(PlatformColor.separator.opacity(0.65))
                .frame(height: 1)
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let errorMessage {
                Label(errorMessage, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
            }
            if let statusMessage {
                Label(statusMessage, systemImage: "checkmark.circle.fill")
                    .foregroundStyle(.green)
            }
        }
        .font(.caption)
        .lineLimit(2)
        .padding(.horizontal, 14)
        .padding(.vertical, (errorMessage == nil && statusMessage == nil) ? 0 : 10)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func explorerRow(entry: WorkspaceEntry, depth: Int) -> AnyView {
        AnyView(
            VStack(alignment: .leading, spacing: 0) {
                MacWorkspaceEntryRow(
                    entry: entry,
                    depth: depth,
                    isSelected: selectedEntry?.id == entry.id || (entry.isDirectory && selectedDirectoryPath == entry.path),
                    isExpanded: expandedDirectoryPaths.contains(entry.path),
                    isLoading: loadingDirectoryPaths.contains(entry.path),
                    toggleDirectory: {
                        toggleDirectory(entry)
                    }
                )
                .contentShape(Rectangle())
                .onTapGesture {
                    open(entry)
                }
                .contextMenu {
                    workspaceContextMenu(for: entry)
                }

                if entry.isDirectory,
                   expandedDirectoryPaths.contains(entry.path),
                   let childListing = listingsByPath[entry.path] {
                    ForEach(sortedEntries(childListing.entries)) { child in
                        explorerRow(entry: child, depth: depth + 1)
                    }
                }
            }
        )
    }

    @ViewBuilder
    private func workspaceContextMenu(for entry: WorkspaceEntry) -> some View {
        Button {
            Task { await download(path: entry.path, name: entry.name) }
        } label: {
            Label("Download", systemImage: "square.and.arrow.down")
        }

        Button {
            moveDraft = MacWorkspaceMoveDraft(path: entry.path)
        } label: {
            Label("Rename or Move", systemImage: "arrow.triangle.2.circlepath")
        }

        Divider()

        Button(role: .destructive) {
            deleteCandidate = entry
        } label: {
            Label("Delete", systemImage: "trash")
        }
    }

    private func open(_ entry: WorkspaceEntry) {
        if entry.isDirectory {
            selectedDirectoryPath = entry.path
            selectedEntry = nil
            toggleDirectory(entry)
        } else {
            selectedDirectoryPath = parentPath(for: entry.path)
            selectedEntry = entry
        }
    }

    private func toggleDirectory(_ entry: WorkspaceEntry) {
        guard entry.isDirectory else {
            return
        }

        if expandedDirectoryPaths.contains(entry.path) {
            expandedDirectoryPaths.remove(entry.path)
        } else {
            expandedDirectoryPaths.insert(entry.path)
            Task {
                await loadDirectory(entry.path)
            }
        }
    }

    private func loadWorkspaceRoot() async {
        await loadDirectory("", force: true)
    }

    private func reloadCurrentExplorerScope() async {
        await loadDirectory(selectedDirectoryPath, force: true)
    }

    private func loadDirectory(_ path: String, force: Bool = false) async {
        if loadingDirectoryPaths.contains(path) {
            return
        }
        if !force, listingsByPath[path] != nil {
            return
        }

        isLoading = true
        loadingDirectoryPaths.insert(path)
        errorMessage = nil
        defer {
            loadingDirectoryPaths.remove(path)
            isLoading = !loadingDirectoryPaths.isEmpty
        }

        do {
            let listing = try await viewModel.loadWorkspaceListing(path: path)
            if path.isEmpty {
                rootListing = listing
            }
            listingsByPath[path] = listing
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func handleImport(_ result: Result<[URL], Error>) async {
        do {
            let urls = try result.get()
            guard !urls.isEmpty else {
                return
            }
            let count = try await viewModel.uploadWorkspaceFiles(fileURLs: urls, targetPath: selectedDirectoryPath)
            statusMessage = "Uploaded \(count) item\(count == 1 ? "" : "s")"
            await loadDirectory(selectedDirectoryPath, force: true)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func download(path: String, name: String) async {
        do {
            let url = try await viewModel.downloadWorkspaceArchive(path: path, suggestedName: name)
            NSWorkspace.shared.activateFileViewerSelecting([url])
            statusMessage = "Downloaded \(name)"
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func movePath(from path: String, to newPath: String) async {
        do {
            let trimmed = newPath.trimmingCharacters(in: .whitespacesAndNewlines)
            try await viewModel.moveWorkspacePath(path, to: trimmed)
            if selectedEntry?.path == path {
                selectedEntry = nil
            }
            statusMessage = "Moved to \(trimmed)"
            await reloadAfterWorkspaceMutation(originalPath: path, newPath: trimmed)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func delete(_ entry: WorkspaceEntry) async {
        do {
            try await viewModel.deleteWorkspacePath(entry.path)
            if selectedEntry?.id == entry.id {
                selectedEntry = nil
            }
            statusMessage = "Deleted \(entry.name)"
            await reloadAfterWorkspaceMutation(originalPath: entry.path)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func reloadAfterWorkspaceMutation(originalPath: String, newPath: String? = nil) async {
        let affectedPaths = Set([
            parentPath(for: originalPath),
            newPath.map(parentPath(for:))
        ].compactMap(\.self))

        for path in affectedPaths {
            await loadDirectory(path, force: true)
        }
    }

    private func sortedEntries(_ entries: [WorkspaceEntry]) -> [WorkspaceEntry] {
        entries.sorted { left, right in
            if left.isDirectory != right.isDirectory {
                return left.isDirectory && !right.isDirectory
            }
            return left.name.localizedStandardCompare(right.name) == .orderedAscending
        }
    }

    private func parentPath(for path: String) -> String {
        let parts = path.split(separator: "/").map(String.init)
        guard parts.count > 1 else {
            return ""
        }
        return parts.dropLast().joined(separator: "/")
    }
}

private struct MacWorkspaceEntryRow: View {
    let entry: WorkspaceEntry
    let depth: Int
    let isSelected: Bool
    let isExpanded: Bool
    let isLoading: Bool
    let toggleDirectory: () -> Void

    var body: some View {
        HStack(spacing: 6) {
            Color.clear
                .frame(width: CGFloat(depth) * 14)

            if entry.isDirectory {
                Button(action: toggleDirectory) {
                    Image(systemName: isExpanded ? "chevron.down" : "chevron.right")
                        .font(.system(size: 10, weight: .bold))
                        .foregroundStyle(.secondary)
                        .frame(width: 14, height: 18)
                }
                .buttonStyle(.plain)
            } else {
                Color.clear
                    .frame(width: 14, height: 18)
            }

            Image(systemName: icon)
                .foregroundStyle(entry.isDirectory ? Color.accentColor : .secondary)
                .frame(width: 18)

            VStack(alignment: .leading, spacing: 2) {
                Text(entry.name)
                    .lineLimit(1)
                    .font(.callout)
            }

            Spacer(minLength: 0)

            if isLoading {
                ProgressView()
                    .controlSize(.small)
            } else if !entry.isDirectory, !sizeLabel.isEmpty {
                Text(sizeLabel)
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(.leading, 10)
        .padding(.trailing, 8)
        .padding(.vertical, 4)
        .frame(minHeight: 26)
        .background {
            if isSelected {
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .fill(Color.accentColor.opacity(0.15))
            }
        }
        .padding(.horizontal, 6)
    }

    private var icon: String {
        if entry.isDirectory {
            return "folder"
        }
        if macIsImagePath(entry.path) {
            return "photo"
        }
        if macIsMarkdownPath(entry.path) {
            return "doc.richtext"
        }
        return "doc.text"
    }

    private var sizeLabel: String {
        guard let bytes = entry.sizeBytes else {
            return ""
        }
        return ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
    }
}

private struct MacWorkspaceFilePreviewPane: View {
    @ObservedObject var viewModel: AppViewModel
    let entry: WorkspaceEntry?
    let onWorkspaceChanged: () -> Void

    @State private var file: WorkspaceFile?
    @State private var isLoading = false
    @State private var errorMessage: String?
    @State private var moveDraft = MacWorkspaceMoveDraft()
    @State private var isDeletePresented = false
    @State private var markdownMode: MacMarkdownPreviewMode = .preview

    var body: some View {
        VStack(spacing: 0) {
            previewToolbar

            Divider()

            Group {
                if entry == nil {
                    ContentUnavailableView("Select a File", systemImage: "doc.text.magnifyingglass")
                } else if isLoading && file == nil {
                    ProgressView("Loading file")
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else if let file {
                    MacWorkspaceFilePreviewContent(file: file, markdownMode: $markdownMode)
                } else {
                    ContentUnavailableView(
                        "Preview Unavailable",
                        systemImage: "doc.questionmark",
                        description: Text(errorMessage ?? "This file cannot be previewed.")
                    )
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .task(id: entry?.id) {
            await loadFile()
        }
        .alert("Rename or Move", isPresented: $moveDraft.isPresented) {
            TextField("Destination path", text: $moveDraft.newPath)
            Button("Cancel", role: .cancel) {
                moveDraft = MacWorkspaceMoveDraft()
            }
            Button("Apply") {
                Task {
                    await movePath(from: moveDraft.path, to: moveDraft.newPath)
                    moveDraft = MacWorkspaceMoveDraft()
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
                Task { await deleteFile() }
            }
        } message: {
            Text("This permanently deletes \(entry?.path ?? "this file").")
        }
    }

    private var previewToolbar: some View {
        HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                Text(entry?.name ?? "Preview")
                    .font(.headline)
                    .lineLimit(1)
                Text(entry?.path ?? "Choose a file from the workspace")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }

            Spacer()

            if isLoading {
                ProgressView()
                    .controlSize(.small)
            }

            Button {
                Task { await loadFile(force: true) }
            } label: {
                Image(systemName: "arrow.clockwise")
            }
            .disabled(entry == nil || isLoading)
            .help("Reload")

            Button {
                Task { await downloadSelectedFile() }
            } label: {
                Image(systemName: "square.and.arrow.down")
            }
            .disabled(entry == nil)
            .help("Download")

            Menu {
                Button {
                    if let entry {
                        moveDraft = MacWorkspaceMoveDraft(path: entry.path)
                    }
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
            .disabled(entry == nil)
            .menuStyle(.borderlessButton)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private func loadFile(force: Bool = false) async {
        guard let entry else {
            file = nil
            errorMessage = nil
            return
        }
        if isLoading {
            return
        }
        errorMessage = nil
        let image = macIsImagePath(entry.path)
        if !image, let sizeBytes = entry.sizeBytes, sizeBytes > 25_000_000 {
            file = nil
            errorMessage = "Preview is limited to 25 MB for this file type. Download it to open locally."
            return
        }

        isLoading = true
        defer { isLoading = false }
        do {
            file = try await viewModel.loadWorkspaceFile(
                path: entry.path,
                previewLimitBytes: image ? 1 : 2_000_000,
                full: image
            )
        } catch {
            file = nil
            errorMessage = error.localizedDescription
        }
    }

    private func downloadSelectedFile() async {
        guard let entry else {
            return
        }
        do {
            let url = try await viewModel.downloadWorkspaceArchive(path: entry.path, suggestedName: entry.name)
            NSWorkspace.shared.activateFileViewerSelecting([url])
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func movePath(from path: String, to newPath: String) async {
        do {
            let trimmed = newPath.trimmingCharacters(in: .whitespacesAndNewlines)
            try await viewModel.moveWorkspacePath(path, to: trimmed)
            file = nil
            onWorkspaceChanged()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func deleteFile() async {
        guard let entry else {
            return
        }
        do {
            try await viewModel.deleteWorkspacePath(entry.path)
            file = nil
            onWorkspaceChanged()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

private enum MacMarkdownPreviewMode: String, CaseIterable {
    case preview = "Preview"
    case source = "Source"
}

private struct MacWorkspaceFilePreviewContent: View {
    let file: WorkspaceFile
    @Binding var markdownMode: MacMarkdownPreviewMode

    var body: some View {
        VStack(spacing: 0) {
            MacWorkspaceFileMetadataBar(file: file)

            Divider()

            if macIsMarkdownPath(file.path) {
                Picker("Markdown Mode", selection: $markdownMode) {
                    ForEach(MacMarkdownPreviewMode.allCases, id: \.self) { mode in
                        Text(mode.rawValue).tag(mode)
                    }
                }
                .pickerStyle(.segmented)
                .frame(width: 220)
                .padding(.vertical, 10)

                Divider()

                if markdownMode == .preview {
                    ScrollView {
                        MarkdownContentView(text: file.data, compact: false)
                            .padding(18)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                } else {
                    MacWorkspaceSourceCodeView(text: file.data, language: macLanguageForPath(file.path))
                }
            } else if macIsImagePath(file.path), let data = file.decodedData, let image = NSImage(data: data) {
                ScrollView([.vertical, .horizontal]) {
                    Image(nsImage: image)
                        .resizable()
                        .scaledToFit()
                        .padding(18)
                        .frame(maxWidth: .infinity)
                }
                .background(Color.black.opacity(0.05))
            } else if file.isText {
                MacWorkspaceSourceCodeView(text: file.data, language: macLanguageForPath(file.path))
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

private struct MacWorkspaceFileMetadataBar: View {
    let file: WorkspaceFile

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: macIsImagePath(file.path) ? "photo" : (macIsMarkdownPath(file.path) ? "doc.richtext" : "doc.text"))
                .font(.headline)
                .foregroundStyle(Color.accentColor)
                .frame(width: 24)

            VStack(alignment: .leading, spacing: 2) {
                Text(file.path)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)
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

private struct MacWorkspaceSourceCodeView: View {
    let text: String
    let language: String?

    var body: some View {
        ScrollView([.vertical, .horizontal]) {
            Text(MacSyntaxHighlighter.highlight(text, language: language))
                .font(.system(.footnote, design: .monospaced))
                .textSelection(.enabled)
                .padding(14)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(PlatformColor.controlBackground)
    }
}

private struct MacWorkspaceMoveDraft {
    var path = ""
    var newPath = ""

    init() {
    }

    init(path: String) {
        self.path = path
        self.newPath = path
    }

    var isPresented: Bool {
        get { !path.isEmpty }
        set {
            if !newValue {
                path = ""
                newPath = ""
            }
        }
    }
}

private func macIsImagePath(_ path: String) -> Bool {
    ["png", "jpg", "jpeg", "gif", "webp", "heic"].contains(macFileExtension(path))
}

private func macIsMarkdownPath(_ path: String) -> Bool {
    ["md", "markdown", "mdown", "mkd"].contains(macFileExtension(path))
}

private func macLanguageForPath(_ path: String) -> String? {
    switch macFileExtension(path) {
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

private func macFileExtension(_ path: String) -> String {
    path.split(separator: ".").last.map { String($0).lowercased() } ?? ""
}

private enum MacSyntaxHighlighter {
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
