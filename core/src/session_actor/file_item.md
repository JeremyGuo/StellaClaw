# ChatMessage File Payload Standard

`FileItem` is the standard payload for every `ChatMessageItem::File` and every `ToolResultContent.file`. It represents a file or media artifact in persisted conversation history. Provider-specific request payloads, such as `input_image`, `input_file`, multipart uploads, or inline base64, are derived from `FileItem` at request time.

Rust definition:

```rust
pub struct FileItem {
    pub uri: String,
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub state: Option<FileState>,
}
```

## Core Rules

- Persist stable references, not provider-specific request bodies.
- Prefer materialized `file://` URIs for user uploads, tool artifacts, and assistant-generated files.
- Use precise `media_type` whenever possible; modality detection depends on it.
- `width` and `height` are image dimensions unless a future schema explicitly defines another visual media convention.
- `state == None` means the file is usable.
- `state == crashed` means the file reference is known but the file could not be loaded, generated, decoded, normalized, or downloaded.
- Request-time media normalization may read `file://` and produce `data:` URLs for a provider request. That does not mutate persisted history.

## JSON Shape

```json
{
  "type": "file",
  "payload": {
    "uri": "file:///workspace/attachments/incoming/input.png",
    "name": "input.png",
    "media_type": "image/png",
    "width": 1024,
    "height": 768,
    "state": {
      "state": "crashed",
      "reason": "failed to decode image"
    }
  }
}
```

Only `uri` is required by the Rust type. For new writers, `name` and `media_type` should be filled whenever they can be known.

## URI Standard

### `file://`

Preferred persisted form for materialized files.

Use for:

- user uploads after the channel downloads or stores the file
- files uploaded into the workspace and then attached to a message
- tool outputs
- assistant-generated files after provider output is persisted

Rules:

- Use an absolute local path.
- The path should be readable from the runtime that will later build provider requests.
- For conversation-local artifacts, place files under a stable conversation/workspace output or attachment directory.
- Do not use relative paths in `uri`.

Example:

```json
{
  "uri": "file:///home/user/workdir/conversations/web-main-1/attachments/incoming/photo.png",
  "name": "photo.png",
  "media_type": "image/png"
}
```

### `data:`

Allowed, but should usually be treated as transport/intermediate form.

Use for:

- request-time normalized provider input
- small provider-returned payloads before they are persisted
- tests or explicitly in-memory flows

Avoid using large `data:` URLs as long-term conversation history when the bytes can be materialized to a file. Persisting a file and storing `file://` keeps history smaller and avoids duplicating binary data in JSONL.

Example request-time normalized image:

```json
{
  "uri": "data:image/png;base64,...",
  "name": "photo.png",
  "media_type": "image/png",
  "width": 1024,
  "height": 768
}
```

### `http://` / `https://`

Allowed for externally hosted files.

Use only when:

- the URL is expected to remain retrievable, or
- the provider requires URL transport, or
- the file is intentionally not materialized locally.

If future turns must reliably access the file, prefer downloading/materializing it and storing `file://`.

### Provider File IDs And Provider-Specific Schemes

Allowed only when the provider translator knows how to handle them.

Examples:

- `file-...` / `file_...`
- `sediment://file-...`

These are not provider-neutral storage for all backends. If the artifact should be reusable across providers, materialize it to `file://`.

## Modality Standard

`media_type` determines the modality used by shared media normalization.

| Modality | `media_type` standard | Required/recommended fields | Notes |
| --- | --- | --- | --- |
| Image | `image/png`, `image/jpeg`, `image/webp`, `image/gif`, etc. | `uri`, `name`, `media_type`; `width` and `height` recommended | `image/*` enables image-specific normalization and provider `input_image` translation. |
| PDF | `application/pdf` | `uri`, `name`, `media_type` | `width` and `height` omitted. |
| Audio | `audio/mpeg`, `audio/wav`, `audio/flac`, `audio/ogg`, `audio/mp4` | `uri`, `name`, `media_type` | Shared normalizer validates known signatures for inline binary transport. |
| Video | `video/mp4`, `video/quicktime`, etc. | `uri`, `name`, `media_type` | Current shared normalizer does not define direct video input; most providers will receive a text reference unless a provider implements video handling. |
| Generic document | precise MIME type when known, otherwise `application/octet-stream` | `uri`, `name`, `media_type` recommended | Requires model/provider support for generic file input. |

