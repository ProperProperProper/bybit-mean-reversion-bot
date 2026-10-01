#!/bin/bash
# Build, test, install and (re)start the paper-trading service (it never places
# orders). Runs from ~/Library/Application Support/BybitMeanReversionBot (outside
# ~/Documents, which macOS privacy protection blocks for background launchd jobs).
set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
RUNTIME="$HOME/Library/Application Support/BybitMeanReversionBot"
LABEL="com.bybitmeanreversion.bot"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

cd "$REPO"
cargo build --release --bin bot
cargo test --release --lib

mkdir -p "$RUNTIME/bin" "$RUNTIME/logs"
install -m 0755 target/release/bot "$RUNTIME/bin/bot.new"
mv -f "$RUNTIME/bin/bot.new" "$RUNTIME/bin/bot"

cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$LABEL</string>
    <key>ProgramArguments</key>
    <array><string>/usr/bin/caffeinate</string><string>-i</string><string>-s</string><string>$RUNTIME/bin/bot</string><string>serve</string></array>
    <key>WorkingDirectory</key><string>$RUNTIME</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>30</integer>
    <key>StandardOutPath</key><string>$RUNTIME/logs/bot.out</string>
    <key>StandardErrorPath</key><string>$RUNTIME/logs/bot.err</string>
    <key>EnvironmentVariables</key>
    <dict><key>RUST_LOG</key><string>info</string></dict>
</dict>
</plist>
PLIST_EOF
plutil -lint "$PLIST" >/dev/null

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
# Wait until launchd has fully removed the old job, then load (retry briefly).
for _ in $(seq 1 30); do launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || break; sleep 1; done
for i in 1 2 3 4 5; do launchctl bootstrap "gui/$(id -u)" "$PLIST" && break; sleep 3; done
echo "deployed: $RUNTIME/bin/bot serve (console http://127.0.0.1:8787, logs $RUNTIME/logs/bot.err)"
