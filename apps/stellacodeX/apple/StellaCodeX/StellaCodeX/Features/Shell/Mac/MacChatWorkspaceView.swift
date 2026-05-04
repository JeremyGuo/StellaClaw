#if os(macOS)
import AppKit
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

struct MacChatWorkspaceView: View {
    @ObservedObject var viewModel: AppViewModel
    var isResizingLayout = false
    @State private var composerReservedHeight: CGFloat = 180
    @State private var bottomLayoutChangeTrigger = 0
    @State private var pendingAttachments: [ComposerAttachment] = []
    @State private var isFileImporterPresented = false
    @State private var isPhotoPickerPresented = false
    @State private var selectedPhotoItems: [PhotosPickerItem] = []
    @State private var attachmentError: String?

    var body: some View {
        ZStack(alignment: .bottom) {
            VStack(spacing: 0) {
                MessageTimelineView(
                    messages: viewModel.messages,
                    hasOlderMessages: viewModel.hasOlderMessages,
                    isLoadingMessages: viewModel.isLoadingMessages,
                    isLoadingOlderMessages: viewModel.isLoadingOlderMessages,
                    activityStatus: viewModel.realtimeStatus,
                    isConversationRunning: viewModel.selectedConversation?.status == .running,
                    turnProgress: viewModel.activeTurnProgress,
                    bottomLayoutChangeTrigger: bottomLayoutChangeTrigger,
                    loadOlderAction: {
                        Task {
                            await viewModel.loadOlderMessages()
                        }
                    },
                    inspectMessageAction: { message in
                        viewModel.inspectMessage(message)
                    },
                    inspectToolAction: { message, tool in
                        viewModel.inspectTool(message: message, tool: tool)
                    }
                )
                    .id(viewModel.selectedConversationID ?? "no-conversation")
                    .safeAreaInset(edge: .bottom) {
                        Color.clear.frame(height: viewModel.selectedConversationRequiresModel ? 0 : composerReservedHeight)
                    }
                MacCodexStatusBar(viewModel: viewModel)
            }

            if viewModel.selectedConversationRequiresModel {
                ModelSelectionGateView(viewModel: viewModel)
                    .padding(.bottom, 34)
                    .transition(.scale(scale: 0.98).combined(with: .opacity))
            } else {
                MessageComposerView(
                    text: $viewModel.composerText,
                    sendTitle: "发送",
                    sendAction: {
                        let files = pendingAttachments.map(\.file)
                        pendingAttachments = []
                        Task {
                            await viewModel.sendComposerMessage(files: files)
                        }
                    },
                    attachments: pendingAttachments,
                    addFileAction: {
                        isFileImporterPresented = true
                    },
                    addImageAction: {
                        isPhotoPickerPresented = true
                    },
                    removeAttachmentAction: { attachment in
                        pendingAttachments.removeAll { $0.id == attachment.id }
                    },
                    pasteImageAction: pasteImageFromPasteboard
                )
                .frame(maxWidth: .infinity)
                .padding(.horizontal, 26)
                .padding(.bottom, 28)
                .background {
                    GeometryReader { proxy in
                        Color.clear.preference(key: MacComposerHeightPreferenceKey.self, value: proxy.size.height)
                    }
                }
            }
        }
        .background(PlatformColor.appBackground)
        .onPreferenceChange(MacComposerHeightPreferenceKey.self) { height in
            let nextHeight = max(120, height + 24)
            guard abs(composerReservedHeight - nextHeight) > 1 else {
                return
            }
            composerReservedHeight = nextHeight
            if !isResizingLayout {
                bottomLayoutChangeTrigger += 1
            }
        }
        .onChange(of: isResizingLayout) { wasResizing, isResizing in
            if wasResizing && !isResizing {
                bottomLayoutChangeTrigger += 1
            }
        }
        .sheet(item: $viewModel.detailPresentation) { presentation in
            ChatMessageDetailView(presentation: presentation)
                .frame(minWidth: 560, idealWidth: 720, minHeight: 520, idealHeight: 700)
        }
        .photosPicker(
            isPresented: $isPhotoPickerPresented,
            selection: $selectedPhotoItems,
            maxSelectionCount: 10,
            matching: .images
        )
        .onChange(of: selectedPhotoItems) { _, items in
            guard !items.isEmpty else {
                return
            }
            Task {
                await loadPhotoPickerItems(items)
                selectedPhotoItems = []
            }
        }
        .fileImporter(isPresented: $isFileImporterPresented, allowedContentTypes: [.item], allowsMultipleSelection: true) { result in
            Task {
                await loadImportedFiles(result)
            }
        }
        .alert("Attachment Error", isPresented: Binding(
            get: { attachmentError != nil },
            set: { if !$0 { attachmentError = nil } }
        )) {
            Button("OK", role: .cancel) {
                attachmentError = nil
            }
        } message: {
            Text(attachmentError ?? "")
        }
    }

    private func loadImportedFiles(_ result: Result<[URL], Error>) async {
        do {
            let urls = try result.get()
            for url in urls {
                let accessed = url.startAccessingSecurityScopedResource()
                defer {
                    if accessed {
                        url.stopAccessingSecurityScopedResource()
                    }
                }
                let data = try Data(contentsOf: url)
                let mediaType = mediaTypeForFilename(url.lastPathComponent) ?? "application/octet-stream"
                let attachment = normalizedAttachmentData(
                    data: data,
                    name: url.lastPathComponent,
                    mediaType: mediaType
                )
                pendingAttachments.append(ComposerAttachment(file: attachment.file))
            }
        } catch {
            attachmentError = error.localizedDescription
        }
    }

