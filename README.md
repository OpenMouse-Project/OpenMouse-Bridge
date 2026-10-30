# OpenMouse Bridge

> [!WARNING]
> **Work in progress: the `GPUI` branch.** This is the new OpenMouse Bridge,
> rebuilt on [GPUI](https://www.gpui.rs/), and it will replace `main`. It is
> version 1.0.0 and not released yet. See
> [docs/gpui-migration.md](docs/gpui-migration.md) for the plan and what has and
> has not been checked.

## Where it stands

GPUI is the only window: there is no other shell. CI builds macOS, Linux, and
Windows artifacts. Windows tray and panel behavior still need testing on a
Windows desktop; Linux has no tray icon yet.

- [x] A panel in Desktop's dark look with its ASCII background: Home, Settings
      and a Bridge info page, opening beside the tray icon
- [x] The tray icon shows the connected mouse's battery
- [x] Remembered mice (`devices.json`), found by scanning the HID bus every 5
      seconds
- [x] Low-battery and game-profile alerts on the on-screen overlay and chime
- [x] The Attack Shark R5 Ultra's battery, read natively
- [x] Updates with an update card and a progress bar
- [x] First-run behaviour, Escape and the close button hide the panel, the tray
      icon is recreated after sleep, and Bridge relaunches after a crash
- [x] Windows: build and test a portable x64 ZIP in CI
- [ ] Windows: run the tray app on a desktop, then build an installer
- [x] Linux: build and verify the AppImage in CI
- [ ] Linux: add a tray icon
- [ ] The in-app updater with `latest.json`, replacing the zip-swap one
- [ ] Moving installs of earlier versions across

```sh
cargo run
```

Bridge is a separate app from OpenMouse Desktop; Desktop is only a reference
for patterns.

---

OpenMouse Bridge is a small per-user companion process for the OpenMouse web
control panel on Windows, macOS, and Linux. Its core and loopback protocol remain
portable so additional desktop adapters do not require changes to the web app.

The initial service provides:

- a compact tray panel for status, startup, battery, and update controls,
  with a device summary row set showing the mouse's current DPI, polling
  rate, and switch mode as read from its onboard memory;
- a live tray icon that shows the connected mouse's battery, drawn square at
  the taskbar's real pixel size with either the native Segoe Fluent Icons
  battery glyph (Segoe MDL2 Assets on Windows 10) or the percentage number in
  bold text — a control-panel choice, since the Windows 11 taskbar only
  renders square tray icons faithfully — tinted by charge state: green while
  charging (the variant with the built-in bolt) or above half, amber through
  the mid band, alert red at or below the low-battery threshold, gray when
  the reading is stale, and a level glyph for every 10% — with a tooltip that
  spells everything out — `Device — 87% · Charging · full in 45 m` —
  including the estimated time remaining from the recent trend;
- process-based detection for configured game executables;
- startup discovery: Bridge scans the HID bus for mice whose brands it has
  drivers for (the same vendor table the native-hid helper drives — Pulsar,
  Logitech, Razer, Delux, VGN, and more), so the tray shows the battery from
  the first second — no web panel connection and no saved profile required;
- hybrid device mode (toggle in the panel): while the OpenMouse web app is
  disconnected, Bridge drives the mouse itself in onboard memory mode — the
  active profile's stored DPI, polling rate, and switch mode are pushed
  natively (switch mode wherever a driver supports it, e.g. Logitech
  LightForce), and the battery is read directly (including a native Pulsar
  battery reader) — and the moment the web app connects, it takes the mouse
  over live, with Bridge restoring the stored settings and reading the
  battery again on disconnect; without a saved profile Bridge leaves the
  mouse's own onboard memory untouched and just monitors it;
- discovery of visible applications and the foreground application (on Linux, foreground detection needs X11; on Wayland every entry reports `foreground: false`);
- persistent application profiles tied to a specific mouse;
- low-battery notifications with a configurable threshold and cooldown, from
  battery levels Bridge reads itself every five minutes for the mice in saved
  profiles, so alerts work without the control panel open;
- startup-at-login registration under the current user;
- native HID access for devices and collections that browsers cannot expose;
- manual or automatic updates from verified stable GitHub releases;
- a versioned HTTP API bound only to `127.0.0.1:17846`;
- an explicit browser-origin allowlist.

It does not run as an elevated Windows Service. It runs in the signed-in user's
session, which is required for the tray icon and desktop notifications and
avoids administrator permissions. Bridge settings and status live in its
custom tray panel while device control remains in the OpenMouse web app.

## Run locally

Install stable Rust, then run:

```sh
cargo run
```

On Linux, install the system libraries for the window first (Ubuntu 22.04 or
24.04):

```sh
sudo apt-get update && sudo apt-get install -y build-essential clang cmake pkg-config libudev-dev libssl-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libvulkan-dev libgl1-mesa-dev libfontconfig-dev libfreetype-dev libasound2-dev
```

Linux notes: there is no tray icon yet, so the panel is an ordinary window and
its close button quits; the overlay banner and foreground detection need X11
(on Wayland the banner falls back to a system notification and `foreground` is
always false), and the chime needs `paplay` or `aplay`. With no
`WAYLAND_DISPLAY` or `DISPLAY` (servers, SSH sessions), or with
`OPENMOUSE_BRIDGE_HEADLESS=1` / `--headless`, Bridge skips the window and serves
its HTTP API, HID, and game profiles headless (Ctrl-C stops it). Startup at
login writes `~/.config/autostart/io.openmouse.bridge.desktop`.

Bridge creates `config.json` in the operating system's per-user application
configuration directory. At startup it refreshes the tracked-game catalog from
the canonical [OpenMouse Desktop list](https://github.com/OpenMouse-Project/Desktop/blob/main/public/games.json)
through jsDelivr. The last successful catalog remains cached in `config.json`
for offline startup.

The relevant part of the generated config looks like this:

```json
{
  "batteryThresholdPercent": 20,
  "alertCooldownMinutes": 360,
  "automaticUpdates": false,
  "games": [
    {
      "name": "Counter-Strike 2",
      "executables": ["cs2.exe"]
    }
  ],
  "profiles": [],
  "allowedOrigins": [
    "https://control.openmouse.app",
    "http://localhost:5173"
  ]
}
```

For development and portable tests, `OPENMOUSE_BRIDGE_CONFIG` can point to an
explicit configuration file, `OPENMOUSE_BRIDGE_GAMES_URL` can override the
catalog endpoint, and `OPENMOUSE_BRIDGE_UPDATE_API_URL` can override the stable
release API endpoint.

## Local games and apps

OpenMouse presets remain the centrally maintained catalog. Personal targets live
separately in `localApplications` in Bridge's local config, so catalog refreshes
cannot replace them. Open Games & apps in the control panel, choose Add game or
app, then pick a running app or enter its executable name/full path. No request,
Discord account, approval or upload is involved. A connected mouse is not needed
to add a target; select one later to save its mouse settings.

Use Game to retain its profile while the matching process is running, including
when you alt-tab away. Use App to apply only while the app is in front. A full
executable path restricts matching to that installation; it never falls back to
another process with the same filename. With only a filename, all matching
executables are eligible. Windows paths ignore case and separator differences;
POSIX paths remain case-sensitive. Bridge detects processes; it does not launch
or execute the saved inputs. Foreground-only apps on Linux require X11.

Each local target has a stable `local:` ID, referenced by a mouse profile's
`application.localId`. Renaming/editing the target preserves linked mouse
settings. Removing it removes only profiles linked to that ID, not a same-name
OpenMouse preset or another target. Clearing a target's mouse settings does not
remove the target. Local data persists across Bridge restarts and is separate
from browser storage. Use the updated control panel and Bridge together; older
Bridges do not implement the local-target endpoints.

## Updates

The tray panel can check for a stable Bridge release on demand. Automatic
updates are opt-in and use the same stable-release channel. Bridge downloads
the platform archive and its published SHA-256 checksum, verifies the archive,
then replaces the installed executable and bundled `native-hid` runtime after
the current process exits. The updated Bridge relaunches automatically.

## Logs

Bridge writes daily `openmouse-bridge.*.log` files to a `logs` directory beside
its generated configuration and retains the latest seven files. On Windows this
is `%APPDATA%\OpenMouse\OpenMouse Bridge\config\logs` by default. The first startup log
line includes the resolved log directory when a custom config path or another
platform is used.

The native HID trace records WebSocket sessions, command names and durations,
enumerated device IDs, transport counts, slow scans, and errors. It does not log
HID report payloads or full device serial numbers. Set `RUST_LOG=openmouse_bridge=debug`
before starting Bridge to include unchanged polling scans and per-device
enumeration details.

## Loopback API

- `GET /v1/status` reports the Bridge version, platform, active games, the
  devices the control panel currently holds open over native HID
  (`activeDevices`, most recently opened first), battery levels, battery
  threshold, autostart and automatic-update preferences, and whether an
  OpenMouse client has completed a recent handshake.
- `PUT /v1/handshake` renews OpenMouse's 20-second connection lease. The client
  sends this heartbeat every five seconds while connected.
- `GET /v1/games` returns presets (`source: "preset"`), the user's custom
  games (`source: "custom"`) and local targets (`source: "local"`, with `id`,
  `path` and `kind`).
- `GET /v1/custom-games` returns the games the user added by hand;
  `PUT /v1/custom-games` replaces them (`[{ name, executables }]`), persists
  them under `customGames` in `config.json` and answers with the saved list.
  They live apart from the catalog, which is replaced on every start.
- `GET /v1/custom-games/image?name=…` returns the image saved for a custom
  game (404 when it has none); `PUT` with a PNG, JPEG or WebP body (at most
  512 KB) saves one. Images live in a `custom-game-images` folder beside
  `config.json` and go away with the game.
- `GET /v1/local-applications` reads personal targets. `PUT` accepts
  `{ applications: [{ id, name, executable, path, kind }] }` and atomically
  validates/persists the complete local list. Limit: 256 targets, names up to 80
  characters, executable leaf names up to 255, full paths up to 1024. Invalid
  input returns 400; failed persistence returns 500 without changing live state.
- `GET /v1/applications` lists relevant visible applications from the preset
  catalog, custom games or local targets and identifies the foreground
  application. Each item includes an `iconId`; requesting
  `GET /v1/applications/{iconId}/icon` returns its extracted icon as a PNG.
- `GET /v1/running-applications` lists visible running apps for the local picker,
  including apps outside the preset catalog.
- `PUT /v1/default-profile` keeps Bridge synchronized with the mouse and
  settings currently selected in OpenMouse. Bridge shows this profile whenever
  no game-specific profile is active.
- `GET /v1/profiles` reads saved application profiles; `PUT /v1/profiles`
  replaces and persists them.
- `PUT /v1/battery` accepts `{ deviceId, deviceName, percent, charging }` and
  applies the notification threshold and cooldown.
- `DELETE /v1/battery/{deviceId}` forgets a cached battery report. Stale reports
  can also be removed from the native panel's connection details.
- `GET /v1/hid` upgrades to the native HID WebSocket transport. It enumerates
  only vendor IDs requested by OpenMouse, reports parsed HID collections, and
  supports open, close, input, output, and feature-report operations. This lets
  the existing `@openmouse/protocol` drivers run unchanged when WebHID is absent
  or blocks a protected collection.

HTTP CORS and WebSocket upgrades both enforce the configured origin allowlist.
The listener binds only to loopback, never to a LAN or public interface.

## Current boundary

The HID WebSocket is active only while the control panel is connected. Bridge
also reads battery levels every five minutes for devices in saved profiles when
no native HID client session is active. This requires a supported native battery
reader; Pulsar currently has no background battery reader. Readings older than
twelve minutes are marked as last known rather than live.

Game detection and saved profile switching run independently in the background.
Native profile application writes DPI and polling rate for supported devices.
Full settings snapshots are stored by Bridge but applied and restored by an open
control panel. A selected profile is not proof that a hardware write succeeded.

## Tray panel

The panel is a small borderless GPUI window in OpenMouse's dark look: near-black
surfaces, the green accent, and a field of ASCII characters behind it. It has
three views, all under the same title bar (drag the title to move it; the
buttons are settings and close):

- **Home** lists the service, the active profile ("Default" unless a game's
  profile is active), and every mouse Bridge has seen, with its battery and DPI
  while it is plugged in or when it was last seen. An update card appears when
  one is available, and a note on the first launch says the app lives in the
  tray. A failed native write is shown under the card in red.
- **Settings** has run on start, hybrid mode, the tray icon style (battery or
  number), the low-battery alert level, notification sound and volume, update
  automatically and check for updates, a test notification, open logs, and a
  link to the info page.
- **Info** shows the version, platform, address, uptime, whether the control
  panel is connected, the device mode, and the config and log paths.

Clicking the tray icon (a left click, a double-click, or on macOS a right click)
opens the panel beside it: centred under the icon below the menu bar on macOS,
above a bottom taskbar on Windows. Closing it, or pressing Escape, hides it to
the tray and Bridge keeps running; Quit stops the process. The first launch
opens the panel and later launches start hidden (`OPENMOUSE_BRIDGE_SHOW_WINDOW=1`
opens it anyway, and `OPENMOUSE_BRIDGE_START_VIEW=settings|info` opens straight
onto a view, for development).

Use `OPENMOUSE_BRIDGE_CONFIG` with a disposable configuration and set
`autostartConfigured` to `true` to avoid first-run startup registration in
tests. Turning the "Run on start" switch on or off in a development build
changes the real login item, so leave it alone there.

## Verify

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

## Install on macOS

Download `OpenMouse-Bridge-macos-universal.dmg` from the latest release, open
it, and drag OpenMouse Bridge to Applications. It runs as a menu bar app with
no Terminal window. `packaging/macos/` holds the scripts that build the app
bundle and DMG; the release workflow signs and notarizes them when the
`MACOS_CERTIFICATE_BASE64`, `MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGN_IDENTITY`,
`APPLE_ID`, `APPLE_TEAM_ID` and `APPLE_APP_PASSWORD` secrets are set.

## Install on Linux

One-line install from the latest stable release (detects missing system
libraries, installs them with sudo, verifies the checksum, installs a hidraw
`udev` rule so the mouse is reachable without root, registers autostart, and
adds an OpenMouse Bridge entry to the GNOME/KDE app launcher):

```sh
curl -fsSL https://openmouse.app/bridge/download/install-linux.sh | bash
```

Flags: `--tag v1.0.0` (pin a release), `--dir DIR` (install location,
default `~/.local/share/openmouse-bridge`), `--system` (system-wide install
to `/opt/openmouse-bridge` with a `/usr/local/bin` symlink), `--no-deps`
(only report missing packages), `--no-udev` / `--no-autostart` /
`--no-launcher` (skip those steps), `--start` (launch after installing),
`--uninstall` (remove), `--yes` (non-interactive package install).

Every successful push to `main` updates the rolling `dev-build` prerelease with a
Windows x64 zip and checksum. The same files remain available as workflow
artifacts for individual runs.

Pushing a stable version tag such as `v1.0.0` publishes it as the latest GitHub
release with generated changelog notes, Windows x64, universal macOS, and Linux
x64 archives, and SHA-256 checksums. Windows signing is automatic when the
repository has `WINDOWS_CERTIFICATE_BASE64` and
`WINDOWS_CERTIFICATE_PASSWORD` secrets; unsigned development builds continue
to work without those secrets.
