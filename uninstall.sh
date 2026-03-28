#!/usr/bin/env bash
# uninstall.sh — Remove build-watcher from the system.
#
# Stops and removes the platform service, deletes the installed binaries and
# desktop entry, and removes the MCP registration from ~/.claude.json and the
# tool permission from ~/.claude/settings.json.
#
# Works regardless of how build-watcher was originally installed (from a GitHub
# release or built from source) — all installation paths are identical.
#
# Config and state files (~/.config/build-watcher/ and
# ~/.local/state/build-watcher/) are intentionally preserved so that watches
# and settings survive a reinstall. Remove them manually if you want a clean
# slate.
#
# Usage: bw uninstall
#        curl -fsSL https://raw.githubusercontent.com/wkirschbaum/build-watcher/main/uninstall.sh | bash
#        ./uninstall.sh

set -euo pipefail

BINARY_NAME="build-watcher"
INSTALL_DIR="$HOME/.local/bin"
CLAUDE_CONFIG="$HOME/.claude.json"
OS="$(uname -s)"

# -- Stop and remove the platform service -------------------------------------

echo "==> Stopping service..."
if [ "$OS" = "Darwin" ]; then
  PLIST_PATH="$HOME/Library/LaunchAgents/com.build-watcher.plist"
  if [ -f "$PLIST_PATH" ]; then
    launchctl bootout "gui/$(id -u)" "$PLIST_PATH" 2>/dev/null || true
    rm -f "$PLIST_PATH"
    echo "  Removed launchd service"
  else
    echo "  No launchd service found (skipping)"
  fi
else
  systemctl --user stop "$BINARY_NAME.service" 2>/dev/null || true
  systemctl --user disable "$BINARY_NAME.service" 2>/dev/null || true
  SERVICE_FILE="$HOME/.config/systemd/user/$BINARY_NAME.service"
  if [ -f "$SERVICE_FILE" ]; then
    rm -f "$SERVICE_FILE"
    systemctl --user daemon-reload
    echo "  Removed systemd service"
  else
    echo "  No systemd service found (skipping)"
  fi
fi

# -- Remove installed binaries ------------------------------------------------

echo "==> Removing binaries..."
rm -f "$INSTALL_DIR/$BINARY_NAME"
rm -f "$INSTALL_DIR/bw"
echo "  Removed $INSTALL_DIR/$BINARY_NAME and $INSTALL_DIR/bw"

# -- Remove desktop entry (Linux only) ----------------------------------------

if [ "$OS" != "Darwin" ]; then
  echo "==> Removing desktop entry..."
  DESKTOP_FILE="$HOME/.local/share/applications/$BINARY_NAME.desktop"
  if [ -f "$DESKTOP_FILE" ]; then
    rm -f "$DESKTOP_FILE"
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$HOME/.local/share/applications" 2>/dev/null || true
    echo "  Removed $DESKTOP_FILE"
  else
    echo "  No desktop entry found (skipping)"
  fi
fi

# -- Remove MCP server from Claude Code config --------------------------------
# Edits ~/.claude.json in place to delete the "build-watcher" entry from
# mcpServers. Uses Python to preserve JSON formatting and structure.

echo "==> Removing MCP server from Claude Code config..."
if [ -f "$CLAUDE_CONFIG" ]; then
  python3 - "$CLAUDE_CONFIG" <<'PYEOF'
import json
import sys

config_path = sys.argv[1]

with open(config_path) as f:
    config = json.load(f)

servers = config.get("mcpServers", {})
if "build-watcher" in servers:
    del servers["build-watcher"]
    with open(config_path, "w") as f:
        json.dump(config, f, indent=2)
        f.write("\n")
    print("  Removed build-watcher from ~/.claude.json")
else:
    print("  build-watcher not found in ~/.claude.json (skipping)")
PYEOF
else
  echo "  ~/.claude.json not found (skipping)"
fi

# -- Remove tool permissions from Claude Code settings -----------------------
# Edits ~/.claude/settings.json to remove the "mcp__build-watcher__*" allow
# entry added during installation.

echo "==> Removing permissions from Claude Code settings..."
CLAUDE_SETTINGS="$HOME/.claude/settings.json"
if [ -f "$CLAUDE_SETTINGS" ]; then
  python3 - "$CLAUDE_SETTINGS" <<'PYEOF'
import json
import sys

settings_path = sys.argv[1]

with open(settings_path) as f:
    settings = json.load(f)

allow = settings.get("permissions", {}).get("allow", [])
entry = "mcp__build-watcher__*"
if entry in allow:
    allow.remove(entry)
    with open(settings_path, "w") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")
    print("  Removed mcp__build-watcher__* permission")
else:
    print("  Permission entry not found (skipping)")
PYEOF
else
  echo "  ~/.claude/settings.json not found (skipping)"
fi

echo ""
echo "Done! build-watcher has been uninstalled."
echo ""
echo "Config and state files are preserved:"
echo "  Config: ~/.config/build-watcher/"
echo "  State:  ~/.local/state/build-watcher/"
echo ""
echo "To remove all data: rm -rf ~/.config/build-watcher ~/.local/state/build-watcher"
