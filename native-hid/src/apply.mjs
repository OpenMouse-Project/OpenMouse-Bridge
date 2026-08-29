#!/usr/bin/env node
// CLI entry point Bridge (Rust) spawns to push a saved profile's DPI/polling
// rate to a mouse over native HID, reusing OpenMouse's own hardware-verified
// WebHID driver classes instead of a bespoke reimplementation.
//
// Usage: node apply.mjs < profile.json
//   stdin: {"brand": "Razer", "dpi": 800, "pollingRateHz": 1000}
//   (dpi / pollingRateHz are each optional — omit to leave that setting alone)
//
// Exit codes (Bridge's src/drivers/mod.rs depends on this contract):
//   0  — a device was found and the requested settings were applied.
//   3  — no native driver is registered for this brand (not an error: most
//        devices are still driven by the OpenMouse web app over WebHID).
//   1  — a driver is registered for this brand, but no matching device
//        answered, or applying the settings failed. Details on stderr.
// mouse-protocol's driver classes were written for a browser and several of
// them (17 of ~18 brands as of this writing) time their HID++/vendor
// request-response waits with `window.setTimeout`/`window.clearTimeout`.
// `window` doesn't exist in Node, so that throws a ReferenceError inside the
// Promise executor — which JS silently turns into an instant rejection, not
// a timeout, before any real reply can arrive. Concretely, this made
// LogitechHidppClient misread that instant non-timeout rejection as "the
// receiver answered but this slot has no sensor" on every pairing slot, so
// it reported a connected mouse as "not a mouse" — nothing to do with HID
// permissions or the mouse being asleep. `window` aliased to `globalThis`
// fixes every driver relying on this pattern at once. Must run before any
// `@openmouse/protocol` driver module is imported, including the dynamic
// imports in probe() below.
globalThis.window ??= globalThis;

import { BRAND_DRIVERS, brandKey } from "./brands.mjs";
import { candidateDevices } from "./hid-device-adapter.mjs";

const EXIT_APPLIED = 0;
const EXIT_FAILED = 1;
const EXIT_NO_DRIVER = 3;

const PROBE_TIMEOUT_MS = 3000;

async function readStdin() {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  return Buffer.concat(chunks).toString("utf8");
}

function withTimeout(promise, ms, label) {
  let timer;
  const timeout = new Promise((_resolve, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Tries one candidate class against one open device. Resolves with the
 * live, probed client on success; rejects (and closes the device) on any
 * failure, so the caller can move on to the next candidate. */
async function probe(device, candidate) {
  const module = await import(candidate.module);
  const Client = module[candidate.exportName];
  const client = new Client(device);
  try {
    await withTimeout(client.open(), PROBE_TIMEOUT_MS, `${candidate.exportName}.open()`);
    // Every SupportedClient implements readStatus() (mouse-protocol's
    // shared client contract) — a cheap, read-only way to confirm this is
    // really the right class for this interface before writing anything.
    await withTimeout(client.readStatus(), PROBE_TIMEOUT_MS, `${candidate.exportName}.readStatus()`);
    return client;
  } catch (error) {
    await device.close().catch(() => undefined);
    throw error;
  }
}

async function main() {
  const input = JSON.parse(await readStdin());
  const brand = typeof input.brand === "string" ? input.brand : "";
  const entry = BRAND_DRIVERS[brandKey(brand)];
  if (!entry) {
    console.error(`[native-hid] no driver registered for brand "${brand}"`);
    process.exit(EXIT_NO_DRIVER);
  }

  const attempts = [];
  let client = null;
  outer: for (const vendorId of entry.vendorIds) {
    for (const device of candidateDevices(vendorId)) {
      for (const candidate of entry.classes) {
        try {
          client = await probe(device, candidate);
          break outer;
        } catch (error) {
          attempts.push(`${candidate.exportName} on ${device.productName || "unknown device"}: ${error.message}`);
        }
      }
    }
  }

  if (!client) {
    console.error(
      `[native-hid] no ${brand} device answered.` + (attempts.length ? ` Tried:\n  ${attempts.join("\n  ")}` : " No matching HID interface was found."),
    );
    process.exit(EXIT_FAILED);
  }

  try {
    if (Number.isFinite(input.dpi)) {
      await withTimeout(client.setDpi(input.dpi), PROBE_TIMEOUT_MS, "setDpi");
    }
    if (Number.isFinite(input.pollingRateHz)) {
      await withTimeout(client.setPollingRate(input.pollingRateHz), PROBE_TIMEOUT_MS, "setPollingRate");
    }
  } catch (error) {
    console.error(`[native-hid] could not apply settings: ${error.message}`);
    await client.close().catch(() => undefined);
    process.exit(EXIT_FAILED);
  }

  await client.close().catch(() => undefined);
  process.exit(EXIT_APPLIED);
}

main().catch((error) => {
  console.error(`[native-hid] unexpected error: ${error.stack ?? error.message}`);
  process.exit(EXIT_FAILED);
});
