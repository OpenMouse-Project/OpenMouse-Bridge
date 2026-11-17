# native-hid

A small Node.js helper Bridge spawns as a subprocess so it can push a saved
profile's DPI/polling rate to a mouse over native HID, and read the mouse's
battery, without a browser tab open holding a WebHID connection.

## Why Node, in a Rust project

Bridge has a dependency-free native Rust driver for the one protocol that's
been hardware-verified in this workspace (Pulsar — see `src/drivers/pulsar.rs`
in the parent crate). Every other brand OpenMouse supports is implemented in
`@openmouse/protocol` (the sibling `mouse-protocol` repo) as a WebHID driver
class, hardware-tested there, not here. Reimplementing all of those in Rust
would mean re-deriving and re-verifying ~15,000 lines of vendor-specific wire
protocol with no hardware in hand to test most of it against — real risk of
silently writing wrong values to someone's mouse.

Instead, this package makes those exact same driver classes runnable outside
a browser: `hid-device-adapter.mjs` wraps `node-hid` so it satisfies the
`HIDDevice` interface the drivers are written against (see
`mouse-protocol/src/drivers/webhid.ts`), and `apply.mjs` constructs the
already-verified class for a given brand directly — no protocol code is
duplicated or reimplemented here.

## How dispatch works

Drivers are chosen the way the control panel chooses them: by
`@openmouse/protocol`'s own registry (`DEVICE_DRIVERS`, first `supports()`
match wins), so every brand the panel supports is covered, and a new brand
in the protocol package needs no change here.

