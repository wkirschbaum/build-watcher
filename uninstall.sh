#!/usr/bin/env bash
set -euo pipefail

BINARY_NAME="build-watcher"
INSTALL_DIR="$HOME/.local/bin"
CLAUDE_CONFIG="$HOME/.claude.json"
OS="$(uname -s)"

echo "==> Stopping service..."
if [ "$OS" = "Darwin" ]; then
  PLIST_PATH="$HOME/Library/LaunchAgents/com.build-watcher.plist"
  if [ -f "$PLIST_PATH" ]; then
    launchctl bootout "gui/$(id -u)" "$PLIST_PATH" 2>/dev/null || true
    rm -f "$PLIST_PATH"
    echo "  Removed launchd service"
  fi
else
  systemctl --user stop "$BINARY_NAME.service" 2>/dev/null || true
  systemctl --user disable "$BINARY_NAME.service" 2>/dev/null || true
  SERVICE_FILE="$HOME/.config/systemd/user/$BINARY_NAME.service"
  if [ -f "$SERVICE_FILE" ]; then
    rm -f "$SERVICE_FILE"
    systemctl --user daemon-reload
    echo "  Removed systemd service"
  fi
fi

echo "==> Removing binary..."
rm -f "$INSTALL_DIR/$BINARY_NAME"

echo "==> Removing desktop entry..."
DESKTOP_FILE="$HOME/.local/share/applications/$BINARY_NAME.desktop"
if [ -f "$DESKTOP_FILE" ]; then
  rm -f "$DESKTOP_FILE"
  command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
  echo "  Removed $DESKTOP_FILE"
fi

echo "==> Removing MCP server from Claude Code config..."
if [ -f "$CLAUDE_CONFIG" ]; then
  python3 - "$CLAUDE_CONFIG" <<'PYEOF'
import json
import sys

config_path = sys.argv[1]

with open(config_path) as f:
    config = json.load(f)

changed = False
servers = config.get("mcpServers", {})
if "build-watcher" in servers:
    del servers["build-watcher"]
    changed = True

if changed:
    with open(config_path, "w") as f:
        json.dump(config, f, indent=2)
        f.write("\n")
PYEOF
  echo "  Removed build-watcher from ~/.claude.json"
fi

echo "==> Removing permissions from Claude Code settings..."
CLAUDE_SETTINGS="$HOME/.claude/settings.json"
if [ -f "$CLAUDE_SETTINGS" ]; then
  python3 - "$CLAUDE_SETTINGS" <<'PYEOF'
import json
import sys

settings_path = sys.argv[1]

with open(settings_path) as f:
    settings = json.load(f)

perms = settings.get("permissions", {})
allow = perms.get("allow", [])

entry = "mcp__build-watcher__*"
if entry in allow:
    allow.remove(entry)
    perms["allow"] = allow
    settings["permissions"] = perms

    with open(settings_path, "w") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")
PYEOF
  echo "  Removed permissions"
fi

echo ""
echo "Done! build-watcher has been uninstalled."
echo ""
echo "Config and state files are preserved:"
echo "  Config: ~/.config/build-watcher/"
echo "  State:  ~/.local/state/build-watcher/"
echo ""
echo "To remove all data: rm -rf ~/.config/build-watcher ~/.local/state/build-watcher"
