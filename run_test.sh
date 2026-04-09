#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 <config>" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG_INPUT="$1"

if [[ "$CONFIG_INPUT" = /* ]]; then
  CONFIG_PATH="$CONFIG_INPUT"
else
  CONFIG_PATH="$SCRIPT_DIR/$CONFIG_INPUT"
fi

if [[ ! -f "$CONFIG_PATH" ]]; then
  echo "Config file not found: $CONFIG_PATH" >&2
  exit 1
fi

CONFIG_DIR="$(cd "$(dirname "$CONFIG_PATH")" && pwd)"
CONFIG_BASENAME="$(basename "$CONFIG_PATH")"
CONFIG_STEM="${CONFIG_BASENAME%.*}"
WORKDIR="$CONFIG_DIR/${CONFIG_STEM}_workdir"

mkdir -p "$WORKDIR"

exec cargo run --manifest-path "$SCRIPT_DIR/agent_host/Cargo.toml" -- \
  --config "$CONFIG_PATH" \
  --workdir "$WORKDIR" \
  --sandbox-auto
