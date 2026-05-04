# StellaCodeX Apple Progress

This file tracks the native Apple client implementation for macOS and iOS. `stellacode2` is the feature and protocol reference, not the visual design reference.

## Product Direction

- Build first-class macOS and iOS clients for Stellaclaw's Web channel from one SwiftUI/Xcode project.
- Prefer native Apple platform patterns: macOS system toolbar, inspector panels, keyboard shortcuts, menus, dense lists, and restrained styling; iOS `NavigationSplitView`/`NavigationStack`, safe-area-aware composer, touch targets, and compact inspector affordances.
- Let iOS appearance follow the system color scheme by default, with native Settings overrides for light and dark mode. Dark references are layout/style references, not a forced dark-only theme.
- Reuse Stellaclaw Web API semantics from `stellacode2`.
- Do not recreate the Electron UI literally.
- Keep Apple client UI under `apple/`; shared protocol notes can live under `apps/stellacodeX/shared/` when needed.

## Design Principles

- Sidebar is for conversations and navigation.
- Main pane is the active conversation workspace.
- Toolbar hosts window-level actions: new conversation, refresh, connection, inspector.
- Inspector is for status, model, sandbox, remote, usage, workspace summary, and diagnostics.
- Chat rendering should be readable and selectable, but not look like a mobile bubble chat.
- Tool calls/results should become compact timeline rows with expandable detail.
- Terminal and workspace browsing should feel like native macOS utility panes, not web panels.

## Feature Parity Reference

From `apps/stellacode2`:

- Server connection profiles.
- Conversation list, create, rename, delete, seen state.
- Conversation messages with pagination.
- Foreground WebSocket stream.
- Send message with `user_name`.
- Status/model/remote/sandbox/reasoning controls.
- Workspace browsing, file preview, upload, download.
- Terminal sessions.
- Attachment rendering.
- Tool call/result grouping.
- Usage/status overview.

## Current Implementation

