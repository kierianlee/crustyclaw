#!/usr/bin/env bash
# Check for the latest crustyclaw release and cache the result.
# Writes the latest version tag to ~/.crustyclaw/latest-version.
# Checks at most once per hour (skips if cache is fresh).

set -euo pipefail

REPO="kierianlee/crustyclaw"
DATA_DIR="${CRUSTYCLAW_DATA_DIR:-$HOME/.crustyclaw}"
CACHE_FILE="${DATA_DIR}/latest-version"

# Skip if cache is less than 1 hour old
if [ -f "$CACHE_FILE" ]; then
  if [ "$(uname -s)" = "Darwin" ]; then
    AGE=$(( $(date +%s) - $(stat -f %m "$CACHE_FILE") ))
  else
    AGE=$(( $(date +%s) - $(stat -c %Y "$CACHE_FILE") ))
  fi
  if [ "$AGE" -lt 3600 ]; then
    exit 0
  fi
fi

LATEST="$(curl -fsSL --max-time 5 "https://api.github.com/repos/${REPO}/releases/latest" \
  2>/dev/null | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//')" || true

if [ -n "$LATEST" ]; then
  mkdir -p "$DATA_DIR"
  echo "$LATEST" > "$CACHE_FILE"
fi
