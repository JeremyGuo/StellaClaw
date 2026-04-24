#!/usr/bin/env python3
"""Refresh embedded OpenRouter token pricing.

OpenRouter returns model pricing in USD/token. Stellaclaw stores USD/1M tokens.
Only token buckets currently represented by TokenUsageCost are written here:
cache_read, cache_write, input, and output.
"""

from __future__ import annotations

import json
import pathlib
import urllib.request
from decimal import Decimal, InvalidOperation


MODELS_URL = "https://openrouter.ai/api/v1/models"
ROOT = pathlib.Path(__file__).resolve().parents[1]
TARGETS = [
    ROOT / "pricing" / "open_router_completion.json",
    ROOT / "pricing" / "open_router_responses.json",
]
MILLION = Decimal("1000000")


def main() -> None:
    with urllib.request.urlopen(MODELS_URL, timeout=30) as response:
        payload = json.load(response)

    pricing = {}
    for model in payload.get("data", []):
        model_id = model.get("id")
        raw_price = model.get("pricing") or {}
        if not isinstance(model_id, str):
            continue

        prompt = decimal_field(raw_price, "prompt")
        completion = decimal_field(raw_price, "completion")
        if prompt is None or completion is None or prompt < 0 or completion < 0:
            continue

        cache_read = decimal_field(raw_price, "input_cache_read")
        cache_write = decimal_field(raw_price, "input_cache_write")
        pricing[model_id] = {
            "cache_read": price_per_million(cache_read if cache_read is not None else prompt),
            "cache_write": price_per_million(cache_write if cache_write is not None else prompt),
            "input": price_per_million(prompt),
            "output": price_per_million(completion),
        }

    rendered = json.dumps(
        dict(sorted(pricing.items())),
        ensure_ascii=False,
        indent=2,
        sort_keys=True,
    )
    for target in TARGETS:
        target.write_text(rendered + "\n", encoding="utf-8")

    print(f"wrote {len(pricing)} OpenRouter model prices to {len(TARGETS)} files")


def decimal_field(value: object, key: str) -> Decimal | None:
    if not isinstance(value, dict) or key not in value:
        return None
    try:
        return Decimal(str(value[key]))
    except (InvalidOperation, ValueError):
        return None


def price_per_million(value: Decimal) -> float:
    return float(value * MILLION)


if __name__ == "__main__":
    main()
