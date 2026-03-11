#!/usr/bin/env bash
# Auto-start crustyclaw daemon on Claude Code session start.
# Only starts if not already running and config exists.

set -euo pipefail

PLUGIN_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${PLUGIN_ROOT}/bin/crustyclaw"

# Download binary from GitHub releases if not present
if [ ! -x "$BIN" ]; then
    "${PLUGIN_ROOT}/scripts/install-binary.sh" 2>/dev/null || exit 0
fi

if [ ! -x "$BIN" ]; then
    exit 0
fi

# Check if config exists (setup has been run)
DATA_DIR="${CRUSTYCLAW_DATA_DIR:-$HOME/.crustyclaw}"
if [ ! -f "${DATA_DIR}/config.json" ]; then
    exit 0
fi

# Check if daemon is already running
if pgrep -f "crustyclaw$" >/dev/null 2>&1; then
    exit 0
fi

# Check for updates in background (non-blocking)
"${PLUGIN_ROOT}/scripts/check-update.sh" >/dev/null 2>&1 &

# Start in background — strip CLAUDECODE so the daemon's child `claude`
# processes don't think they're nested inside Claude Code.
env -u CLAUDECODE nohup "$BIN" > /tmp/crustyclaw.log 2>&1 &
