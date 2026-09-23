#!/bin/bash
# install-agent-linux.sh — 无头 Linux/VPS：crontab 定时采集（无托盘，无需 GUI）
# 采集器是 Rust 二进制 devin-usage-tray（--no-default-features 编译），
# 不再需要 python；旧 python crontab 行会被清掉。
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/devin-usage-tray/target/release/devin-usage-tray"

if [ "$1" = "--uninstall" ]; then
    crontab -l 2>/dev/null | grep -v -e devin_usage -e devin-usage-tray | crontab - || true
    echo "uninstalled"; exit 0
fi

if [ ! -x "$BIN" ]; then
    echo "agent 二进制不存在，先编译："
    echo "  cd $DIR/devin-usage-tray && cargo build --release --no-default-features"
    exit 1
fi

mkdir -p "$DIR/data"
(crontab -l 2>/dev/null | grep -v -e devin_usage -e devin-usage-tray; \
 echo "*/15 * * * * $BIN collect >> $DIR/data/cron.log 2>&1") | crontab -
echo "cron installed: */15 * * * * $BIN collect"
echo "verify now:     $BIN collect"
