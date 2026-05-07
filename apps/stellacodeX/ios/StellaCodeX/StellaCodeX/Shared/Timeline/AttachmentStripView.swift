import SwiftUI

#if os(macOS)
import AppKit
#else
import Photos
import UIKit
#endif

struct AttachmentStripView: View {
    let attachments: [ChatAttachment]
    var compact = false
    var alignment: HorizontalAlignment = .leading
    var fillsWidth = true

    var body: some View {
        if !attachments.isEmpty {
            VStack(alignment: alignment, spacing: 8) {
                ForEach(attachments) { attachment in
                    if attachment.isImage {
                        ImageAttachmentPreview(
                            attachment: attachment,
                            compact: compact,
                            alignment: frameAlignment,
                            fillsWidth: fillsWidth
                        )
                    } else {
                        AttachmentCardView(attachment: attachment, compact: compact)
                    }
                }
            }
            .modifier(AttachmentStripWidthModifier(fillsWidth: fillsWidth, alignment: frameAlignment))
        }
    }

    private var frameAlignment: Alignment {
        alignment == .trailing ? .trailing : .leading
    }
}

private struct ImageAttachmentPreview: View {
    let attachment: ChatAttachment
    let compact: Bool
    let alignment: Alignment
    let fillsWidth: Bool
    @State private var isPreviewPresented = false

    var body: some View {
        Button {
            isPreviewPresented = true
        } label: {
            ZStack {
                RoundedRectangle(cornerRadius: compact ? 14 : 16, style: .continuous)
                    .fill(PlatformColor.secondaryBackground.opacity(0.76))

                if let image = attachment.thumbnailImage {
                    image
                        .resizable()
                        .scaledToFit()
                        .frame(width: previewSize.width, height: previewSize.height)
                } else {
                    Image(systemName: "photo")
                        .font(.system(size: 34, weight: .semibold))
                        .foregroundStyle(.secondary)
                }
            }
            .frame(width: previewSize.width, height: previewSize.height)
            .clipShape(RoundedRectangle(cornerRadius: compact ? 14 : 16, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: compact ? 14 : 16, style: .continuous)
                    .strokeBorder(PlatformColor.separator.opacity(0.34))
            }
            .contextMenu {
                Button {
                    Pasteboard.copy(attachment.path.isEmpty ? attachment.uri : attachment.path)
                } label: {
                    Label("Copy Image Path", systemImage: "doc.on.doc")
                }
            }
        }
        .buttonStyle(.plain)
        .modifier(AttachmentStripWidthModifier(fillsWidth: fillsWidth, alignment: alignment))
        .imageAttachmentPresentation(isPresented: $isPreviewPresented, attachment: attachment)
    }

    private var previewSize: CGSize {
        let maxWidth: CGFloat = compact ? 280 : 380
        let maxHeight: CGFloat = compact ? 300 : 380
        let minWidth: CGFloat = compact ? 132 : 180
        let fallback = CGSize(width: compact ? 220 : 300, height: compact ? 150 : 200)
        guard let width = attachment.width,
              let height = attachment.height,
              width > 0,
              height > 0
        else {
            return fallback
        }

        let aspect = CGFloat(width) / CGFloat(height)
        var displayWidth = min(maxWidth, max(minWidth, CGFloat(width)))
        var displayHeight = displayWidth / aspect
        if displayHeight > maxHeight {
            displayHeight = maxHeight
            displayWidth = displayHeight * aspect
        }
        return CGSize(width: displayWidth, height: displayHeight)
    }
}

private struct AttachmentStripWidthModifier: ViewModifier {
    let fillsWidth: Bool
    let alignment: Alignment