- [x] Xcode SwiftUI project under `StellaCodeX/` targeting macOS and iOS.
- [x] Moved Apple development from `macos/` to `apple/`.
- [x] Removed accidental nested `.git`.
- [x] First native shell: sidebar, chat workspace, composer, status bar.
- [x] Mock client for local UI iteration.
- [x] Initial Swift Testing coverage.
- [x] Command-line macOS build and test pass.
- [x] Command-line iOS Simulator build pass.
- [x] Native macOS sidebar direction: system `List`, no search, lightweight rows.
- [x] macOS sidebar toggle uses a stable split shell that keeps the sidebar mounted and animates width only.
- [x] Native macOS composer direction: multiline editor, toolbar-style bottom bar.
- [x] Desktop message timeline direction instead of mobile chat bubbles.
- [x] macOS header actions moved out of the title bar to avoid toolbar layout jitter.
- [x] macOS shell redesigned toward Codex desktop: left app/project navigation, centered transcript, right inspector, floating composer, compact bottom status.
- [x] iOS Telegram-like shell: Chats/Settings tabs, chat list, conversation navigation, files sheet.
- [x] iOS Chats home redesigned toward the compact Telegram-style list structure while retaining system dynamic colors.
- [x] Tool batching model and compact expandable tool summaries.
- [x] Consecutive turn tool call/result messages are grouped into default-collapsed process rows, following the `stellacode2` interaction model.
- [x] Conversation history pagination using Web Channel `offset`/`limit`.
- [x] Conversation entry shows a loading state and jumps to the latest loaded message without playing a fast initial scroll animation.
- [x] ChatMessage detail sheet with rendered message, attachments summary, and tool batch/result detail.
- [x] Tappable ChatMessage-level tool call/result previews in both iOS and macOS timelines.
- [x] Empty messages are skipped in the timeline unless they carry tools, attachments, pending/error state, or token usage.
- [x] ChatMessage token usage decoding and native token usage pill/popover rendering.
- [x] Attachment rendering in timeline/detail with image thumbnails, file cards, metadata, and copy affordance.
- [x] Lightweight native Markdown rendering with fenced code blocks, folding, text selection, and copy.
- [x] Improved native Markdown block rendering for headings, lists, quotes, separators, and code blocks.
- [x] Markdown table rendering with alignment parsing and horizontal scrolling for wide tables.
- [x] Structured tool detail rendering for JSON, diff, file references, and long logs.
- [x] Conversation activity strip for progress/thinking/tool-running state in the active timeline.
- [x] iOS Telegram-style user messages with full-width assistant transcript blocks for richer Markdown, attachment, and tool rendering.
- [x] iOS conversation screen hides the root Chats/Settings tab bar and uses a Telegram-like floating chat header.
- [x] iOS composer redesigned toward Telegram-style controls with a round attachment button, capsule message field, and trailing action button.
- [x] iOS composer input grows from one line to multiple lines based on user text instead of reserving a fixed editor height.
- [x] Image attachments render as image previews instead of ordinary file attachment cards.
- [x] Conversation rename through `PATCH /api/conversations/{conversation_id}` with native iOS/macOS dialogs and optimistic list updates.
- [x] Conversation pin/unpin with app-local persistence, pinned-first sorting, and native iOS/macOS context menu actions.
- [x] Conversation seen state through `POST /api/conversations/{conversation_id}/seen`, with selected conversations marked read after latest message sync.
- [x] Global conversation stream through `/api/conversations/stream`, including snapshot, upsert, delete, processing, turn completed, and seen events.
- [x] Conversation list unread red dots on iOS and macOS, computed from `last_message_id > last_seen_message_id`.
- [x] Native local notification for completed replies in non-selected unread conversations.
- [x] iOS chat header Files/Actions buttons navigate to full pages instead of transient sheets/menus.
- [x] iOS Files page uses the real workspace list/file/download/upload Web API, with text/image previews and share-sheet downloads.
- [x] iOS Files page now supports multi-file upload, delete, rename/move, and share/open-in-place downloads through native list actions.
- [x] iOS Actions page shows current conversation status first, then command subpages for model, reasoning, remote, and sandbox.
- [x] iOS Actions page now loads `GET /api/conversations/{conversation_id}/status` and shows stellacode2-aligned Token Usage, cache hit rate, cost, bucket breakdown, background, and subagent status.
- [x] iOS Sandbox action options aligned with backend-supported `/sandbox default|subprocess|bubblewrap` modes and Web `sandbox_source`.
- [x] macOS chat workspace now uses the same active turn progress feed as iOS, so thinking/tool-running/progress state appears in the desktop timeline.
- [x] macOS toolbar now exposes native Files and Actions utility windows for the selected conversation.
- [x] macOS Files window uses the real workspace list/file/download/upload/delete/move Web API, with native directory navigation, AppKit download reveal, image preview, text/code preview, and Markdown Preview/Source modes.
- [x] macOS Actions window mirrors the iOS command surface with conversation status, Token Usage/cache/cost breakdown, model, reasoning, remote, sandbox, refresh/status/continue/cancel controls.
- [x] Local Chat message cache keyed by server profile and conversation, with cached conversation entry, cached older-page loading, WebSocket ack gap repair, Settings clear action, and automatic cleanup after 30 days without access.
- [x] Conversation entry now shows cached latest messages immediately while foreground WebSocket ack/backfill continues in parallel, with a bottom loading indicator and capped automatic render window for long-chat performance.
- [x] Shared iOS/macOS localization foundation with app language override, system language following, and English/Simplified Chinese/Japanese `Localizable.strings` resources.
- [x] iOS multimodal message sending uses the Web Channel `files` payload with file picker, photo library, and camera capture sources.
- [x] Auxiliary user messages align with `stellacode2`: Incoming User Metadata, Runtime Prompt Updates, Runtime Skill Updates, System Context, Developer Context, and Tool Context attach to the next user message as compact dots with popover detail instead of occupying normal chat content.
- [x] Swift SSH client package integration through Citadel/NIO/NIOSSH.
- [x] First-launch Ed25519 SSH key generation with private key stored in Keychain and public key shown in iOS Settings.
- [x] macOS SSH identity storage moved to Data Protection Keychain queries to reduce debug Keychain permission prompts.
- [x] Initial SSH Proxy mode that opens a local direct-tcpip tunnel before Web Channel REST requests.
- [x] SSH Proxy forwarder uses a dedicated URLSession and gentler half-close handling to reduce conversation-switch socket noise.
- [x] Local development default points at `NAT-pl1` with target Web Channel `http://127.0.0.1:3011`.
- [x] Server profile persistence using app-local `UserDefaults` JSON storage, with environment overrides still taking priority.
- [x] Models API integration through `GET /api/models`, exposed in iOS Settings and native model menus.
- [x] Model switching via existing Web Channel control command `/model <alias>`.
- [x] New conversation creation now requires a nickname and explicit model selection on iOS/macOS, and conversations without a model show an in-chat model selection gate before sending.
- [x] Initial real Web Channel REST client shape:
  - [x] `GET /api/conversations?limit=80`
  - [x] `GET /api/conversations/{conversation_id}/messages?offset=&limit=`
  - [x] `GET /api/conversations/{conversation_id}/messages/{message_id}`
  - [x] `POST /api/conversations/{conversation_id}/messages`
  - [x] `POST /api/conversations`
  - [x] `PATCH /api/conversations/{conversation_id}`
  - [x] `DELETE /api/conversations/{conversation_id}`
  - [x] `GET /api/models`
  - [x] `GET /api/conversations/{conversation_id}/workspace?path=&limit=`
  - [x] `GET /api/conversations/{conversation_id}/workspace/file?path=&offset=&limit_bytes=`
  - [x] `GET /api/conversations/{conversation_id}/workspace/download?path=`
  - [x] `POST /api/conversations/{conversation_id}/workspace/upload?path=`
  - [x] `DELETE /api/conversations/{conversation_id}/workspace?path=`
  - [x] `PATCH /api/conversations/{conversation_id}/workspace`
  - [x] `GET /api/conversations/{conversation_id}/terminals`
  - [x] `POST /api/conversations/{conversation_id}/terminals`
  - [x] `DELETE /api/conversations/{conversation_id}/terminals/{terminal_id}`
  - [x] `GET /api/conversations/{conversation_id}/terminals/{terminal_id}/stream`
  - [x] `GET /api/conversations/{conversation_id}/status`
