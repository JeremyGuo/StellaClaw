# ChatMessage Data Model

`ChatMessage` is the canonical conversation-history record used inside `stellaclaw_core`. Providers translate this structure into their own request protocols, and provider responses are converted back into this structure before persistence.

The Rust definitions live in `core/src/session_actor/chat_message.rs`.

## ChatMessage

```rust
pub struct ChatMessage {
    pub role: ChatRole,
    pub user_name: Option<String>,
    pub message_time: Option<String>,
    pub token_usage: Option<TokenUsage>,
    pub data: Vec<ChatMessageItem>,
}
```

### `role`

The side that authored the message.

- `ChatRole::User`: external user input, runtime notices, or synthetic user-side context inserted by the system.
- `ChatRole::Assistant`: model output, including final text, reasoning, tool calls, tool results recorded during assistant turns, and assistant-generated files.

Serialized as lowercase snake case:

```json
{ "role": "user" }
{ "role": "assistant" }
```

### `user_name`

Optional display/source name for a user message, for example `Stellacode` or a channel-specific speaker name.

This is metadata. Providers should not rely on it as semantic content unless they intentionally render metadata into prompt text elsewhere.

Serialization:

- omitted when `None`
- string when present

### `message_time`

Optional timestamp string for the message. Current callers generally store RFC3339-like timestamps from the channel/runtime.

This is metadata. It is not a schema-validated timestamp type.

Serialization:

- omitted when `None`
- string when present

### `token_usage`

Optional usage reported for this message, normally set on assistant messages returned by providers.

It is absent on ordinary user messages and on messages where the provider did not report usage.

### `data`

Ordered list of `ChatMessageItem`s. Order matters: providers preserve or interpret sequence when converting to request messages. A single `ChatMessage` may contain mixed content, such as reasoning followed by tool calls, or text followed by files.

## TokenUsage

```rust
pub struct TokenUsage {
    pub cache_read: u64,
    pub cache_write: u64,
    pub uncache_input: u64,
    pub output: u64,
    pub cost_usd: Option<TokenUsageCost>,
}
```

Fields:

- `cache_read`: input tokens read from provider cache.
- `cache_write`: input tokens written into provider cache.
- `uncache_input`: input tokens not served from cache.
- `output`: output tokens.
- `cost_usd`: optional computed USD cost for the same buckets.

`cost_usd` is omitted when absent. Costs are computed by pricing logic from `ModelConfig` and `TokenUsage`; providers should not invent cost values from provider-specific fields.

## TokenUsageCost

```rust
pub struct TokenUsageCost {
    pub cache_read: f64,
    pub cache_write: f64,
    pub uncache_input: f64,
    pub output: f64,
}
```

USD cost values corresponding to the token buckets above.

## ChatMessageItem

```rust
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ChatMessageItem {
    Reasoning(ReasoningItem),
    Context(ContextItem),
    File(FileItem),
    ToolCall(ToolCallItem),
    ToolResult(ToolResultItem),
}
```

Serialized as tagged objects:

```json
{ "type": "context", "payload": { "text": "hello" } }
```

### `Context`

Plain textual content.

```rust
pub struct ContextItem {
    pub text: String,
}
```

Typical uses:

- user text
- assistant text
- runtime notices represented as text
- textual tool outputs
- fallback text when media cannot be sent directly to a model

`text` may contain structured strings such as JSON, XML-like tool-call fallbacks, or system-generated metadata blocks.

### `Reasoning`

Model reasoning metadata.

```rust
pub struct ReasoningItem {
    pub text: String,
    pub codex_summary: Option<String>,
    pub codex_encrypted_content: Option<String>,
}
```

Fields:

- `text`: visible or fallback reasoning text. Empty strings are omitted during serialization.
- `codex_summary`: optional Codex reasoning summary.
- `codex_encrypted_content`: optional encrypted Codex reasoning payload used for Codex Responses history replay.

Behavior:

- Plain reasoning is often dropped before provider requests.
- Codex encrypted reasoning is preserved for Codex subscription replay.
- Reasoning should not be treated as ordinary user/assistant visible text unless a provider explicitly maps it that way.

