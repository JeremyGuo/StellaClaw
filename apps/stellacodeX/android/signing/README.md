# StellacodeX Android signing

`stellacodex-dev-release.jks` is a repository-scoped development release key used to keep StellacodeX Android APK signatures stable across local builds and GitHub Actions releases.

This key is not a production store key. It exists so alpha APKs with package `com.stellaclaw.stellacodex` can be upgraded with `adb install -r` after the one-time migration away from machine-local debug keystores.

Credentials are intentionally non-secret for this development distribution:

- store password: `stellacodex`
- key alias: `stellacodex`
- key password: `stellacodex`
