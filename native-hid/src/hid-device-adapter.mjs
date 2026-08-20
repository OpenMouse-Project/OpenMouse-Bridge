// Wraps one or more node-hid devices so together they satisfy the WebHID
// `HIDDevice` interface mouse-protocol's driver classes are written against
// (see mouse-protocol/src/drivers/webhid.ts, an ambient-type declaration
// only — this file is the runtime implementation of that same shape for
// Node).
//
// This lets Bridge reuse OpenMouse's actual, hardware-verified driver
// classes unmodified: the drivers only ever call the methods below, never
// touch node-hid directly, and don't know they're not really in a browser.
//
// One WebHID `HIDDevice` == one USB/BT *interface*, which can carry several
// top-level HID collections (e.g. a short and a long report variant on the
// same vendor interface — this is exactly Logitech's HID++ receiver shape:
// report 0x10 in one collection, report 0x11 in another, same interface).
// node-hid enumerates one entry *per collection*, not per interface — on
// macOS in particular, IOHIDManager opens a separate IOHIDDevice service per
// top-level collection it finds, each capable of receiving input reports
// only for the report ids declared in *its own* collection. Opening only
// one of those, as this adapter used to, can send a query fine and then
// silently miss the reply if the reply's report id landed on a collection
// backed by a different node-hid entry. So candidateDevices() groups
// node-hid's split entries back into one logical device per interface
// (matched on vendor/product id + node-hid's own `interface` field), and
// HidDeviceAdapter opens and listens on every split within that group.

import { HID, devices } from "node-hid";

const REPLAY_ID_MAX = 255;

/**
 * @param {import("node-hid").Device[]} infos - every node-hid entry that
 *   splits off the same underlying USB/BT interface (see module docs above).
 */
export class HidDeviceAdapter {
  constructor(infos) {
    const [primary] = infos;
    this.vendorId = primary.vendorId;
    this.productId = primary.productId;
    this.productName = primary.product ?? "";
    // Real report-descriptor parsing isn't implemented here — callers must
    // not rely on collections-based `isSupported()` gating (see
    // native-hid/README.md); construct the known-correct driver class
    // directly instead.
    this.collections = [];
    this.opened = false;

    this._infos = infos;
    this._devices = [];
    this._listeners = new Set();
    this._onData = this._onData.bind(this);
    this._onError = this._onError.bind(this);
  }

  async open() {
    if (this.opened) return;
    this._devices = this._infos.map((info) => {
      const device = new HID(info.path);
      device.on("data", this._onData);
      device.on("error", this._onError);
      return device;
    });
    this.opened = true;
  }

  async close() {
    if (!this.opened) return;
    for (const device of this._devices) {
      device.removeAllListeners();
      device.close();
    }
    this._devices = [];
    this.opened = false;
  }

  /** @param {number} reportId @param {BufferSource} data */
  async sendReport(reportId, data) {
    this._requireOpen();
    const frame = this._frame(reportId, data);
    // Any split for this interface can carry the write on real hardware —
    // the split is a macOS/IOHIDManager enumeration artifact per top-level
    // collection, not a hardware restriction on which handle can transmit
    // which report id. Writing through all of them costs little (these are
    // idempotent HID++-style query/set commands, and setDpi/setPollingRate
    // read back to confirm regardless) and removes any doubt about which
    // split the device actually expects the write on.
    let sent = false;
    let lastError;
    for (const device of this._devices) {
      try {
        device.write(frame);
        sent = true;
      } catch (error) {
        lastError = error;
      }
    }
    if (!sent) throw lastError ?? new Error("No HID interface accepted the report.");
  }

  /** @param {number} reportId @param {BufferSource} data */
  async sendFeatureReport(reportId, data) {
    this._requireOpen();
    const frame = this._frame(reportId, data);
    let sent = false;
    let lastError;
    for (const device of this._devices) {
      try {
        device.sendFeatureReport(frame);
        sent = true;
      } catch (error) {
        lastError = error;
      }
    }
    if (!sent) throw lastError ?? new Error("No HID interface accepted the feature report.");
  }

