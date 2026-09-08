import { HID, devices } from "node-hid";

export const X11_VENDOR_ID = 0x1d57;
export const X11_PRODUCT_IDS = new Set([0xfa55, 0xfa60, 0xfa61]);
export const X11_SETTINGS_INTERFACE = 2;

const POLLING_CODES = new Map([
  [125, 0x08],
  [250, 0x04],
  [500, 0x02],
  [1000, 0x01],
]);

export function x11DeviceInfos(infos = devices()) {
  return infos.filter((info) =>
    info.vendorId === X11_VENDOR_ID
      && X11_PRODUCT_IDS.has(info.productId)
      && info.interface === X11_SETTINGS_INTERFACE
      && typeof info.path === "string"
      && info.path.length > 0,
  );
}

export function buildX11PollingReport(pollingRateHz) {
  const code = POLLING_CODES.get(pollingRateHz);
  if (code === undefined) {
    throw new Error(`Attack Shark X11 does not support ${pollingRateHz} Hz; expected 125, 250, 500, or 1000 Hz.`);
  }
  return [0x06, 0x09, 0x01, code, (0xff - code) & 0xff, 0x00, 0x00, 0x00, 0x00];
}

export function applyX11PollingRate(
  pollingRateHz,
  { infos = devices(), open = (path) => new HID(path) } = {},
) {
  const candidates = x11DeviceInfos(infos);
  if (candidates.length === 0) return false;

  const report = buildX11PollingReport(pollingRateHz);
  const attempts = [];
  for (const candidate of candidates) {
    let device;
    try {
      device = open(candidate.path);
      device.sendFeatureReport(report);
      return true;
    } catch (error) {
      attempts.push(`PID 0x${candidate.productId.toString(16)} interface 2: ${error.message}`);
    } finally {
      device?.close();
    }
  }

  throw new Error(`No Attack Shark X11 settings interface accepted the polling report. Tried:\n  ${attempts.join("\n  ")}`);
}
