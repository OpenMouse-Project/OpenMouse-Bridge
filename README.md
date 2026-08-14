# OpenMouse Bridge

OpenMouse Bridge is a small per-user companion process for the OpenMouse web
control panel. The first target is Windows. Its core and loopback protocol are
portable so Linux and macOS adapters can follow without changing the web app.

The initial service provides:

- process-based detection for configured game executables;
- discovery of visible Windows applications and the foreground application;
- persistent application profiles tied to a specific mouse;
- low-battery notifications with a configurable threshold and cooldown;
- Windows startup-at-login registration under the current user;
- a versioned HTTP API bound only to `127.0.0.1:17846`;
- an explicit browser-origin allowlist.

It does not run as an elevated Windows Service. It runs in the signed-in user's
session, which is required for desktop notifications and avoids administrator
permissions.

## Run locally

Install stable Rust, then run:

```sh
cargo run
```

Bridge creates `config.json` in the operating system's per-user application
configuration directory. Add games there, for example:

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
  threshold, and autostart state.
- `PUT /v1/games` replaces the tracked game list and persists it.
- `GET /v1/applications` lists visible Windows applications and identifies the
  foreground application.
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

GitHub Actions tests and lints the service on both Windows and Linux. Every
successful push to `main` updates the rolling `dev-build` prerelease with a
Windows x64 zip and checksum. The same files remain available as workflow
artifacts for individual runs.

Pushing a version tag such as `v0.1.0` creates a GitHub release containing a
Windows zip and SHA-256 checksum. Release signing is automatic when the
repository has `WINDOWS_CERTIFICATE_BASE64` and
`WINDOWS_CERTIFICATE_PASSWORD` secrets; unsigned development builds continue
to work without those secrets.
