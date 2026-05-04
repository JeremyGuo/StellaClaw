# StellacodeX Apple

Native Apple client for StellaCodeX, covering macOS and iOS from one SwiftUI/Xcode project.

## Current state

The Xcode project lives under `StellaCodeX/` and currently contains the first SwiftUI shell:

- native macOS split-view conversation workspace
- Telegram-like iOS Chats/Settings tab shell
- platform-specific composers
- compact expandable tool batching summaries
- iOS files sheet entry point
- Citadel/NIO based SSH Proxy client path
- status bar
- mock API client and conversation/message models

The backend target remains Stellaclaw's Web channel. The mock client should be replaced incrementally with real REST and foreground WebSocket clients.

The local development profile defaults to SSH Proxy mode through `NAT-pl1`, forwarding Web Channel requests to `http://127.0.0.1:3011` on the remote side. Override with `STELLACODEX_CONNECTION_MODE=direct` and `STELLACODEX_SERVER_URL=...` for a direct connection.

Implementation progress is tracked in [APPLE_PROGRESS.md](APPLE_PROGRESS.md).

## Build

Open `StellaCodeX/StellaCodeX.xcodeproj` in Xcode, select the `StellaCodeX` scheme, and choose either a macOS or iOS Simulator destination.

macOS command-line build:

```bash
xcodebuild \
  -project StellaCodeX/StellaCodeX.xcodeproj \
  -scheme StellaCodeX \
  -destination 'platform=macOS' \
  build
```

iOS Simulator command-line build:

```bash
xcodebuild \
  -project StellaCodeX/StellaCodeX.xcodeproj \
  -scheme StellaCodeX \
  -destination 'generic/platform=iOS Simulator' \
  build
```
