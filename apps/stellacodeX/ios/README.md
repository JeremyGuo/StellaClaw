# StellacodeX iOS

Native iOS client for StellaCodeX.

## Current state

The Xcode project lives under `StellaCodeX/` and currently contains the iOS SwiftUI shell:

- Telegram-like iOS Chats/Settings tab shell
- iOS composer and message timeline
- compact expandable tool batching summaries
- iOS files sheet entry point
- Citadel/NIO based SSH Proxy client path
- status bar
- mock API client and conversation/message models

The backend target remains Stellaclaw's Web channel. The mock client should be replaced incrementally with real REST and foreground WebSocket clients.

The local development profile defaults to SSH Proxy mode through `NAT-pl1`, forwarding Web Channel requests to `http://127.0.0.1:3011` on the remote side. Override with `STELLACODEX_CONNECTION_MODE=direct` and `STELLACODEX_SERVER_URL=...` for a direct connection.

Implementation progress is tracked in [IOS_PROGRESS.md](IOS_PROGRESS.md).

## Build

Open `StellaCodeX/StellaCodeX.xcodeproj` in Xcode, select the `StellaCodeX` scheme, and choose an iOS Simulator destination.

iOS Simulator command-line build:

```bash
xcodebuild \
  -project StellaCodeX/StellaCodeX.xcodeproj \
  -scheme StellaCodeX \
  -destination 'generic/platform=iOS Simulator' \
  build
```
