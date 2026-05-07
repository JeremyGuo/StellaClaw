# StellacodeX Shared Assets

Shared, platform-neutral assets live here so each native client can consume the same source files before converting them into platform-specific bundles.

## Icons

`icons/stellacodex/` contains the StellacodeX source logo files:

- `StellacodeX-icon-light.png`: RGB app icon source for light/default app icon exports;
- `StellacodeX-icon-dark.png`: RGB app icon source for dark-surface uses;
- `StellacodeX-icon-transparent.png`: transparent logo source for launch screens and in-app logo rendering.

Client-specific bundles should generate their required PNG, `.icns`, or `.ico` files from these sources.
