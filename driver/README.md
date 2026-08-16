# OpenMouse Attack Shark X11 WinUSB driver package

`OpenMouse-AttackShark-X11.inf` binds Microsoft's in-box **WinUSB** driver to
**interface 2** (the configuration interface) of the Attack Shark X11 / R1
family, so the OpenMouse Bridge can send settings over raw USB. It replaces the
need for Zadig — it makes the same WinUSB assignment, but scoped and packaged by
us.

## Why this is safe

- **No firmware is written.** This only changes which Windows driver serves one
  USB interface. The mouse's firmware is never modified, so installing or
  removing this package **cannot brick the mouse**.
- **It matches only interface 2 of three product IDs**
  (`VID_1D57&PID_FA60&MI_02`, `…FA55&MI_02`, `…FA61&MI_02`). Windows binds
  drivers by exact hardware ID, so this package **cannot** attach to the
  pointing interface (interface 1), the keyboard interface, or any other device.
  Pointing and clicking are never affected.
- **Fully reversible.** Remove the package and Windows restores the default HID
  driver on the next reconnect. While interface 2 is on WinUSB, that interface
  stops acting as HID (its media keys, if any, pause); everything else works.

## Requirements

Windows will only install a driver package whose catalog (`.cat`) is signed with
a trusted certificate. There are two routes.

### A. Production (recommended for release)

1. Generate the catalog with the WDK's `inf2cat`:
   ```
   inf2cat /driver:. /os:10_X64,10_ARM64
   ```
2. Sign `OpenMouse-AttackShark-X11.cat` with your code-signing certificate
   (attestation-signed through the Microsoft Partner Center for a fully silent,
   universally trusted install), e.g.:
   ```
   signtool sign /fd sha256 /a /tr http://timestamp.digicert.com /td sha256 OpenMouse-AttackShark-X11.cat
   ```
3. Ship the `.inf` + signed `.cat` with the Bridge.

### B. Local testing (developers, one machine) — the easy way

Run the bundled script from an **administrator** PowerShell. It creates a free
self-signed certificate, trusts it, builds and signs the catalog, and installs
the package — no paid certificate and no Windows Driver Kit required:

```powershell
powershell -ExecutionPolicy Bypass -File .\sign-and-install.ps1
```

To revert:

```powershell
powershell -ExecutionPolicy Bypass -File .\uninstall.ps1
```

The script uses only built-in Windows/PowerShell tooling (`New-SelfSignedCertificate`,
`New-FileCatalog`, `Set-AuthenticodeSignature`, `pnputil`), and automatically
uses the WDK's `inf2cat` instead if you happen to have it installed.

## Install

From an **administrator** prompt, with the mouse plugged in:

```
pnputil /add-driver OpenMouse-AttackShark-X11.inf /install
```

`pnputil` adds the package to the driver store and applies it to any matching
present device. Because the `[Models]` section lists only interface 2 of the
three known PIDs, it can only bind there. Reconnect the mouse if prompted; the
OpenMouse Bridge should then show it as controllable.

## Uninstall (revert to the normal HID driver)

Find the published name of the package, then delete it:

```
pnputil /enum-drivers                      &:: look for Provider "OpenMouse"
pnputil /delete-driver oemNN.inf /uninstall
```

(Replace `oemNN.inf` with the published name shown by `/enum-drivers`.) Or, in
Device Manager, right-click the "Attack Shark … config interface (OpenMouse
WinUSB)" device → Uninstall device → tick "Attempt to remove the driver". Unplug
and replug the mouse afterwards.
