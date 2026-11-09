# Changelog

Each release has an entry here, under a `## <version>` heading that matches
the version in `Cargo.toml` and the release tag. The release workflow
publishes the entry as the release notes and in `latest.json` (what Bridge's
update check shows), and refuses to release a version without one.

## 1.0.0

- A new Bridge, rebuilt from the ground up: a tray panel in OpenMouse's dark
  style with Home, Settings and a Bridge info page.
- The tray icon shows your mouse's battery, as a glyph or a percentage, in
  colours that follow the charge, with a tooltip that includes the time left.
- Bridge remembers your mice and finds them as soon as they are plugged in.
  Home lists one row per mouse rather than its receiver or dongle, and shows
  a mouse that does not answer while its receiver is plugged in as Asleep.
- Low-battery and game-profile alerts appear on an on-screen overlay with a
  chime, and Settings can send a test notification.
- Reads the Attack Shark R5 Ultra's battery directly.
- Updates install from inside the app: Bridge downloads them in the
  background and applies them when you choose Restart to update. Automatic
  updates are on by default.
- What's new: the release notes are built into Bridge. After an update Home
  offers them once, the update card shows the incoming version's notes, and
  Bridge info lists every release.
- Real installers: a per-user Windows installer, a macOS app in a
  DMG, and an AppImage on Linux.
- Linux: a tray icon on KDE Plasma and other desktops with StatusNotifierItem
  support, and the panel opens in the corner beside it.
- Linux: when mice cannot be opened because a udev rule is missing, the panel
  offers a Fix permissions button that installs it. The rule covers every USB
  mouse and receiver, whatever the brand, and only the signed-in user gets
  access.
- Linux: the control panel reconnects faster to mice paired through a
  Logitech receiver.
- Linux: on KDE Plasma's Wayland session, Bridge sees which window has focus,
  so focused-app profiles switch for native Wayland apps too.
- Linux: games running through Proton or Wine are recognised, so their game
  profiles switch on.
- macOS: Bridge no longer freezes the cursor while it talks to a mouse.
