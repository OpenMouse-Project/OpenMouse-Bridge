# OpenMouse Bridge

OpenMouse Bridge is a small per-user companion process for the OpenMouse web
control panel. The first target is Windows. Its core and loopback protocol are
portable so Linux and macOS adapters can follow without changing the web app.

The initial service provides:

- a minimal native Windows and macOS status window with a shortcut to OpenMouse;
- process-based detection for configured game executables;
- discovery of visible Windows and macOS applications and the foreground application;
- persistent application profiles tied to a specific mouse;
- low-battery notifications with a configurable threshold and cooldown;
- Windows startup-at-login registration under the current user;
- a versioned HTTP API bound only to `127.0.0.1:17846`;
- an explicit browser-origin allowlist.

It does not run as an elevated Windows Service. It runs in the signed-in user's
session, which is required for desktop notifications and avoids administrator
permissions. Closing the status window hides it to the system tray while game
detection and the loopback API continue running. The tray menu can restore the
window, open OpenMouse, or explicitly quit Bridge.

## Run locally

Install stable Rust, then run:

```sh
cargo run
```

Bridge creates `config.json` in the operating system's per-user application
configuration directory. It automatically seeds and updates its tracked games
from the bundled [`games.json`](games.json) catalog. Custom entries written via
the API or added to the config are preserved when new catalog entries ship.

The relevant part of the generated config looks like this:

```json
{
  "batteryThresholdPercent": 20,
  "alertCooldownMinutes": 360,
  "games": [
    {
      "name": "Counter-Strike 2",
      "executables": ["cs2.exe"]
    }
  ],
  "profiles": [],
  "allowedOrigins": [
    "https://dev.openmouse.app",
    "http://localhost:5173"
  ]
}
```

For development and portable tests, `OPENMOUSE_BRIDGE_CONFIG` can point to an
explicit configuration file.

## Loopback API

- `GET /v1/status` reports the Bridge version, platform, active games, battery
  threshold, autostart state, and whether an OpenMouse client has completed a
  recent handshake.
- `PUT /v1/handshake` renews OpenMouse's 20-second connection lease. The client
  sends this heartbeat every five seconds while connected.
- `GET /v1/games` returns the full executable catalog currently being tracked.
- `PUT /v1/games` adds or updates custom tracked games and persists them. Bundled
  catalog entries are retained so a client cannot accidentally disable detection.
- `GET /v1/applications` lists running games and identifies the foreground
  game. Only applications from the registered catalog are
  returned. Each item includes an `iconId`; requesting
  `GET /v1/applications/{iconId}/icon` returns its extracted icon as a PNG.
- `PUT /v1/default-profile` keeps Bridge synchronized with the mouse and
  settings currently selected in OpenMouse. Bridge shows this profile whenever
  no game-specific profile is active.
- `GET /v1/profiles` reads saved application profiles; `PUT /v1/profiles`
  replaces and persists them.
- `PUT /v1/battery` accepts `{ deviceId, deviceName, percent, charging }` and
  applies the notification threshold and cooldown.
- `PUT /v1/autostart` accepts `{ enabled }`. It is implemented on Windows.

Only configured web origins receive CORS access. The listener never binds to a
LAN or public interface.

## Native HID for browsers without WebHID

`GET /v1/hid` upgrades to a WebSocket carrying raw HID: enumerate, open,
send and receive reports, and a stream of input reports. It exists so Firefox
and other browsers with no WebHID can run the OpenMouse control panel exactly
as Chrome does — the web app wraps this socket back into a `navigator.hid`
shim, and `@openmouse/protocol`'s driver classes run unchanged on top of it.

Unlike the rest of the API, this endpoint parses each device's HID report
descriptor (`src/hid/descriptor.rs`) and reports real `collections`. That is
what lets the web app's driver registry auto-detect a mouse here the same way
it does over WebHID, instead of falling back to a hand-maintained brand table
the way `native-hid/` has to.

Two rules are enforced on every socket:

- The handshake must carry an `Origin` from `allowedOrigins`. A WebSocket
  handshake is not covered by CORS, so this check is made by hand — it is all
  that stands between any page the user visits and their mouse.
- Generic Desktop mouse and keyboard collections are never listed or opened.
  Chrome withholds the same ones from WebHID, and opening one natively freezes
  the device's own input on macOS.

Enumeration is limited to the vendor ids the client asks for, which the web app
takes from its own supported-device filters, so a page never learns about HID
devices OpenMouse has no driver for. Every device a socket opened is closed
when it disconnects.

## Current boundary

Battery readings initially come from the connected OpenMouse control panel.
True alerts while the browser is closed require Bridge to poll a device on its
own schedule; `/v1/hid` only moves reports while a browser tab is driving it.
Game detection already runs independently in the background.

## Verify

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

## Automated builds

GitHub Actions tests and lints the service on Windows, macOS, and Linux. Every
successful push to `main` updates the rolling `dev-build` prerelease with a
Windows x64 zip and checksum. The same files remain available as workflow
artifacts for individual runs.

Pushing a stable version tag such as `v1.0.0` publishes it as the latest GitHub
release with generated changelog notes, Windows x64 and universal macOS
archives, and SHA-256 checksums. Windows signing is automatic when the
repository has `WINDOWS_CERTIFICATE_BASE64` and
`WINDOWS_CERTIFICATE_PASSWORD` secrets; unsigned development builds continue
to work without those secrets.
