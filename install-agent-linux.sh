#!/bin/bash
# install-agent-linux.sh — 无头 Linux/VPS：crontab 定时采集（无托盘，无需 GUI）
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
PY="$(command -v python3 || true)"
[ -z "$PY" ] && PY=/usr/bin/python3

if [ "$1" = "--uninstall" ]; then
    crontab -l 2>/dev/null | grep -v devin_usage | crontab - || true
    echo "uninstalled"; exit 0
fi

mkdir -p "$DIR/data"
(crontab -l 2>/dev/null | grep -v devin_usage; \
 echo "*/15 * * * * cd $DIR && $PY devin_usage.py collect >> data/cron.log 2>&1") | crontab -
echo "cron installed: */15 * * * * cd $DIR && $PY devin_usage.py collect"
echo "verify now:     $PY $DIR/devin_usage.py collect"