If `media_type` is missing, shared normalization treats the item as a generic file rather than image/PDF/audio. New code should avoid omitting it for known media.

## Source-Specific Expectations

### User Uploads

Expected storage:

1. Channel receives upload bytes or a channel file id.
2. Channel materializes the file into the conversation/workspace attachment area.
3. Channel creates a `FileItem` with `file://`, `name`, and `media_type`.
4. The user `ChatMessage` stores that `FileItem`.

Example:

```json
{
  "role": "user",
  "data": [
    {
      "type": "context",
      "payload": {
        "text": "What is in this image?"
      }
    },
    {
      "type": "file",
      "payload": {
        "uri": "file:///workdir/conversations/web-main-1/attachments/incoming/input.png",
        "name": "input.png",
        "media_type": "image/png",
        "width": 800,
        "height": 600
      }
    }
  ]
}
```

If a frontend sends a `FileItem` directly through REST, it should already refer to a materialized or otherwise retrievable file. The server should not assume every incoming `uri` is already durable unless the channel contract says so.

### Tool Results

Tool output text goes in `ToolResultContent.context`. The primary produced file goes in `ToolResultContent.file`.

```json
{
  "type": "tool_result",
  "payload": {
    "tool_call_id": "call_1",
    "tool_name": "image_load",
    "result": {
      "context": {
        "text": "{\"status\":\"loaded\"}"
      },
      "file": {
        "uri": "file:///workdir/conversations/web-main-1/.output/loaded.png",
        "name": "loaded.png",
        "media_type": "image/png",
        "width": 1024,
        "height": 768
      }
    }
  }
}
```

If a tool produces multiple files, choose a primary file for `result.file` and describe or list secondary files in `context` until a stable multi-file schema is introduced.

### Assistant-Generated Files

Provider-generated artifacts should be materialized and stored as assistant `File` items.

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "file",
      "payload": {
        "uri": "file:///workdir/conversations/web-main-1/.output/generated.png",
        "name": "generated.png",
        "media_type": "image/png",
        "width": 1536,
        "height": 1024
      }
    }
  ]
}
```

Do not persist provider request compatibility wrappers such as fake user image messages. Those are provider translation details.

### Failed Or Partial Files

If a file reference exists but is unusable, keep the `FileItem` and set `state`:

```json
{
  "uri": "file:///workdir/conversations/web-main-1/attachments/incoming/photo.png",
  "name": "photo.png",
  "media_type": "image/png",
  "state": {
    "state": "crashed",
    "reason": "download failed: timeout"
  }
}
```

This preserves the event in history while allowing model normalization to produce a clear text fallback.

## Request-Time Normalization

Before provider translation, the session actor calls shared media normalization for the target model.

For user-side `FileItem`s:

- if the model supports the media capability and transport is `inline_base64`, local `file://` is read and converted to `data:<media_type>;base64,...`
- if transport is `file_reference`, the `FileItem` remains unchanged
- if the model does not support the modality, the file becomes a `Context` text reference
- if reading or decoding fails, the file becomes a crashed-file text prompt

For assistant-side `FileItem`s:

- image history is preserved only when the model has the relevant output/history capability, or a provider implements its own replay strategy
- otherwise assistant files become text references

Provider docs describe the second step: how normalized `FileItem`s become provider request messages.

## What Not To Store

- Do not store provider-specific `input_image`, `input_file`, multipart field names, or Responses API item shapes in `ChatMessage`.
- Do not store large base64 blobs in `Context.text`.
- Do not omit `media_type` when the modality is known.
- Do not store relative paths in `uri`.
- Do not silently drop failed files; store a crashed `FileItem` or a textual error in the relevant tool result.