### `File`

Reference to a file or media object.

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

Fields:

- `uri`: required location or payload reference.
  Common forms include:
  - `file:///absolute/path`
  - `data:<media-type>;base64,...`
  - HTTP(S) URLs
  - provider file ids or provider-specific URI schemes
- `name`: optional display filename or upload filename.
- `media_type`: optional MIME type, for example `image/png`, `application/pdf`, or `audio/mpeg`.
- `width`: optional pixel width for image-like media.
- `height`: optional pixel height for image-like media.
- `state`: optional file state. Currently only crashed files are represented.

`name`, `media_type`, `width`, `height`, and `state` are omitted during serialization when absent.

Provider behavior depends on model capabilities and media config:

- supported user media may be sent as direct input
- unsupported media is usually converted into a textual file reference
- assistant-generated files may be replayed as provider-specific output history or synthetic visual context

See `core/src/session_actor/file_item.md` for the standard payload rules for each modality, URI scheme, and source type.

### `FileState`

```rust
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FileState {
    Crashed { reason: String },
}
```

Current states:

- `crashed`: the file could not be loaded, generated, decoded, or normalized. `reason` explains the failure.

When a crashed file is normalized for model input, it usually becomes a text prompt describing the failed file reference and crash reason.

### `ToolCall`

Assistant request to execute a tool.

```rust
pub struct ToolCallItem {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: ContextItem,
}
```

Fields:

- `tool_call_id`: stable id used to pair a later `ToolResult` with this call.
- `tool_name`: tool/function name.
- `arguments`: raw argument text, usually a JSON object string stored in `ContextItem.text`.

Provider translations normally map this to native tool/function call protocol objects. Providers without native tool-call support may serialize it as text.

### `ToolResult`

Result of a tool execution.

```rust
pub struct ToolResultItem {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: ToolResultContent,
}
```

Fields:

- `tool_call_id`: id of the corresponding `ToolCall`.
- `tool_name`: tool name repeated for readability and provider compatibility.
- `result`: structured result payload.

Provider translations normally map this to native function/tool output protocol objects. If the result contains media, providers may additionally append synthetic user media messages to make the file visible to multimodal models.

## ToolResultContent

```rust
pub struct ToolResultContent {
    pub context: Option<ContextItem>,
    pub file: Option<FileItem>,
}
```

Fields:

- `context`: optional textual tool output.
- `file`: optional file/media output.

Either or both may be present. Both fields are omitted during serialization when absent.

## Expected Conversions

This section describes the expected way runtime events and provider protocol objects should be converted into `ChatMessage`. The goal is to keep persisted history provider-neutral and let provider translators handle protocol-specific replay.

### User Input

Plain user input should become one `role=user` message:

```json
{
  "role": "user",
  "user_name": "Stellacode",
  "message_time": "2026-05-02T12:00:00Z",
  "data": [
    {
      "type": "context",
      "payload": { "text": "帮我生成一个巴黎" }
    }
  ]
}
```

Attachments supplied by the user should be added as `File` items in the same message when they are part of the same user turn:

```json
{
  "type": "file",
  "payload": {
    "uri": "file:///workspace/input.png",
    "name": "input.png",
    "media_type": "image/png"
  }
}
```

### Assistant Text

Assistant natural-language output should become one `role=assistant` message with `Context` items:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "context",
      "payload": { "text": "已经生成。" }
    }
  ]
}
```

Provider token usage, when available, belongs on `token_usage`, not in `data`.

### Assistant Tool Call

When a provider asks to run a tool, the provider-specific tool/function call object should become a `ToolCall` item in an assistant message:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "tool_call",
      "payload": {
        "tool_call_id": "call_abc",
        "tool_name": "image_generation",
        "arguments": {
          "text": "{\"prompt\":\"Paris at sunset with the Eiffel Tower\"}"
        }
      }
    }
  ]
}
```

Rules:

- Preserve the provider/tool call id exactly when the provider supplies one.
- Store arguments as the raw JSON argument string in `arguments.text`.
- Do not pre-execute or flatten the tool call into assistant text.

### Tool Result

