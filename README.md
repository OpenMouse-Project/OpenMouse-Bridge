# OpenMouse Bridge

OpenMouse Bridge is a small per-user companion process for the OpenMouse web
control panel. The first target is Windows. Its core and loopback protocol are
portable so Linux and macOS adapters can follow without changing the web app.

The initial service provides:

- a tray icon with **Run on Start**, **Open OpenMouse**, and **Exit** actions;
- process-based detection for configured game executables;
- discovery of visible Windows and macOS applications and the foreground application;
- persistent application profiles tied to a specific mouse;
- low-battery notifications with a configurable threshold and cooldown;
- startup-at-login registration under the current user;
- native HID access for devices and collections that browsers cannot expose;
- a versioned HTTP API bound only to `127.0.0.1:17846`;
- an explicit browser-origin allowlist.

It does not run as an elevated Windows Service. It runs in the signed-in user's
session, which is required for the tray icon and desktop notifications and
avoids administrator permissions. Bridge has no status or settings window; the
tray menu is its complete native interface while device control remains in the
OpenMouse web app.

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
    "https://control.openmouse.app",
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
- `GET /v1/hid` upgrades to the native HID WebSocket transport. It enumerates
  only vendor IDs requested by OpenMouse, reports parsed HID collections, and
  supports open, close, input, output, and feature-report operations. This lets
  the existing `@openmouse/protocol` drivers run unchanged when WebHID is absent
  or blocks a protected collection.

HTTP CORS and WebSocket upgrades both enforce the configured origin allowlist.
The listener binds only to loopback, never to a LAN or public interface.

## Current boundary

The HID WebSocket is active only while the control panel is connected. Battery
readings still come from that panel, so true battery alerts while the browser is
closed require a background native protocol reader. Game detection and saved
profile switching already run independently in the background.

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
