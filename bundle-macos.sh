#!/bin/bash
# bundle-macos.sh — 把 --panel 面板打成 DevinUsage.app（Dock/Spotlight 可启动）
set -e
DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/devin-usage-tray/target/release/devin-usage-tray"
APP="${1:-$HOME/Applications/DevinUsage.app}"

[ -x "$BIN" ] || { echo "先构建: cd devin-usage-tray && cargo build --release"; exit 1; }

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cat > "$APP/Contents/MacOS/devin-usage" <<EOF
#!/bin/bash
exec "$BIN" --panel
EOF
chmod +x "$APP/Contents/MacOS/devin-usage"

# 图标：优先直接复用 Devin.app 的官方 icns（最高 1024px，最清晰）；
# 找不到再从 assets 里的 png 生成
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
fi

cat > "$APP/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Devin 用量</string>
  <key>CFBundleDisplayName</key><string>Devin 用量</string>
  <key>CFBundleIdentifier</key><string>com.meltemi.devin-usage</string>
  <key>CFBundleExecutable</key><string>devin-usage</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF
echo "done: $APP  （可从 Dock / Spotlight 启动；窗口内容即用量面板）"
