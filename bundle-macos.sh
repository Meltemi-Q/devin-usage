#!/bin/bash
# bundle-macos.sh — 打 DevinUsage.app（自包含：二进制拷进包内，Dock/Spotlight 可启动）
# 用法: ./bundle-macos.sh [输出路径.app]   默认 ~/Applications/DevinUsage.app
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/devin-usage-tray/target/release/devin-usage-tray"
APP="${1:-$HOME/Applications/DevinUsage.app}"
VER=$(grep -m1 '^version' "$DIR/devin-usage-tray/Cargo.toml" | cut -d'"' -f2)

[ -x "$BIN" ] || { echo "先构建: cd devin-usage-tray && cargo build --release"; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# 自包含：二进制本体进包，更新时重跑本脚本即可
cp "$BIN" "$APP/Contents/MacOS/devin-usage-tray"
cat > "$APP/Contents/MacOS/devin-usage" <<'EOF'
#!/bin/bash
exec "$(dirname "$0")/devin-usage-tray" --panel
EOF
chmod +x "$APP/Contents/MacOS/devin-usage" "$APP/Contents/MacOS/devin-usage-tray"

# 图标：优先复用 Devin.app 官方 icns；找不到再从 assets png 生成
DEVIN_ICNS="/Applications/Devin.app/Contents/Resources/Devin.icns"
if [ -f "$DEVIN_ICNS" ]; then
  cp "$DEVIN_ICNS" "$APP/Contents/Resources/icon.icns"
else
  ICONSET="$(mktemp -d)/icon.iconset"
  mkdir -p "$ICONSET"
  SRC="$DIR/devin-usage-tray/assets/devin-logo-1024.png"
  [ -f "$SRC" ] || SRC="$DIR/devin-usage-tray/assets/devin-logo.png"
  for s in 16 32 64 128 256 512; do
    sips -z $s $s "$SRC" --out "$ICONSET/icon_${s}x${s}.png" >/dev/null 2>&1
  done
  cp "$ICONSET/icon_32x32.png"   "$ICONSET/icon_16x16@2x.png"
  cp "$ICONSET/icon_64x64.png"   "$ICONSET/icon_32x32@2x.png"
  cp "$ICONSET/icon_256x256.png" "$ICONSET/icon_128x128@2x.png"
  cp "$ICONSET/icon_512x512.png" "$ICONSET/icon_256x256@2x.png"
  sips -z 1024 1024 "$SRC" --out "$ICONSET/icon_512x512@2x.png" >/dev/null 2>&1
  iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/icon.icns" 2>/dev/null || true
  rm -rf "$(dirname "$ICONSET")"
fi

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Devin 用量</string>
  <key>CFBundleDisplayName</key><string>Devin 用量</string>
  <key>CFBundleIdentifier</key><string>com.meltemi.devin-usage</string>
  <key>CFBundleExecutable</key><string>devin-usage</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VER</string>
  <key>CFBundleVersion</key><string>$VER</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
EOF

# adhoc 签名：本机 Gatekeeper/启动检查需要有效签名结构（自用足够；
# 要分发给别人才需要 Apple Developer 证书 + notarize）
codesign --force --deep -s - "$APP" 2>/dev/null || true
echo "done: $APP  v$VER（自包含包，Dock/Spotlight 可启动）"
