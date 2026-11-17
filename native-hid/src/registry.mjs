// Chooses drivers the way the control panel does: with @openmouse/protocol's
// own registry (`DEVICE_DRIVERS`, first `supports()` match wins), run against
// the devices Bridge described from their real report descriptors.
//
// Bridge only describes devices matching the registry's SUPPORTED_HID_FILTERS
// (the filters the panel requests devices with), and a driver is only used on
// a device its own `supports()` accepts — the same product-id and descriptor
// checks as in the browser — so nothing outside what OpenMouse itself would
// open is ever written to.
//
// The modules are imported lazily: loading the registry pulls in every driver,
// and apply.mjs must alias `window` before any of them load.

import { DescribedDeviceAdapter } from "./hid-device-adapter.mjs";

/** Receivers that stop answering while a sibling collection is read: only
 * the collection on this usage page is opened (see candidateDevices()). */
const ONLY_USAGE_PAGE = { scyrox: 0xff02 };

export async function loadRegistry() {
  const [{ DEVICE_DRIVERS }, { SUPPORTED_HID_FILTERS }] = await Promise.all([
    import("@openmouse/protocol/drivers/registry"),
    import("@openmouse/protocol/drivers/vendors"),
  ]);
  return { drivers: DEVICE_DRIVERS, filters: SUPPORTED_HID_FILTERS };
}

export function brandKey(brand) {
  return String(brand ?? "").trim().toLowerCase();
}

/** The registry entry that claims `device`, as `createSupportedClient` picks
 * it: the first whose `supports()` accepts the device. */
export function driverFor(drivers, device) {
  return (
    drivers.find((driver) => {
      try {
        return driver.supports(device);
      } catch {
        return false;
      }
    }) ?? null
  );
}

/** The WebHID-shaped view `supports()` checks: no paths, never opened. */
function view(described) {
  return {
    vendorId: described.vendorId,
    productId: described.productId,
    productName: described.productName ?? "",
    collections: described.collections ?? [],
    opened: false,
  };
}

/** The brand the registry files each described device under, or null for
 * a device no driver claims. */
export function identify(drivers, devices) {
  return devices.map((described) => ({
    key: described.key,
    brand: driverFor(drivers, view(described))?.brand ?? null,
  }));
}

/**
 * The (device, client) pairs to try for a `brand` request, in the order
 * Bridge listed the devices. Without `matchStatusBrand` only devices the
 * registry files under `brand` are used; with it every claimed device is,
 * and probe() keeps one whose own status reports `brand`.
 */
export function registryPairs(drivers, devices, brand, { matchStatusBrand = false, Adapter = DescribedDeviceAdapter } = {}) {
  const wanted = brandKey(brand);
  const pairs = [];
  for (const described of devices) {
    const driver = driverFor(drivers, view(described));
    if (!driver) continue;
    if (!matchStatusBrand && brandKey(driver.brand) !== wanted) continue;
    const device = new Adapter(described, { onlyUsagePage: ONLY_USAGE_PAGE[brandKey(driver.brand)] });
    const client = driver.create(device);
    pairs.push({
      device,
      candidate: {
        exportName: client.constructor?.name || driver.brand,
        client,
        requireBrand: matchStatusBrand ? wanted : undefined,
      },
    });
  }
  return pairs;
}

/** Whether the registry has a driver for `brand` at all. */
export function knowsBrand(drivers, brand) {
  const wanted = brandKey(brand);
  return drivers.some((driver) => brandKey(driver.brand) === wanted);
}