    private func loadPhotoPickerItems(_ items: [PhotosPickerItem]) async {
        do {
            for (index, item) in items.enumerated() {
                guard let data = try await item.loadTransferable(type: Data.self) else {
                    continue
                }
                let type = item.supportedContentTypes.first ?? .jpeg
                let mediaType = type.preferredMIMEType ?? "application/octet-stream"
                let ext = type.preferredFilenameExtension ?? defaultExtension(for: mediaType)
                let name = "photo-\(Date().timeIntervalSince1970)-\(index + 1).\(ext)"
                let attachment = normalizedAttachmentData(data: data, name: name, mediaType: mediaType)
                pendingAttachments.append(ComposerAttachment(file: attachment.file))
            }
        } catch {
            attachmentError = error.localizedDescription
        }
    }

    private func pasteImageFromPasteboard() -> Bool {
        let pasteboard = NSPasteboard.general
        if let image = NSImage(pasteboard: pasteboard),
           let attachment = normalizedImageAttachmentData(
               image: image,
               name: "pasted-\(Int(Date().timeIntervalSince1970)).png",
               preferredMediaType: "image/png"
           ) {
            pendingAttachments.append(ComposerAttachment(file: attachment.file))
            return true
        }

        guard let fileURL = pasteboard.readObjects(forClasses: [NSURL.self], options: nil)?.first as? URL,
              let mediaType = mediaTypeForFilename(fileURL.lastPathComponent),
              mediaType.hasPrefix("image/")
        else {
            return false
        }

        do {
            let data = try Data(contentsOf: fileURL)
            let attachment = normalizedAttachmentData(
                data: data,
                name: fileURL.lastPathComponent,
                mediaType: mediaType
            )
            pendingAttachments.append(ComposerAttachment(file: attachment.file))
            return true
        } catch {
            attachmentError = error.localizedDescription
            return true
        }
    }

    private func normalizedAttachmentData(data: Data, name: String, mediaType: String) -> NormalizedMacAttachmentData {
        guard mediaType.hasPrefix("image/"),
              let image = NSImage(data: data),
              let normalized = normalizedImageAttachmentData(
                  image: image,
                  name: name,
                  preferredMediaType: mediaType
              )
        else {
            return NormalizedMacAttachmentData(
                data: data,
                name: name,
                mediaType: mediaType,
                width: nil,
                height: nil
            )
        }
        return normalized
    }

    private func normalizedImageAttachmentData(
        image: NSImage,
        name: String,
        preferredMediaType: String
    ) -> NormalizedMacAttachmentData? {
        guard let cgImage = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
            return nil
        }

        let bitmap = NSBitmapImageRep(cgImage: cgImage)
        let shouldPreservePNG = preferredMediaType == "image/png"
        let encodedData = bitmap.representation(
            using: shouldPreservePNG ? .png : .jpeg,
            properties: shouldPreservePNG ? [:] : [.compressionFactor: 0.9]
        )
        guard let encodedData else {
            return nil
        }

        return NormalizedMacAttachmentData(
            data: encodedData,
            name: replacingPathExtension(in: name, with: shouldPreservePNG ? "png" : "jpg"),
            mediaType: shouldPreservePNG ? "image/png" : "image/jpeg",
            width: cgImage.width,
            height: cgImage.height
        )
    }

    private func replacingPathExtension(in name: String, with newExtension: String) -> String {
        let base = (name as NSString).deletingPathExtension
        guard !base.isEmpty else {
            return "attachment.\(newExtension)"
        }
        return "\(base).\(newExtension)"
    }
}

private struct MacComposerHeightPreferenceKey: PreferenceKey {
    static var defaultValue: CGFloat = 0

    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = max(value, nextValue())
    }
}

private struct NormalizedMacAttachmentData {
    var data: Data
    var name: String
    var mediaType: String
    var width: Int?
    var height: Int?

    var file: OutgoingMessageFile {
        let dataURL = "data:\(mediaType);base64,\(data.base64EncodedString())"
        return OutgoingMessageFile(
            uri: dataURL,
            mediaType: mediaType,
            name: name,
            sizeBytes: data.count,
            width: width,
            height: height,
            thumbnailDataURL: mediaType.hasPrefix("image/") ? dataURL : nil
        )
    }
}

private func mediaTypeForFilename(_ filename: String) -> String? {
    let ext = filename.split(separator: ".").last.map(String.init) ?? ""
    guard !ext.isEmpty,
          let type = UTType(filenameExtension: ext)
    else {
        return nil
    }
    return type.preferredMIMEType
}

private func defaultExtension(for mediaType: String) -> String {
    UTType(mimeType: mediaType)?.preferredFilenameExtension ?? "bin"
}

private struct MacCodexStatusBar: View {
    @ObservedObject var viewModel: AppViewModel

    var body: some View {
        HStack(spacing: 16) {
            Label(viewModel.profile.connectionMode == .sshProxy ? "SSH Proxy" : "Direct", systemImage: "display")
            Label(viewModel.selectedConversation?.remote.isEmpty == false ? viewModel.selectedConversation?.remote ?? "" : "local", systemImage: "point.3.connected.trianglepath.dotted")
            Label(viewModel.realtimeStatus, systemImage: "dot.radiowaves.left.and.right")

            Spacer()

            Text(viewModel.profile.connectionSummary)
                .lineLimit(1)
        }
        .font(.caption)
        .foregroundStyle(.secondary)
        .padding(.horizontal, 18)
        .frame(height: 28)
        .background(PlatformColor.statusBackground)
        .overlay(alignment: .top) {
            Rectangle()
                .fill(PlatformColor.separator.opacity(0.45))
                .frame(height: 1)
        }
    }
}

#Preview {
    MacChatWorkspaceView(viewModel: .mock())
}
#endif
