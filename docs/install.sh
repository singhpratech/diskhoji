#!/bin/sh
# Diskhoji installer — https://diskhoji.org
# Linux x86_64: downloads the latest release binary.
# Everything else (incl. macOS): builds from source via cargo.
set -e

REPO="singhpratech/diskhoji"
OS="$(uname -s)"
ARCH="$(uname -m)"

if [ "$OS" = "Linux" ] && [ "$ARCH" = "x86_64" ]; then
  URL=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep browser_download_url | grep linux-x86_64 | head -1 | cut -d'"' -f4)
  if [ -z "$URL" ]; then
    echo "diskhoji: no linux-x86_64 asset found in the latest release" >&2
    exit 1
  fi
  TMP=$(mktemp -d)
  trap 'rm -rf "$TMP"' EXIT
  echo "▦ diskhoji — fetching $URL"
  curl -fsSL "$URL" -o "$TMP/diskhoji.tar.gz"
  tar -xzf "$TMP/diskhoji.tar.gz" -C "$TMP"
  BIN=$(find "$TMP" -type f -name diskhoji | head -1)
  DEST="${DISKHOJI_INSTALL_DIR:-$HOME/.local/bin}"
  mkdir -p "$DEST"
  install -m 755 "$BIN" "$DEST/diskhoji"
  echo "✓ installed to $DEST/diskhoji"
  case ":$PATH:" in
    *":$DEST:"*) ;;
    *) echo "  note: add it to your PATH →  export PATH=\"$DEST:\$PATH\"" ;;
  esac
  echo "  set sail:  diskhoji"
else
  echo "▦ diskhoji — no prebuilt binary for $OS/$ARCH yet; building from source"
  if ! command -v cargo >/dev/null 2>&1; then
    echo "diskhoji: cargo not found — install Rust first: https://rustup.rs" >&2
    exit 1
  fi
  cargo install --git "https://github.com/$REPO" --locked
  echo "✓ installed via cargo — run: diskhoji"
fi
