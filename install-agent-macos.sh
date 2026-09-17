#!/bin/bash
# install-agent-macos.sh — macOS: launchd 托盘常驻 + crontab 定时采集
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
TRAY="$DIR/devin-usage-tray/target/release/devin-usage-tray"
PLIST=~/Library/LaunchAgents/com.devin.usage-tray.plist

if [ "$1" = "--uninstall" ]; then
    launchctl unload "$PLIST" 2>/dev/null || true
    rm -f "$PLIST"
    crontab -l 2>/dev/null | grep -v devin_usage | crontab - || true
    echo "uninstalled"; exit 0
fi

mkdir -p "$DIR/data"
if [ -x "$TRAY" ]; then
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.devin.usage-tray</string>
  <key>ProgramArguments</key><array><string>$TRAY</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>$DIR/data/tray.log</string>
  <key>StandardErrorPath</key><string>$DIR/data/tray.log</string>
</dict></plist>
EOF
launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"
echo "tray agent loaded"
else
    echo "tray exe not found, skipping (build: cd devin-usage-tray && cargo build --release)"
fi

(crontab -l 2>/dev/null | grep -v devin_usage; \
 echo "*/15 * * * * cd $DIR && /usr/bin/python3 devin_usage.py collect >> data/cron.log 2>&1") | crontab -
echo "cron installed (every 15min)"
