#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 <config>" >&2
}

if [[ $# -ne 1 ]]; then
  usage
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG_INPUT="$1"

if [[ "$CONFIG_INPUT" = /* ]]; then
  CONFIG_PATH="$CONFIG_INPUT"
elif [[ -f "$CONFIG_INPUT" ]]; then
  CONFIG_PATH="$(cd "$(dirname "$CONFIG_INPUT")" && pwd)/$(basename "$CONFIG_INPUT")"
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

DEFAULT_WORKDIR="$CONFIG_DIR/${CONFIG_STEM}_workdir"
STEM_WORKDIR="$CONFIG_DIR/$CONFIG_STEM"
if [[ -d "$STEM_WORKDIR" && ( -f "$STEM_WORKDIR/STELLA_VERSION" || -f "$STEM_WORKDIR/VERSION" ) ]]; then
  DEFAULT_WORKDIR="$STEM_WORKDIR"
fi
WORKDIR="${STELLACLAW_WORKDIR:-$DEFAULT_WORKDIR}"
export STELLACLAW_LOG_STDOUT="${STELLACLAW_LOG_STDOUT:-1}"

if [[ -f "$SCRIPT_DIR/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$SCRIPT_DIR/.env"
  set +a
fi

cargo build --workspace --release

echo "Starting stellaclaw in foreground ..."
echo "  config: $CONFIG_PATH"
echo "  workdir: $WORKDIR"
echo "  stdout logs: $STELLACLAW_LOG_STDOUT"

exec "$SCRIPT_DIR/target/release/stellaclaw" \
  --config "$CONFIG_PATH" \
  --workdir "$WORKDIR"
