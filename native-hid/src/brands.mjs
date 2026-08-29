// Maps a MouseStatus `brand` string (mouse-protocol/src/drivers/mouse-types.ts)
// to the HID vendor id(s) to enumerate and the candidate driver classes to
// try constructing against each interface found, in the same order
// mouse-protocol/src/drivers/registry.ts's DEVICE_DRIVERS lists them for
// that brand (first-match-wins there; here, first class whose open() +
// readStatus() probe succeeds wins — see apply.mjs).
//
// Deliberately excluded:
// - "Pulsar": Bridge has a dependency-free native Rust driver for it
//   (src/drivers/pulsar.rs) that claims the brand before this helper is ever
//   invoked — see src/drivers/mod.rs.
// - G-Wolves: not yet a finished, exported driver in the local mouse-protocol
//   checkout (untracked src/g-wolves/, src/drivers/g-wolves/, no package.json
//   export or registry.ts entry as of this writing) — add it here once it
//   ships upstream.
//
// Vendor ids are copied from mouse-protocol/src/drivers/vendors.ts and each
// driver's own `isSupported()`; class import paths use the package's
// "./drivers/*" subpath export (mouse-protocol/package.json), which maps
// straight onto its compiled dist/drivers/ layout.

const client = (module, exportName) => ({ module, exportName });

export const BRAND_DRIVERS = {
  zaunkoenig: {
    vendorIds: [0x0483],
    classes: [client("@openmouse/protocol/drivers/zaunkoenig/hid", "ZaunkoenigHidClient")],
  },
  finalmouse: {
    vendorIds: [0x361d],
    classes: [client("@openmouse/protocol/drivers/finalmouse/hid", "FinalmouseHidClient")],
  },
  "endgame gear": {
    vendorIds: [0x3367],
    classes: [
      client("@openmouse/protocol/drivers/endgame/egg-op1-hid", "EggOp1HidClient"),
      client("@openmouse/protocol/drivers/endgame/egg-we-hid", "EggWeHidClient"),
    ],
  },
  teevolution: {
    vendorIds: [0x3554],
    classes: [client("@openmouse/protocol/drivers/teevolution/hid", "TeevolutionHidClient")],
  },
  vgn: {
    vendorIds: [0x3554],
    classes: [client("@openmouse/protocol/drivers/vgn/hid", "VgnF2HidClient")],
  },
  logitech: {
    vendorIds: [0x046d],
    classes: [client("@openmouse/protocol/drivers/logitech/hidpp", "LogitechHidppClient")],
  },
  wlmouse: {
    vendorIds: [0x36a7],
    classes: [client("@openmouse/protocol/drivers/wlmouse/hid", "WLMouseHidClient")],
  },
  lamzu: {
    vendorIds: [0x373e],
    classes: [client("@openmouse/protocol/drivers/lamzu/hid", "LamzuHidClient")],
  },
  // Same CompX ODM hardware/class as Lamzu; readStatus() reports the brand
  // that matches the product id.
  crdrako: {
    vendorIds: [0x373e],
    classes: [client("@openmouse/protocol/drivers/lamzu/hid", "LamzuHidClient")],
  },
  moddomouse: {
    vendorIds: [0x2fe3],
    classes: [client("@openmouse/protocol/drivers/moddo/hid", "ModdoHidClient")],
  },
  ninjutso: {
    // NINJUTSO_VENDOR_ID (current) and NINJUTSO_LEGACY_VENDOR_ID (shared
    // with Orbital) — see mouse-protocol/src/ninjutso/index.ts.
    vendorIds: [0x093a, 0x1915],
    classes: [client("@openmouse/protocol/drivers/ninjutso/hid", "NinjutsoHidClient")],
  },
  orbital: {
    vendorIds: [0x1915],
    classes: [client("@openmouse/protocol/drivers/orbital/hid", "OrbitalHidClient")],
  },
  razer: {
    vendorIds: [0x1532],
    classes: [
      client("@openmouse/protocol/drivers/razer/hid", "RazerHidClient"),
      client("@openmouse/protocol/drivers/razer/cobra-hid", "RazerCobraHidClient"),
      client("@openmouse/protocol/drivers/razer/viper-mini-hid", "RazerViperMiniHidClient"),
      client("@openmouse/protocol/drivers/razer/viper-hid", "RazerViperHidClient"),
      client("@openmouse/protocol/drivers/razer/viper-v4-pro-hid", "RazerViperV4ProHidClient"),
    ],
  },
  atk: {
    vendorIds: [0x373b],
    classes: [client("@openmouse/protocol/drivers/atk/hid", "AtkHidClient")],
  },
  "attack shark": {
    vendorIds: [0x1d57, 0x25a7, 0x373e],
    classes: [client("@openmouse/protocol/drivers/attackshark/hid", "AttackSharkHidClient")],
  },
  keychron: {
    vendorIds: [0x3434],
    classes: [client("@openmouse/protocol/drivers/keychron/hid", "KeychronHidClient")],
  },
  fantech: {
    vendorIds: [0x3151],
    classes: [client("@openmouse/protocol/drivers/fantech/hid", "FantechHidClient")],
  },
  wooting: {
    vendorIds: [0x31e3],
    classes: [client("@openmouse/protocol/drivers/wooting/hid", "WootingHidClient")],
  },
  wallhack: {
    vendorIds: [0x3879, 0x1caa],
    classes: [
      client("@openmouse/protocol/drivers/wallhack/mouse-hid", "WallhackMouseHidClient"),
      client("@openmouse/protocol/drivers/wallhack/keyboard-hid", "WallhackKeyboardHidClient"),
    ],
  },
};

/** Normalizes a MouseStatus `brand` string / Bridge `device.id` prefix into
 * a BRAND_DRIVERS key. */
export function brandKey(brand) {
  return brand.trim().toLowerCase();
}
