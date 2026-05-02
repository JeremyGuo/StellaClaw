# openrouter_completion ChatMessage Translation

This provider translates `List<ChatMessage>` into OpenRouter Chat Completions `messages`.

## Capability Gates

- `image_in`: user images can be encoded as content parts with `image_url`.
- no `image_in`: shared media normalization turns image files into text references before provider serialization.
- `pdf_in` / `audio_in` / `file_in`: handled by shared normalization. The completion payload only has text and image-url parts, so unsupported or non-image files generally become text.
- `image_out`: adds output `modalities` (`image`, and `text` when `chat` is also present), enables streaming when no tools are attached, and allows returned images to become `FileItem`s. Assistant history images are serialized as text URI references unless they are user-side image input.

## System Prompt

If `ProviderRequest.system_prompt` is non-empty, it is prepended as:

```json
{ "role": "system", "content": "..." }
```

## User ChatMessage

User messages become OpenRouter chat messages:

- visible text is collected from `Context` items and non-user-image `File` references
- user image `File` items become content parts:
  ```json
  { "type": "image_url", "image_url": { "url": file.uri } }
  ```
- if there are no images, `content` is a string
- if there are images, `content` is an array of `{type:"text"}` and `{type:"image_url"}` parts
- `Reasoning`, `ToolCall`, and `ToolResult` are not included in the main user content

If a `ToolResult` appears in the same message, an extra OpenRouter `role=tool` message is appended with `tool_call_id` and text content. If that tool result contains a non-crashed image, the provider also appends a temporary user image message after the tool message, subject to media capability normalization.

## Assistant ChatMessage

Assistant messages become OpenRouter chat messages:

- `Context`: joined into text content
- `File`: URI is included as text, including assistant image files
- `ToolCall`: serialized in `tool_calls` with `id`, `type=function`, function `name`, and JSON argument string
- `Reasoning`: ignored
- `ToolResult`: not part of assistant content; if present, emitted separately as `role=tool`

Empty assistant messages are skipped unless they carry tool calls.

## Response Back To ChatMessage

Returned assistant text becomes `Context`; returned tool calls become `ToolCall`; returned image URLs or image data are materialized as `FileItem`s.