When the runtime finishes a tool call, the result should become a `ToolResult` item with the same `tool_call_id` and `tool_name`.

Text-only tool result:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "tool_result",
      "payload": {
        "tool_call_id": "call_abc",
        "tool_name": "read_file",
        "result": {
          "context": {
            "text": "file contents..."
          }
        }
      }
    }
  ]
}
```

File-only tool result:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "tool_result",
      "payload": {
        "tool_call_id": "call_abc",
        "tool_name": "image_load",
        "result": {
          "file": {
            "uri": "file:///workspace/.output/loaded.png",
            "name": "loaded.png",
            "media_type": "image/png",
            "width": 1024,
            "height": 768
          }
        }
      }
    }
  ]
}
```

Tool result with both text and file:

```json
{
  "type": "tool_result",
  "payload": {
    "tool_call_id": "call_abc",
    "tool_name": "image_generation",
    "result": {
      "context": {
        "text": "{\"status\":\"completed\",\"generation_id\":\"image_generation_123\"}"
      },
      "file": {
        "uri": "file:///workspace/.output/generated.png",
        "name": "generated.png",
        "media_type": "image/png"
      }
    }
  }
}
```

Rules:

- `ToolResult.tool_call_id` must match the corresponding `ToolCall.tool_call_id`.
- `ToolResult.tool_name` should match the original tool name.
- Put human/model-readable textual output in `result.context.text`.
- Put the primary output artifact in `result.file`.
- If a tool produces multiple files, either choose the primary file for `result.file` and mention the rest in `context`, or emit additional messages/items according to the caller's explicit multi-file policy. Do not hide extra files in provider-specific JSON without a stable convention.
- If the tool failed before producing a file, store the error text in `context`.
- If the file object exists but cannot be loaded or normalized, use `FileState::Crashed { reason }` on the `FileItem`.

Provider translators are responsible for converting `ToolResult` to provider-native tool output messages. If a tool result includes an image, a provider may also append a synthetic user-side image message at request time so a multimodal model can inspect the file. That synthetic message is not part of persisted history.

### Assistant-Generated File

When a provider itself returns an image or file as assistant output, persist it as a normal assistant `File` item, not as a fake `ToolResult`:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "file",
      "payload": {
        "uri": "file:///workspace/.output/paris.png",
        "name": "paris.png",
        "media_type": "image/png"
      }
    }
  ]
}
```

Provider translators may later replay this assistant file in provider-specific ways. For example, Codex subscription currently translates assistant image history into a temporary user message containing `[Generated by Assistant]` and `input_image`.

### Provider Reasoning

Provider reasoning should become `Reasoning`, not `Context`, unless it is intentionally exposed to the user as normal assistant text.

Codex encrypted reasoning should populate `codex_encrypted_content` so Codex can replay it. Plain fallback summaries may go in `text` or `codex_summary` depending on provider semantics.

## Example JSON

```json
{
  "role": "assistant",
  "token_usage": {
    "cache_read": 10,
    "cache_write": 2,
    "uncache_input": 20,
    "output": 30,
    "cost_usd": {
      "cache_read": 0.001,
      "cache_write": 0.002,
      "uncache_input": 0.003,
      "output": 0.004
    }
  },
  "data": [
    {
      "type": "reasoning",
      "payload": {
        "text": "need to inspect workspace"
      }
    },
    {
      "type": "tool_result",
      "payload": {
        "tool_call_id": "call_1",
        "tool_name": "read_file",
        "result": {
          "context": { "text": "file loaded" },
          "file": {
            "uri": "file:///tmp/demo.png",
            "name": "demo.png",
            "media_type": "image/png",
            "width": 320,
            "height": 240
          }
        }
      }
    }
  ]
}
```

## Design Notes

- `ChatMessage` is provider-neutral. Provider-specific request shapes belong in provider translators, not in this data model.
- Metadata fields (`user_name`, `message_time`, `token_usage`) should not be confused with message content.
- `data` item order is part of the message semantics.
- Synthetic provider compatibility messages, such as temporary user-side image replay for Codex, are request-time translations and should not be persisted as real `ChatMessage`s unless they are genuine conversation events.
