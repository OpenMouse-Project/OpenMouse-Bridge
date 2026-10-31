import assert from "node:assert/strict";
import test from "node:test";

import { BRAND_DRIVERS } from "./brands.mjs";
import { candidateDevices } from "./hid-device-adapter.mjs";

// The Scyrox V8's "SCYROX 8K Dongle" (0x3554:0xf5f7) as node-hid lists it on
// Windows: one entry per top-level collection, each its own path, with the
// keyboard on interface 0, the vendor collections on interface 1 and the
// mouse on interface 2.
const dongle = (usagePage, usage, iface = 1) => ({
  vendorId: 0x3554,
  productId: 0xf5f7,
  product: "SCYROX 8K Dongle",
  usagePage,
  usage,
  interface: iface,
  path: `\\\\?\\HID#VID_3554&PID_F5F7&MI_0${iface}#${usagePage.toString(16)}:${usage.toString(16)}`,
});
const WINDOWS_DONGLE = [
  dongle(0x0001, 0x06, 0),
  dongle(0xff05, 0x00),
  dongle(0xff03, 0x00),
  dongle(0x000c, 0x01),
  dongle(0x0001, 0x80),
  dongle(0xff02, 0x02),
  dongle(0xff04, 0x02),
  dongle(0xff06, 0x02),
  dongle(0x0001, 0x02, 2),
];
// A Teevolution Terra Pro on the same vendor id.
const TERRA_PRO = { ...dongle(0xff02, 0x02), productId: 0xf520, product: "Terra Pro", path: "terra" };

const paths = (adapters) => adapters.map((adapter) => adapter._infos.map((info) => info.path));

test("without hints every collection of an interface is opened together", () => {
  const adapters = candidateDevices(0x3554, {}, () => [...WINDOWS_DONGLE, TERRA_PRO]);
  assert.deepEqual(adapters.map((adapter) => adapter.productId), [0xf5f7, 0xf520]);
  assert.equal(adapters[0]._infos.length, 7, "every vendor and system collection of interface 1");
});

test("the Scyrox entry opens only the dongle's 0xff02 collection", () => {
  const adapters = candidateDevices(0x3554, BRAND_DRIVERS.scyrox, () => [
    ...WINDOWS_DONGLE.map((info) => ({ ...info, productId: 0xf517 })),
    TERRA_PRO,
  ]);
  assert.deepEqual(paths(adapters), [[dongle(0xff02, 0x02).path]]);
  assert.equal(adapters[0].productId, 0xf517);
});

test("a product the entry does not list is left out", () => {
  const adapters = candidateDevices(0x3554, { productIds: [0xf515] }, () => [...WINDOWS_DONGLE, TERRA_PRO]);
  assert.deepEqual(adapters, []);
});

test("an interface listed without usage pages is kept whole", () => {
  // hidraw on Linux lists one entry per interface, with no usage page.
  const hidraw = { ...dongle(0xff02, 0x02), usagePage: 0, usage: 0, path: "/dev/hidraw3" };
  const adapters = candidateDevices(0x3554, { usagePage: 0xff02 }, () => [hidraw]);
  assert.deepEqual(paths(adapters), [["/dev/hidraw3"]]);
});