- [x] Foreground WebSocket stream for selected conversation:
  - [x] `/api/conversations/{conversation_id}/foreground/ws`
  - [x] token query authentication fallback
  - [x] subscription ack, message page, progress, error, and delete event handling
  - [x] WebSocket-first conversation entry: wait for `subscription_ack`, load the first page from `current_message_id`, and use `next_message_id` to backfill missing messages.
  - [x] optimistic user message reconciliation
- [x] Conversation list right-side metadata now prioritizes unread count and agent working state instead of timestamps.
- [x] iOS edit-mode deletion now uses a two-step minus-to-delete reveal with 5-second undo before the server delete is sent.
- [x] iOS Chats no longer auto-selects the first conversation in the list, so background completions can remain unread until the user opens that conversation.
- [x] Conversation timeline initial load now re-anchors to the bottom after layout to avoid opening at the top of long chats.
- [x] macOS conversation sidebar now follows the iOS interaction semantics more closely: single conversation list, chat-style rows, unread/working indicators, mark-read context action, and delayed delete with undo.
- [x] iOS terminal sessions entry in the chat header, with session picker, create/delete, WebSocket attach, resize, binary input/output, and a local xterm-style screen cache keyed by conversation/session offset.
- [x] Terminal attach now ignores empty or stale local cache snapshots before choosing a replay offset, preventing connected-but-blank terminal panes after server restart or reused terminal ids.
- [x] iOS Terminal page now locks to landscape orientation while active and restores the normal app orientation policy after leaving.
- [x] iOS Settings appearance control for System, Light, and Dark modes, applied app-wide through SwiftUI color scheme preference.
- [x] StellaCodeX Apple app icon generated and wired into `AppIcon.appiconset` for iOS, iPadOS, macOS, and App Store marketing sizes, using a flat modern iOS-style mark.
- [x] Chat bottom anchoring now follows dynamic bottom UI changes: iOS keyboard/composer height changes and macOS tool/Terminal expansion keep the latest message aligned to the visible bottom when the user is already at the bottom.
- [x] macOS side/file bar resize no longer triggers repeated chat bottom-scroll alignment, and Markdown text measurement is rounded to reduce resize-time transcript flicker.
- [x] macOS root layout now gives Conversation/Chat higher compression priority and clamps the Files pane to remaining window width so side panels cannot push the conversation list offscreen.
- [x] macOS composer and selectable Markdown text now disable AppKit automatic text completion/checking services to reduce noisy `NSXPCDecoder` warnings triggered by Chinese IME/XPC text services.
- [x] macOS composer actions now keep only file attachment and image-library picking, with attachment chips and pasteboard image support routed through the same outgoing attachment pipeline.
- [x] macOS Terminal text view now tracks the viewport width, disables horizontal scrolling, and applies fallback white monospace attributes so terminal output does not disappear on a black background.
- [x] macOS Terminal ANSI rendering now uses AppKit-native attributed strings for foreground/background colors, and plain character input goes through AppKit text interpretation once to avoid duplicated keystrokes like `ls` becoming `lls`.
- [x] macOS/iOS conversation timelines now reset their scroll state per selected conversation, so opening a chat re-runs initial bottom anchoring instead of inheriting stale scroll position.
- [x] iOS launch screen now uses a centered transparent StellaCodeX logo with no internal dark plate on the system background before SwiftUI starts, avoiding icon/background color seams.
- [x] iOS Chats list now removes the extra top spacer/content margin under the custom header, so the first conversation starts directly below the bar.
- [x] iOS chat keyboard behavior now matches Telegram more closely: dragging the transcript dismisses the keyboard interactively, and keyboard/composer height changes keep the bottom aligned only when the user is already at the bottom.
- [x] iOS Chats rows now center the text group vertically with the avatar and draw separators as row overlays, avoiding the bottom-heavy item layout.

