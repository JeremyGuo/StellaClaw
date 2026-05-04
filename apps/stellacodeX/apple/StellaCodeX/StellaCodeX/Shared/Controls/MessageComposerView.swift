import SwiftUI
#if os(macOS)
import AppKit
#endif

struct MessageComposerView: View {
    @Binding var text: String
    let sendTitle: String
    let sendAction: () -> Void
    #if os(macOS)
    var attachments: [ComposerAttachment]
    var addFileAction: () -> Void
    var addImageAction: () -> Void
    var removeAttachmentAction: (ComposerAttachment) -> Void
    var pasteImageAction: () -> Bool
    #endif

    #if os(macOS)
    init(
        text: Binding<String>,
        sendTitle: String,
        sendAction: @escaping () -> Void,
        attachments: [ComposerAttachment] = [],
        addFileAction: @escaping () -> Void = {},
        addImageAction: @escaping () -> Void = {},
        removeAttachmentAction: @escaping (ComposerAttachment) -> Void = { _ in },
        pasteImageAction: @escaping () -> Bool = { false }
    ) {
        self._text = text
        self.sendTitle = sendTitle
        self.sendAction = sendAction
        self.attachments = attachments
        self.addFileAction = addFileAction
        self.addImageAction = addImageAction
        self.removeAttachmentAction = removeAttachmentAction
        self.pasteImageAction = pasteImageAction
    }
    #else
    init(
        text: Binding<String>,
        sendTitle: String,
        sendAction: @escaping () -> Void
    ) {
        self._text = text
        self.sendTitle = sendTitle
        self.sendAction = sendAction
    }
    #endif

    var body: some View {
        #if os(macOS)
        macBody
        #else
        HStack(alignment: .bottom, spacing: 8) {
            ZStack(alignment: .topLeading) {
                TextEditor(text: $text)
                    .font(.body)
                    .scrollContentBackground(.hidden)
                    .frame(height: editorHeight)
                    .padding(.horizontal, editorHorizontalPadding)
                    .padding(.vertical, editorVerticalPadding)

                if text.isEmpty {
                    Text("Message")
                        .font(.body)
                        .foregroundStyle(.tertiary)
                        .padding(.horizontal, placeholderHorizontalPadding)
                        .padding(.vertical, placeholderVerticalPadding)
                        .allowsHitTesting(false)
                }
            }
            .frame(maxWidth: .infinity)

            Button(action: sendAction) {
                Image(systemName: "paperplane.fill")
                    .font(.system(size: sendIconSize, weight: .semibold))
                    .frame(width: sendButtonSize, height: sendButtonSize)
                    .foregroundStyle(sendButtonForeground)
                    .background(sendButtonBackground)
                    .clipShape(RoundedRectangle(cornerRadius: sendButtonCornerRadius, style: .continuous))
            }
            .buttonStyle(.plain)
            .keyboardShortcut(.return, modifiers: [.command])
            .disabled(isSendDisabled)
            .accessibilityLabel(sendTitle)
            .help(sendTitle)
        }
        .padding(.horizontal, horizontalPadding)
        .padding(.vertical, verticalPadding)
        .background(composerBackground)
        .clipShape(RoundedRectangle(cornerRadius: composerCornerRadius, style: .continuous))
        .shadow(color: shadowColor, radius: 18, y: 8)
        .overlay {
            RoundedRectangle(cornerRadius: composerCornerRadius, style: .continuous)
                .strokeBorder(PlatformColor.separator.opacity(borderOpacity))
        }
        #endif
    }

