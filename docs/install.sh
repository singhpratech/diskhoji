#!/bin/sh
# Diskhoji installer — https://diskhoji.org
# macOS Apple Silicon installs Diskhoji.app (Launchpad + Spotlight); Linux x86_64
# installs the binary with an application-menu entry. Intel Macs and anything
# else build from source via cargo.
set -e

REPO="singhpratech/diskhoji"
OS="$(uname -s)"
ARCH="$(uname -m)"

latest_asset() {
  curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
    | grep browser_download_url | grep "$1" | head -1 | cut -d'"' -f4
}

# ── macOS Apple Silicon: install the real Diskhoji.app ──────────────────────
if [ "$OS" = "Darwin" ] && [ "$ARCH" = "arm64" ]; then
  URL=$(latest_asset "macos-arm64\.app\.zip")
  if [ -n "$URL" ]; then
    TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
    echo "▦ diskhoji — fetching $URL"
    curl -fsSL "$URL" -o "$TMP/app.zip"
    ditto -x -k "$TMP/app.zip" "$TMP/u" 2>/dev/null || unzip -q "$TMP/app.zip" -d "$TMP/u"
    SRC=$(find "$TMP/u" -maxdepth 2 -name 'Diskhoji.app' -type d | head -1)
    APPS="/Applications"; [ -w "$APPS" ] || APPS="$HOME/Applications"
    mkdir -p "$APPS"; rm -rf "$APPS/Diskhoji.app"; cp -R "$SRC" "$APPS/Diskhoji.app"
    xattr -dr com.apple.quarantine "$APPS/Diskhoji.app" 2>/dev/null || true
    codesign --force --deep --sign - "$APPS/Diskhoji.app" 2>/dev/null || true
    DEST="${DISKHOJI_INSTALL_DIR:-$HOME/.local/bin}"; mkdir -p "$DEST"
    ln -sf "$APPS/Diskhoji.app/Contents/MacOS/diskhoji" "$DEST/diskhoji"
    echo "✓ installed $APPS/Diskhoji.app"
    echo "  open from Launchpad/Spotlight, or:  open -a Diskhoji"
    case ":$PATH:" in
      *":$DEST:"*) ;;
      *) echo "  for the CLI, add to PATH →  export PATH=\"$DEST:\$PATH\"" ;;
    esac
    exit 0
  fi
  echo "diskhoji: no .app asset in the latest release; falling back to the CLI binary" >&2
  # fall through to the tar.gz path below
fi

# ── Linux x86_64 / macOS CLI fallback: prebuilt tarball ─────────────────────
ASSET=""
case "$OS-$ARCH" in
  Linux-x86_64)  ASSET="linux-x86_64" ;;
  Darwin-arm64)  ASSET="macos-arm64" ;;
esac

if [ -n "$ASSET" ]; then
  URL=$(latest_asset "$ASSET\.tar\.gz")
fi

if [ -n "${URL:-}" ]; then
  TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
  echo "▦ diskhoji — fetching $URL"
  curl -fsSL "$URL" -o "$TMP/diskhoji.tar.gz"
  tar -xzf "$TMP/diskhoji.tar.gz" -C "$TMP"
  BIN=$(find "$TMP" -type f -name diskhoji | head -1)
  DEST="${DISKHOJI_INSTALL_DIR:-$HOME/.local/bin}"
  mkdir -p "$DEST"
  install -m 755 "$BIN" "$DEST/diskhoji"
  if [ "$OS" = "Darwin" ]; then
    xattr -cr "$DEST/diskhoji" 2>/dev/null || true
    codesign --force --sign - "$DEST/diskhoji" 2>/dev/null || true
  fi
  if [ "$OS" = "Linux" ]; then
    # Application-menu integration: icon + .desktop so Diskhoji appears in the
    # menu with the right icon. Exec is an absolute path because ~/.local/bin is
    # often absent from the launcher PATH; StartupWMClass matches the egui app_id.
    ICON256=$(find "$TMP" -type f -name 'icon-256.png' | head -1)
    ICON64=$(find "$TMP" -type f -name 'icon-64.png' | head -1)
    DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
    APPSDIR="$DATA/applications"
    HICOLOR="$DATA/icons/hicolor"
    mkdir -p "$APPSDIR" "$HICOLOR/256x256/apps" "$HICOLOR/64x64/apps"
    [ -n "$ICON256" ] && cp "$ICON256" "$HICOLOR/256x256/apps/diskhoji.png"
    [ -n "$ICON64" ]  && cp "$ICON64"  "$HICOLOR/64x64/apps/diskhoji.png"
    printf '%s\n' \
      '[Desktop Entry]' \
      'Type=Application' \
      'Version=1.0' \
      'Name=Diskhoji' \
      'GenericName=Disk Usage Analyzer' \
      'Comment=Every byte, accounted for' \
      "Exec=$DEST/diskhoji" \
      'Icon=diskhoji' \
      'Terminal=false' \
      'Categories=System;Utility;Filesystem;' \
      'Keywords=disk;storage;usage;treemap;heatmap;cleanup;' \
      'StartupWMClass=diskhoji' > "$APPSDIR/diskhoji.desktop"
    chmod 644 "$APPSDIR/diskhoji.desktop"
    gtk-update-icon-cache -f -t "$HICOLOR" >/dev/null 2>&1 || true
    update-desktop-database "$APPSDIR" >/dev/null 2>&1 || true
    echo "  added to your applications menu (Diskhoji)"
  fi
  echo "✓ installed to $DEST/diskhoji"
  case ":$PATH:" in
    *":$DEST:"*) ;;
    *) echo "  note: add it to your PATH →  export PATH=\"$DEST:\$PATH\"" ;;
  esac
  echo "  set sail:  diskhoji"
else
  echo "▦ diskhoji — no prebuilt binary for $OS/$ARCH; building from source"
  if ! command -v cargo >/dev/null 2>&1; then
    echo "diskhoji: cargo not found — install Rust first: https://rustup.rs" >&2
    exit 1
  fi
  cargo install --git "https://github.com/$REPO" --locked
  echo "✓ installed via cargo — run: diskhoji"
fi
