#!/bin/bash
# Assembles "OpenMouse Bridge.app" from a built Bridge binary and a prepared
# native-hid directory (deps + bundled Node.js, see release.yml).
#
#   build-app.sh <binary> <native-hid-dir> <version> <out-dir>
#
# Signs ad-hoc by default. Set MACOS_SIGN_IDENTITY to a Developer ID
# Application identity for a distributable (notarizable) build.
set -euo pipefail

binary="$1"
native_hid="$2"
version="$3"
out_dir="$4"
identity="${MACOS_SIGN_IDENTITY:--}"

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
app="$out_dir/OpenMouse Bridge.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cp "$binary" "$app/Contents/MacOS/openmouse-bridge"
chmod +x "$app/Contents/MacOS/openmouse-bridge"

ditto "$native_hid" "$app/Contents/Resources/native-hid"
# Bridge's updater refuses packages that omit the bundled font's license.
cp "$repo/assets/fonts/DM-Sans-OFL.txt" "$app/Contents/Resources/DM-Sans-OFL.txt"
# npm's .bin entries are symlinks, which the update zip cannot carry.
find "$app/Contents/Resources/native-hid" -name .bin -type d -prune -exec rm -rf {} +
if [ -n "$(find "$app" -type l)" ]; then
  echo "bundle contains symlinks:" >&2
  find "$app" -type l >&2
  exit 1
fi

# 1.0.0-beta.19 -> short version as-is, build number numeric.
build="${version%%-*}"
sed -e "s/@VERSION@/$version/" -e "s/@BUILD@/$build/" \
  "$here/Info.plist" > "$app/Contents/Info.plist"

iconset="$(mktemp -d)/OpenMouse.iconset"
mkdir -p "$iconset"
icon="$here/icon-1024.png"  # regenerate with make-assets.py
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$icon" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$icon" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/OpenMouse.icns"

sign_flags=(--force --sign "$identity" --entitlements "$here/entitlements.plist")
if [ "$identity" != "-" ]; then
  sign_flags+=(--options runtime --timestamp)
fi

# Inside-out: nested Mach-O (Node, node-hid addon) first, then the app.
while IFS= read -r -d '' file; do
  if file -b "$file" | grep -q "Mach-O"; then
    codesign "${sign_flags[@]}" "$file"
  fi
done < <(find "$app/Contents/Resources" -type f -print0)
codesign "${sign_flags[@]}" "$app"

codesign --verify --deep --strict "$app"
echo "$app"