  /**
   * node-hid's `getFeatureReport` needs an expected length; WebHID's
   * `receiveFeatureReport` does not, because the browser already knows it
   * from the parsed report descriptor. Drivers here call this with the
   * length they themselves expect back baked into their own retry/parsing
   * logic, so a generous fixed buffer (the largest report any known driver
   * here uses) is safe — short replies are simply the leading bytes of it.
   * Tries every split (the report id in question may only be declared on
   * one of them) and returns the first that answers.
   */
  async receiveFeatureReport(reportId) {
    this._requireOpen();
    let lastError;
    for (const device of this._devices) {
      try {
        const bytes = device.getFeatureReport(reportId, 64);
        return new DataView(Uint8Array.from(bytes).buffer);
      } catch (error) {
        lastError = error;
      }
    }
    throw lastError ?? new Error("No HID interface answered the feature report request.");
  }

  /** @param {"inputreport"} type @param {(event: object) => void} listener */
  addEventListener(type, listener) {
    if (type !== "inputreport") return;
    this._listeners.add(listener);
  }

  /** @param {"inputreport"} type @param {(event: object) => void} listener */
  removeEventListener(type, listener) {
    if (type !== "inputreport") return;
    this._listeners.delete(listener);
  }

  _requireOpen() {
    if (!this.opened) throw new Error("The HID device is not open.");
  }

  /** Report id prefixed onto the write buffer, as node-hid (and the OS HID
   * stack underneath it) expects for a numbered report. */
  _frame(reportId, data) {
    const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (reportId < 0 || reportId > REPLAY_ID_MAX) throw new Error(`Report id ${reportId} is out of range.`);
    return [reportId, ...bytes];
  }

  /** @param {Buffer} data */
  _onData(data) {
    // Some backends prefix numbered reports with the report id on read,
    // some don't (mirrors the same ambiguity the Rust Pulsar driver
    // handles) — there is no fully reliable way to tell from the bytes
    // alone, so report id 0 is assumed when in doubt; drivers here match
    // responses by their own payload content, not strictly by report id.
    const reportId = data.length > 0 ? data[0] : 0;
    const payload = data.length > 0 ? data.subarray(1) : data;
    const view = new DataView(Uint8Array.from(payload).buffer);
    const event = { reportId, data: view };
    for (const listener of this._listeners) listener(event);
  }

  /** @param {Error} error */
  _onError(error) {
    // Surfaced to whoever is waiting on a pending exchange via their own
    // timeout, matching how a real device disconnect looks over WebHID —
    // there is no dedicated "error" WebHID event drivers here listen for.
    // Logged so it's not silently swallowed.
    console.error(`[native-hid] device error: ${error.message}`);
  }
}

/**
 * Lists every HID *interface* for a vendor id, ready to be opened — one
 * HidDeviceAdapter per interface, merging any node-hid entries that split
 * off the same interface into separate top-level collections (see module
 * docs above).
 * @param {number} vendorId
 * @returns {HidDeviceAdapter[]}
 */
export function candidateDevices(vendorId) {
  const groups = new Map();
  for (const info of devices()) {
    if (info.vendorId !== vendorId) continue;
    const key = `${info.productId}:${info.interface}`;
    const group = groups.get(key);
    if (group) group.push(info);
    else groups.set(key, [info]);
  }
  return [...groups.values()].map((infos) => {
    // Several node-hid entries for one interface commonly share the same
    // underlying `path` (one physical device, listed once per top-level
    // collection its descriptor declares) — open each distinct path once.
    const seen = new Set();
    const unique = infos.filter((info) => (seen.has(info.path) ? false : seen.add(info.path)));
    return new HidDeviceAdapter(unique);
  });
}
