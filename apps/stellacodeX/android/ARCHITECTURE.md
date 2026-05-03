# StellacodeX Android Architecture

This document defines the initial Android client architecture for StellacodeX.

## Product role

The Android app is a native client for an existing Stellaclaw Web channel server.
It does not run Stellaclaw, an agent runtime, or terminal sessions locally in the first version.

Initial goals:

1. Manage Stellaclaw server connection profiles.
2. List, create, rename, delete, and open conversations.
3. Render conversation messages and foreground progress.
4. Send foreground user messages.
5. Subscribe to foreground events over WebSocket.
6. Browse and preview workspace files.
7. Show conversation status, model, sandbox, remote binding, turn state, and usage.

Terminal support, offline cache, notifications, and local agent runtime are deferred.

## Stack

- Language: Kotlin
- UI: Jetpack Compose + Material 3
- Architecture: MVVM with lightweight MVI-style UI state and intents
- Async/state: Kotlin Coroutines + Flow
- Network: OkHttp
- SSH tunnel: JSch (`com.github.mwiede:jsch`) for Stellacode2-style local port forwarding to `targetUrl`
- JSON: kotlinx.serialization
- Storage: DataStore first; add encrypted token storage or Room only when needed
- Navigation: Navigation Compose
- Dependency injection: none initially; add DI only when wiring becomes noisy
- Minimum SDK: 28

## Module strategy

Use a single Android `:app` Gradle module for the first version.

Reasons:

- Product boundaries are still forming.
- Multi-module Android setup adds Gradle, resource, preview, and dependency overhead.
- A single module keeps the first end-to-end vertical slices easier to build and verify.

The package layout still follows future module boundaries so code can be split later if necessary.

## Package layout

```text
com.stellaclaw.stellacodex/
  MainActivity.kt
  StellacodeXApp.kt

  core/
    network/
    storage/
    result/
    time/
    logging/

  data/
    api/
    dto/
    mapper/
    repository/
    store/

  domain/
    model/
    repository/
    usecase/

  ui/
    app/
    theme/
    navigation/
    components/
    connections/
    conversations/
    chat/
    workspace/
    settings/
    status/
```

## Layer responsibilities

### `core`

Platform and infrastructure helpers that do not know product concepts.

Examples:

- `HttpClientFactory`
- `AuthInterceptor`
- `WebSocketFactory`
- `ApiError`
- `AppResult`
- `DataStoreFactory`
- logging helpers

Keep this layer small. Avoid generic frameworks unless they remove real current duplication.

### `data`

Concrete IO implementations.

Examples:

- REST API wrappers for `/api/models`, conversations, messages, status, and workspace.
- WebSocket stream client for `/api/conversations/stream`.
- kotlinx.serialization DTOs.
- DTO-to-domain mappers.
- DataStore-backed connection/profile settings.
- Repository implementations.

UI must not consume DTOs directly.

### `domain`

Product models and repository contracts.

Examples:

- `ConnectionProfile`
- `Conversation`
- `ConversationStatus`
- `ChatMessage`
- `ChatMessageItem`
- `WorkspaceEntry`
- `ModelInfo`
- `StreamEvent`

Use cases should be added only for cross-repository flows or non-trivial business steps. Simple CRUD can go directly through repositories from ViewModels.

### `ui`

Compose screens, ViewModels, UI state, and UI-only models.

Each major screen owns:

- `Screen`
- `ViewModel`
- `UiState`
- optional `Intent` / event sealed type
- small composables close to the screen when not broadly reused

Common visual elements live under `ui/components`.

## Initial screens

### Connection setup

Responsibilities:

- Add/edit/delete connection profiles.
- Store direct server base URL or SSH proxy settings plus bearer token.
- Select active connection.
- Validate connection by calling a lightweight endpoint such as `/api/models`.

Start route:

- If no active connection exists: show connection setup.
- Otherwise: show conversation list.

### Conversation list

Responsibilities:

- Load conversations.
- Create new conversation.
- Rename/delete existing conversation.
- Show status indicators and updated time.

