#!/bin/bash
# Builds the small "bridge" archive published under the original, unsuffixed
# asset names (openmouse-bridge-<platform>.zip) for macOS and Linux.
#
# Bridge 1.0.0-beta.19 and .20 cannot open an update archive over 16 MiB: they
# read the whole file looking for the zip index and give up at a 16 MiB budget
# ("ZIP metadata read budget exceeded"). The full packages are far larger, so
# those installs get this archive instead. It carries the real binary plus the
# files their package check requires, with a placeholder where the bundled
# Node.js runtime goes; on first launch the new binary sees the placeholder
# and installs the full package (openmouse-bridge-<platform>-full.zip).
#
#   build-bridge-package.sh <binary> <output.zip> [extra top-level files...]
#
# Run from the repository root after native-hid has been prepared. Needs zip.
set -euo pipefail

binary="$1"
output="$2"
shift 2

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

cp "$binary" "$stage/openmouse-bridge"
chmod +x "$stage/openmouse-bridge"
cp assets/fonts/DM-Sans-OFL.txt README.md "$stage/"
for extra in "$@"; do
  cp "$extra" "$stage/"
done

hid="$stage/native-hid"
mkdir -p "$hid/node/bin" "$hid/node_modules/node-hid" \
  "$hid/node_modules/@openmouse/protocol" "$hid/node_modules/@openmouse/keyboard-protocol"
cp native-hid/package.json "$hid/"
cp -R native-hid/src "$hid/src"
cp native-hid/node_modules/node-hid/package.json "$hid/node_modules/node-hid/"
cp native-hid/node_modules/@openmouse/protocol/package.json "$hid/node_modules/@openmouse/protocol/"
cp native-hid/node_modules/@openmouse/keyboard-protocol/package.json "$hid/node_modules/@openmouse/keyboard-protocol/"
printf '#!/bin/sh\n# Placeholder: Bridge replaces this with the real Node.js runtime on first launch.\nexit 1\n' \
  > "$hid/node/bin/node"
chmod +x "$hid/node/bin/node"

output="$(cd "$(dirname "$output")" && pwd)/$(basename "$output")"
rm -f "$output"
(cd "$stage" && zip -r -X -q -9 "$output" .)

size="$(wc -c < "$output" | tr -d ' ')"
# No size cap: Bridge 1.0.0 is a fresh release that nothing older updates from,
# so the 16 MiB read budget of beta.19/.20 no longer constrains this archive.
echo "$output: $size bytes"
