# StellacodeX Android

Android is the first native StellacodeX client target.

## Confirmed stack

Use a direct native Android stack:

- Language: Kotlin
- UI: Jetpack Compose + Material 3
- Architecture: MVVM or lightweight MVI with explicit state holders
- Async/state: Kotlin Coroutines + Flow
- HTTP/WebSocket: OkHttp
- SSH proxy/tunnel: JSch (`com.github.mwiede:jsch`) for local port forwarding to servers whose Web channel only binds localhost
- JSON: kotlinx.serialization
- Local settings/storage: Jetpack DataStore first; add Room later only if durable structured cache becomes necessary
- Navigation: Navigation Compose
- Dependency injection: avoid at first; add Koin or Hilt only when wiring becomes noisy
- Minimum Android version: target modern devices; start with `minSdk 28` unless a concrete device requirement says otherwise

Avoid React Native, Flutter, WebView shell, or Kotlin Multiplatform UI. StellacodeX should be native per platform, with only protocol contracts, generated schemas/types, shared assets, and fixtures in `../shared`.

## Goal

Provide the same user-facing capabilities as the current Electron Stellacode client while using Android-native UI, lifecycle, storage, notifications, and file handling.

The Android app is a client only. It connects to an existing Stellaclaw server through the Web channel; it does not run Stellaclaw or an agent runtime locally.

## First version scope

Terminal support is intentionally out of scope for the first Android version.

Initial feature baseline:

- Manage one or more Stellaclaw server connections.
- Store server base URL and bearer token in Android-native storage.
- List, create, rename, delete, and select conversations.
- Show conversation messages with rendered text, tool output, attachments, and status/progress updates.
- Send foreground user messages.
- Subscribe to conversation foreground events over WebSocket.
- Browse workspace files and preview/download supported files.
- Upload files or archives into a conversation workspace when supported by the server API.
- Show conversation status including model, sandbox, remote binding, turn state, and usage.
- Expose controls equivalent to existing Stellacode actions rather than Telegram commands where practical.

Later scope:

- Terminal session creation, streaming, input, and ANSI rendering.
- Offline/cache improvements if message and workspace browsing need local persistence.
- Android notifications for completed/failed turns.

## Backend contract

The Android client should use the same Web channel API consumed by `apps/stellacode2/src/lib/api.js`, excluding terminal endpoints for the first version.

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

Deferred terminal endpoints:

- `GET /api/conversations/{conversation_id}/terminals`
- `POST /api/conversations/{conversation_id}/terminals`
- `DELETE /api/conversations/{conversation_id}/terminals/{terminal_id}`
- `GET /api/conversations/{conversation_id}/terminals/{terminal_id}/stream` as WebSocket

Authentication should mirror the Web channel bearer-token model. Native credential storage should be platform-specific and live in the Android project once created.

## Suggested project layout

```text
apps/stellacodeX/android/
  settings.gradle.kts
  build.gradle.kts
  gradle.properties
  app/
    build.gradle.kts
    src/main/
      AndroidManifest.xml
      java/com/stellaclaw/stellacodex/
        MainActivity.kt
        StellacodeXApp.kt
        data/
          api/
          model/
          store/
        domain/
        ui/
          conversations/
          chat/
          workspace/
          settings/
```

## Project status

Android build system and a minimal Compose skeleton are present. See [ARCHITECTURE.md](ARCHITECTURE.md) for the client architecture, package boundaries, first-version scope, and milestone plan.
