# Bridge on GPUI, with real installers

Status: plan, nothing implemented. Branch: `GPUI`.

## Why

- The zip-swap updater has been the main source of release trouble. The 16 MiB
  archive-metadata budget in beta.19/20, and a `\\?\` path that Windows
  PowerShell 5.1 cannot walk (fixed for new binaries in #57), both live in that
  path. Beta.19/20 Windows installs probably cannot auto-update at all.
- Bridge is a tray app. egui/eframe plus `tray-icon` gives it a UI that does not
  match the rest of OpenMouse.

## Decisions

- Bridge and OpenMouse Desktop stay **separate apps**. Bridge is a new GPUI app
  in this repo. `../Desktop` is a reference to copy patterns from; it is not
  edited, and Bridge does not depend on it.
- Everything tied to the tray moves to GPUI: the tray icon and menu, the panel,
  the profile-switch overlay and native notifications.
- Windows gets a **custom installer**. macOS keeps the signed `.app` and DMG.
  Linux ships an **AppImage**.
- Updates happen inside the app and should be as seamless as possible.

## What stays, what changes

Stays (UI-independent core): `service`, `api`, `hid`, `drivers`, `buttons`,
`config`, `games`, `applications`, `local_targets`, `remote_image`, `native-hid/`.

Replaced:

| Today | Becomes |
| --- | --- |
| `src/desktop.rs` (egui/eframe panel, `tray-icon`, about 2.6k lines) | GPUI tray and panel |
| `src/native_ui.rs`, `src/overlay/*` | GPUI overlay and notifications |
| `src/updater.rs` zip download and per-OS swap scripts | in-app updater below |
| zip packages and `install-linux.sh` | installers below |

Reference in `../Desktop/gpui`: `tray.rs`, `autostart.rs`, `overlay.rs`,
`update_check.rs`, `installer/` (a Rust exe that embeds the app as its payload),
the `gpui-kit` dependency and the vendored `gpui-pre-apple` patch, and
`.github/workflows/release.yml` (`latest.json` plus the installer asset). Its
update check is check-only today, so in-app download and install are ours to
build.

## Installers

- **Windows:** a per-user installer, no admin prompt. Installs under
  `%LOCALAPPDATA%`, adds Start menu and uninstall entries, sets up autostart,
  and supports a silent update mode that waits for the running Bridge to exit.
  Built like Desktop's `gpui/installer`, with Bridge as the payload.
- **macOS:** the existing signed and notarized `.app` and DMG
  (`packaging/macos`), with the in-app updater replacing the bundle.
- **Linux:** an AppImage as the supported install and the update target.
  Mice need udev rules in `/etc/udev/rules.d`, which needs root once, so the
  first run asks through `pkexec` and writes the rule (the vendor list now in
  `install-linux.sh`). Things to test: `libfuse2` is missing on newer Ubuntu
  (use a current AppImage runtime, extract-and-run as the fallback), and
  Wayland/X11 plus GPU drivers. `.deb`/`.rpm` can be added later if asked for.
  Flatpak is out: its sandbox makes HID and udev awkward.

## Updates

1. Each release publishes `latest.json`: version, notes, and per-platform asset
   URL plus SHA-256, as Desktop's does.
2. Bridge checks on start and on a timer, downloads in the background, and
   verifies the SHA-256.
3. The update is staged. The tray shows "Restart to update" and the panel shows
   progress. Nothing is applied while the user is mid-action.
4. On restart a small helper applies it and relaunches: the installer in silent
   mode on Windows, an `.app` swap on macOS, a file replace on Linux. It logs to
   the Bridge log directory, so a failed update leaves a trace (the old Windows
   helper wrote nothing anywhere).
5. A failed apply rolls back to the previous version and says so in the tray.

## Existing installs

Beta.19 to 21 installs look for the legacy-named zips
(`openmouse-bridge-{macos-universal,windows-x64,linux-x64}.zip`) and use the
old updater. The first GPUI release has to keep those assets and keep each one
under about 15.5 MiB (the old reader scans the whole file against a 16 MiB
budget). Each should be a small bridge archive whose binary installs the new
installer on first launch. `packaging/bridge/` and the placeholder-runtime check
in `src/updater.rs` (PR #56, on `main`) are the starting point for that.
Windows beta.19/20 installs probably still need one manual install, because
their own install helper fails before it swaps anything.

## Phases

1. **Shell:** GPUI app that runs the existing core. Tray icon and menu, the panel
   with parity to today's screens, autostart. No installer or updater work yet.
2. **Overlay and notifications:** profile-switch overlay and native notices per
   platform.
3. **Updater:** `latest.json`, background download and verify, staged update,
   "Restart to update", the apply helpers, rollback and logging.
4. **Installers:** Windows installer, macOS bundle update path, Linux AppImage
   with the udev step.
5. **Release and migration:** release workflow, `latest.json`, the bridge
   archives for existing installs, and a test pass of an upgrade from each
   previous version on each OS.

## Open questions

- Which GPUI version and which `gpui-kit` pieces to take from Desktop, given its
  vendored macOS patch.
- Whether the panel keeps its current layout or follows Desktop's design.
- Code signing for the Windows installer, and where the certificate lives.
- How long to keep publishing the legacy-named bridge archives.

## Progress

Phase 1 is under way.

- `Cargo.toml` has an optional `gpui` feature (`gpui-kit` 0.7) and the
  `gpui-pre-apple` patch, vendored from Desktop into `vendor/gpui-pre-apple`.
  Default builds do not use either.
- `src/desktop.rs` exposes what a second shell needs (`BackgroundServer`,
  `BridgeCommand`, `UpdateEvent`, the label helpers, `tray_icon`,
  `restart_bridge`, window size constants). Behaviour is unchanged.
- `src/gpui_shell/` (feature `gpui`, selected with
  `OPENMOUSE_BRIDGE_SHELL=gpui`) starts the same `BackgroundServer` and polls its
  channels every 200 ms. The palette and the ASCII field are ported from
  Desktop (dark only). The panel resizes to its content and does not repaint
  while hidden in the tray.
  - Home: service, profile and remembered mice, with Open OpenMouse / Restart /
    Quit.
  - Settings: run on start, update automatically, check for updates (with an
    install button when one is found), open logs, and a link to the info page.
  - Info: version, platform, address, uptime, control-panel connection, device
    mode, profile and remembered-mouse counts, config and log paths.
  - System notifications stand in for the overlay.
- Core: `devices.json` (beside `config.json`) remembers every mouse seen: name,
  last battery reading, DPI, polling rate and when it was last seen. It is
  reported as `knownDevices` in `/v1/status`. The HID bus is listed every 5
  seconds (about 5 ms, no device opened); "connected" follows the latest scan,
  matched by brand because the control panel names a mouse differently from the
  bus. A mouse that appears gets a battery read right away. Known limitation:
  two mice of one brand cannot be told apart for the connected state. Discovery
  is by driver brand, so a keyboard from such a brand (a Wooting) is listed too.

Checked on macOS: it builds, starts, serves the API, remembers mice across
restarts (unit tests), and refreshes "last seen" every scan against real
hardware. Idle CPU on a debug build was about 1.7% of a core with the panel
hidden and 7% visible. Not checked: clicks on the tray icon, the switches and
buttons, anything on Windows (the tray code has never been compiled there), and
Linux (no tray yet).

Next: panel placement at the tray icon and hide-on-escape, Home parity with the
egui panel (battery details, remove-stale control), then the GPUI overlay and
notifications.
