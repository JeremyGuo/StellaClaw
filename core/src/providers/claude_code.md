# claude_code ChatMessage Translation

This provider translates `List<ChatMessage>` into Anthropic/Claude-style `messages`.

## Capability Gates

- `image_in`: user image files can become Claude `image` blocks after shared media normalization.
- no `image_in`: image files become text references before fake tool-result image replay.
- `pdf_in` / `audio_in` / `file_in`: handled by shared normalization. Claude serialization here only emits text and image blocks, so unsupported media becomes text.
- `image_out`: assistant image history is normally normalized by shared logic; provider-side assistant files are serialized as text URI blocks.

## System Prompt

`ProviderRequest.system_prompt` is sent separately as Claude `system` content. Optional cache control can be attached to the system block or the last message block.

## User ChatMessage

User content becomes one Claude message:

```json
{
  "role": "user",
  "content": [...]
}
```

Item mapping:

- `Context`: `{ "type": "text", "text": context.text }`
- user image `File`: Claude image block
  - data URL: `{ "type": "image", "source": { "type": "base64", "media_type": ..., "data": ... } }`
  - otherwise: `{ "type": "image", "source": { "type": "url", "url": file.uri } }`
- non-image `File`: text block containing `file.uri`
- `Reasoning`, `ToolCall`, `ToolResult`: ignored in the main message content

After the main message, every `ToolResult` item becomes a separate user message containing Claude `tool_result` blocks. If a tool result contains a non-crashed image, another temporary user message is appended with the image block, subject to model media capabilities.

## Assistant ChatMessage

Assistant content becomes a Claude assistant message:

- `Context`: text block
- `File`: text block containing `file.uri`
- `ToolCall`: appended as Claude `tool_use` block with `id`, `name`, and parsed JSON `input`
- `Reasoning`: ignored when sending history
- `ToolResult`: emitted separately as user `tool_result` blocks if present

## Response Back To ChatMessage

Claude `text` becomes `Context`; `thinking` / `reasoning` becomes plain `Reasoning`; `tool_use` becomes `ToolCall`.
