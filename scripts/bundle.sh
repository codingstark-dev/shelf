#!/bin/sh
# Assemble Shelf.app. Usage: scripts/bundle.sh <version> <binary> <out-dir>
# Used by CI (release.yml) and local builds so both produce the same bundle.
set -eu
VERSION=$1 BIN=$2 OUT=$3
ROOT=$(cd "$(dirname "$0")/.." && pwd)
APP="$OUT/Shelf.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/shelf"
cp "$ROOT/app/assets/icons/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Shelf</string>
  <key>CFBundleDisplayName</key><string>Shelf</string>
  <key>CFBundleIdentifier</key><string>dev.shelf.app</string>
  <key>CFBundleExecutable</key><string>shelf</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><false/>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"