    func body(content: Content) -> some View {
        if fillsWidth {
            content.frame(maxWidth: .infinity, alignment: alignment)
        } else {
            content.fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct AttachmentCardView: View {
    let attachment: ChatAttachment
    let compact: Bool

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            thumbnail

            VStack(alignment: .leading, spacing: 4) {
                Text(attachment.name)
                    .font(.caption.weight(.semibold))
                    .lineLimit(2)
                    .textSelection(.enabled)

                HStack(spacing: 6) {
                    if !attachment.kind.isEmpty {
                        Text(attachment.kind)
                    }
                    if let size = formattedSize {
                        Text(size)
                    }
                    if let dimensions {
                        Text(dimensions)
                    }
                }
                .font(.caption2)
                .foregroundStyle(.secondary)

                if !attachment.path.isEmpty {
                    Text(attachment.path)
                        .font(.caption2.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .textSelection(.enabled)
                }
            }

            Spacer(minLength: 8)

            Button {
                Pasteboard.copy(attachment.path.isEmpty ? attachment.uri : attachment.path)
            } label: {
                Image(systemName: "doc.on.doc")
            }
            .buttonStyle(.plain)
            .foregroundStyle(.secondary)
            .accessibilityLabel("Copy Attachment Path")
        }
        .padding(9)
        .background(PlatformColor.secondaryBackground.opacity(0.68))
        .clipShape(RoundedRectangle(cornerRadius: compact ? 11 : 9, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: compact ? 11 : 9, style: .continuous)
                .strokeBorder(PlatformColor.separator.opacity(0.34))
        }
    }

    @ViewBuilder
    private var thumbnail: some View {
        if attachment.isImage, let image = attachment.thumbnailImage {
            image
                .resizable()
                .scaledToFill()
                .frame(width: compact ? 54 : 72, height: compact ? 54 : 72)
                .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
        } else {
            ZStack {
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(PlatformColor.controlBackground.opacity(0.8))
                Image(systemName: iconName)
                    .font(.system(size: compact ? 18 : 22, weight: .semibold))
                    .foregroundStyle(.secondary)
            }
            .frame(width: compact ? 42 : 48, height: compact ? 42 : 48)
        }
    }

    private var iconName: String {
        if attachment.kind == "image" || attachment.mediaType?.hasPrefix("image/") == true {
            return "photo"
        }
        if attachment.mediaType?.contains("pdf") == true || attachment.name.lowercased().hasSuffix(".pdf") {
            return "doc.richtext"
        }
        if attachment.name.lowercased().hasSuffix(".zip") || attachment.name.lowercased().hasSuffix(".gz") {
            return "archivebox"
        }
        return "doc"
    }

    private var formattedSize: String? {
        guard let sizeBytes = attachment.sizeBytes else {
            return nil
        }
        return ByteCountFormatter.string(fromByteCount: Int64(sizeBytes), countStyle: .file)
    }

    private var dimensions: String? {
        guard let width = attachment.width, let height = attachment.height else {
            return nil
        }
        return "\(width)x\(height)"
    }
}

private struct ImageAttachmentPresentationModifier: ViewModifier {
    @Binding var isPresented: Bool
    let attachment: ChatAttachment

    func body(content: Content) -> some View {
        #if os(iOS)
        content.fullScreenCover(isPresented: $isPresented) {
            ImageAttachmentViewer(attachment: attachment)
        }
        #else
        content.sheet(isPresented: $isPresented) {
            ImageAttachmentViewer(attachment: attachment)
                .frame(minWidth: 560, minHeight: 420)
        }
        #endif
    }
}

private extension View {
    func imageAttachmentPresentation(isPresented: Binding<Bool>, attachment: ChatAttachment) -> some View {
        modifier(ImageAttachmentPresentationModifier(isPresented: isPresented, attachment: attachment))
    }
}

private struct ImageAttachmentViewer: View {
    let attachment: ChatAttachment
    @Environment(\.dismiss) private var dismiss
    @State private var displayedImage: Image?
    @State private var originalData: Data?
    @State private var isLoadingOriginal = false
    @State private var statusText: String?

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()

            Group {
                if let displayedImage {
                    displayedImage
                        .resizable()
                        .scaledToFit()
                } else if let thumbnailImage = attachment.thumbnailImage {
                    thumbnailImage
                        .resizable()
                        .scaledToFit()
                } else {
                    VStack(spacing: 10) {
                        Image(systemName: "photo")
                            .font(.system(size: 46, weight: .semibold))
                        Text("Preview unavailable")
                            .font(.subheadline.weight(.semibold))
                    }
                    .foregroundStyle(.white.opacity(0.72))
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            VStack {
                HStack {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 17, weight: .bold))
                            .frame(width: 42, height: 42)
                            .background(.ultraThinMaterial)
                            .clipShape(Circle())
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(.white)
                    .accessibilityLabel("Close Image Preview")

                    Spacer()
                }
                .padding(.horizontal, 16)
                .padding(.top, 14)

                Spacer()

                HStack(alignment: .bottom, spacing: 10) {
                    if let statusText {
                        Text(statusText)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.white)
                            .padding(.horizontal, 11)
                            .padding(.vertical, 8)
                            .background(.ultraThinMaterial)
                            .clipShape(Capsule())
                    }

                    Spacer()

                    Button {
                        Task {
                            await loadOriginal()
                        }
                    } label: {
                        if isLoadingOriginal {
                            ProgressView()
                                .controlSize(.small)
                                .frame(width: 42, height: 42)
                        } else {
                            Image(systemName: "arrow.down.circle")
                                .font(.system(size: 19, weight: .semibold))
                                .frame(width: 42, height: 42)
                        }
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(.white)
                    .background(.ultraThinMaterial)
                    .clipShape(Circle())
                    .disabled(isLoadingOriginal)
                    .accessibilityLabel("Load Original Image")

                    Button {
                        Task {
                            await saveOriginal()
                        }
                    } label: {
                        Image(systemName: "square.and.arrow.down")
                            .font(.system(size: 18, weight: .semibold))
                            .frame(width: 42, height: 42)
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(.white)
                    .background(.ultraThinMaterial)
                    .clipShape(Circle())
                    .accessibilityLabel("Save Original Image")
                }
                .padding(.horizontal, 16)
                .padding(.bottom, 18)
            }
        }
        .onAppear {
            displayedImage = attachment.thumbnailImage
        }
    }

    private func loadOriginal() async {
        if originalData != nil {
            statusText = "Original loaded"
            return
        }
        isLoadingOriginal = true
        defer {
            isLoadingOriginal = false
        }

        do {
            let data = try await attachment.loadOriginalImageData()
            guard let image = imageFromData(data) else {
                statusText = "Unable to decode image"
                return
            }
            originalData = data
            displayedImage = image
            statusText = "Original loaded"
        } catch {
            statusText = "Original unavailable"
        }
    }

    private func saveOriginal() async {
        if originalData == nil {
            await loadOriginal()
        }
        guard let originalData else {
            return
        }

        #if os(iOS)
        guard let image = UIImage(data: originalData) else {
            statusText = "Unable to decode image"
            return
        }
        let granted = await PhotoLibrarySaver.requestAddAccess()
        guard granted else {
            statusText = "Photo access denied"
            return
        }
        do {
            try await PhotoLibrarySaver.save(image)
            statusText = "Saved to Photos"
        } catch {
            statusText = "Save failed"
        }
        #else
        statusText = "Save is available on iOS"
        #endif
    }

    private func imageFromData(_ data: Data) -> Image? {
        #if os(macOS)
        guard let nsImage = NSImage(data: data) else {
            return nil
        }
        return Image(nsImage: nsImage)
        #else
        guard let uiImage = UIImage(data: data) else {
            return nil
        }
        return Image(uiImage: uiImage)
        #endif
    }
}

#if os(iOS)
private enum PhotoLibrarySaver {
    static func requestAddAccess() async -> Bool {
        let status = PHPhotoLibrary.authorizationStatus(for: .addOnly)
        switch status {
        case .authorized, .limited:
            return true
        case .notDetermined:
            let updated = await PHPhotoLibrary.requestAuthorization(for: .addOnly)
            return updated == .authorized || updated == .limited
        case .denied, .restricted:
            return false
        @unknown default:
            return false
        }
    }

