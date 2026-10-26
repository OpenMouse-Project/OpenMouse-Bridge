# OpenMouse Bridge

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

On Linux, install the system libraries for the tray icon and window first
(Ubuntu 24.04):

```sh
sudo apt-get update && sudo apt-get install -y libudev-dev libgtk-3-dev libayatana-appindicator3-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libssl-dev libgl1-mesa-dev pkg-config
```

Linux notes: the tray icon needs a StatusNotifier/AppIndicator host (stock
GNOME shows nothing without an AppIndicator extension; the panel still opens
with `OPENMOUSE_BRIDGE_SHOW_WINDOW=1`), the overlay banner and foreground
detection need X11 (on Wayland the banner falls back to a system notification
and `foreground` is always false), and the chime needs `paplay` or `aplay`.
With no `WAYLAND_DISPLAY` or `DISPLAY` (servers, SSH sessions), or with
`OPENMOUSE_BRIDGE_HEADLESS=1` / `--headless`, Bridge skips the tray panel
and serves its HTTP API, HID, and game profiles headless (Ctrl-C stops it).
Startup at login writes `~/.config/autostart/io.openmouse.bridge.desktop`.

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

The native tray panel uses OpenMouse's lit surfaces, inset controls, raised
actions, trailing switches, Emerald palette and embedded DM Sans. Home starts at
360 by 280 logical points; Settings is 360 by 420. Real content can grow Home up
to 340 points high. Extended connection and battery reports live behind the info
controls rather than filling the default view.
Home separates the Bridge service, browser connection, live HID sessions, saved
profile selection, and battery readings instead of treating them as one status.
Settings keeps startup, battery alerts, notification sound, updates, and logs in
one scrollable view.

Drag the header beside the OpenMouse name, or beside the Settings title, to move
the borderless window. Settings and Back leave its position unchanged. Clicking
the tray icon opens the panel near that icon, constrained to the monitor work
area on Windows. The titlebar minimize and close controls hide to the tray;
background operation continues. Escape also dismisses the panel. Quit Bridge
stops the process.

`OPENMOUSE_BRIDGE_TEST_POSITION=x,y` starts the panel at an explicit logical
position without requesting focus, for isolated secondary-monitor UI checks.
Use `OPENMOUSE_BRIDGE_CONFIG` with a disposable configuration and set
`autostartConfigured` to `true` to avoid first-run startup registration in tests.

## Verify

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

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
