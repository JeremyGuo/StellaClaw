#if os(iOS)
import Combine
import PhotosUI
import SwiftUI
import UIKit
import UniformTypeIdentifiers

struct IOSChatWorkspaceView: View {
    @ObservedObject var viewModel: AppViewModel
    @Environment(\.dismiss) private var dismiss
    @State private var renameName = ""
    @State private var isRenamePresented = false
    @State private var destination: IOSConversationDestination?
    @State private var pendingAttachments: [PendingMessageAttachment] = []
    @State private var isAttachmentMenuPresented = false
    @State private var isFileImporterPresented = false
    @State private var isPhotoPickerPresented = false
    @State private var isCameraPresented = false
    @State private var selectedPhotoItems: [PhotosPickerItem] = []
    @State private var attachmentError: String?
    @State private var keyboardScrollTrigger = 0

    var body: some View {
        ZStack {
            IOSChatWallpaper()

            VStack(spacing: 0) {
                MessageTimelineView(
                    messages: viewModel.messages,
                    hasOlderMessages: viewModel.hasOlderMessages,
                    isLoadingMessages: viewModel.isLoadingMessages,
                    isLoadingOlderMessages: viewModel.isLoadingOlderMessages,
                    activityStatus: viewModel.realtimeStatus,
                    isConversationRunning: viewModel.selectedConversation?.status == .running,
                    turnProgress: viewModel.activeTurnProgress,
                    bottomScrollTrigger: keyboardScrollTrigger,
                    bottomScrollRequiresNearBottom: true,
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
            }

            if viewModel.selectedConversationRequiresModel {
                ModelSelectionGateView(viewModel: viewModel)
                    .transition(.scale(scale: 0.98).combined(with: .opacity))
            }
        }
        .background(IOSInteractivePopGestureEnabler())
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillShowNotification)) { notification in
            requestKeyboardBottomScroll(notification)
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardWillChangeFrameNotification)) { notification in
            requestKeyboardBottomScroll(notification)
        }
        .onReceive(NotificationCenter.default.publisher(for: UIResponder.keyboardDidShowNotification)) { _ in
            keyboardScrollTrigger += 1
        }
        .toolbar(.hidden, for: .navigationBar)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if !viewModel.selectedConversationRequiresModel {
                IOSMessageComposerView(
                    text: $viewModel.composerText,
                    attachments: $pendingAttachments,
                    addAttachmentAction: {
                        isAttachmentMenuPresented = true
                    },
                    removeAttachmentAction: { attachment in
                        pendingAttachments.removeAll { $0.id == attachment.id }
                    },
                    cameraAction: {
                        isCameraPresented = true
                    },
                    sendAction: {
                        let files = pendingAttachments.map(\.file)
                        pendingAttachments = []
                        Task {
                            await viewModel.sendComposerMessage(files: files)
                        }
                    },
                    layoutChangeAction: {
                        requestComposerBottomScroll()
                    }
                )
            }
        }
        .safeAreaInset(edge: .top, spacing: 0) {
            IOSChatHeader(
                title: viewModel.selectedConversation?.title ?? "Conversation",
                subtitle: headerSubtitle,
                isActive: viewModel.activeTurnProgress?.isActive == true || viewModel.selectedConversation?.status == .running,
                onBack: {
                    dismiss()
                },
                onFiles: {
                    destination = .files
                },
                onTerminal: {
                    destination = .terminal
                },
                onRename: {
                    renameName = viewModel.selectedConversation?.title ?? ""
                    isRenamePresented = true
                },
                onActions: {
                    destination = .actions
                }
            )
        }
        .navigationDestination(item: $destination) { destination in
            switch destination {
            case .files:
                IOSWorkspaceFilesView(viewModel: viewModel)
            case .terminal:
                IOSTerminalSessionsView(viewModel: viewModel)
            case .actions:
                IOSConversationActionsView(
                    viewModel: viewModel,
                    onRename: {
                        renameName = viewModel.selectedConversation?.title ?? ""
                        isRenamePresented = true
                    }
                )
            }
        }
        .sheet(item: $viewModel.detailPresentation) { presentation in
            ChatMessageDetailView(presentation: presentation)
                .presentationDetents([.medium, .large])
        }
        .sheet(isPresented: $isAttachmentMenuPresented) {
            IOSAttachmentPickerPanel(
                selectedPhotoItems: $selectedPhotoItems,
                closeAction: {
                    isAttachmentMenuPresented = false
                },
                fileAction: {
                    isAttachmentMenuPresented = false
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) {
                        isFileImporterPresented = true
                    }
                }
            )
            .presentationDetents([.height(224)])
            .presentationDragIndicator(.visible)
            .presentationCornerRadius(34)
            .presentationBackground(.ultraThinMaterial)
        }
        .photosPicker(
            isPresented: $isPhotoPickerPresented,
            selection: $selectedPhotoItems,
            maxSelectionCount: 10,
            matching: .any(of: [.images, .videos])
        )
        .onChange(of: selectedPhotoItems) { _, items in
            guard !items.isEmpty else {
                return
            }
            isAttachmentMenuPresented = false
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
        .fullScreenCover(isPresented: $isCameraPresented) {
            CameraCaptureView { image in
                addCameraImage(image)
                isCameraPresented = false
            } onCancel: {
                isCameraPresented = false
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
        .alert("Rename Conversation", isPresented: $isRenamePresented) {
            TextField("Name", text: $renameName)
                .textInputAutocapitalization(.words)

            Button("Cancel", role: .cancel) {
                isRenamePresented = false
            }

            Button("Rename") {
                if let id = viewModel.selectedConversationID {
                    viewModel.renameConversation(id: id, nickname: renameName)
                }
                isRenamePresented = false
            }
            .disabled(renameName.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        } message: {
            Text("Set a display name for this conversation.")
        }
    }

    private func requestKeyboardBottomScroll(_ notification: Notification) {
        let duration = notification.userInfo?[UIResponder.keyboardAnimationDurationUserInfoKey] as? Double ?? 0.25
        keyboardScrollTrigger += 1

        DispatchQueue.main.asyncAfter(deadline: .now() + max(0.05, duration * 0.72)) {
            keyboardScrollTrigger += 1
        }
    }

    private func requestComposerBottomScroll() {
        DispatchQueue.main.async {
            keyboardScrollTrigger += 1
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.16) {
            keyboardScrollTrigger += 1
        }
    }

    private var headerSubtitle: String {
        if viewModel.selectedConversationRequiresModel {
            return "Choose a model"
        }
        if let progress = viewModel.activeTurnProgress, progress.isActive {
            return progress.subtitle.isEmpty ? progress.title.lowercased() : progress.subtitle
        }
        if viewModel.selectedConversation?.status == .running {
            return "running..."
        }
        if shouldDisplayStatus(viewModel.realtimeStatus) {
            return viewModel.realtimeStatus
        }
        let model = viewModel.selectedConversation?.model ?? ""
        return model.isEmpty ? "StellaCodeX" : model
    }

    private func shouldDisplayStatus(_ status: String) -> Bool {
        let normalized = status.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        return !normalized.isEmpty
            && normalized != "connected"
            && normalized != "subscribed"
            && normalized != "disconnected"
            && normalized != "done"
            && normalized != "done: done"
            && normalized != "completed"
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
                pendingAttachments.append(
                    PendingMessageAttachment(
                        data: attachment.data,
                        name: attachment.name,
                        mediaType: attachment.mediaType,
                        width: attachment.width,
                        height: attachment.height
                    )
                )
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
                pendingAttachments.append(
                    PendingMessageAttachment(
                        data: attachment.data,
                        name: attachment.name,
                        mediaType: attachment.mediaType,
                        width: attachment.width,
                        height: attachment.height
                    )
                )
            }
        } catch {
            attachmentError = error.localizedDescription
        }
    }

    private func addCameraImage(_ image: UIImage) {
        guard let attachment = normalizedImageAttachmentData(
            image: image,
            name: "camera-\(Int(Date().timeIntervalSince1970)).jpg",
            preferredMediaType: "image/jpeg"
        ) else {
            attachmentError = "Unable to encode camera image."
            return
        }
        pendingAttachments.append(
            PendingMessageAttachment(
                data: attachment.data,
                name: attachment.name,
                mediaType: attachment.mediaType,
                width: attachment.width,
                height: attachment.height
            )
        )
    }

    private func normalizedAttachmentData(data: Data, name: String, mediaType: String) -> NormalizedAttachmentData {
        guard mediaType.hasPrefix("image/"),
              let image = UIImage(data: data),
              let normalized = normalizedImageAttachmentData(
                image: image,
                name: name,
                preferredMediaType: mediaType
              )
        else {
            return NormalizedAttachmentData(
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
        image: UIImage,
        name: String,
        preferredMediaType: String
    ) -> NormalizedAttachmentData? {
        let format = UIGraphicsImageRendererFormat.default()
        format.scale = image.scale
        format.opaque = false
        let renderer = UIGraphicsImageRenderer(size: image.size, format: format)
        let normalized = renderer.image { _ in
            image.draw(in: CGRect(origin: .zero, size: image.size))
        }

        let shouldPreservePNG = preferredMediaType == "image/png"
        let encodedData = shouldPreservePNG
            ? normalized.pngData()
            : normalized.jpegData(compressionQuality: 0.9)
        guard let encodedData else {
            return nil
        }

        let mediaType = shouldPreservePNG ? "image/png" : "image/jpeg"
        let outputName = replacingPathExtension(
            in: name,
            with: shouldPreservePNG ? "png" : "jpg"
        )
        return NormalizedAttachmentData(
            data: encodedData,
            name: outputName,
            mediaType: mediaType,
            width: Int((normalized.size.width * normalized.scale).rounded()),
            height: Int((normalized.size.height * normalized.scale).rounded())
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

private struct IOSInteractivePopGestureEnabler: UIViewControllerRepresentable {
    func makeUIViewController(context: Context) -> Controller {
        Controller()
    }

    func updateUIViewController(_ uiViewController: Controller, context: Context) {
        uiViewController.enableInteractivePopGesture()
    }

    final class Controller: UIViewController {
        override func didMove(toParent parent: UIViewController?) {
            super.didMove(toParent: parent)
            enableInteractivePopGesture()
        }

        override func viewDidAppear(_ animated: Bool) {
            super.viewDidAppear(animated)
            enableInteractivePopGesture()
        }

        func enableInteractivePopGesture() {
            DispatchQueue.main.async { [weak self] in
                guard let navigationController = self?.nearestNavigationController() else {
                    return
                }

                navigationController.interactivePopGestureRecognizer?.isEnabled = navigationController.viewControllers.count > 1
                navigationController.interactivePopGestureRecognizer?.delegate = nil
            }
        }

        private func nearestNavigationController() -> UINavigationController? {
            var controller: UIViewController? = self
            while let current = controller {
                if let navigationController = current.navigationController {
                    return navigationController
                }
                controller = current.parent
            }
            return nil
        }
    }
}

private struct NormalizedAttachmentData {
    var data: Data
    var name: String
    var mediaType: String
    var width: Int?
    var height: Int?
}

private struct IOSChatHeader: View {
    let title: String
    let subtitle: String
    let isActive: Bool
    let onBack: () -> Void
    let onFiles: () -> Void
    let onTerminal: () -> Void
    let onRename: () -> Void
    let onActions: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            Button(action: onBack) {
                Image(systemName: "chevron.left")
                    .font(.system(size: 22, weight: .semibold))
                    .frame(width: 48, height: 48)
                    .background(.ultraThinMaterial)
                    .clipShape(Circle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Back")

            Spacer(minLength: 4)

            VStack(spacing: 1) {
                Text(title)
                    .font(.headline.weight(.semibold))
                    .lineLimit(1)

                if !subtitle.isEmpty {
                    HStack(spacing: 5) {
                        if isActive {
                            ProgressView()
                                .controlSize(.mini)
                        }

                        Text(subtitle)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                }
            }
            .padding(.horizontal, 20)
            .frame(minWidth: 128, maxWidth: 230, minHeight: 54)
            .background(.ultraThinMaterial)
            .clipShape(Capsule())
            .overlay {
                Capsule()
                    .strokeBorder(Color.primary.opacity(0.08))
            }
            .layoutPriority(1)

            Spacer(minLength: 4)

            HStack(spacing: 2) {
                Button(action: onFiles) {
                    Image(systemName: "folder")
                        .font(.system(size: 20, weight: .semibold))
                        .frame(width: 44, height: 44)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Files")

                Button(action: onTerminal) {
                    Image(systemName: "terminal")
                        .font(.system(size: 20, weight: .semibold))
                        .frame(width: 44, height: 44)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Terminal Sessions")

                Button(action: onActions) {
                    Image(systemName: "ellipsis")
                        .font(.system(size: 20, weight: .semibold))
                        .frame(width: 44, height: 44)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Conversation Actions")
            }
            .padding(4)
            .background(.ultraThinMaterial)
            .clipShape(Capsule())
            .overlay {
                Capsule()
                    .strokeBorder(Color.primary.opacity(0.08))
            }
        }
        .foregroundStyle(.primary)
        .padding(.horizontal, 16)
        .padding(.top, 7)
        .padding(.bottom, 8)
        .background {
            Rectangle()
                .fill(.ultraThinMaterial)
                .mask(
                    LinearGradient(
                        stops: [
                            .init(color: .black, location: 0),
                            .init(color: .black, location: 0.72),
                            .init(color: .clear, location: 1)
                        ],
                        startPoint: .top,
                        endPoint: .bottom
                    )
                )
                .ignoresSafeArea(edges: .top)
        }
    }
}

private struct IOSAttachmentPickerPanel: View {
    @Binding var selectedPhotoItems: [PhotosPickerItem]
    let closeAction: () -> Void
    let fileAction: () -> Void

    private let panelHorizontalPadding: CGFloat = 20
    private let optionCornerRadius: CGFloat = 24

    var body: some View {
        VStack(spacing: 18) {
            HStack {
                Button(action: closeAction) {
                    Image(systemName: "xmark")
                        .font(.system(size: 18, weight: .semibold))
                        .frame(width: 46, height: 46)
                        .background(PlatformColor.controlBackground.opacity(0.9))
                        .clipShape(Circle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Close Attachment Picker")

                Spacer()

                Text("Attach")
                    .font(.headline.weight(.semibold))

                Spacer()

                Color.clear
                    .frame(width: 46, height: 46)
            }
            .padding(.horizontal, panelHorizontalPadding)

            HStack(spacing: 14) {
                PhotosPicker(
                    selection: $selectedPhotoItems,
                    maxSelectionCount: 10,
                    matching: .any(of: [.images, .videos])
                ) {
                    attachmentOption(title: "Images", systemImage: "photo.on.rectangle.angled")
                }
                .buttonStyle(.plain)

                Button(action: fileAction) {
                    attachmentOption(title: "Files", systemImage: "doc")
                }
                .buttonStyle(.plain)
            }
            .padding(.horizontal, panelHorizontalPadding)
        }
        .padding(.top, 10)
        .padding(.bottom, 22)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(Color.clear)
    }

    private func attachmentOption(title: String, systemImage: String) -> some View {
        VStack(spacing: 8) {
            Image(systemName: systemImage)
                .font(.system(size: 26, weight: .semibold))
                .frame(width: 54, height: 54)
                .background(Color.accentColor.opacity(0.14))
                .foregroundStyle(Color.accentColor)
                .clipShape(RoundedRectangle(cornerRadius: 17, style: .continuous))

            Text(title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(.primary)
        }
        .frame(maxWidth: .infinity)
        .frame(height: 104)
        .background(PlatformColor.secondaryBackground.opacity(0.9))
        .clipShape(RoundedRectangle(cornerRadius: optionCornerRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: optionCornerRadius, style: .continuous)
                .strokeBorder(Color.primary.opacity(0.07))
        }
        .contentShape(RoundedRectangle(cornerRadius: optionCornerRadius, style: .continuous))
    }
}

private enum IOSConversationDestination: String, Identifiable {
    case files
    case terminal
    case actions

    var id: String { rawValue }
}

private struct IOSChatWallpaper: View {
    var body: some View {
        ZStack {
            PlatformColor.groupedBackground
                .ignoresSafeArea()

            GeometryReader { proxy in
                let columns = max(5, Int(proxy.size.width / 72))
                let rows = max(9, Int(proxy.size.height / 88))
                VStack(spacing: 34) {
                    ForEach(0..<rows, id: \.self) { row in
                        HStack(spacing: 38) {
                            ForEach(0..<columns, id: \.self) { column in
                                Image(systemName: wallpaperSymbols[(row + column) % wallpaperSymbols.count])
                                    .font(.system(size: 18, weight: .regular))
                                    .foregroundStyle(Color.secondary.opacity(0.055))
                                    .rotationEffect(.degrees(Double((row * 17 + column * 23) % 36) - 18))
                            }
                        }
                        .offset(x: row.isMultiple(of: 2) ? -24 : 18)
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .allowsHitTesting(false)
        }
    }

    private let wallpaperSymbols = [
        "chevron.left.forwardslash.chevron.right",
        "terminal",
        "curlybraces",
        "command",
        "doc.text",
        "folder",
        "sparkles"
    ]
}

private struct IOSMessageComposerView: View {
    @Binding var text: String
    @Binding var attachments: [PendingMessageAttachment]
    let addAttachmentAction: () -> Void
    let removeAttachmentAction: (PendingMessageAttachment) -> Void
    let cameraAction: () -> Void
    let sendAction: () -> Void
    let layoutChangeAction: () -> Void
    @State private var textViewHeight: CGFloat = 24
    @State private var isTextViewFocused = false

    var body: some View {
        VStack(spacing: 8) {
            if !attachments.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        ForEach(attachments) { attachment in
                            PendingAttachmentChip(attachment: attachment) {
                                removeAttachmentAction(attachment)
                            }
                        }
                    }
                    .padding(.horizontal, 12)
                }
            }

            HStack(alignment: .bottom, spacing: 9) {
                Button(action: addAttachmentAction) {
                    Image(systemName: "paperclip")
                        .font(.system(size: 23, weight: .semibold))
                        .frame(width: 46, height: 46)
                        .background(.ultraThinMaterial)
                        .clipShape(Circle())
                        .overlay {
                            Circle()
                                .strokeBorder(Color.primary.opacity(0.08))
                        }
                }
                .buttonStyle(.plain)
                .foregroundStyle(.primary.opacity(0.82))
                .accessibilityLabel("Add Attachment")

                HStack(alignment: .bottom, spacing: 0) {
                    ZStack(alignment: .topLeading) {
                        AutoSizingMessageTextView(
                            text: $text,
                            measuredHeight: $textViewHeight,
                            isFocused: $isTextViewFocused,
                            minHeight: 24,
                            maxHeight: 126
                        )
                        .frame(height: textViewHeight)

                        if text.isEmpty {
                            Text("Message")
                                .font(.body)
                                .foregroundStyle(.tertiary)
                                .padding(.top, 1)
                                .allowsHitTesting(false)
                        }
                    }
                    .padding(.leading, 13)
                    .padding(.trailing, 13)
                    .padding(.vertical, 11)
                }
                .frame(minHeight: 46)
                .background(.ultraThinMaterial)
                .clipShape(RoundedRectangle(cornerRadius: 23, style: .continuous))
                .overlay {
                    RoundedRectangle(cornerRadius: 23, style: .continuous)
                        .strokeBorder(inputBorderColor)
                }
                .shadow(color: Color.black.opacity(0.08), radius: 14, y: 5)
                .contentShape(RoundedRectangle(cornerRadius: 23, style: .continuous))
                .onTapGesture {
                    isTextViewFocused = true
                }

                Button {
                    if canSend {
                        sendAction()
                    } else {
                        cameraAction()
                    }
                } label: {
                    Image(systemName: canSend ? "arrow.up" : "camera.fill")
                        .font(.system(size: canSend ? 18 : 21, weight: .bold))
                        .foregroundStyle(canSend ? Color.white : Color.primary.opacity(0.82))
                        .frame(width: 46, height: 46)
                        .background(sendButtonBackground)
                        .clipShape(Circle())
                        .overlay {
                            Circle()
                                .strokeBorder(Color.primary.opacity(canSend ? 0 : 0.08))
                        }
                }
                .buttonStyle(.plain)
                .accessibilityLabel(canSend ? "Send" : "Camera")
            }
        }
        .padding(.horizontal, 12)
        .padding(.top, 9)
        .padding(.bottom, 8)
        .background {
            Rectangle()
                .fill(.ultraThinMaterial)
                .mask(
                    LinearGradient(
                        stops: [
                            .init(color: .clear, location: 0),
                            .init(color: .black, location: 0.2),
                            .init(color: .black, location: 1)
                        ],
                        startPoint: .top,
                        endPoint: .bottom
                    )
                )
                .ignoresSafeArea(edges: .bottom)
        }
        .animation(.smooth(duration: 0.18), value: canSend)
        .onChange(of: textViewHeight) { _, _ in
            layoutChangeAction()
        }
        .onChange(of: attachments.count) { _, _ in
            layoutChangeAction()
        }
        .onChange(of: isTextViewFocused) { _, focused in
            if focused {
                layoutChangeAction()
            }
        }
    }

    private var canSend: Bool {
        !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !attachments.isEmpty
    }

    private var inputBorderColor: Color {
        if isTextViewFocused {
            return Color.accentColor.opacity(0.28)
        }
        return Color.primary.opacity(0.08)
    }

    private var sendButtonBackground: some ShapeStyle {
        if canSend {
            return AnyShapeStyle(Color.accentColor)
        }
        return AnyShapeStyle(.ultraThinMaterial)
    }
}

private struct AutoSizingMessageTextView: UIViewRepresentable {
    @Binding var text: String
    @Binding var measuredHeight: CGFloat
    @Binding var isFocused: Bool
    let minHeight: CGFloat
    let maxHeight: CGFloat

    func makeUIView(context: Context) -> UITextView {
        let textView = UITextView()
        textView.delegate = context.coordinator
        textView.backgroundColor = .clear
        textView.font = .preferredFont(forTextStyle: .body)
        textView.adjustsFontForContentSizeCategory = true
        textView.textContainerInset = .zero
        textView.textContainer.lineFragmentPadding = 0
        textView.isScrollEnabled = false
        textView.showsVerticalScrollIndicator = false
        textView.returnKeyType = .default
        textView.keyboardDismissMode = .interactive
        textView.autocorrectionType = .no
        textView.spellCheckingType = .no
        textView.smartDashesType = .no
        textView.smartQuotesType = .no
        textView.smartInsertDeleteType = .no
        textView.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        textView.inputAssistantItem.leadingBarButtonGroups = []
        textView.inputAssistantItem.trailingBarButtonGroups = []
        return textView
    }

    func updateUIView(_ textView: UITextView, context: Context) {
        context.coordinator.update(parent: self)

        if textView.text != text {
            textView.text = text
        }
        textView.isScrollEnabled = measuredHeight >= maxHeight

        if isFocused, !textView.isFirstResponder {
            textView.becomeFirstResponder()
        } else if !isFocused, textView.isFirstResponder {
            textView.resignFirstResponder()
        }

        context.coordinator.recalculateHeight(for: textView)
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(parent: self)
    }

    final class Coordinator: NSObject, UITextViewDelegate {
        private var parent: AutoSizingMessageTextView

        init(parent: AutoSizingMessageTextView) {
            self.parent = parent
        }

        func update(parent: AutoSizingMessageTextView) {
            self.parent = parent
        }

        func textViewDidBeginEditing(_ textView: UITextView) {
            setFocused(true)
        }

        func textViewDidEndEditing(_ textView: UITextView) {
            setFocused(false)
        }

        func textViewDidChange(_ textView: UITextView) {
            setText(textView.text)
            recalculateHeight(for: textView)
        }

        func recalculateHeight(for textView: UITextView) {
            let width = textView.bounds.width
            guard width > 0 else {
                return
            }

            let fittingSize = CGSize(width: width, height: .greatestFiniteMagnitude)
            let nextHeight = min(max(ceil(textView.sizeThatFits(fittingSize).height), parent.minHeight), parent.maxHeight)
            guard abs(parent.measuredHeight - nextHeight) > 0.5 else {
                return
            }

            DispatchQueue.main.async {
                guard abs(self.parent.measuredHeight - nextHeight) > 0.5 else {
                    return
                }
                self.parent.measuredHeight = nextHeight
                textView.isScrollEnabled = nextHeight >= self.parent.maxHeight
            }
        }

        private func setText(_ value: String) {
            guard parent.text != value else {
                return
            }
            DispatchQueue.main.async {
                guard self.parent.text != value else {
                    return
                }
                self.parent.text = value
            }
        }

        private func setFocused(_ value: Bool) {
            guard parent.isFocused != value else {
                return
            }
            DispatchQueue.main.async {
                guard self.parent.isFocused != value else {
                    return
                }
                self.parent.isFocused = value
            }
        }
    }
}

private struct PendingMessageAttachment: Identifiable, Hashable {
    let id = UUID()
    var file: OutgoingMessageFile

    init(data: Data, name: String, mediaType: String, width: Int? = nil, height: Int? = nil) {
        let dataURL = "data:\(mediaType);base64,\(data.base64EncodedString())"
        self.file = OutgoingMessageFile(
            uri: dataURL,
            mediaType: mediaType,
            name: name,
            sizeBytes: data.count,
            width: width,
            height: height,
            thumbnailDataURL: mediaType.hasPrefix("image/") ? dataURL : nil
        )
    }

    var name: String {
        file.name ?? "attachment"
    }

    var mediaType: String {
        file.mediaType ?? "application/octet-stream"
    }

    var formattedSize: String {
        ByteCountFormatter.string(fromByteCount: Int64(file.sizeBytes ?? 0), countStyle: .file)
    }

    var thumbnail: UIImage? {
        guard let thumbnailDataURL = file.thumbnailDataURL,
              let comma = thumbnailDataURL.firstIndex(of: ","),
              let data = Data(base64Encoded: String(thumbnailDataURL[thumbnailDataURL.index(after: comma)...]))
        else {
            return nil
        }
        return UIImage(data: data)
    }
}

private struct PendingAttachmentChip: View {
    let attachment: PendingMessageAttachment
    let remove: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            thumbnail

            VStack(alignment: .leading, spacing: 2) {
                Text(attachment.name)
                    .font(.caption.weight(.semibold))
                    .lineLimit(1)
                Text(attachment.formattedSize)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: 150, alignment: .leading)

            Button(action: remove) {
                Image(systemName: "xmark.circle.fill")
                    .font(.system(size: 17, weight: .semibold))
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 7)
        .background(.ultraThinMaterial)
        .clipShape(Capsule())
        .overlay {
            Capsule()
                .strokeBorder(Color.primary.opacity(0.08))
        }
    }

    @ViewBuilder
    private var thumbnail: some View {
        if let image = attachment.thumbnail {
            Image(uiImage: image)
                .resizable()
                .scaledToFill()
                .frame(width: 34, height: 34)
                .clipShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
        } else {
            Image(systemName: iconName)
                .font(.system(size: 16, weight: .semibold))
                .frame(width: 34, height: 34)
                .background(PlatformColor.controlBackground)
                .clipShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
        }
    }

    private var iconName: String {
        if attachment.mediaType.hasPrefix("image/") {
            return "photo"
        }
        if attachment.mediaType.contains("pdf") || attachment.name.lowercased().hasSuffix(".pdf") {
            return "doc.richtext"
        }
        return "doc"
    }
}

private struct CameraCaptureView: UIViewControllerRepresentable {
    let onImage: (UIImage) -> Void
    let onCancel: () -> Void

    func makeUIViewController(context: Context) -> UIImagePickerController {
        let picker = UIImagePickerController()
        picker.sourceType = .camera
        picker.cameraCaptureMode = .photo
        picker.delegate = context.coordinator
        return picker
    }

    func updateUIViewController(_ uiViewController: UIImagePickerController, context: Context) {
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(onImage: onImage, onCancel: onCancel)
    }

    final class Coordinator: NSObject, UINavigationControllerDelegate, UIImagePickerControllerDelegate {
        let onImage: (UIImage) -> Void
        let onCancel: () -> Void

        init(onImage: @escaping (UIImage) -> Void, onCancel: @escaping () -> Void) {
            self.onImage = onImage
            self.onCancel = onCancel
        }

        func imagePickerController(_ picker: UIImagePickerController, didFinishPickingMediaWithInfo info: [UIImagePickerController.InfoKey: Any]) {
            if let image = info[.originalImage] as? UIImage {
                onImage(image)
            } else {
                onCancel()
            }
        }

        func imagePickerControllerDidCancel(_ picker: UIImagePickerController) {
            onCancel()
        }
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

private struct IOSTerminalSessionsView: View {
    @ObservedObject var viewModel: AppViewModel
    @StateObject private var model = IOSTerminalSessionsModel()
    @State private var command = ""

    var body: some View {
        VStack(spacing: 0) {
            terminalBody

            if model.activeTerminal != nil {
                IOSTerminalInputBar(
                    command: $command,
                    isConnected: model.isConnected,
                    sendAction: {
                        let submitted = command
                        command = ""
                        Task {
                            await model.sendCommand(submitted)
                        }
                    }
                )
            }
        }
        .background(Color.black.ignoresSafeArea())
        .navigationTitle("Terminal")
        .navigationBarTitleDisplayMode(.inline)
        .landscapeTerminalRequirement()
        .toolbar {
            ToolbarItem(placement: .principal) {
                IOSTerminalSessionPicker(model: model)
            }

            ToolbarItemGroup(placement: .navigationBarTrailing) {
                Button {
                    Task {
                        await model.refresh()
                    }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .disabled(model.isLoading)
                .accessibilityLabel("Refresh Terminal Sessions")

                Button {
                    Task {
                        await model.createTerminal()
                    }
                } label: {
                    Image(systemName: "plus")
                }
                .accessibilityLabel("New Terminal Session")

                Button(role: .destructive) {
                    Task {
                        await model.deleteActiveTerminal()
                    }
                } label: {
                    Image(systemName: "trash")
                }
                .disabled(model.activeTerminal == nil)
                .accessibilityLabel("Delete Terminal Session")
            }
        }
        .task {
            await model.configure(viewModel: viewModel)
        }
        .onDisappear {
            model.close()
        }
        .alert("Terminal Error", isPresented: Binding(
            get: { model.errorMessage != nil },
            set: { if !$0 { model.errorMessage = nil } }
        )) {
            Button("OK", role: .cancel) {
                model.errorMessage = nil
            }
        } message: {
            Text(model.errorMessage ?? "")
        }
    }

    @ViewBuilder
    private var terminalBody: some View {
        if model.isLoading && model.activeTerminal == nil {
            ProgressView("Opening terminal...")
                .foregroundStyle(.white)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if model.terminals.isEmpty {
            ContentUnavailableView {
                Label("No Terminal Sessions", systemImage: "terminal")
            } description: {
                Text("Create a session connected to this conversation workspace.")
            } actions: {
                Button("New Terminal") {
                    Task {
                        await model.createTerminal()
                    }
                }
                .buttonStyle(.borderedProminent)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(PlatformColor.groupedBackground)
        } else {
            IOSTerminalScreenView(
                text: model.screenText,
                attributedText: model.screenAttributedText,
                statusText: model.statusText,
                droppedBytes: model.droppedBytes,
                onViewportChange: { size in
                    model.updateViewport(size)
                }
            )
        }
    }
}

private struct IOSTerminalSessionPicker: View {
    @ObservedObject var model: IOSTerminalSessionsModel

    var body: some View {
        Menu {
            ForEach(model.terminals) { terminal in
                Button {
                    Task {
                        await model.selectTerminal(terminal)
                    }
                } label: {
                    HStack {
                        Text(displayName(for: terminal))
                        if terminal.id == model.activeTerminal?.id {
                            Image(systemName: "checkmark")
                        }
                    }
                }
            }
        } label: {
            HStack(spacing: 6) {
                Text(model.activeTerminal.map(displayName(for:)) ?? "Terminal")
                    .font(.headline.weight(.semibold))
                    .lineLimit(1)
                Image(systemName: "chevron.down")
                    .font(.caption.weight(.bold))
            }
            .foregroundStyle(.primary)
        }
        .disabled(model.terminals.isEmpty)
    }

    private func displayName(for terminal: TerminalSummary) -> String {
        let shortID = terminal.terminalID.split(separator: "-").last.map(String.init) ?? terminal.terminalID
        let shellName = terminal.shell.split(separator: "/").last.map(String.init) ?? terminal.shell
        return "\(shellName) \(shortID.prefix(6))"
    }
}

private struct LandscapeTerminalRequirementModifier: ViewModifier {
    func body(content: Content) -> some View {
        GeometryReader { proxy in
            ZStack {
                content

                if proxy.size.height > proxy.size.width {
                    Color.black
                        .ignoresSafeArea()

                    VStack(spacing: 14) {
                        Image(systemName: "iphone.landscape")
                            .font(.system(size: 36, weight: .semibold))
                        Text("Rotate to Landscape")
                            .font(.headline.weight(.semibold))
                        Text("Terminal sessions use a landscape viewport.")
                            .font(.subheadline)
                            .foregroundStyle(.white.opacity(0.68))
                    }
                    .foregroundStyle(.white)
                    .multilineTextAlignment(.center)
                    .padding(24)
                }
            }
        }
        .onAppear {
            IOSOrientationLock.lockLandscape()
        }
        .onDisappear {
            IOSOrientationLock.unlockDefault()
        }
    }
}

private extension View {
    func landscapeTerminalRequirement() -> some View {
        modifier(LandscapeTerminalRequirementModifier())
    }
}

private struct IOSTerminalScreenView: View {
    private let terminalFontSize: CGFloat = 13
    private let terminalCellWidth: CGFloat = 8.2
    private let terminalLineHeight: CGFloat = 17.5
    private let terminalContentInset: CGFloat = 14

    let text: String
    let attributedText: AttributedString
    let statusText: String
    let droppedBytes: UInt64
    let onViewportChange: (TerminalViewportSize) -> Void

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    if !statusText.isEmpty || droppedBytes > 0 {
                        HStack(spacing: 8) {
                            Circle()
                                .fill(statusText == "connected" ? Color.green : Color.orange)
                                .frame(width: 8, height: 8)
                            Text(statusText.isEmpty ? "terminal" : statusText)
                            if droppedBytes > 0 {
                                Text("dropped \(droppedBytes) bytes")
                                    .foregroundStyle(.orange)
                            }
                        }
                        .font(.caption.monospaced())
                        .foregroundStyle(.white.opacity(0.72))
                    }

                    Text(text.isEmpty ? AttributedString(" ") : attributedText)
                        .font(.system(size: terminalFontSize, weight: .regular, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)

                    Color.clear
                        .frame(height: 1)
                        .id("terminal-bottom")
                }
                .padding(terminalContentInset)
            }
            .background(Color.black)
            .background {
                GeometryReader { geometry in
                    Color.clear.preference(
                        key: TerminalViewportSizePreferenceKey.self,
                        value: viewportSize(for: geometry.size)
                    )
                }
            }
            .onPreferenceChange(TerminalViewportSizePreferenceKey.self) { size in
                onViewportChange(size)
            }
            .onChange(of: text) { _, _ in
                withAnimation(.easeOut(duration: 0.16)) {
                    proxy.scrollTo("terminal-bottom", anchor: .bottom)
                }
            }
            .onAppear {
                proxy.scrollTo("terminal-bottom", anchor: .bottom)
            }
        }
    }

    private func viewportSize(for size: CGSize) -> TerminalViewportSize {
        let horizontalInsets = terminalContentInset * 2
        let verticalInsets = terminalContentInset * 2 + (statusText.isEmpty && droppedBytes == 0 ? 0 : 28)
        let cols = Int(((size.width - horizontalInsets) / terminalCellWidth).rounded(.down))
        let rows = Int(((size.height - verticalInsets) / terminalLineHeight).rounded(.down))
        return TerminalViewportSize(cols: min(max(cols, 24), 140), rows: min(max(rows, 8), 90))
    }
}

private struct TerminalViewportSize: Equatable {
    var cols: Int
    var rows: Int
}

private struct TerminalViewportSizePreferenceKey: PreferenceKey {
    static var defaultValue = TerminalViewportSize(cols: 100, rows: 32)

    static func reduce(value: inout TerminalViewportSize, nextValue: () -> TerminalViewportSize) {
        value = nextValue()
    }
}

private struct IOSTerminalInputBar: View {
    @Binding var command: String
    let isConnected: Bool
    let sendAction: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            TextField("Command", text: $command, axis: .vertical)
                .font(.system(size: 15, weight: .regular, design: .monospaced))
                .lineLimit(1...4)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .padding(.horizontal, 13)
                .padding(.vertical, 10)
                .background(Color.white.opacity(0.10))
                .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
                .overlay {
                    RoundedRectangle(cornerRadius: 18, style: .continuous)
                        .strokeBorder(Color.white.opacity(0.10))
                }
                .submitLabel(.send)
                .onSubmit {
                    guard canSend else {
                        return
                    }
                    sendAction()
                }

            Button(action: sendAction) {
                Image(systemName: "arrow.up")
                    .font(.system(size: 17, weight: .bold))
                    .frame(width: 38, height: 38)
                    .background(canSend ? Color.accentColor : Color.white.opacity(0.14))
                    .foregroundStyle(canSend ? Color.white : Color.white.opacity(0.45))
                    .clipShape(Circle())
            }
            .buttonStyle(.plain)
            .disabled(!canSend)
            .accessibilityLabel("Send Command")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(.ultraThinMaterial)
    }

    private var canSend: Bool {
        isConnected && !command.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}

@MainActor
private final class IOSTerminalSessionsModel: ObservableObject {
    @Published var terminals: [TerminalSummary] = []
    @Published var activeTerminal: TerminalSummary?
    @Published var screenText = ""
    @Published var screenAttributedText = AttributedString(" ")
    @Published var statusText = ""
    @Published var isConnected = false
    @Published var isLoading = false
    @Published var droppedBytes: UInt64 = 0
    @Published var errorMessage: String?

    private weak var viewModel: AppViewModel?
    private var conversationID: ConversationSummary.ID?
    private var websocket: TerminalWebSocketSession?
    private var streamTask: Task<Void, Never>?
    private var screen = XtermScreenBuffer(cols: 100, rows: 32)
    private var viewportSize = TerminalViewportSize(cols: 100, rows: 32)
    private var nextOffset: UInt64 = 0
    private var cachedRawOutput = Data()

    func configure(viewModel: AppViewModel) async {
        guard self.viewModel !== viewModel || conversationID != viewModel.selectedConversationID else {
            return
        }
        self.viewModel = viewModel
        conversationID = viewModel.selectedConversationID
        await refresh(selectExisting: true)
    }

    func refresh(selectExisting: Bool = false) async {
        guard let viewModel else {
            return
        }
        isLoading = true
        defer {
            isLoading = false
        }

        do {
            let updated = try await viewModel.listTerminals()
            terminals = updated.sorted { $0.updatedMS > $1.updatedMS }

            if let current = activeTerminal,
               let replacement = terminals.first(where: { $0.id == current.id }) {
                activeTerminal = replacement
            } else if selectExisting || activeTerminal == nil {
                activeTerminal = terminals.first
            }

            if let activeTerminal {
                await selectTerminal(activeTerminal)
            } else {
                close()
                screenText = ""
                screenAttributedText = AttributedString(" ")
                statusText = ""
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func createTerminal() async {
        guard let viewModel else {
            return
        }
        isLoading = true
        defer {
            isLoading = false
        }

        do {
            let terminal = try await viewModel.createTerminal(
                options: TerminalCreateOptions(shell: nil, cwd: nil, cols: viewportSize.cols, rows: viewportSize.rows)
            )
            terminals.insert(terminal, at: 0)
            await selectTerminal(terminal)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func deleteActiveTerminal() async {
        guard let viewModel,
              let activeTerminal
        else {
            return
        }

        do {
            _ = try await viewModel.terminateTerminal(id: activeTerminal.id)
            TerminalOutputCacheStore.remove(conversationID: activeTerminal.conversationID, terminalID: activeTerminal.id)
            terminals.removeAll { $0.id == activeTerminal.id }
            close()
            self.activeTerminal = terminals.first
            if let next = self.activeTerminal {
                await selectTerminal(next)
            } else {
                screenText = ""
                screenAttributedText = AttributedString(" ")
                statusText = ""
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func selectTerminal(_ terminal: TerminalSummary) async {
        guard let viewModel else {
            return
        }

        close()
        activeTerminal = terminal
        isConnected = false
        statusText = "connecting..."
        droppedBytes = 0

        let cached = TerminalOutputCacheStore.load(conversationID: terminal.conversationID, terminalID: terminal.id)
        let initialSize = TerminalViewportSize(
            cols: max(viewportSize.cols, 24),
            rows: max(viewportSize.rows, 8)
        )
        screen = XtermScreenBuffer(cols: initialSize.cols, rows: initialSize.rows)
        if let cached,
           cached.hasUsableContent,
           cached.nextOffset <= terminal.nextOffset {
            cachedRawOutput = cached.rawData
            if cachedRawOutput.isEmpty {
                screen.replace(with: cached.text)
            } else {
                screen.feed(cachedRawOutput)
            }
            nextOffset = cached.nextOffset
        } else {
            if cached != nil {
                TerminalOutputCacheStore.remove(conversationID: terminal.conversationID, terminalID: terminal.id)
            }
            cachedRawOutput = Data()
            nextOffset = 0
        }
        screenText = screen.renderedText
        screenAttributedText = screen.renderedAttributedText

        do {
            let session = try await viewModel.openTerminalSession(id: terminal.id, offset: nextOffset)
            websocket = session
            streamTask = Task { [weak self, session] in
                do {
                    for try await event in session.events {
                        self?.handle(event, terminal: terminal)
                    }
                } catch {
                    await MainActor.run {
                        self?.isConnected = false
                        self?.statusText = "disconnected"
                        self?.errorMessage = error.localizedDescription
                    }
                }
            }
            session.resize(cols: initialSize.cols, rows: initialSize.rows)
        } catch {
            isConnected = false
            statusText = "disconnected"
            errorMessage = error.localizedDescription
        }
    }

    func updateViewport(_ size: TerminalViewportSize) {
        guard viewportSize != size else {
            return
        }
        viewportSize = size
        websocket?.resize(cols: size.cols, rows: size.rows)

        guard activeTerminal != nil else {
            return
        }

        if cachedRawOutput.isEmpty {
            screen.resize(cols: size.cols, rows: size.rows)
        } else {
            screen = XtermScreenBuffer(cols: size.cols, rows: size.rows)
            screen.feed(cachedRawOutput)
        }
        screenText = screen.renderedText
        screenAttributedText = screen.renderedAttributedText
    }

    func sendCommand(_ command: String) async {
        let trimmed = command.trimmingCharacters(in: .newlines)
        guard !trimmed.isEmpty else {
            return
        }

        websocket?.sendInput(trimmed + "\n")
    }

    func close() {
        streamTask?.cancel()
        streamTask = nil
        websocket?.close()
        websocket = nil
        isConnected = false
    }

    private func handle(_ event: TerminalStreamEvent, terminal: TerminalSummary) {
        switch event {
        case let .attached(offset, running):
            nextOffset = max(nextOffset, offset)
            isConnected = running
            statusText = running ? "connected" : "exited"
        case let .output(data):
            nextOffset += UInt64(data.count)
            cachedRawOutput.append(data)
            if cachedRawOutput.count > 500_000 {
                cachedRawOutput.removeFirst(cachedRawOutput.count - 500_000)
            }
            screen.feed(data)
            screenText = screen.renderedText
            screenAttributedText = screen.renderedAttributedText
            TerminalOutputCacheStore.save(
                TerminalCacheSnapshot(text: screenText, nextOffset: nextOffset, rawData: cachedRawOutput),
                conversationID: terminal.conversationID,
                terminalID: terminal.id
            )
        case let .dropped(bytes):
            droppedBytes += bytes
            statusText = "replay gap"
        case .exit:
            isConnected = false
            statusText = "exited"
        case let .detached(reason):
            isConnected = false
            statusText = reason
        case let .error(message):
            isConnected = false
            statusText = "error"
            errorMessage = message
        }
    }
}

private struct TerminalCacheSnapshot: Codable {
    var text: String
    var nextOffset: UInt64
    var rawBase64: String?

    var hasUsableContent: Bool {
        !rawData.isEmpty || !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var rawData: Data {
        get {
            rawBase64.flatMap { Data(base64Encoded: $0) } ?? Data()
        }
        set {
            rawBase64 = newValue.isEmpty ? nil : newValue.base64EncodedString()
        }
    }

    init(text: String, nextOffset: UInt64, rawData: Data = Data()) {
        self.text = text
        self.nextOffset = nextOffset
        self.rawBase64 = rawData.isEmpty ? nil : rawData.base64EncodedString()
    }
}

private enum TerminalOutputCacheStore {
    static func load(conversationID: String, terminalID: String) -> TerminalCacheSnapshot? {
        guard let data = try? Data(contentsOf: cacheURL(conversationID: conversationID, terminalID: terminalID)) else {
            return nil
        }
        return try? JSONDecoder().decode(TerminalCacheSnapshot.self, from: data)
    }

    static func save(_ snapshot: TerminalCacheSnapshot, conversationID: String, terminalID: String) {
        do {
            let directory = cacheDirectory().appendingPathComponent(safePathComponent(conversationID), isDirectory: true)
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            let trimmed = snapshot.text.suffix(400_000)
            var raw = snapshot.rawData
            if raw.count > 500_000 {
                raw.removeFirst(raw.count - 500_000)
            }
            let payload = TerminalCacheSnapshot(text: String(trimmed), nextOffset: snapshot.nextOffset, rawData: raw)
            let data = try JSONEncoder().encode(payload)
            try data.write(to: cacheURL(conversationID: conversationID, terminalID: terminalID), options: [.atomic])
        } catch {
            // Terminal cache is a performance hint; connection state remains authoritative.
        }
    }

    static func remove(conversationID: String, terminalID: String) {
        try? FileManager.default.removeItem(at: cacheURL(conversationID: conversationID, terminalID: terminalID))
    }

    private static func cacheURL(conversationID: String, terminalID: String) -> URL {
        cacheDirectory()
            .appendingPathComponent(safePathComponent(conversationID), isDirectory: true)
            .appendingPathComponent("\(safePathComponent(terminalID)).json")
    }

    private static func cacheDirectory() -> URL {
        let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first ?? FileManager.default.temporaryDirectory
        return base.appendingPathComponent("StellaCodeXTerminalCache", isDirectory: true)
    }

    private static func safePathComponent(_ value: String) -> String {
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-_."))
        let scalars = value.unicodeScalars.map { allowed.contains($0) ? Character($0) : "_" }
        let result = String(scalars).trimmingCharacters(in: CharacterSet(charactersIn: "._-"))
        return result.isEmpty ? "terminal" : result
    }
}

private final class XtermScreenBuffer {
    private enum ParserState {
        case normal
        case escape
        case csi(String)
    }

    private let scrollbackLimit = 2_000
    private var cols: Int
    private var rows: Int
    private var lines: [[TerminalCell]]
    private var cursorRow = 0
    private var cursorCol = 0
    private var savedCursorRow = 0
    private var savedCursorCol = 0
    private var parserState: ParserState = .normal
    private var currentStyle = TerminalStyle()

    init(cols: Int, rows: Int) {
        self.cols = max(cols, 20)
        self.rows = max(rows, 8)
        self.lines = [Array(repeating: TerminalCell.blank, count: max(cols, 20))]
    }

    var renderedText: String {
        lines
            .map { String($0.map(\.character)).trimmingCharacters(in: .whitespaces) }
            .joined(separator: "\n")
    }

    var renderedAttributedText: AttributedString {
        var output = AttributedString()
        for (lineIndex, line) in lines.enumerated() {
            let cells = trimmedCells(line)
            if cells.isEmpty {
                if lineIndex < lines.count - 1 {
                    output += AttributedString("\n")
                }
                continue
            }

            var runStart = 0
            while runStart < cells.count {
                let style = cells[runStart].style
                var runEnd = runStart + 1
                while runEnd < cells.count, cells[runEnd].style == style {
                    runEnd += 1
                }

                var segment = AttributedString(String(cells[runStart..<runEnd].map(\.character)))
                segment.foregroundColor = style.foregroundColor
                if let backgroundColor = style.backgroundColor {
                    segment.backgroundColor = backgroundColor
                }
                if style.bold {
                    segment.inlinePresentationIntent = .stronglyEmphasized
                }
                output += segment
                runStart = runEnd
            }

            if lineIndex < lines.count - 1 {
                output += AttributedString("\n")
            }
        }
        return output
    }

    func replace(with text: String) {
        let split = text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        lines = split.isEmpty ? [blankLine()] : split.map { paddedLine($0) }
        cursorRow = max(lines.count - 1, 0)
        cursorCol = lines.last.map { min(visibleLength($0), cols - 1) } ?? 0
        trimScrollback()
    }

    func resize(cols: Int, rows: Int) {
        let nextCols = max(cols, 20)
        let nextRows = max(rows, 8)
        guard nextCols != self.cols || nextRows != self.rows else {
            return
        }

        self.cols = nextCols
        self.rows = nextRows
        lines = lines.map { line in
            var resized = Array(line.prefix(nextCols))
            if resized.count < nextCols {
                resized.append(contentsOf: Array(repeating: TerminalCell.blank, count: nextCols - resized.count))
            }
            return resized
        }
        if lines.isEmpty {
            lines = [blankLine()]
        }
        cursorRow = min(cursorRow, max(lines.count - 1, 0))
        cursorCol = min(cursorCol, nextCols - 1)
        trimScrollback()
    }

    func feed(_ data: Data) {
        let string = String(decoding: data, as: UTF8.self)
        for scalar in string.unicodeScalars {
            feed(scalar)
        }
    }

    private func feed(_ scalar: UnicodeScalar) {
        switch parserState {
        case .normal:
            handleNormal(scalar)
        case .escape:
            if scalar == "[" {
                parserState = .csi("")
            } else if scalar == "7" {
                savedCursorRow = cursorRow
                savedCursorCol = cursorCol
                parserState = .normal
            } else if scalar == "8" {
                cursorRow = savedCursorRow
                cursorCol = savedCursorCol
                ensureCursorLine()
                parserState = .normal
            } else {
                parserState = .normal
            }
        case let .csi(buffer):
            let value = scalar.value
            if (0x40...0x7E).contains(value) {
                handleCSI(buffer, final: Character(scalar))
                parserState = .normal
            } else {
                parserState = .csi(buffer + String(scalar))
            }
        }
    }

    private func handleNormal(_ scalar: UnicodeScalar) {
        switch scalar {
        case "\u{1B}":
            parserState = .escape
        case "\r":
            cursorCol = 0
        case "\n":
            newLine()
        case "\u{08}":
            cursorCol = max(cursorCol - 1, 0)
        case "\t":
            let spaces = max(1, 4 - (cursorCol % 4))
            for _ in 0..<spaces {
                write(" ")
            }
        default:
            guard !CharacterSet.controlCharacters.contains(scalar) else {
                return
            }
            write(Character(scalar))
        }
    }

    private func handleCSI(_ rawParameters: String, final: Character) {
        let parameters = csiParameters(rawParameters)
        let first = parameters.first ?? 0

        switch final {
        case "m":
            applySGR(rawParameters)
        case "A":
            cursorRow = max(cursorRow - max(first, 1), 0)
        case "B":
            cursorRow = min(cursorRow + max(first, 1), max(lines.count - 1, 0))
        case "C":
            cursorCol = min(cursorCol + max(first, 1), cols - 1)
        case "D":
            cursorCol = max(cursorCol - max(first, 1), 0)
        case "G":
            cursorCol = min(max(first, 1) - 1, cols - 1)
        case "d":
            cursorRow = max(first - 1, 0)
            ensureCursorLine()
        case "H", "f":
            cursorRow = max((parameters.first ?? 1) - 1, 0)
            cursorCol = max((parameters.dropFirst().first ?? 1) - 1, 0)
            ensureCursorLine()
        case "J":
            if first == 0 {
                clearDisplayFromCursor()
            } else if first == 1 {
                clearDisplayToCursor()
            } else if first == 2 || first == 3 {
                lines = [blankLine()]
                cursorRow = 0
                cursorCol = 0
            }
        case "K":
            if first == 1 {
                clearLineToCursor()
            } else if first == 2 {
                lines[cursorRow] = blankLine()
            } else {
                clearLineFromCursor()
            }
        case "s":
            savedCursorRow = cursorRow
            savedCursorCol = cursorCol
        case "u":
            cursorRow = savedCursorRow
            cursorCol = savedCursorCol
            ensureCursorLine()
        default:
            break
        }
    }

    private func write(_ character: Character) {
        ensureCursorLine()
        lines[cursorRow][cursorCol] = TerminalCell(character: character, style: currentStyle)
        cursorCol += 1
        if cursorCol >= cols {
            cursorCol = 0
            newLine()
        }
    }

    private func newLine() {
        cursorRow += 1
        cursorCol = 0
        ensureCursorLine()
        trimScrollback()
    }

    private func ensureCursorLine() {
        while cursorRow >= lines.count {
            lines.append(blankLine())
        }
    }

    private func blankLine() -> [TerminalCell] {
        Array(repeating: TerminalCell(character: " ", style: currentStyle.backgroundOnly), count: cols)
    }

    private func paddedLine(_ text: String) -> [TerminalCell] {
        var cells = text.prefix(cols).map { TerminalCell(character: $0, style: TerminalStyle()) }
        if cells.count < cols {
            cells.append(contentsOf: Array(repeating: TerminalCell.blank, count: cols - cells.count))
        }
        return cells
    }

    private func visibleLength(_ line: [TerminalCell]) -> Int {
        String(line.map(\.character)).trimmingCharacters(in: .whitespaces).count
    }

    private func trimScrollback() {
        let limit = max(scrollbackLimit, rows)
        if lines.count > limit {
            let overflow = lines.count - limit
            lines.removeFirst(overflow)
            cursorRow = max(cursorRow - overflow, 0)
        }
    }

    private func clearDisplayFromCursor() {
        ensureCursorLine()
        clearLineFromCursor()
        if cursorRow + 1 < lines.count {
            for row in (cursorRow + 1)..<lines.count {
                lines[row] = blankLine()
            }
        }
    }

    private func clearDisplayToCursor() {
        ensureCursorLine()
        if cursorRow > 0 {
            for row in 0..<cursorRow {
                lines[row] = blankLine()
            }
        }
        clearLineToCursor()
    }

    private func clearLineFromCursor() {
        ensureCursorLine()
        guard cursorCol < cols else {
            return
        }
        for index in cursorCol..<cols {
            lines[cursorRow][index] = TerminalCell(character: " ", style: currentStyle.backgroundOnly)
        }
    }

    private func clearLineToCursor() {
        ensureCursorLine()
        for index in 0...min(cursorCol, cols - 1) {
            lines[cursorRow][index] = TerminalCell(character: " ", style: currentStyle.backgroundOnly)
        }
    }

    private func trimmedCells(_ line: [TerminalCell]) -> [TerminalCell] {
        var end = line.count
        while end > 0, line[end - 1].character == " " {
            end -= 1
        }
        return Array(line.prefix(end))
    }

    private func csiParameters(_ raw: String) -> [Int] {
        raw.split(separator: ";", omittingEmptySubsequences: false).map { part in
            let digits = part.filter { $0.isNumber || $0 == "-" }
            return Int(digits) ?? 0
        }
    }

    private func applySGR(_ raw: String) {
        var parameters = raw.isEmpty ? [0] : raw.split(separator: ";", omittingEmptySubsequences: false).map { Int($0) ?? 0 }
        if parameters.isEmpty {
            parameters = [0]
        }

        var index = 0
        while index < parameters.count {
            let value = parameters[index]
            switch value {
            case 0:
                currentStyle = TerminalStyle()
            case 1:
                currentStyle.bold = true
            case 22:
                currentStyle.bold = false
            case 7:
                currentStyle.inverse = true
            case 27:
                currentStyle.inverse = false
            case 30...37:
                currentStyle.foreground = TerminalColor.palette(value - 30)
            case 39:
                currentStyle.foreground = nil
            case 40...47:
                currentStyle.background = TerminalColor.palette(value - 40)
            case 49:
                currentStyle.background = nil
            case 90...97:
                currentStyle.foreground = TerminalColor.palette(value - 90 + 8)
            case 100...107:
                currentStyle.background = TerminalColor.palette(value - 100 + 8)
            case 38, 48:
                let isForeground = value == 38
                if index + 2 < parameters.count, parameters[index + 1] == 5 {
                    setColor(TerminalColor.ansi256(parameters[index + 2]), foreground: isForeground)
                    index += 2
                } else if index + 4 < parameters.count, parameters[index + 1] == 2 {
                    setColor(
                        TerminalColor.rgb(
                            red: parameters[index + 2],
                            green: parameters[index + 3],
                            blue: parameters[index + 4]
                        ),
                        foreground: isForeground
                    )
                    index += 4
                }
            default:
                break
            }
            index += 1
        }
    }

    private func setColor(_ color: TerminalColor, foreground: Bool) {
        if foreground {
            currentStyle.foreground = color
        } else {
            currentStyle.background = color
        }
    }
}

private struct TerminalCell: Equatable {
    var character: Character
    var style: TerminalStyle

    static let blank = TerminalCell(character: " ", style: TerminalStyle())
}

private struct TerminalStyle: Equatable {
    var foreground: TerminalColor?
    var background: TerminalColor?
    var bold = false
    var inverse = false

    var backgroundOnly: TerminalStyle {
        TerminalStyle(foreground: nil, background: background, bold: false, inverse: false)
    }

    var foregroundColor: Color {
        if inverse {
            return background?.color ?? .black
        }
        if let foreground {
            return foreground.color
        }
        return bold ? .white : Color.white.opacity(0.92)
    }

    var backgroundColor: Color? {
        if inverse {
            return foreground?.color ?? Color.white.opacity(0.92)
        }
        return background?.color
    }
}

private struct TerminalColor: Equatable {
    var red: Double
    var green: Double
    var blue: Double

    var color: Color {
        Color(red: red, green: green, blue: blue)
    }

    static func palette(_ index: Int) -> TerminalColor {
        let palette: [TerminalColor] = [
            rgb(red: 0, green: 0, blue: 0),
            rgb(red: 205, green: 49, blue: 49),
            rgb(red: 13, green: 188, blue: 121),
            rgb(red: 229, green: 229, blue: 16),
            rgb(red: 36, green: 114, blue: 200),
            rgb(red: 188, green: 63, blue: 188),
            rgb(red: 17, green: 168, blue: 205),
            rgb(red: 229, green: 229, blue: 229),
            rgb(red: 102, green: 102, blue: 102),
            rgb(red: 241, green: 76, blue: 76),
            rgb(red: 35, green: 209, blue: 139),
            rgb(red: 245, green: 245, blue: 67),
            rgb(red: 59, green: 142, blue: 234),
            rgb(red: 214, green: 112, blue: 214),
            rgb(red: 41, green: 184, blue: 219),
            rgb(red: 255, green: 255, blue: 255)
        ]
        return palette[max(0, min(index, palette.count - 1))]
    }

    static func ansi256(_ index: Int) -> TerminalColor {
        let clamped = max(0, min(index, 255))
        if clamped < 16 {
            return palette(clamped)
        }
        if clamped < 232 {
            let value = clamped - 16
            let red = value / 36
            let green = (value % 36) / 6
            let blue = value % 6
            return rgb(
                red: cubeComponent(red),
                green: cubeComponent(green),
                blue: cubeComponent(blue)
            )
        }
        let gray = 8 + (clamped - 232) * 10
        return rgb(red: gray, green: gray, blue: gray)
    }

    static func rgb(red: Int, green: Int, blue: Int) -> TerminalColor {
        TerminalColor(
            red: Double(max(0, min(red, 255))) / 255.0,
            green: Double(max(0, min(green, 255))) / 255.0,
            blue: Double(max(0, min(blue, 255))) / 255.0
        )
    }

    private static func cubeComponent(_ value: Int) -> Int {
        value == 0 ? 0 : 55 + value * 40
    }
}

#Preview {
    NavigationStack {
        IOSChatWorkspaceView(viewModel: .mock())
    }
}
#endif