    static func save(_ image: UIImage) async throws {
        try await PHPhotoLibrary.shared().performChanges {
            PHAssetChangeRequest.creationRequestForAsset(from: image)
        }
    }
}
#endif

private extension ChatAttachment {
    var thumbnailImage: Image? {
        guard let thumbnailDataURL,
              let comma = thumbnailDataURL.firstIndex(of: ",")
        else {
            return nil
        }

        let encoded = String(thumbnailDataURL[thumbnailDataURL.index(after: comma)...])
        guard let data = Data(base64Encoded: encoded) else {
            return nil
        }

        #if os(macOS)
        guard let image = NSImage(data: data) else {
            return nil
        }
        return Image(nsImage: image)
        #else
        guard let image = UIImage(data: data) else {
            return nil
        }
        return Image(uiImage: image)
        #endif
    }

    func loadOriginalImageData() async throws -> Data {
        if let data = decodeDataURL(uri) {
            return data
        }
        if let data = decodeDataURL(url) {
            return data
        }
        if let data = decodeDataURL(thumbnailDataURL ?? "") {
            return data
        }
        if let remoteURL = URL(string: url), remoteURL.scheme == "http" || remoteURL.scheme == "https" {
            let (data, _) = try await URLSession.shared.data(from: remoteURL)
            return data
        }
        if let fileURL = URL(string: uri), fileURL.isFileURL {
            return try Data(contentsOf: fileURL)
        }
        if let fileURL = URL(string: url), fileURL.isFileURL {
            return try Data(contentsOf: fileURL)
        }
        throw ImageAttachmentError.originalUnavailable
    }

    private func decodeDataURL(_ value: String) -> Data? {
        guard value.hasPrefix("data:"),
              let comma = value.firstIndex(of: ",")
        else {
            return nil
        }
        let payload = String(value[value.index(after: comma)...])
        if value[..<comma].contains(";base64") {
            return Data(base64Encoded: payload)
        }
        return payload.removingPercentEncoding?.data(using: .utf8)
    }
}

private enum ImageAttachmentError: Error {
    case originalUnavailable
}
