#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="/home/liuhao/ClawParty"
SERVICE="clawparty.service"
LOG="$SCRIPT_DIR/restart.log"

log() { echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" | tee -a "$LOG"; }

log "=== systemd restart started ==="
log "Using systemd user service as the only stellaclaw owner: $SERVICE"

# Stop any manually-started duplicate instances that are not owned by the user service.
SERVICE_PID="$(systemctl --user show "$SERVICE" -p MainPID --value 2>/dev/null || echo 0)"
OLD_PIDS="$(pgrep -f 'stellaclaw.*deploy_prod.json' 2>/dev/null || true)"
if [[ -n "$OLD_PIDS" ]]; then
    for PID in $OLD_PIDS; do
        if [[ "$PID" == "$$" || "$PID" == "$SERVICE_PID" ]]; then
            continue
        fi
        log "Stopping non-systemd stellaclaw instance (PID: $PID) ..."
        kill "$PID" 2>/dev/null || true
    done
fi

# Wait briefly for manual instances to exit before allowing systemd to bind ports/poll Telegram.
for _ in $(seq 1 10); do
    REMAINING=""
    for PID in $(pgrep -f 'stellaclaw.*deploy_prod.json' 2>/dev/null || true); do
        SERVICE_PID="$(systemctl --user show "$SERVICE" -p MainPID --value 2>/dev/null || echo 0)"
        if [[ "$PID" != "$SERVICE_PID" ]]; then
            REMAINING+="$PID "
        fi
    done
    if [[ -z "$REMAINING" ]]; then
        break
    fi
    sleep 1
done

if [[ -n "${REMAINING:-}" ]]; then
    log "Force killing remaining non-systemd instances: $REMAINING"
    for PID in $REMAINING; do
        kill -9 "$PID" 2>/dev/null || true
    done
    sleep 1
fi

log "Reloading user systemd units ..."
systemctl --user daemon-reload

log "Restarting $SERVICE ..."
systemctl --user restart "$SERVICE"

log "Service status:"
systemctl --user --no-pager --full status "$SERVICE" | tee -a "$LOG"

log "=== systemd restart complete ==="
