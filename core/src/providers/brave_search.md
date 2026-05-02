# brave_search ChatMessage Translation

This provider is a web search provider, not a chat message provider. It extracts a query from chat history and returns JSON search results as assistant text.

## Capability Gates

- Expected capability: `web_search`.
- `chat`, `image_in`, `image_out`, `pdf_in`, `audio_in`, and `file_in` do not affect message translation.

## Request Extraction From ChatMessage

The provider scans messages from newest to oldest and takes the first message with non-empty text extracted from `Context` items.

Item handling:

- `Context`: joined with newlines and used as candidate query text
- `File`: ignored
- `Reasoning`: ignored
- `ToolCall`: ignored
- `ToolResult`: ignored

If no query text exists, the provider returns an error.

## Response Back To ChatMessage

The Brave web search response is summarized into JSON text and returned as:

```json
{
  "role": "assistant",
  "data": [
    { "type": "context", "payload": { "text": "{...}" } }
  ]
}
```
