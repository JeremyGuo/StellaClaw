# brave_search_video ChatMessage Translation

This provider is a video search provider. It extracts text query content from chat history and returns JSON video results as assistant text.

## Capability Gates

- Expected capability: `web_search`.
- `chat`, `image_in`, `image_out`, `pdf_in`, `audio_in`, and `file_in` do not affect translation.

## Request Extraction From ChatMessage

Messages are scanned from newest to oldest. The first non-empty text built from `Context` items is used as the video search query.

Item handling:

- `Context`: joined with newlines into query text
- `File`: ignored
- `Reasoning`: ignored
- `ToolCall`: ignored
- `ToolResult`: ignored

## Response Back To ChatMessage

The provider returns one assistant `Context` item containing JSON text with the query, citations, and video result summaries.