    #if os(macOS)
    private var macBody: some View {
        VStack(spacing: 0) {
            if !attachments.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        ForEach(attachments) { attachment in
                            ComposerAttachmentChip(attachment: attachment) {
                                removeAttachmentAction(attachment)
                            }
                        }
                    }
                    .padding(.horizontal, 12)
                    .padding(.top, 10)
                    .padding(.bottom, 4)
                }
            }

            ZStack(alignment: .topLeading) {
                MacMessageTextView(
                    text: $text,
                    submitAction: sendAction,
                    pasteImageAction: pasteImageAction
                )
                    .frame(height: editorHeight)
                    .padding(.horizontal, editorHorizontalPadding)
                    .padding(.vertical, editorVerticalPadding)

                if text.isEmpty {
                    Text("消息")
                        .font(.body)
                        .foregroundStyle(.tertiary)
                        .padding(.horizontal, placeholderHorizontalPadding)
                        .padding(.vertical, placeholderVerticalPadding)
                        .allowsHitTesting(false)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            HStack(spacing: 12) {
                composerToolButton(systemName: "folder", help: "Attach File", action: addFileAction)
                composerToolButton(systemName: "photo.on.rectangle", help: "Choose Image", action: addImageAction)

                Spacer(minLength: 12)

                Button(action: sendAction) {
                    Text(sendTitle)
                        .font(.caption.weight(.semibold))
                        .frame(minWidth: 52, minHeight: 26)
                        .foregroundStyle(sendButtonForeground)
                        .background(sendButtonBackground)
                        .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
                }
                .buttonStyle(.plain)
                .keyboardShortcut(.return, modifiers: [.command])
                .disabled(isSendDisabled)
                .accessibilityLabel(sendTitle)
                .help(sendTitle)
            }
            .padding(.horizontal, 12)
            .padding(.bottom, 10)
        }
        .background(composerBackground)
        .clipShape(RoundedRectangle(cornerRadius: composerCornerRadius, style: .continuous))
        .shadow(color: shadowColor, radius: 16, y: 8)
        .overlay {
            RoundedRectangle(cornerRadius: composerCornerRadius, style: .continuous)
                .strokeBorder(PlatformColor.separator.opacity(borderOpacity))
        }
    }

    private func composerToolButton(systemName: String, help: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: systemName)
                .font(.system(size: 14, weight: .medium))
                .frame(width: 24, height: 24)
                .foregroundStyle(.secondary)
        }
        .buttonStyle(.plain)
        .help(help)
    }
    #endif

    private var editorHeight: CGFloat {
        #if os(macOS)
        let explicitLines = text.reduce(1) { count, character in
            character == "\n" ? count + 1 : count
        }
        return min(132, max(76, CGFloat(explicitLines) * 20 + 42))
        #else
        54
        #endif
    }

    private var horizontalPadding: CGFloat {
        #if os(macOS)
        8
        #else
        12
        #endif
    }

    private var verticalPadding: CGFloat {
        #if os(macOS)
        8
        #else
        10
        #endif
    }

    private var editorHorizontalPadding: CGFloat {
        #if os(macOS)
        14
        #else
        8
        #endif
    }

    private var editorVerticalPadding: CGFloat {
        #if os(macOS)
        11
        #else
        8
        #endif
    }

    private var placeholderHorizontalPadding: CGFloat {
        #if os(macOS)
        editorHorizontalPadding
        #else
        13
        #endif
    }

    private var placeholderVerticalPadding: CGFloat {
        #if os(macOS)
        editorVerticalPadding
        #else
        15
        #endif
    }

    private var composerBackground: some ShapeStyle {
        #if os(macOS)
        Color(nsColor: .textBackgroundColor)
        #else
        .bar
        #endif
    }

    private var composerCornerRadius: CGFloat {
        #if os(macOS)
        10
        #else
        0
        #endif
    }

    private var borderOpacity: Double {
        #if os(macOS)
        0.44
        #else
        0
        #endif
    }

    private var shadowColor: Color {
        #if os(macOS)
        Color.black.opacity(0.08)
        #else
        Color.clear
        #endif
    }

    private var sendButtonSize: CGFloat {
        #if os(macOS)
        36
        #else
        34
        #endif
    }

    private var sendIconSize: CGFloat {
        #if os(macOS)
        14
        #else
        15
        #endif
    }

    private var sendButtonCornerRadius: CGFloat {
        #if os(macOS)
        10
        #else
        17
        #endif
    }

    private var isSendDisabled: Bool {
        #if os(macOS)
        text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && attachments.isEmpty
        #else
        text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        #endif
    }

    private var sendButtonForeground: Color {
        isSendDisabled ? .secondary.opacity(0.62) : .white
    }

    private var sendButtonBackground: Color {
        isSendDisabled ? PlatformColor.separator.opacity(0.22) : Color.accentColor
    }
}

#if os(macOS)
struct ComposerAttachment: Identifiable, Hashable {
    let id = UUID()
    var file: OutgoingMessageFile

    var name: String {
        file.name ?? "attachment"
    }

    var mediaType: String {
        file.mediaType ?? "application/octet-stream"
    }

    var formattedSize: String {
        ByteCountFormatter.string(fromByteCount: Int64(file.sizeBytes ?? 0), countStyle: .file)
    }

    var thumbnail: NSImage? {
        guard let dataURL = file.thumbnailDataURL,
              let comma = dataURL.firstIndex(of: ","),
              let data = Data(base64Encoded: String(dataURL[dataURL.index(after: comma)...]))
        else {
            return nil
        }
        return NSImage(data: data)
    }
}

private struct ComposerAttachmentChip: View {
    let attachment: ComposerAttachment
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
            .frame(maxWidth: 160, alignment: .leading)

            Button(action: remove) {
                Image(systemName: "xmark.circle.fill")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Remove Attachment")
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .background(PlatformColor.secondaryBackground)
        .clipShape(Capsule())
        .overlay {
            Capsule()
                .strokeBorder(PlatformColor.separator.opacity(0.6))
        }
    }

