#!/bin/bash
# install-agent-macos.sh — macOS: launchd 托盘常驻 + crontab 定时采集
# 采集器是 Rust 二进制 devin-usage-tray（托盘进程本身也会每 15min 自采一轮，
# crontab 只是冗余兜底）；不再需要 python。
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
TRAY="$DIR/devin-usage-tray/target/release/devin-usage-tray"
PLIST=~/Library/LaunchAgents/com.devin.usage-tray.plist

if [ "$1" = "--uninstall" ]; then
    launchctl unload "$PLIST" 2>/dev/null || true
    rm -f "$PLIST"
    crontab -l 2>/dev/null | grep -v -e devin_usage -e devin-usage-tray | crontab - || true
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
bash "$DIR/bundle-macos.sh" || true   # 面板 .app（菜单栏拥挤时用）
else
    echo "tray exe not found, skipping (build: cd devin-usage-tray && cargo build --release)"
fi

if [ -x "$TRAY" ]; then
(crontab -l 2>/dev/null | grep -v -e devin_usage -e devin-usage-tray; \
 echo "*/15 * * * * $TRAY collect >> $DIR/data/cron.log 2>&1") | crontab -
echo "cron installed (every 15min, rust binary)"
fi
