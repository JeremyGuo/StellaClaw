#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONFIG="$SCRIPT_DIR/deploy_prod.json"
WORKDIR="${STELLACLAW_WORKDIR:-/home/liuhao/clawparty_workdir}"
LOG="$SCRIPT_DIR/restart.log"

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$LOG"; }

log "=== Restart script started ==="

# 1. Stop old stellaclaw processes (only ours, not sunjinbo's)
OLD_PID=$(pgrep -f "stellaclaw.*deploy_prod.json.*clawparty_workdir" 2>/dev/null || true)
if [[ -n "$OLD_PID" ]]; then
  log "Stopping old stellaclaw (PID: $OLD_PID) ..."
  kill "$OLD_PID" || true
  # Wait up to 10s for graceful shutdown
  for i in $(seq 1 10); do
    if ! kill -0 "$OLD_PID" 2>/dev/null; then
      log "Old process exited."
      break
    fi
    sleep 1
  done
  # Force kill if still running
  if kill -0 "$OLD_PID" 2>/dev/null; then
    log "Force killing old process ..."
    kill -9 "$OLD_PID" || true
    sleep 1
  fi
else
  log "No old stellaclaw process found."
fi

# 2. Build
log "Building stellaclaw (release) ..."
cd "$SCRIPT_DIR"
cargo build --workspace --release 2>&1 | tee -a "$LOG"
log "Build complete."

# 3. Start new stellaclaw
log "Starting stellaclaw ..."
log "  config: $CONFIG"
log "  workdir: $WORKDIR"

cd "$SCRIPT_DIR"
export STELLACLAW_LOG_STDOUT=1
if [[ -f "$SCRIPT_DIR/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$SCRIPT_DIR/.env"
  set +a
fi

exec "$SCRIPT_DIR/target/release/stellaclaw" \
  --config "$CONFIG" \
  --workdir "$WORKDIR"
