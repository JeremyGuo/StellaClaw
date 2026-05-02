# codex_subscription ChatMessage Translation

`codex_subscription` translates Stellaclaw's provider-neutral `ChatMessage` history into OpenAI Responses-style `input` items for the Codex subscription websocket.

This document focuses on two things:

1. The `ChatMessage` format this provider consumes and produces.
2. How `ChatMessage`s with different roles and item types become Codex Responses `input` messages.

## 1. ChatMessage Format

`ChatMessage` is the persisted, provider-neutral conversation record:

```json
{
  "role": "user | assistant",
  "user_name": "optional display/source name",
  "message_time": "optional timestamp string",
  "token_usage": {
    "cache_read": 0,
    "cache_write": 0,
    "uncache_input": 0,
    "output": 0,
    "cost_usd": {
      "cache_read": 0.0,
      "cache_write": 0.0,
      "uncache_input": 0.0,
      "output": 0.0
    }
  },
  "data": [
    {
      "type": "context | file | reasoning | tool_call | tool_result",
      "payload": {}
    }
  ]
}
```

Optional fields are omitted when absent. `data` is ordered and order is semantically important.

### Roles

- `user`: real user input, channel/runtime metadata represented as user-side notices, or persisted user attachments.
- `assistant`: model output, model reasoning, provider tool calls, runtime tool results, and assistant-generated files.

Request-time compatibility messages created by this provider, such as synthetic user image replay, are not persisted as real `ChatMessage`s.

### Item Types

#### `Context`

Plain text.

```json
{
  "type": "context",
  "payload": {
    "text": "hello"
  }
}
```

#### `File`

File or media reference.

```json
{
  "type": "file",
  "payload": {
    "uri": "file:///workspace/.output/paris.png",
    "name": "paris.png",
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

`name`, `media_type`, `width`, `height`, and `state` are optional. `state` currently only supports `crashed`.

For real user uploads, the preferred persisted shape is a materialized local file reference:

```json
{
  "type": "file",
  "payload": {
    "uri": "file:///workspace/attachments/incoming/photo.png",
    "name": "photo.png",
    "media_type": "image/png"
  }
}
```

`data:` URLs are still valid `FileItem.uri` values, but they are better treated as transport/intermediate form. For Codex requests, local `file://` inputs are commonly read and converted to inline data URLs during request-time media normalization.

#### `Reasoning`

Reasoning metadata.

```json
{
  "type": "reasoning",
  "payload": {
    "text": "fallback visible reasoning",
    "codex_summary": "optional summary",
    "codex_encrypted_content": "encrypted Codex reasoning payload"
  }
}
```

For Codex subscription, encrypted reasoning is replayable history. Plain reasoning without encrypted content is dropped by provider normalization before the request.

#### `ToolCall`

Assistant request to execute a tool.

```json
{
  "type": "tool_call",
  "payload": {
    "tool_call_id": "call_abc",
    "tool_name": "image_generation",
    "arguments": {
      "text": "{\"prompt\":\"Paris at sunset\"}"
    }
  }
}
```

`arguments.text` stores the raw JSON argument string.

#### `ToolResult`

Runtime result for a previous tool call.

```json
{
  "type": "tool_result",
  "payload": {
    "tool_call_id": "call_abc",
    "tool_name": "image_generation",
    "result": {
      "context": {
        "text": "{\"status\":\"completed\"}"
      },
      "file": {
        "uri": "file:///workspace/.output/paris.png",
        "name": "paris.png",
        "media_type": "image/png"
      }
    }
  }
}
```

`result.context` and `result.file` are both optional, but a useful result normally has at least one. If a tool generated an artifact, put the primary artifact in `result.file`.

## Capability Gates Before Translation

The session actor first runs shared media normalization for the target `ModelConfig`; then Codex-specific translation runs.

- `image_in`: user images can be sent as `input_image`. Tool-result images and assistant-generated images can be replayed as temporary user image messages.
- no `image_in`: image files become text references, or assistant image outputs become assistant `output_text` references.
- `pdf_in` / `audio_in` / `file_in`: shared media normalization decides whether files are preserved, inlined, or converted to text references.
- `image_out`: affects shared normalization for assistant image history, but Codex-specific assistant image replay relies on `image_in` because Codex must see pixels as input.

