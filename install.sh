#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLATFORM_DIR="$SCRIPT_DIR/src/platform"
BINARY_NAME="build-watcher"
INSTALL_DIR="$HOME/.local/bin"
BINARY_PATH="$INSTALL_DIR/$BINARY_NAME"
CLAUDE_CONFIG="$HOME/.claude.json"
PORT=8417
OS="$(uname -s)"

# -- Pre-flight checks --
command -v gh >/dev/null 2>&1 || { echo "Error: gh (GitHub CLI) is required but not found. Install it from https://cli.github.com"; exit 1; }

echo "==> Building release binary..."
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"

echo "==> Installing binary to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"

# Stop the running service before overwriting the binary (Text file busy)
if [ "$OS" = "Darwin" ]; then
  PLIST_PATH="$HOME/Library/LaunchAgents/com.build-watcher.plist"
  [ -f "$PLIST_PATH" ] && launchctl bootout "gui/$(id -u)" "$PLIST_PATH" 2>/dev/null || true
else
  systemctl --user disable --now "$BINARY_NAME.service" 2>/dev/null || true
fi
# Kill any orphan processes not managed by the service (e.g. leftover from MCP clients)
pkill -f "$BINARY_PATH" 2>/dev/null || true
sleep 0.5

cp "$SCRIPT_DIR/target/release/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"

# -- Seed config file if missing --

CONFIG_DIR="$HOME/.config/build-watcher"
CONFIG_FILE="$CONFIG_DIR/config.json"
mkdir -p "$CONFIG_DIR"

if [ ! -f "$CONFIG_FILE" ]; then
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
else
  echo "==> Config already exists at $CONFIG_FILE"
fi

# -- Install .desktop file (Linux) --

echo "==> Installing desktop entry..."
if [ "$OS" != "Darwin" ]; then
  DESKTOP_DIR="$HOME/.local/share/applications"
  mkdir -p "$DESKTOP_DIR"
  cp "$SCRIPT_DIR/build-watcher.desktop" "$DESKTOP_DIR/build-watcher.desktop"
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$DESKTOP_DIR" 2>/dev/null || true
  echo "  Desktop:  $DESKTOP_DIR/build-watcher.desktop"
fi

# -- Platform-specific service install --

if [ "$OS" = "Darwin" ]; then
  echo "==> Installing launchd service (macOS)..."
  PLIST_DIR="$HOME/Library/LaunchAgents"
  PLIST_PATH="$PLIST_DIR/com.build-watcher.plist"
  mkdir -p "$PLIST_DIR"

  sed -e "s|@@BINARY_PATH@@|$BINARY_PATH|g" \
      -e "s|@@HOME@@|$HOME|g" \
      "$PLATFORM_DIR/macos/com.build-watcher.plist" > "$PLIST_PATH"

  launchctl bootout "gui/$(id -u)" "$PLIST_PATH" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST_PATH"
  echo "  Service:  $PLIST_PATH (running)"

else
  echo "==> Installing systemd user service (Linux)..."
  SYSTEMD_DIR="$HOME/.config/systemd/user"
  mkdir -p "$SYSTEMD_DIR"

  sed -e "s|@@BINARY_PATH@@|$BINARY_PATH|g" \
      "$PLATFORM_DIR/linux/build-watcher.service" > "$SYSTEMD_DIR/$BINARY_NAME.service"

  systemctl --user daemon-reload
  systemctl --user enable --now "$BINARY_NAME.service"
  echo "  Service:  $SYSTEMD_DIR/$BINARY_NAME.service (running)"
fi

# -- Claude Code MCP config --

echo "==> Configuring Claude Code MCP server..."
"$BINARY_PATH" --register --port "$PORT"

echo ""
echo "Done! build-watcher is installed and running."
echo ""
echo "  Binary:   $INSTALL_DIR/$BINARY_NAME"
echo "  MCP:      http://127.0.0.1:$PORT/mcp"
echo "  Config:   $CONFIG_FILE"
echo "  State:    ~/.local/state/build-watcher/watches.json"
echo ""
echo "All Claude Code sessions share the same watcher daemon."
echo "Watches persist across restarts."
echo "Restart Claude Code to pick up the new MCP server."
