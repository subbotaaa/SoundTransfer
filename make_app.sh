#!/bin/sh
# Собирает SoundTransfer.app для macOS из release-бинарника.
# Запуск на Mac:  sh make_app.sh
set -e
cd "$(dirname "$0")"

cargo build --release

APP=SoundTransfer.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Иконка: собираем .icns из assets/icon.png штатными sips/iconutil
if [ -f assets/icon.png ]; then
  ICONSET=$(mktemp -d)/icon.iconset
  mkdir -p "$ICONSET"
  for sz in 16 32 64 128 256 512; do
    sips -z $sz $sz assets/icon.png --out "$ICONSET/icon_${sz}x${sz}.png" >/dev/null
    dbl=$((sz * 2))
    sips -z $dbl $dbl assets/icon.png --out "$ICONSET/icon_${sz}x${sz}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/icon.icns"
fi

cat > "$APP/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>SoundTransfer</string>
  <key>CFBundleDisplayName</key><string>SoundTransfer</string>
  <key>CFBundleIdentifier</key><string>local.soundtransfer</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>st</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

cp target/release/st "$APP/Contents/MacOS/st"

echo "Готово: $(pwd)/$APP"
echo "Перетащите SoundTransfer.app в «Программы» (Applications) — и запускайте как обычное приложение."