## 2. ChatMessage To Codex Responses Input

The target request shape is a Responses `input` array. Each `ChatMessage` can produce zero, one, or multiple top-level input items.

### User Message With Text And Image

Input `ChatMessage`:

```json
{
  "role": "user",
  "user_name": "Stellacode",
  "message_time": "2026-05-02T12:00:00Z",
  "data": [
    {
      "type": "context",
      "payload": {
        "text": "Describe this image"
      }
    },
    {
      "type": "file",
      "payload": {
        "uri": "file:///workspace/attachments/incoming/input.png",
        "name": "input.png",
        "media_type": "image/png"
      }
    }
  ]
}
```

Codex Responses `input` when `image_in` is supported and the model's media input transport is inline base64:

```json
[
  {
    "type": "message",
    "role": "user",
    "content": [
      {
        "type": "input_text",
        "text": "Describe this image"
      },
      {
        "type": "input_image",
        "image_url": "data:image/png;base64,AAA..."
      }
    ]
  }
]
```

The persisted `ChatMessage` keeps the stable `file://` URI. The data URL above is produced during request-time normalization by reading the local file.

If the model's media input transport is file reference, the image URL may remain the original `file://...` URI. If `image_in` is not supported, shared normalization converts the file into a text reference before Codex translation, so the provider sees only text-like `Context` items.

### User Message With Non-Image File

Input `ChatMessage`:

```json
{
  "role": "user",
  "data": [
    {
      "type": "context",
      "payload": {
        "text": "Summarize this file"
      }
    },
    {
      "type": "file",
      "payload": {
        "uri": "file:///workspace/report.txt",
        "name": "report.txt",
        "media_type": "text/plain"
      }
    }
  ]
}
```

Codex Responses `input` when the file survives media normalization:

```json
[
  {
    "type": "message",
    "role": "user",
    "content": [
      {
        "type": "input_text",
        "text": "Summarize this file"
      },
      {
        "type": "input_file",
        "filename": "report.txt",
        "file_data": "data:text/plain;base64,..."
      }
    ]
  }
]
```

Non-image file mapping:

- `data:` URI: `input_file` with `filename` and `file_data`
- OpenAI file id (`file-*` or `file_*`, including `sediment://file-*`): `input_file` with `file_id`
- local `file://`: read file bytes and send `file_data`
- otherwise: `input_file` with `file_url`

### Assistant Text And Tool Call

Input `ChatMessage`:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "context",
      "payload": {
        "text": "I will generate the image."
      }
    },
    {
      "type": "tool_call",
      "payload": {
        "tool_call_id": "call_img_1",
        "tool_name": "image_generation",
        "arguments": {
          "text": "{\"prompt\":\"Paris at sunset with the Eiffel Tower\"}"
        }
      }
    }
  ]
}
```

Codex Responses `input`:

```json
[
  {
    "type": "message",
    "role": "assistant",
    "content": [
      {
        "type": "output_text",
        "text": "I will generate the image."
      }
    ]
  },
  {
    "type": "function_call",
    "name": "image_generation",
    "arguments": "{\"prompt\":\"Paris at sunset with the Eiffel Tower\"}",
    "call_id": "call_img_1"
  }
]
```

The translator flushes accumulated assistant text before emitting `function_call`, preserving order.

### Tool Result With Text Only

Input `ChatMessage`:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "tool_result",
      "payload": {
        "tool_call_id": "call_read_1",
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

Codex Responses `input`:

```json
[
  {
    "type": "function_call_output",
    "call_id": "call_read_1",
    "output": "file contents..."
  }
]
```

`tool_name` is not sent in Codex `function_call_output`; matching is by `call_id`.

### Tool Result With Image

Input `ChatMessage`:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "tool_result",
      "payload": {
        "tool_call_id": "call_load_1",
        "tool_name": "image_load",
        "result": {
          "context": {
            "text": "{\"status\":\"loaded\"}"
          },
          "file": {
            "uri": "data:image/png;base64,BBB...",
            "name": "loaded.png",
            "media_type": "image/png"
          }
        }
      }
    }
  ]
}
```

