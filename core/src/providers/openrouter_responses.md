# openrouter_responses ChatMessage Translation

This provider translates `List<ChatMessage>` into OpenRouter Responses API `input` items.

## Capability Gates

- `image_in`: user image files can be carried as `input_image` after model media normalization.
- no `image_in`: image files are converted to text references by `normalize_messages_for_model` before fake tool-result image replay.
- `pdf_in` / `audio_in` / `file_in`: handled by the shared media normalizer. Unsupported files become text references.
- `image_out`: adds output `modalities` (`image`, and `text` when `chat` is also present) and allows returned image-generation references to become `FileItem`s. Assistant files in history are still serialized as text URI references in Responses input.

## User ChatMessage

`role=user` becomes one Responses `message` when content is non-empty:

```json
{
  "type": "message",
  "role": "user",
  "content": [...]
}
```

Item mapping:

- `Context`: `{ "type": "input_text", "text": context.text }`
- image `File`: `{ "type": "input_image", "image_url": file.uri }`
- non-image `File`: `{ "type": "input_file", ... }`
  - if `file.name` exists, include `filename`
  - if `file.uri` is `data:`, use `file_data`
  - otherwise use `file_url`
- `ToolCall`: text fallback as `<tool_call name="...">...</tool_call>`
- `Reasoning` and `ToolResult`: ignored in message content

After the message body, all `ToolResult` items become top-level `function_call_output` items. If a tool result includes a non-crashed image, the provider appends a temporary `role=user` message after the function output so the image can be supplied as normal image input.

## Assistant ChatMessage

Assistant content becomes one Responses `message` when content is non-empty:

- `Context`: `{ "type": "output_text", "text": context.text }`
- `File`: `{ "type": "output_text", "text": file.uri }`
- `ToolCall`: top-level `function_call`
- `Reasoning`: ignored
- `ToolResult`: appended as top-level `function_call_output`

Tool-result images are handled the same way as user messages: append a temporary user image message after the `function_call_output`.

## Response Back To ChatMessage

Assistant `message` content becomes `Context` and `File` items. `reasoning` text becomes plain `Reasoning`. `function_call` becomes `ToolCall`. `image_generation_call` / `openrouter:image_generation` references are materialized into `FileItem`s when possible.
