import assert from "node:assert/strict";
import test from "node:test";

import { HidDeviceAdapter } from "./hid-device-adapter.mjs";

// A device whose firmware stalls a GET_REPORT longer than its 64-byte feature
// report, as the wired G-Wolves HTS Ultra (0x33e4:0x5308) does.
function strictDevice(reportLength) {
  const asked = [];
  class FakeHID {
    on() {}
    removeAllListeners() {}
    close() {}
    sendFeatureReport() {}
    getFeatureReport(id, length) {
      asked.push(length);
      if (length > reportLength) throw new Error("Get Input/Feature Report DeviceIoControl: (0x0000001F)");
      return [id, ...new Uint8Array(reportLength - 1)];
    }
  }
  const info = { vendorId: 0x33e4, productId: 0x5308, product: "HTS Ultra", path: "strict", interface: 2 };
  return { device: new HidDeviceAdapter([info], null, FakeHID), asked };
}

test("a report the firmware refuses at full size is asked again at its sent length", async () => {
  const { device, asked } = strictDevice(65);
  await device.open();
  await device.sendFeatureReport(0, new Uint8Array(64));
  const reply = await device.receiveFeatureReport(0);
  assert.equal(reply.byteLength, 64);
  assert.deepEqual(asked, [91, 65]);
});

test("a short request still reads the whole reply when the firmware allows it", async () => {
  const { device, asked } = strictDevice(91);
  await device.open();
  await device.sendFeatureReport(0, new Uint8Array(8));
  const reply = await device.receiveFeatureReport(0);
  assert.equal(reply.byteLength, 90);
  assert.deepEqual(asked, [91]);
});

test("a report never sent is asked once, at full size", async () => {
  const { device, asked } = strictDevice(65);
  await device.open();
  await assert.rejects(device.receiveFeatureReport(0), /0x0000001F/);
  assert.deepEqual(asked, [91]);
});