Codex Responses `input` when `image_in` is supported:

```json
[
  {
    "type": "function_call_output",
    "call_id": "call_load_1",
    "output": "{\"status\":\"loaded\"}\ndata:image/png;base64,BBB..."
  },
  {
    "type": "message",
    "role": "user",
    "content": [
      {
        "type": "input_image",
        "image_url": "data:image/png;base64,BBB..."
      }
    ]
  }
]
```

The first item keeps the native tool-output chain. The second item is synthetic visual context so Codex can inspect the image pixels. This synthetic user message is request-time only and is not persisted.

If `image_in` is not supported, shared normalization turns the fake user file into text, so the appended synthetic message contains a text file reference instead of `input_image`.

### Assistant-Generated Image

When Codex itself generates an image, persisted history stores it as an assistant `File`, not as a tool result:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "file",
      "payload": {
        "uri": "data:image/png;base64,CCC...",
        "name": "paris.png",
        "media_type": "image/png"
      }
    }
  ]
}
```

Codex Responses `input` on the next request when `image_in` is supported:

```json
[
  {
    "type": "message",
    "role": "user",
    "content": [
      {
        "type": "input_text",
        "text": "[Generated by Assistant]"
      },
      {
        "type": "input_image",
        "image_url": "data:image/png;base64,CCC..."
      }
    ]
  }
]
```

This is intentionally not serialized as `image_generation_call`. The effective fallback for Codex visual continuity is a temporary user-side `input_image` message with a short marker text.

If `image_in` is not supported:

```json
[
  {
    "type": "message",
    "role": "assistant",
    "content": [
      {
        "type": "output_text",
        "text": "[Image output omitted]\nuri: data:image/png;base64,CCC...\nname: paris.png\nmedia_type: image/png"
      }
    ]
  }
]
```

### Assistant Reasoning

Input `ChatMessage` with Codex encrypted reasoning:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "reasoning",
      "payload": {
        "codex_summary": "Inspected the image.",
        "codex_encrypted_content": "gAAAA..."
      }
    },
    {
      "type": "context",
      "payload": {
        "text": "The image shows Paris."
      }
    }
  ]
}
```

Codex Responses `input`:

```json
[
  {
    "type": "reasoning",
    "summary": [
      {
        "type": "summary_text",
        "text": "Inspected the image."
      }
    ],
    "encrypted_content": "gAAAA..."
  },
  {
    "type": "message",
    "role": "assistant",
    "content": [
      {
        "type": "output_text",
        "text": "The image shows Paris."
      }
    ]
  }
]
```

Plain reasoning without `codex_encrypted_content` is dropped before this provider builds the request.

### Mixed Assistant Message Ordering

Input:

```json
{
  "role": "assistant",
  "data": [
    {
      "type": "context",
      "payload": { "text": "First." }
    },
    {
      "type": "tool_call",
      "payload": {
        "tool_call_id": "call_1",
        "tool_name": "ls",
        "arguments": { "text": "{\"path\":\".\"}" }
      }
    },
    {
      "type": "context",
      "payload": { "text": "After call." }
    }
  ]
}
```

Output:

```json
[
  {
    "type": "message",
    "role": "assistant",
    "content": [
      { "type": "output_text", "text": "First." }
    ]
  },
  {
    "type": "function_call",
    "name": "ls",
    "arguments": "{\"path\":\".\"}",
    "call_id": "call_1"
  },
  {
    "type": "message",
    "role": "assistant",
    "content": [
      { "type": "output_text", "text": "After call." }
    ]
  }
]
```

Assistant text is batched until an ordering boundary, such as a tool call or assistant image replay, forces a flush.

## Response Back To ChatMessage

Codex Responses output is converted back into assistant `ChatMessage`:

- assistant `message` text/image content becomes `Context` or `File`
- `reasoning` becomes `Reasoning` with Codex fields preserved when present
- `function_call` becomes `ToolCall`
- `image_generation_call.result` is materialized into a `FileItem`

Tool execution itself is performed by the runtime after a `ToolCall`; the runtime then appends a corresponding `ToolResult` message/item.
