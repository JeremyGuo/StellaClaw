# StellacodeX

StellacodeX is the multi-client track for Stellacode. The Electron client now lives in this tree as `apps/stellacodeX/electron`, while platform-native clients live beside it.

The backend remains Stellaclaw's Web channel. Clients should share protocol semantics with the Electron implementation: server connection profiles, conversation list/detail, foreground event streams, message send, workspace browsing, file preview/upload/download, terminal sessions, status/model/remote/sandbox controls, and attachment rendering.

## Directory layout

```text
apps/stellacodeX/
  android/   # Android native client placeholder/prototype.
  electron/  # React/Electron desktop client and release target.
  ios/       # iOS native client project.
  windows/   # Windows native client: WinUI 3 direction with portable zip release path.
  linux/     # Linux native client placeholder.
  shared/    # Cross-platform protocol notes, API contracts, assets, and generated schemas.
```

Each platform family directory owns its project files and build tooling. Shared code should only live in `shared/` when it is genuinely platform-neutral, such as protocol documentation, generated API schema/types, common icons, or test fixtures.

## Migration intent

`apps/stellacodeX/electron` remains the desktop Electron client and release target. Native clients should reuse the same Stellaclaw server APIs instead of introducing a parallel backend.

Initial focus:

1. Keep the iOS native client focused and separate from macOS optimization work.
2. Define the shared Web channel API contract needed by all native clients.
3. Port the current Electron feature set incrementally.
4. Add each platform's release/build pipeline only after its native project is real.
