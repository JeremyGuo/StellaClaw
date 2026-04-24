# Provider Pricing

Each `<provider_type>.json` file is embedded into `stellaclaw_core` at compile time.

The file format is a model-name dictionary. Prices are USD per 1M tokens:

```json
{
  "model-id": {
    "cache_read": 1.0,
    "cache_write": 1.0,
    "input": 1.0,
    "output": 1.0
  }
}
```

Omit models or leave a provider file empty for subscription-style or externally billed providers.
