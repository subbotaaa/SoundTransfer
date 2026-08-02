#!/bin/sh
# Собирает SoundTransfer.app для macOS из release-бинарника.
#   sh make_app.sh              — только собрать .app
#   sh make_app.sh --autostart  — собрать, положить в /Applications и
#                                 настроить автозапуск при входе со
#                                 скрытым окном (только строка меню)
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

if [ "$1" = "--autostart" ]; then
  echo "Устанавливаю в /Applications и настраиваю автозапуск…"
  rm -rf /Applications/SoundTransfer.app
  cp -R "$APP" /Applications/SoundTransfer.app

  PLIST="$HOME/Library/LaunchAgents/local.soundtransfer.plist"
  mkdir -p "$HOME/Library/LaunchAgents"
  cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>local.soundtransfer</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Applications/SoundTransfer.app/Contents/MacOS/st</string>
    <string>--hidden</string>
  </array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
EOF
  # Перезагружаем агент (bootstrap — современный способ, load — запасной)
  launchctl bootout "gui/$(id -u)/local.soundtransfer" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>/dev/null || launchctl load "$PLIST"

  echo "Автозапуск настроен: при входе в систему приложение стартует"
  echo "скрытым — только иконка в строке меню. Если раньше добавляли"
  echo "SoundTransfer в «Объекты входа» — удалите его там, чтобы не"
  echo "запускалось дважды."
else
  echo "Перетащите SoundTransfer.app в «Программы» (Applications) — и запускайте как обычное приложение."
  echo "Для автозапуска в скрытом виде: sh make_app.sh --autostart"
fi
