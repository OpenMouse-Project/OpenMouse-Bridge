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
- `PUT /v1/handshake` renews OpenMouse's 12-second connection lease. The client
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

## Current boundary

Battery readings initially come from the connected OpenMouse control panel.
True alerts while the browser is closed require native HID/protocol support in
Bridge and are a later milestone. Game detection already runs independently in
the background.

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

Pushing a version tag such as `v0.1.0` creates a GitHub release containing a
Windows zip and SHA-256 checksum. Release signing is automatic when the
repository has `WINDOWS_CERTIFICATE_BASE64` and
`WINDOWS_CERTIFICATE_PASSWORD` secrets; unsigned development builds continue
to work without those secrets.
