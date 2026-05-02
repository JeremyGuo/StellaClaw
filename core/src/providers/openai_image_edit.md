# openai_image_edit ChatMessage Translation

This provider does not translate chat history into chat `messages`. It extracts an image generation/edit prompt and optionally one input image, then sends OpenAI Images API multipart or JSON requests.

## Capability Gates

- The model is expected to have `chat`, `image_in`, and `image_out` capabilities in config.
- `image_in` matters operationally only when an input image is present; the provider accepts the first user image file it sees.
- `image_out` is expected because the response is converted into assistant `FileItem`s.
- `pdf_in`, `audio_in`, and `file_in` are not used by this provider.

## Request Extraction From ChatMessage

The provider scans `ProviderRequest.system_prompt` and all `role=user` messages:

- non-empty system prompt: appended to prompt text
- user `Context`: appended to prompt text
- first user image `File`: selected as edit input image
- additional user images: ignored
- assistant messages: ignored for prompt extraction
- `Reasoning`, `ToolCall`, `ToolResult`, non-image files: ignored

The final prompt is all collected text joined with blank lines. If no prompt text exists, the provider returns an error.

## Images API Shape

- no input image: send image generation request to `/images/generations`
- with input image: send image edit request to `/images/edits` with the image as multipart
- data URL image input: decode base64 for multipart
- local `file://` image input: read file bytes for multipart
- other image URI schemes are rejected

## Response Back To ChatMessage

The provider returns an assistant `ChatMessage`:

- `revised_prompt`: `Context`
- `b64_json`: persisted to local output and returned as `FileItem`
- `url`: returned as `FileItem` with `media_type = image/*`
