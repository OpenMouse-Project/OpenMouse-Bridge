# Testing in-app updates

Run this on each OS before tagging a release that changes the updater. It
points Bridge at a fake `latest.json` served from your own machine, so it
needs no real release. Linux (AppImage) was tested this way when the updater
landed; the Windows installer and macOS bundle paths have not been run yet.

The fake release offers version `1.0.1`, which is newer than the build you
run, using a copy of an asset you already have. Bridge only compares version
numbers, so the "new" build can be the same one.

## What you need

- A build of Bridge installed the normal way for that OS: from the Windows
  installer, or the `.app` from the DMG on macOS. Bridge picks its update path
  from how it was installed, so a `cargo run` build is not enough.
- The matching update asset, from the same CI run or release:
  - Windows: `OpenMouse-Bridge-windows-x64-setup.exe`
  - macOS: `openmouse-bridge-macos-app.zip`
- Python 3, to serve the files.

## 1. Serve a fake release

Put the asset in an empty folder, then write `latest.json` next to it with the
asset's SHA-256 and size in bytes.

macOS:

```sh
cd ~/bridge-update-test
asset=openmouse-bridge-macos-app.zip
cat > latest.json <<EOF
{"version":"1.0.1","notes":"- Update test","platforms":{"darwin-universal":{
  "url":"http://127.0.0.1:8765/$asset",
  "sha256":"$(shasum -a 256 "$asset" | cut -d' ' -f1)",
  "size":$(stat -f%z "$asset")}}}
EOF
python3 -m http.server 8765 --bind 127.0.0.1
```

Windows (PowerShell):

```powershell
cd $HOME\bridge-update-test
$asset = 'OpenMouse-Bridge-windows-x64-setup.exe'
$hash = (Get-FileHash $asset -Algorithm SHA256).Hash.ToLower()
$size = (Get-Item $asset).Length
@"
{"version":"1.0.1","notes":"- Update test","platforms":{"windows-x86_64":{
  "url":"http://127.0.0.1:8765/$asset","sha256":"$hash","size":$size}}}
"@ | Set-Content -Encoding ascii latest.json
python -m http.server 8765 --bind 127.0.0.1
```

Plain HTTP is accepted only from `127.0.0.1` and `localhost`.

## 2. Start Bridge against it

Quit Bridge from its panel first, then start the installed copy with the feed
override:

- macOS:
  `OPENMOUSE_BRIDGE_UPDATE_FEED_URL=http://127.0.0.1:8765/latest.json "/Applications/OpenMouse Bridge.app/Contents/MacOS/openmouse-bridge"`
- Windows (PowerShell):
  `$env:OPENMOUSE_BRIDGE_UPDATE_FEED_URL = 'http://127.0.0.1:8765/latest.json'; & "$env:LOCALAPPDATA\Programs\OpenMouse Bridge\openmouse-bridge.exe"`

## 3. Update

In the panel:

1. Settings → **Check for updates**: it should say "Version 1.0.1 is
   available" with **Install 1.0.1**. (With automatic updates on, it downloads
   by itself; skip to step 3.)
2. **Install 1.0.1**: a progress bar, then Home shows "Update 1.0.1 is ready".
3. **Restart to update**: Bridge quits, the helper installs, and Bridge comes
   back within a few seconds.

## 4. Check

- Bridge is running again from the same place (the Start menu entry on
  Windows, `/Applications` on macOS).
- `update.log` in the Bridge log folder ends with `Done`:
  - macOS: `~/Library/Application Support/io.OpenMouse.OpenMouse-Bridge/logs/`
  - Windows: `%APPDATA%\OpenMouse\OpenMouse Bridge\config\logs\`
- On Windows, `update-installer.log` in the same folder is Inno Setup's own
  log of the silent install.
- There is no `update-failed.txt` in that folder. If there is, the panel shows
  its message after the restart; keep it with `update.log` for the report.
- Nothing is left in the staging folder: `openmouse-update-*` folders in
  `%LOCALAPPDATA%` (Windows) or `$TMPDIR` (macOS).

## 5. Failure path (optional)

Edit one character of `sha256` in `latest.json` and repeat step 3: the
download must be refused with "update checksum does not match", and nothing
is installed.

Start Bridge without the override afterwards, so it checks the real releases
again.
