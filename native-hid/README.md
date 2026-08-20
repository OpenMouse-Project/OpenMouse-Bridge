# native-hid

A small Node.js helper Bridge spawns as a subprocess so it can push a saved
profile's DPI/polling rate to a mouse over native HID, without a browser tab
open holding a WebHID connection.

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

`brands.mjs` maps a `MouseStatus.brand` string (Bridge's saved profile
`device.id` is `"<brand>:<name>"`, matching what the OpenMouse web app
writes) to that brand's HID vendor id(s) and its candidate driver class(es),
in the same order `mouse-protocol/src/drivers/registry.ts` tries them.

`apply.mjs` doesn't replicate WebHID's report-descriptor-based
`isSupported()` auto-detection (this adapter reports `collections: []` —
real report-descriptor parsing isn't implemented). Instead, since Bridge
already knows the brand from the saved profile, it opens each HID interface
for that brand's vendor id(s) and tries each candidate class directly:
`client.open()` then `client.readStatus()` — a cheap, read-only call every
`SupportedClient` implements — as a probe. The first class that doesn't
throw is treated as correct, and `setDpi()`/`setPollingRate()` are called on
it.

## Exit code contract (Bridge depends on this — see `src/drivers/native_hid.rs`)

- `0` — a device was found and the requested settings were applied.
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

- The adapter and dispatch logic are structurally tested (module resolution,
  exit codes, control flow) but have not been run against real hardware from
  this environment — no gaming mouse was reachable here. Validate against an
  actual device before relying on this for a non-Pulsar mouse.
- G-Wolves is deliberately left out of `brands.mjs`: it isn't a finished,
  exported driver in `mouse-protocol` yet (untracked source, no
  `package.json` export, no `registry.ts` entry as of this writing).
