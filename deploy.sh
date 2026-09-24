#!/bin/bash
# deploy.sh — 把本机源码同步到远端设备并原地重建/重启。
# 用法: ./deploy.sh          # 部署全部已配置设备
#       ./deploy.sh mac vps  # 只部署指定设备
#
# 设备表：host:远程目录:feature标志(:重启命令)
#   gui    = cargo build --release（含托盘/面板）
#   headless = cargo build --release --no-default-features（纯采集器）
# 注意：远端不需要 git；本脚本只同步源码，data/ 与 target/ 不动。
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR"

declare -A REMOTE=(
  [mac]="/Users/meltemi/devin-usage"
  [vps]="/root/devin-usage"
)
declare -A FLAVOR=( [mac]=gui [vps]=headless )

hosts=("$@")
[ ${#hosts[@]} -eq 0 ] && hosts=(mac vps)

for h in "${hosts[@]}"; do
  r="${REMOTE[$h]}"
  [ -z "$r" ] && { echo "?? 未知设备 $h（在 REMOTE 表里加）"; continue; }
  echo "=== $h → $r ==="

  # 1) 同步源码（tar over ssh，排除 target/data/.git）；
  #    捎带本机 commit hash，远端编译出的 --version 与源一致
  git rev-parse --short HEAD > devin-usage-tray/GIT_HASH 2>/dev/null || true
  tar czf - --exclude=target --exclude=data --exclude=.git \
      devin-usage-tray devin_usage.py bundle-macos.sh \
      install-agent-macos.sh install-agent-linux.sh install-task.ps1 \
      README.md COLLECTORS.md 2>/dev/null | ssh "$h" "cd $r && tar xzf -"

  # 2) 远端重建
  if [ "${FLAVOR[$h]}" = "gui" ]; then
    ssh "$h" "cd $r/devin-usage-tray && ~/.cargo/bin/cargo build --release 2>&1 | tail -2"
    # 托盘走 launchd KeepAlive：杀掉即自动以新二进制重启
    ssh "$h" 'pkill -f "devin-usage-tray$" 2>/dev/null; sleep 1; true'
    # 面板 .app 重打包（自包含二进制已更新）
    ssh "$h" "$r/bundle-macos.sh 2>&1 | tail -1"
  else
    ssh "$h" "cd $r/devin-usage-tray && ~/.cargo/bin/cargo build --release --no-default-features 2>&1 | tail -2"
  fi

  # 3) 报告远端版本
  ssh "$h" "$r/devin-usage-tray/target/release/devin-usage-tray --version" || true
done
echo "=== deploy 完成 ==="
