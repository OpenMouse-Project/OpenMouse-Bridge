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

| Was | Is |
| --- | --- |
| `src/desktop.rs` (egui/eframe panel, `tray-icon`, about 2.6k lines) | **Done.** `src/gpui_shell/` (the window) and `src/runtime.rs` (the background server, commands, update events and relaunch logic the window sits on); the egui code, `native_ui.rs` and the `eframe` and `gtk` dependencies are gone |
| `src/overlay/*` (its own native window, drawn on the CPU) | Kept, and driven by the GPUI shell. Restyling it to match Desktop is optional |
| `src/updater.rs` zip download and per-OS swap scripts | In-app updater below (not started) |
| zip packages and `install-linux.sh` | Installers below (not started) |

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

This branch is the new Bridge, version 1.0.0, and is meant to replace `main`.
GPUI is the only window: the egui panel, `native_ui.rs`, `eframe`, `gtk` and the
`gpui` cargo feature are all gone, so a plain `cargo run` starts it.

- **The window** (`src/gpui_shell/`): Desktop's dark palette and ASCII
  background, a title bar (the title drags the window; settings and close
  buttons), and three views. Home shows the service, the profile ("Default"
  unless a game's profile is active), the remembered mice with their battery,
  DPI and last-seen time, a forget button for ones that are unplugged, an update
  card, and a first-run note. Settings has run on start, hybrid mode, the tray
  icon style, the alert level, sound and volume, updates, a test notification,
  open logs, and a link to the info page. Info shows version, platform, address,
  uptime, connection state and paths. The panel resizes to its content and does
  not repaint while hidden.
- **The tray:** the connected mouse's battery on the icon (glyph or number, the
  colour following charge state, a tooltip with the time left). The panel opens
  beside the icon: centred under it below the macOS menu bar, above a bottom
  Windows taskbar. macOS has no tray menu, because an attached menu swallows
  every click; a left or right click shows the panel, and Windows keeps a menu.
  Closing or Escape hides the panel. The icon is rebuilt after the PC sleeps.
  Linux has no tray yet, so its window is an ordinary one and closing it quits.
- **Alerts:** low-battery and game-profile notices show on Bridge's overlay and
  chime.
- **Remembered mice:** `devices.json` beside `config.json` keeps every mouse
  seen, reported as `knownDevices` in `/v1/status`. The HID bus is listed every 5
  seconds (about 5 ms, no device opened); "connected" follows the latest scan,
  matched by brand and then by model name, because the control panel names a
  mouse differently from the bus. Known limitation: two mice of one brand cannot
  be told apart for the connected state, and a keyboard from a brand with a
  driver (a Wooting) is listed too.
- **Batteries:** the Attack Shark R5 Ultra's is read natively; the Node helper
  cannot read it. HID enumeration runs behind one lock and hidapi is
  initialised on a thread that never exits, which fixed an intermittent
  SIGTRAP on macOS.
- **Updates:** wired to the existing updater until the in-app one replaces it.
  The updater reports how far the download has got; the panel draws a progress
  bar and percentage (a sweeping bar while the download is checked and
  unpacked), and the app exits when the updater says it is restarting, since the
  helper waits for the process to end before it installs and relaunches.
- **Relaunch:** if the window ends for any reason other than quit, restart or an
  update, Bridge starts again, with the same crash-loop limits as before.

Checked on macOS: it builds and runs with no flags; the tray reads and follows
the battery; the panel lands beside the icon and keeps its top edge when it
grows; the overlay appears and fades; headless mode serves the API with no
window; an end-to-end update against a local fake release (the archive
downloaded, the app exited by itself, the helper installed the new version and
relaunched it, no lock or temporary folders left); first launch opens the panel
once and later launches start hidden; the full test suite passes repeatedly.

Not checked: how the panel, the progress bar and the tray icon look (no
screenshots from the test environment); clicks on the panel's buttons and
switches, and Escape; anything on Windows (the tray, placement and hide code is
written and type-checks against the Windows bindings, but it has never been
built or run there); anything on Linux; and the GitHub workflows, which have
not run yet.

Next: Windows (build and run it, then the installer); the Linux tray and
AppImage, including relaunching `$APPIMAGE` rather than the mounted binary for
restart, update and login startup; the in-app updater with `latest.json`;
moving installs of earlier versions across; `install-linux.sh`, whose package
lists still name the GTK libraries; and replacing `release.yml`, which still
builds the older zip packages.