UI:

- Top app bar with active connection indicator and settings button.
- Conversation list rows.
- Floating action button for new conversation.

### Chat

Responsibilities:

- Load messages.
- Load conversation status.
- Send foreground user messages.
- Subscribe to WebSocket stream events.
- Mark conversation seen.
- Render text, tool output, attachments, and progress.

Recommended UI structure:

```text
ChatScreen
  TopAppBar
  TurnStatusBar
  MessageList
  MessageComposer
```

### Workspace

Responsibilities:

- Browse workspace path.
- Preview supported files.
- Download or share files through Android-native flows.

First supported previews:

- text
- markdown as text
- image
- fallback download/share for binary

### Settings

Responsibilities:

- Current connection summary.
- Theme preference.
- Token/base URL editing entry point.
- About/debug information.

## Navigation

Use Navigation Compose.

Initial routes:

```text
connections
conversations
conversations/{conversationId}
conversations/{conversationId}/workspace?path={path}
conversations/{conversationId}/workspace/file?path={path}
settings
```

Avoid hard-coding phone-only assumptions so a two-pane tablet layout can be added later.

## Network contract

First-version endpoints:

- `GET /api/models`
- `GET /api/conversations?limit=...`
- `POST /api/conversations`
- `PATCH /api/conversations/{conversation_id}`
- `DELETE /api/conversations/{conversation_id}`
- `GET /api/conversations/{conversation_id}/messages?offset=...&limit=...`
- `POST /api/conversations/{conversation_id}/messages`
- `POST /api/conversations/{conversation_id}/seen`
- `GET /api/conversations/{conversation_id}/status`
- `GET /api/conversations/{conversation_id}/workspace?path=...&limit=...`
- `GET /api/conversations/{conversation_id}/workspace/file?path=...`
- `GET /api/conversations/stream` as WebSocket

Deferred terminal endpoints stay out of the first Android version.

## State management

ViewModels expose `StateFlow<UiState>`. UI sends explicit intents or calls small ViewModel methods.

Example chat state:

```kotlin
data class ChatUiState(
    val isLoading: Boolean = false,
    val conversationId: String,
    val title: String = "",
    val messages: List<ChatMessageUi> = emptyList(),
    val composerText: String = "",
    val status: ConversationStatusUi? = null,
    val isSending: Boolean = false,
    val error: String? = null,
)
```

Example chat intents:

```kotlin
sealed interface ChatIntent {
    data class ComposerChanged(val text: String) : ChatIntent
    data object SendClicked : ChatIntent
    data object RefreshClicked : ChatIntent
    data object OpenWorkspaceClicked : ChatIntent
}
```

Do not add a global Redux/MVI framework initially.

## WebSocket behavior

`ConversationStreamClient` exposes a `Flow<StreamEvent>`.

Responsibilities:

- Connect to `/api/conversations/stream`.
- Add bearer-token authentication.
- Decode events.
- Reconnect with backoff after transient failures.
- Surface authentication failures explicitly.
- Stop or reconnect according to Android lifecycle.

Chat screens filter events by current conversation id and reduce them into `ChatUiState`.

## Storage policy

First version persists only:

- connection profiles
- active connection id
- theme and small UI preferences

Do not persist full message history initially. Server-side conversation history remains the source of truth. Add Room later only if offline access or startup performance requires durable structured cache.

## Error handling

Use consistent UI states:

- loading
- empty
- inline error with retry
- snackbar for transient failures
- persistent banner for WebSocket disconnect

Classify errors at least as:

- unauthorized
- unreachable server
- server HTTP error
- decode/protocol mismatch
- unknown

## Implementation milestones

1. Android Gradle/Compose skeleton.
2. Theme, app root, navigation, fake preview screens.
3. Connection profile storage and `/api/models` validation.
4. Conversation list CRUD.
5. Chat message loading and sending.
6. WebSocket stream integration.
7. Workspace browsing and preview.
8. UI polish and error states.
