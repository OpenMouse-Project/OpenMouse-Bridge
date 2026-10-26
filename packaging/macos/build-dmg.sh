#!/bin/bash
# Wraps the app in a styled drag-to-Applications disk image. Needs dmgbuild
# (pip install dmgbuild).
#
#   build-dmg.sh <app> <output.dmg>
set -euo pipefail

app="$1"
dmg="$2"
identity="${MACOS_SIGN_IDENTITY:--}"
here="$(cd "$(dirname "$0")" && pwd)"

rm -f "$dmg"
dmgbuild -s "$here/dmg-settings.py" \
  -D app="$app" \
  -D icon="$app/Contents/Resources/OpenMouse.icns" \
  -D background="$here/dmg-background.tiff" \
  "OpenMouse Bridge" "$dmg"

if [ "$identity" != "-" ]; then
  codesign --force --sign "$identity" --timestamp "$dmg"
fi
echo "$dmg"
