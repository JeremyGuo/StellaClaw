# StellacodeX

StellacodeX is the native-client track for Stellacode. The goal is feature parity with the current Electron client (`apps/stellacode2`) while moving platform UI and OS integration into first-class native apps.

The backend remains Stellaclaw's Web channel. Native clients should share protocol semantics with `stellacode2`: server connection profiles, conversation list/detail, foreground event streams, message send, workspace browsing, file preview/upload/download, terminal sessions, status/model/remote/sandbox controls, and attachment rendering.

## Directory layout

```text
apps/stellacodeX/
  android/   # Android native client placeholder/prototype.
  apple/     # Apple native client project for macOS and iOS.
  ios/       # Deprecated placeholder; Apple development moved to apple/.
  windows/   # Windows native client placeholder.
  linux/     # Linux native client placeholder.
  shared/    # Cross-platform protocol notes, API contracts, assets, and generated schemas.
```

Each platform family directory owns its native project files and build tooling. Apple platforms share one SwiftUI/Xcode project under `apple/`, with platform-specific UI branching kept inside the native app where needed. Shared code should only live in `shared/` when it is genuinely platform-neutral, such as protocol documentation, generated API schema/types, common icons, or test fixtures. Do not recreate an Electron-style shared UI layer here.

## Migration intent

`apps/stellacode2` remains the existing Electron client and release target while StellacodeX is developed. StellacodeX should reuse the same Stellaclaw server APIs instead of introducing a parallel backend.

Initial focus:

1. Build the shared Apple SwiftUI client for macOS and iOS.
2. Define the shared Web channel API contract needed by all native clients.
3. Port the current `stellacode2` feature set incrementally.
4. Add each platform's release/build pipeline only after its native project is real.
