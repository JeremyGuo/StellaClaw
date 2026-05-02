# forkserver ChatMessage Translation

The forkserver is not a model protocol provider. It is an isolation and process-lifetime wrapper around another provider selected by `ModelConfig.provider_type`.

## Capability Gates

The forkserver does not inspect or transform capabilities. It serializes `ProviderRequestOwned` across a subprocess boundary and lets the real provider apply all capability-dependent message translation.

## Request Shape

`ProviderRequest` is cloned into:

```json
{
  "system_prompt": "...",
  "messages": [ChatMessage, ...],
  "tools": [ToolDefinition, ...]
}
```

Then the worker process reconstructs a borrowed `ProviderRequest` and calls the underlying provider.

## ChatMessage Handling

All `ChatMessage` roles and item types are preserved byte-for-byte through serde:

- `Context`
- `File`
- `Reasoning`
- `ToolCall`
- `ToolResult`

No conversion to provider `messages` happens in `forkserver.rs`; see the markdown file for the underlying provider that the worker hosts.
