# brave_search_image ChatMessage Translation

This provider is an image search provider, not a multimodal chat provider. It extracts a query from chat history and returns JSON image-search results as assistant text.

## Capability Gates

- Expected capability: `web_search`.
- `image_in` and `image_out` do not change translation; input images are ignored.
- `pdf_in`, `audio_in`, and `file_in` are not used.

## Request Extraction From ChatMessage

Messages are scanned from newest to oldest. The first non-empty text built from `Context` items becomes the Brave image search query.

Item handling:

- `Context`: joined with newlines into query text
- `File`: ignored
- `Reasoning`: ignored
- `ToolCall`: ignored
- `ToolResult`: ignored

## Response Back To ChatMessage

Results are summarized into JSON text containing query, citations, and image result metadata. The provider returns one assistant `Context` item with that JSON string.