    @ViewBuilder
    private var thumbnail: some View {
        if let image = attachment.thumbnail {
            Image(nsImage: image)
                .resizable()
                .scaledToFill()
                .frame(width: 30, height: 30)
                .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
        } else {
            Image(systemName: iconName)
                .font(.system(size: 14, weight: .semibold))
                .frame(width: 30, height: 30)
                .background(PlatformColor.controlBackground)
                .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
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

private struct MacMessageTextView: NSViewRepresentable {
    @Binding var text: String
    let submitAction: () -> Void
    let pasteImageAction: () -> Bool

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = NSScrollView()
        scrollView.drawsBackground = false
        scrollView.hasVerticalScroller = false
        scrollView.hasHorizontalScroller = false
        scrollView.borderType = .noBorder
        scrollView.autohidesScrollers = true
        scrollView.scrollerStyle = .overlay
        scrollView.contentInsets = NSEdgeInsets(top: 0, left: 0, bottom: 0, right: 0)
        scrollView.automaticallyAdjustsContentInsets = false

        let textStorage = NSTextStorage()
        let layoutManager = NSLayoutManager()
        let textContainer = NSTextContainer(size: CGSize(width: scrollView.contentSize.width, height: 10_000_000))
        textContainer.widthTracksTextView = true
        textContainer.heightTracksTextView = false
        textContainer.lineFragmentPadding = 0
        layoutManager.addTextContainer(textContainer)
        textStorage.addLayoutManager(layoutManager)

        let textView = MacComposerNSTextView(frame: .zero, textContainer: textContainer)
        textView.delegate = context.coordinator
        textView.submitAction = { currentText in
            context.coordinator.submit(currentText)
        }
        textView.pasteImageAction = pasteImageAction
        textView.drawsBackground = false
        textView.isEditable = true
        textView.isSelectable = true
        textView.isRichText = false
        textView.importsGraphics = false
        textView.allowsUndo = true
        textView.usesFindBar = false
        textView.usesFontPanel = false
        textView.usesRuler = false
        textView.allowsDocumentBackgroundColorChange = false
        textView.isContinuousSpellCheckingEnabled = false
        textView.isGrammarCheckingEnabled = false
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticDashSubstitutionEnabled = false
        textView.isAutomaticTextReplacementEnabled = false
        textView.isAutomaticSpellingCorrectionEnabled = false
        textView.isAutomaticLinkDetectionEnabled = false
        textView.isAutomaticDataDetectionEnabled = false
        textView.isAutomaticTextCompletionEnabled = false
        textView.enabledTextCheckingTypes = 0
        textView.font = .preferredFont(forTextStyle: .body)
        textView.textColor = .labelColor
        textView.insertionPointColor = .labelColor
        textView.textContainerInset = .zero
        textView.minSize = CGSize(width: 0, height: 0)
        textView.maxSize = CGSize(width: 10_000_000, height: 10_000_000)
        textView.isHorizontallyResizable = false
        textView.isVerticallyResizable = true
        textView.autoresizingMask = [.width]
        scrollView.documentView = textView

        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        guard let textView = scrollView.documentView as? NSTextView else {
            return
        }

        if textView.string != text {
            textView.string = text
        }
        textView.font = .preferredFont(forTextStyle: .body)
        textView.textColor = .labelColor
        textView.insertionPointColor = .labelColor
        if let composerTextView = textView as? MacComposerNSTextView {
            composerTextView.submitAction = { currentText in
                context.coordinator.submit(currentText)
            }
            composerTextView.pasteImageAction = pasteImageAction
        }
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(text: $text, submitAction: submitAction)
    }

    final class Coordinator: NSObject, NSTextViewDelegate {
        @Binding private var text: String
        private let submitAction: () -> Void

        init(text: Binding<String>, submitAction: @escaping () -> Void) {
            self._text = text
            self.submitAction = submitAction
        }

        func textDidChange(_ notification: Notification) {
            guard let textView = notification.object as? NSTextView else {
                return
            }
            let nextText = textView.string
            guard text != nextText else {
                return
            }
            text = nextText
        }

        func submit(_ currentText: String) {
            if text != currentText {
                text = currentText
            }
            DispatchQueue.main.async {
                self.submitAction()
            }
        }
    }
}

private final class MacComposerNSTextView: NSTextView {
    var submitAction: ((String) -> Void)?
    var pasteImageAction: (() -> Bool)?

    override func keyDown(with event: NSEvent) {
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        if flags.contains(.command), event.keyCode == 36 || event.keyCode == 76 {
            submitAction?(string)
            return
        }
        super.keyDown(with: event)
    }

    override func complete(_ sender: Any?) {
    }

    override func paste(_ sender: Any?) {
        if pasteImageAction?() == true {
            return
        }
        super.paste(sender)
    }

    override func orderFrontSubstitutionsPanel(_ sender: Any?) {
    }
}
#endif
