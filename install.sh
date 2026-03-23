#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY_NAME="build-watcher"
INSTALL_DIR="$HOME/.local/bin"
CLAUDE_CONFIG="$HOME/.claude.json"
PORT=8417
OS="$(uname -s)"

echo "==> Building release binary..."
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"

echo "==> Installing binary to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"

# Stop the running service before overwriting the binary (Text file busy)
if [ "$OS" = "Darwin" ]; then
  PLIST_PATH="$HOME/Library/LaunchAgents/com.build-watcher.plist"
  [ -f "$PLIST_PATH" ] && launchctl bootout "gui/$(id -u)" "$PLIST_PATH" 2>/dev/null || true
else
  systemctl --user stop "$BINARY_NAME.service" 2>/dev/null || true
fi

cp "$SCRIPT_DIR/target/release/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"

# -- Platform-specific service install --

if [ "$OS" = "Darwin" ]; then
  echo "==> Installing launchd service (macOS)..."
  PLIST_DIR="$HOME/Library/LaunchAgents"
  PLIST_PATH="$PLIST_DIR/com.build-watcher.plist"
  mkdir -p "$PLIST_DIR"

  cat > "$PLIST_PATH" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.build-watcher</string>
  <key>ProgramArguments</key>
  <array>
    <string>$INSTALL_DIR/$BINARY_NAME</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>build_watcher=info</string>
    <key>PATH</key>
    <string>/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>$HOME/Library/Logs/build-watcher.log</string>
  <key>StandardErrorPath</key>
  <string>$HOME/Library/Logs/build-watcher.log</string>
</dict>
</plist>
EOF

  launchctl bootout "gui/$(id -u)" "$PLIST_PATH" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST_PATH"
  echo "  Service:  $PLIST_PATH (running)"

else
  echo "==> Installing systemd user service (Linux)..."
  SYSTEMD_DIR="$HOME/.config/systemd/user"
  mkdir -p "$SYSTEMD_DIR"

  cat > "$SYSTEMD_DIR/$BINARY_NAME.service" <<EOF
[Unit]
Description=Build Watcher MCP Server
After=network.target

[Service]
Type=simple
ExecStart=$INSTALL_DIR/$BINARY_NAME
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=build_watcher=info

[Install]
WantedBy=default.target
EOF

  systemctl --user daemon-reload
  systemctl --user enable --now "$BINARY_NAME.service"
  echo "  Service:  $SYSTEMD_DIR/$BINARY_NAME.service (running)"
fi

# -- Claude Code MCP config --

echo "==> Configuring Claude Code MCP server..."
if [ ! -f "$CLAUDE_CONFIG" ]; then
  echo '{}' > "$CLAUDE_CONFIG"
fi

python3 - "$CLAUDE_CONFIG" "$PORT" <<'PYEOF'
import json
import sys

config_path = sys.argv[1]
port = sys.argv[2]

with open(config_path) as f:
    config = json.load(f)

if "mcpServers" not in config:
    config["mcpServers"] = {}

config["mcpServers"]["build-watcher"] = {
    "type": "http",
    "url": f"http://127.0.0.1:{port}/mcp"
}

with open(config_path, "w") as f:
    json.dump(config, f, indent=2)
    f.write("\n")
PYEOF

echo "==> Adding permissions to Claude Code settings..."
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
if entry not in allow:
    allow.append(entry)
    perms["allow"] = allow
    settings["permissions"] = perms

    with open(settings_path, "w") as f:
        json.dump(settings, f, indent=2)
        f.write("\n")
PYEOF
fi

echo ""
echo "Done! build-watcher is installed and running."
echo ""
echo "  Binary:   $INSTALL_DIR/$BINARY_NAME"
echo "  MCP:      http://127.0.0.1:$PORT/mcp"
echo "  State:    ~/.local/state/build-watcher/watches.json"
echo ""
echo "All Claude Code sessions share the same watcher daemon."
echo "Watches persist across restarts."
echo "Restart Claude Code to pick up the new MCP server."
