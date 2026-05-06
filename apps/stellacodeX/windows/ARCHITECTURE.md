# StellacodeX Windows Architecture

## Boundary

Windows StellacodeX is a native client for Stellaclaw's Web channel. Server state remains in Stellaclaw; the client owns only UI state, connection profiles, local caches, and OS integration.

```text
Windows UI
  -> StellaApiClient / StellaRealtimeClient
  -> Direct base URL or SSH Proxy resolved base URL
  -> Stellaclaw Web channel REST/WebSocket API
  -> Conversation / SessionActor backend
```

## Main modules

```text
src/StellaCodeX.Windows/
  Core/        shared app primitives: storage, result, logging, network helpers
  Domain/      client-side models used by ViewModels
  Data/Api     Web channel paths, DTOs, API client, WebSocket sessions
  Data/Ssh     SSH tunnel process and tunnel reuse logic
  UI/          WinUI app shell and feature views (to be added on Windows)
```

Current committed code starts with a platform-neutral bootstrap executable and modules that are independent of WinUI so they can be reviewed on non-Windows machines. The bootstrap supports a `--models` smoke test against a Stellaclaw Web channel. The WinUI app layer should be added from a Windows environment after the Windows App SDK tooling is verified.

## Server profile model

Windows follows the established native-client profile shape:

- `Direct`: requests use `baseUrl`.
- `SshProxy`: client opens a local tunnel through `ssh.exe`, then requests use the resolved `127.0.0.1:<port>` base URL while the remote side reaches `targetUrl`.

First implementation should prioritize SSH alias mode, matching `apps/stellacode2`: users may enter a `~/.ssh/config` Host alias in `sshHost`. Explicit host/user/port/private-key UI can be added after alias mode is stable.

## API coverage

The Windows client should implement the same Web channel surface already used by the Electron and Apple clients:

- `GET /api/models`
- `GET /api/conversations?limit=...`
- `POST /api/conversations`
- `PATCH /api/conversations/{conversation_id}`
- `DELETE /api/conversations/{conversation_id}`
- `POST /api/conversations/{conversation_id}/seen`
- `GET /api/conversations/{conversation_id}/status`
- `GET /api/conversations/{conversation_id}/messages?offset=...&limit=...`
- `GET /api/conversations/{conversation_id}/messages/{message_id}`
- `POST /api/conversations/{conversation_id}/messages`
- `GET /api/conversations/stream`
- `GET /api/conversations/{conversation_id}/foreground/ws`
- workspace list/file/download/upload/delete/move endpoints
- terminal list/create/delete/stream endpoints

WebSocket authentication should send both `Authorization: Bearer <token>` and `?token=<token>` fallback.

## Portable packaging

First Windows releases should be a zip containing a self-contained publish output directory. There is no installer and no auto-update requirement in this phase. The package must be runnable after extraction.