## In Progress

- [ ] Host key trust policy and public key rotation UI.
- [ ] Add richer workspace preview actions for large/binary files.

## Next Milestones

1. Improve terminal emulation coverage for more ANSI/xterm escape sequences and color attributes.
2. Add attachment download/open-in-place actions beyond the current preview/copy support.
3. Refine macOS Files into a true outline/tree browser if deep workspace navigation becomes common.
4. Add host key trust policy and SSH public key rotation UI.

## Verification

Run from the repository root:

```bash
xcodebuild \
  -project apps/stellacodeX/apple/StellaCodeX/StellaCodeX.xcodeproj \
  -scheme StellaCodeX \
  -destination 'platform=macOS' \
  build

xcodebuild \
  -project apps/stellacodeX/apple/StellaCodeX/StellaCodeX.xcodeproj \
  -scheme StellaCodeX \
  -destination 'platform=macOS' \
  test

xcodebuild \
  -project apps/stellacodeX/apple/StellaCodeX/StellaCodeX.xcodeproj \
  -scheme StellaCodeX \
  -destination 'generic/platform=iOS Simulator' \
  build
```

Current local development environment variables:

```bash
STELLACODEX_CONNECTION_MODE=ssh_proxy
STELLACODEX_SSH_HOST=NAT-pl1
STELLACODEX_SSH_PORT=22
STELLACODEX_SSH_USER=$USER
STELLACODEX_TARGET_URL=http://127.0.0.1:3011
```

Without overrides, the app uses the SSH Proxy defaults above and sends API requests to a local tunnel that forwards to `127.0.0.1:3011` from `NAT-pl1`. Authentication uses the generated public key shown in iOS Settings; add it to the remote user's `authorized_keys`.
