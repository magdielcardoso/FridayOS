#!/usr/bin/env bash
# Builds FridayOS.app with the Info.plist keys macOS needs for microphone,
# speech recognition, automation and personal data. TCC grants are per bundle,
# so always launch the .app (not target/debug/friday-app) for real use.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo build -p friday-app -p friday-tools

APP="$ROOT/dist/FridayOS.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$ROOT/target/debug/friday-app" "$APP/Contents/MacOS/FridayOS"
cp "$ROOT/target/debug/friday-mcp" "$APP/Contents/MacOS/friday-mcp"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>FridayOS</string>
  <key>CFBundleDisplayName</key><string>Friday</string>
  <key>CFBundleIdentifier</key><string>com.magdiel.fridayos</string>
  <key>CFBundleExecutable</key><string>FridayOS</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>A Friday precisa do microfone para ouvir seus comandos de voz.</string>
  <key>NSSpeechRecognitionUsageDescription</key>
  <string>A Friday usa reconhecimento de fala no dispositivo para a wake word.</string>
  <key>NSAppleEventsUsageDescription</key>
  <string>A Friday controla aplicativos e o sistema para executar suas ordens.</string>
  <key>NSCalendarsUsageDescription</key>
  <string>A Friday lê e cria eventos no seu calendário.</string>
  <key>NSContactsUsageDescription</key>
  <string>A Friday consulta contatos quando você pede.</string>
  <key>NSRemindersUsageDescription</key>
  <string>A Friday gerencia lembretes quando você pede.</string>
  <key>NSAppleMusicUsageDescription</key>
  <string>A Friday controla a música quando você pede.</string>
</dict>
</plist>
PLIST

/usr/bin/codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true
echo "$APP"
