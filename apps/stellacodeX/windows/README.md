# StellacodeX Windows

Native Windows client for Stellaclaw's Web channel.

## Role

The Windows app is a client only. It connects to an existing Stellaclaw Web channel and does not run `stellaclaw`, `agent_server`, providers, or tools locally.

## Direction

- UI: WinUI 3 / Windows App SDK on Windows.
- Language/runtime: C# / .NET 8.
- Protocol: same Web channel REST/WebSocket semantics used by `apps/stellacode2`, Apple, and Android clients.
- Shared code: only platform-neutral protocol/schema/assets/fixtures under `apps/stellacodeX/shared`; no shared UI layer.
- Release shape: portable self-contained directory zipped for distribution. Users unzip and run the executable; no installer, MSIX, or auto-update in the first phase.

## Initial scope

1. Direct server profile and `/api/models` connection test.
2. Conversation list/create/rename/delete/seen.
3. Message history/detail/send and foreground realtime WebSocket.
4. Global conversation stream.
5. Status/actions for model, reasoning, remote, sandbox, continue, and cancel semantics exposed by the Web channel.
6. Workspace list/preview/upload/download/delete/move.
7. Terminal list/create/delete/stream.
8. SSH Proxy through Windows OpenSSH local port forwarding.
9. Windows notifications, layout/theme persistence, and portable packaging.

## Local implementation status

This directory now starts with a platform-neutral C# bootstrap executable and domain/API skeleton files. The bootstrap can smoke-test `GET /api/models`; the WinUI shell should be added from a Windows environment after the Windows App SDK tooling is verified. The current Linux server does not have `dotnet` installed and cannot build WinUI.

See [ARCHITECTURE.md](ARCHITECTURE.md) and [DEVELOPMENT.md](DEVELOPMENT.md).