The registry's `supports()` checks need what WebHID gives the panel: each
device's collections, parsed from its report descriptors. node-hid cannot
read descriptors, so Bridge does it (`describe_devices` in `src/hid.rs`, the
same grouping and parser as the panel's own Bridge transport) and sends the
described devices with each request:

1. Once per run, Bridge asks for the registry's `SUPPORTED_HID_FILTERS`
   (`{"action": "filters"}`), the filters the panel requests devices with.
   Bridge only ever opens a device one of them matches, and only to read its
   descriptors, so hardware that shares a mouse maker's chip vendor id (a
   motherboard's RGB controller, a touchpad) is never touched.
2. When the devices on the bus change, Bridge asks which brand the registry
   files each under (`{"action": "identify", "devices": [...]}`). Those are
   the mice it lists.
3. A status or apply request carries the described devices of that brand
   (`"devices": [...]`). `registry.mjs` builds the registry's client for each
   on a `DescribedDeviceAdapter`, which sends a report down only the
   collection whose descriptor declares it, padded to the declared size as
   Chrome does (Windows refuses a feature report of any other length), and
   reads feature reports at their declared size. `client.open()` then
   `client.readStatus()` — a cheap, read-only call every `SupportedClient`
   implements — probe it before anything is written. When a profile's brand
   is not the one the registry files its mouse under (`"Attack Shark"` saved
   for hardware filed as `"Delux"`), Bridge sends `"matchStatusBrand": true`
   and every supported device, and the helper keeps the one whose status
   reports the brand.

`select-client.mjs` prefers a device whose status includes a battery, so a
receiver's battery-less interfaces never hide the mouse's.

Without `devices` in the request (Node.js could not load the registry, or
Zaunkoenig's acknowledged automation, which identifies its endpoint by
node-hid path), the helper falls back to `brands.mjs`: a brand's vendor ids
and candidate classes, every interface tried in turn.

The Attack Shark X11 family is the partial exception. Its settings channel is not
visible through WebHID, so `apply.mjs` first checks for VID `0x1d57`, PIDs
`0xfa55`/`0xfa60`/`0xfa61`, interface 2: polling-only requests are sent as the
verified polling feature report directly through `node-hid`. This keeps the normal operating-system
HID driver in place; WinUSB and Zadig are not required. Requests involving DPI
fall through to the generic `AttackSharkHidClient` path, which drives the 0x04
six-stage table for model `0x55` (mouse-protocol #174), preserving the stages
the request does not touch.

The Scyrox V8's dongle stops answering on its `0xff02` config collection on
Windows while its sibling vendor collections are open and read, so only that
collection is opened for Scyrox (`ONLY_USAGE_PAGE` in `registry.mjs`, and the
`usagePage` of its `brands.mjs` entry for the fallback).

## Button listener

`button-listener.mjs` is the one long-running helper. While a game's profile assigns
the side buttons of a Logitech mouse, Bridge (`src/buttons.rs`) starts it with
`{"brand": "Logitech"}` on stdin. It puts the mouse on the Host profile if it was
on an onboard one, turns the mouse's button events on, and prints
`{"event": "buttons", "mask": 16}` lines; Bridge presses the assigned key. Closing
stdin stops it: the events go off and the mouse returns to the mode it was in.
It needs an `@openmouse/protocol` release that has `startButtonListener`.

## Exit code contract (Bridge depends on this — see `src/drivers/native_hid.rs`)

- `0` — a device was found and the requested settings were applied. In
  status mode (`{"brand": "…", "action": "status"}` on stdin) the device's
  status was read instead, and its battery is printed on stdout as
  `{"batteryPercent": 41, "batteryState": "Discharging"}`.
- `3` — no driver is registered for this brand. Not an error.
- `1` — a driver exists for this brand, but no device answered, or applying
  failed. Details on stderr.

## Setup

```sh
npm install
```

`@openmouse/protocol` is a normal git dependency pinned to a specific commit
(`package.json`), matching how `openmouse` itself depends on it — not a
local `file:`/symlink reference to the sibling `../../mouse-protocol`
checkout, deliberately: that checkout can have local work in progress (it
did when this package was created — untracked G-Wolves driver files that
briefly broke its own build), and a `file:` dependency would rebuild it as a
side effect of `npm install` here. Bump the pinned commit by hand when you
want a newer `mouse-protocol`.

Both dependencies have install scripts (`node-hid` needs to select its
prebuilt native binding; `@openmouse/protocol` needs to compile its
TypeScript) — this environment's npm requires approving those explicitly:

```sh
npm approve-scripts node-hid "@openmouse/protocol"
npm install   # re-run once scripts are approved, so they actually execute
```

## Bundling for distribution

Bridge's release packaging (`.github/workflows/release.yml`) bundles this
whole directory next to the `openmouse-bridge` executable, plus a
standalone Node.js runtime under `native-hid/node/` — end users don't need
Node.js installed. `src/drivers/native_hid.rs` looks for that bundled
runtime first and falls back to `node` on `PATH`, so local development
(this README's setup above) works without it.

## Status

- Attack Shark X11 polling was hardware-verified on Windows with the wireless
  PID `0xfa60`, interface 2, using the standard Microsoft HID driver. Switching
  between 500 Hz and 1000 Hz succeeded without WinUSB or Zadig. DPI goes
  through the generic `AttackSharkHidClient` 0x04 path (six-stage table,
  model `0x55`); the reporter in mouse-protocol #161 confirmed DPI and
  polling controls working after that fix, with a read-back write round-trip
  still to be captured.
- Scyrox V8 reads were hardware-verified on Windows through its 8K dongle
  (`0x3554:0xf5f7`): a `HidDeviceAdapter` on the `0xff02` collection alone
  read 1,600 DPI and 2,000 Hz, while one on every interface-1 collection got
  no answer. The full `apply.mjs` path still needs a run on that mouse, and
  an `@openmouse/protocol` release with the V8 in its Scyrox catalog.
- The registry path was hardware-verified on Windows with a Keychron G6 HE
  8K on its TurboLink 8K receiver (battery, DPI and polling read), an Endgame
  Gear OP1 8k (DPI and polling read; its `0xa0` config report comes back with
  `0x00` in the id slot), and a Wooting 60HE+ (identified and read).
- Other brands are covered by the registry's own `supports()` checks and the
  adapter's tests, and still require validation on their hardware.
