#!/usr/bin/env bash
# install.sh — Install build-watcher from a GitHub release or local build.
#
# Downloads the pre-built binaries for the current platform from the latest
# GitHub release, installs the daemon and CLI to ~/.local/bin/, sets up the
# platform service (systemd on Linux, launchd on macOS), and registers the MCP
# server in ~/.claude.json.
#
# This script handles both fresh installs and upgrades. If a previous install
# exists (regardless of whether it was built from source or downloaded from a
# release), it safely stops the running service before overwriting the binaries.
#
# Requirements:
#   - gh (GitHub CLI), authenticated (unless --local): https://cli.github.com
#
# Usage: curl -fsSL https://raw.githubusercontent.com/wkirschbaum/build-watcher/main/install.sh | bash
#        ./install.sh            # install from latest GitHub release (from repo checkout)
#        ./install.sh --local    # build from source and install (from repo checkout)

set -euo pipefail

REPO="wkirschbaum/build-watcher"
RAW_URL="https://raw.githubusercontent.com/$REPO/main"
BINARY_NAME="build-watcher"
INSTALL_DIR="$HOME/.local/bin"
BINARY_PATH="$INSTALL_DIR/$BINARY_NAME"
CLAUDE_CONFIG="$HOME/.claude.json"
PORT=8417
OS="$(uname -s)"
ARCH="$(uname -m)"
LOCAL=false

for arg in "$@"; do
  case "$arg" in
    --local) LOCAL=true ;;
    *) echo "Unknown option: $arg"; exit 1 ;;
  esac
done

# -- Acquire binaries ---------------------------------------------------------

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

if [ "$LOCAL" = true ]; then
  # -- Build from source ------------------------------------------------------
  command -v cargo >/dev/null 2>&1 || {
    echo "Error: cargo is required for --local builds."
    exit 1
  }

  echo "==> Building from source (release)..."
  cargo build --release

  cp target/release/build-watcher "$TMPDIR/$BINARY_NAME"
  cp target/release/bw "$TMPDIR/bw"
else
  # -- Download release binaries ----------------------------------------------
  command -v gh >/dev/null 2>&1 || {
    echo "Error: gh (GitHub CLI) is required but not found."
    echo "Install it from https://cli.github.com and run 'gh auth login'."
    exit 1
  }

  case "$OS/$ARCH" in
    Linux/x86_64)   TARGET="x86_64-unknown-linux-gnu" ;;
    Linux/aarch64)  TARGET="aarch64-unknown-linux-gnu" ;;
    Darwin/x86_64)  TARGET="x86_64-apple-darwin" ;;
    Darwin/arm64)   TARGET="aarch64-apple-darwin" ;;
    *)
      echo "Error: unsupported platform $OS/$ARCH"
      exit 1
      ;;
  esac

  echo "==> Downloading latest release for $TARGET..."
  gh release download \
    --repo wkirschbaum/build-watcher \
    --pattern "bw-${TARGET}.tar.gz" \
    --pattern "build-watcher-${TARGET}.tar.gz" \
    --dir "$TMPDIR"

  tar -xzf "$TMPDIR/bw-${TARGET}.tar.gz" -C "$TMPDIR"
  tar -xzf "$TMPDIR/build-watcher-${TARGET}.tar.gz" -C "$TMPDIR"
fi

# -- Stop any running instance ------------------------------------------------
# The daemon binary must not be running when we overwrite it (Linux raises
# "Text file busy" otherwise). This covers both service-managed and orphan
# processes left by MCP clients starting the daemon directly.

echo "==> Stopping existing service (if running)..."
if [ "$OS" = "Darwin" ]; then
  PLIST_PATH="$HOME/Library/LaunchAgents/com.build-watcher.plist"
  [ -f "$PLIST_PATH" ] && launchctl bootout "gui/$(id -u)" "$PLIST_PATH" 2>/dev/null || true
else
  systemctl --user disable --now "$BINARY_NAME.service" 2>/dev/null || true
fi
# Kill orphan processes not managed by the service manager.
pkill -f "$BINARY_PATH" 2>/dev/null || true
sleep 0.5

# -- Install binaries ---------------------------------------------------------

echo "==> Installing binaries to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"
cp "$TMPDIR/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
cp "$TMPDIR/bw" "$INSTALL_DIR/bw"

