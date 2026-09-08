import assert from "node:assert/strict";
import test from "node:test";

import {
  applyX11PollingRate,
  buildX11PollingReport,
  x11DeviceInfos,
} from "./attack-shark-x11.mjs";

test("buildX11PollingReport matches the verified feature reports", () => {
  assert.deepEqual(buildX11PollingReport(125), [0x06, 0x09, 0x01, 0x08, 0xf7, 0, 0, 0, 0]);
  assert.deepEqual(buildX11PollingReport(250), [0x06, 0x09, 0x01, 0x04, 0xfb, 0, 0, 0, 0]);
  assert.deepEqual(buildX11PollingReport(500), [0x06, 0x09, 0x01, 0x02, 0xfd, 0, 0, 0, 0]);
  assert.deepEqual(buildX11PollingReport(1000), [0x06, 0x09, 0x01, 0x01, 0xfe, 0, 0, 0, 0]);
  assert.throws(() => buildX11PollingReport(2000), /does not support 2000 Hz/);
});

test("x11DeviceInfos selects only X11-family interface 2 paths", () => {
  const match = { vendorId: 0x1d57, productId: 0xfa60, interface: 2, path: "x11" };
  assert.deepEqual(x11DeviceInfos([
    match,
    { ...match, interface: 1, path: "wrong-interface" },
    { ...match, productId: 0x1234, path: "wrong-product" },
    { ...match, vendorId: 0x25a7, path: "wrong-vendor" },
    { ...match, path: undefined },
  ]), [match]);
});

test("applyX11PollingRate sends the feature report and always closes the handle", () => {
  const reports = [];
  let closed = false;
  const applied = applyX11PollingRate(500, {
    infos: [{ vendorId: 0x1d57, productId: 0xfa55, interface: 2, path: "wired" }],
    open: () => ({
      sendFeatureReport: (report) => reports.push(report),
      close: () => { closed = true; },
    }),
  });

  assert.equal(applied, true);
  assert.deepEqual(reports, [[0x06, 0x09, 0x01, 0x02, 0xfd, 0, 0, 0, 0]]);
  assert.equal(closed, true);
});

test("applyX11PollingRate falls through when no X11 settings interface exists", () => {
  assert.equal(applyX11PollingRate(1000, { infos: [], open: () => assert.fail("must not open") }), false);
});
