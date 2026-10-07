#!/bin/sh
# Regenerates AppIcon.icns and app-icon-256.png from app-icon.png. Run from this directory.
set -e
T=$(mktemp -d); I="$T/AppIcon.iconset"; mkdir "$I"
for s in 16 32 128 256 512; do
  sips -z $s $s app-icon.png --out "$I/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) app-icon.png --out "$I/icon_${s}x${s}@2x.png" >/dev/null
done
sips -z 256 256 app-icon.png --out app-icon-256.png >/dev/null  # embedded by ui::hero
iconutil -c icns "$I" -o AppIcon.icns
rm -rf "$T"