# -- Seed config file if missing ----------------------------------------------
# Only written on a fresh install; existing config is never overwritten so
# user-added repos and settings are preserved across upgrades.

CONFIG_DIR="$HOME/.config/build-watcher"
CONFIG_FILE="$CONFIG_DIR/config.json"
mkdir -p "$CONFIG_DIR"

if [ ! -f "$CONFIG_FILE" ]; then
  # Recover from backup or draft left by a crash during save.
  DRAFT_FILE="$CONFIG_FILE.draft"
  BAK_FILE="$CONFIG_FILE.bak"
  if [ -f "$DRAFT_FILE" ] && python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$DRAFT_FILE" 2>/dev/null; then
    echo "==> Recovering config from draft file..."
    mv "$DRAFT_FILE" "$CONFIG_FILE"
  elif [ -f "$BAK_FILE" ]; then
    echo "==> Recovering config from backup..."
    cp "$BAK_FILE" "$CONFIG_FILE"
  else
    echo "==> Creating default config at $CONFIG_FILE..."
    cat > "$CONFIG_FILE" <<'CONFJSON'
{
  "default_branches": ["main"],
  "notifications": {
    "build_started": "normal",
    "build_success": "normal",
    "build_failure": "critical"
  },
  "repos": {}
}
CONFJSON
    echo "  Edit $CONFIG_FILE to add repos, or use the watch_builds MCP tool."
  fi
else
  echo "==> Config already exists at $CONFIG_FILE (preserved)"
fi

# -- Install .desktop file (Linux only) ---------------------------------------

if [ "$OS" != "Darwin" ]; then
  echo "==> Installing desktop entry..."
  DESKTOP_DIR="$HOME/.local/share/applications"
  mkdir -p "$DESKTOP_DIR"
  curl -fsSL "$RAW_URL/build-watcher.desktop" -o "$DESKTOP_DIR/build-watcher.desktop"
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
  echo "  Desktop:  $DESKTOP_DIR/build-watcher.desktop"
fi

# -- Platform-specific service install ----------------------------------------
# Generates the service file from the template (substituting the binary path),
# then registers and starts it. On upgrades the service is re-enabled with the
# updated binary without any manual intervention.

if [ "$OS" = "Darwin" ]; then
  echo "==> Installing launchd service (macOS)..."
  PLIST_DIR="$HOME/Library/LaunchAgents"
  PLIST_PATH="$PLIST_DIR/com.build-watcher.plist"
  mkdir -p "$PLIST_DIR"

  curl -fsSL "$RAW_URL/src/platform/macos/com.build-watcher.plist" \
    | sed -e "s|@@BINARY_PATH@@|$BINARY_PATH|g" \
          -e "s|@@HOME@@|$HOME|g" \
    > "$PLIST_PATH"

  launchctl bootout "gui/$(id -u)" "$PLIST_PATH" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST_PATH"
  echo "  Service:  $PLIST_PATH (running)"

else
  echo "==> Installing systemd user service (Linux)..."
  SYSTEMD_DIR="$HOME/.config/systemd/user"
  mkdir -p "$SYSTEMD_DIR"

  curl -fsSL "$RAW_URL/src/platform/linux/build-watcher.service" \
    | sed -e "s|@@BINARY_PATH@@|$BINARY_PATH|g" \
    > "$SYSTEMD_DIR/$BINARY_NAME.service"

  systemctl --user daemon-reload
  systemctl --user enable --now "$BINARY_NAME.service"
  echo "  Service:  $SYSTEMD_DIR/$BINARY_NAME.service (running)"
fi

# -- Claude Code MCP registration ---------------------------------------------
# Writes the MCP server entry into ~/.claude.json so Claude Code can discover
# the running daemon. Safe to run on upgrades — the binary handles idempotency.

echo "==> Configuring Claude Code MCP server..."
"$BINARY_PATH" --register --port "$PORT"

echo ""
echo "Done! build-watcher is installed and running."
echo ""
echo "  Daemon:   $INSTALL_DIR/$BINARY_NAME"
echo "  CLI:      $INSTALL_DIR/bw"
echo "  MCP:      http://127.0.0.1:$PORT/mcp"
echo "  Config:   $CONFIG_FILE"
echo "  State:    ~/.local/state/build-watcher/watches.json"
echo ""
echo "All Claude Code sessions share the same watcher daemon."
echo "Watches persist across restarts."
echo "Restart Claude Code to pick up the new MCP server."
