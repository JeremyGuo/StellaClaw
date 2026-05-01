#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="/home/liuhao/ClawParty"
CONFIG="$SCRIPT_DIR/deploy_prod.json"
WORKDIR="/home/liuhao/clawparty_workdir"
LOG="$SCRIPT_DIR/restart.log"

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$LOG"; }

log "=== Graceful restart started ==="

# 1. Kill old stellaclaw using deploy_prod.json (match only liuhao's processes)
OLD_PIDS=$(pgrep -f "stellaclaw.*deploy_prod.json" 2>/dev/null || true)
if [[ -n "$OLD_PIDS" ]]; then
    for PID in $OLD_PIDS; do
        log "Stopping old stellaclaw (PID: $PID) ..."
        kill "$PID" 2>/dev/null || true
    done
    # Wait up to 10 seconds for graceful shutdown
    for i in $(seq 1 10); do
        REMAINING=$(pgrep -f "stellaclaw.*deploy_prod.json" 2>/dev/null || true)
        if [[ -z "$REMAINING" ]]; then
            log "All old processes exited after ${i}s"
            break
        fi
        sleep 1
    done
    # Force kill any survivors
    REMAINING=$(pgrep -f "stellaclaw.*deploy_prod.json" 2>/dev/null || true)
    if [[ -n "$REMAINING" ]]; then
        log "Force killing remaining processes ..."
        for PID in $REMAINING; do
            kill -9 "$PID" 2>/dev/null || true
        done
        sleep 1
    fi
else
    log "No old stellaclaw process found"
fi

# 2. Source env (critical for API keys)
cd "$SCRIPT_DIR"
if [[ -f "$SCRIPT_DIR/.env" ]]; then
    set -a
    # shellcheck disable=SC1091
    source "$SCRIPT_DIR/.env"
    set +a
    log "Loaded .env"
else
    log "WARNING: .env not found at $SCRIPT_DIR/.env"
fi

# 3. Start new stellaclaw in background
log "Starting new stellaclaw ..."
log "  config: $CONFIG"
log "  workdir: $WORKDIR"

nohup "$SCRIPT_DIR/target/release/stellaclaw" \
    --config "$CONFIG" \
    --workdir "$WORKDIR" \
    >> "$LOG" 2>&1 &

NEW_PID=$!
log "New stellaclaw started (PID: $NEW_PID)"

# 4. Quick health check
sleep 3
if kill -0 "$NEW_PID" 2>/dev/null; then
    log "New process is running. Restart complete."
else
    log "WARNING: New process may have exited early. Check $LOG"
fi
