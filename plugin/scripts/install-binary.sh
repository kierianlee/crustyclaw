#!/usr/bin/env bash
# Download the crustyclaw binary for the current platform from GitHub releases.
#
# Usage:
#   install-binary.sh [--version VERSION] [--force]
#
# If --version is omitted, fetches the latest release.
# --force re-downloads even if the binary already exists.
# Requires: curl, tar (macOS/Linux come with both)

set -euo pipefail

REPO="kierianlee/crustyclaw"
PLUGIN_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="${PLUGIN_ROOT}/bin"
BIN="${BIN_DIR}/crustyclaw"

# --- Parse args ---------------------------------------------------------------
VERSION=""
FORCE=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --force) FORCE=true; shift ;;
    *) shift ;;
  esac
done

# --- Skip if binary already exists and no specific version/force requested ----
if [ -x "$BIN" ] && [ -z "$VERSION" ] && [ "$FORCE" = false ]; then
  exit 0
fi

# --- Detect platform ----------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin) PLATFORM_OS="apple-darwin" ;;
  Linux)  PLATFORM_OS="unknown-linux-gnu" ;;
  *)
    echo "error: unsupported OS: $OS" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  arm64|aarch64) PLATFORM_ARCH="aarch64" ;;
  x86_64)        PLATFORM_ARCH="x86_64" ;;
  *)
    echo "error: unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

TARGET="${PLATFORM_ARCH}-${PLATFORM_OS}"

# --- Resolve version ----------------------------------------------------------
if [ -z "$VERSION" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//')"
  if [ -z "$VERSION" ]; then
    echo "error: could not determine latest release" >&2
    exit 1
  fi
fi

# --- Download & extract -------------------------------------------------------
ASSET_NAME="crustyclaw-${TARGET}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${ASSET_NAME}"

echo "Downloading crustyclaw ${VERSION} for ${TARGET}..."

mkdir -p "$BIN_DIR"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

HTTP_CODE="$(curl -fsSL -w '%{http_code}' -o "${TMP_DIR}/${ASSET_NAME}" "$DOWNLOAD_URL" 2>/dev/null || true)"

if [ ! -f "${TMP_DIR}/${ASSET_NAME}" ] || [ "$HTTP_CODE" != "200" ]; then
  echo "error: failed to download ${DOWNLOAD_URL} (HTTP ${HTTP_CODE:-???})" >&2
  echo "hint: you can build from source with ./build.sh" >&2
  exit 1
fi

tar -xzf "${TMP_DIR}/${ASSET_NAME}" -C "$TMP_DIR"
mv "${TMP_DIR}/crustyclaw" "$BIN"
chmod +x "$BIN"

echo "Installed crustyclaw ${VERSION} to ${BIN}"

# --- Also install to stable path (~/.crustyclaw/bin/) -------------------------
# This path survives plugin cache version bumps. Commands use it as the primary
# binary location so version upgrades don't break anything.
STABLE_DIR="${HOME}/.crustyclaw/bin"
mkdir -p "$STABLE_DIR"
cp "$BIN" "${STABLE_DIR}/crustyclaw"
chmod +x "${STABLE_DIR}/crustyclaw"
echo "Installed crustyclaw ${VERSION} to ${STABLE_DIR}/crustyclaw"

# --- Register statusline early so Claude Code picks it up on first session ---
"${STABLE_DIR}/crustyclaw" register-statusline 2>/dev/null || true
