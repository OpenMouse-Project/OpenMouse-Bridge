import { test } from "node:test";
import assert from "node:assert/strict";
import { EventEmitter } from "node:events";

import { DescribedDeviceAdapter } from "./hid-device-adapter.mjs";
import { identify, knowsBrand, registryPairs } from "./registry.mjs";

/** A node-hid handle that records what is sent through it. */
function fakeHandles({ failWrites = [] } = {}) {
  const log = [];
  class Handle extends EventEmitter {
    constructor(path) {
      super();
      this.path = path;
      log.push(["open", path]);
    }
    write(frame) {
      if (failWrites.includes(this.path)) throw new Error("refused");
      log.push(["write", this.path, frame]);
    }
    sendFeatureReport(frame) {
      log.push(["feature", this.path, frame]);
    }
    getFeatureReport(id, length) {
      log.push(["get", this.path, id, length]);
      // Windows hands a numbered report back with 0 in the id slot.
      return [0, ...Array.from({ length: length - 1 }, (_, index) => index + 1)];
    }
    close() {
      log.push(["close", this.path]);
    }
  }
  return { Handle, log };
}

const collection = (usagePage, usage = 1) => ({ usagePage, usage, children: [], inputReports: [], outputReports: [], featureReports: [] });

function described(overrides = {}) {
  return {
    key: "hid-test",
    vendorId: 0x3367,
    productId: 0x1964,
    productName: "Test Mouse",
    collections: [collection(1, 2), collection(0xff01), collection(0xff02)],
    paths: [
      { path: "mouse", collections: [collection(1, 2)], inputReports: {}, outputReports: {}, featureReports: {} },
      { path: "config", collections: [collection(0xff01)], inputReports: {}, outputReports: {}, featureReports: { 160: 1040, 161: 63 } },
      { path: "notify", collections: [collection(0xff02)], inputReports: { 9: 15 }, outputReports: { 9: 15 }, featureReports: {} },
    ],
    ...overrides,
  };
}

test("the standard mouse collection is never opened", async () => {
  const { Handle, log } = fakeHandles();
  const device = new DescribedDeviceAdapter(described(), { Handle });
  await device.open();
  assert.deepEqual(log.filter(([kind]) => kind === "open").map(([, path]) => path), ["config", "notify"]);
});

test("only paths that declare input reports are listened on", async () => {
  const { Handle } = fakeHandles();
  const device = new DescribedDeviceAdapter(described(), { Handle });
  await device.open();
  const listening = device._open.filter(({ device: handle }) => handle.listenerCount("data") > 0).map(({ path }) => path.path);
  assert.deepEqual(listening, ["notify"]);
});

test("a path with only unnumbered input keeps its first byte; numbered input still drops the id", async () => {
  const handles = [];
  class Handle extends EventEmitter {
    constructor(path) {
      super();
      handles.push([path, this]);
    }
    close() {}
  }
  const raw = collection(0xff60, 0x61);
  const device = new DescribedDeviceAdapter(
    described({
      paths: [
        { path: "via", collections: [raw], inputReports: { 0: 32 }, outputReports: { 0: 32 }, featureReports: {} },
        { path: "notify", collections: [collection(0xff02)], inputReports: { 9: 15 }, outputReports: {}, featureReports: {} },
      ],
    }),
    { Handle },
  );
  await device.open();
  const seen = [];
  device.addEventListener("inputreport", (event) => seen.push([event.reportId, [...new Uint8Array(event.data.buffer)]]));
  handles.find(([path]) => path === "via")[1].emit("data", Buffer.from([1, 0, 11, 0]));
  handles.find(([path]) => path === "notify")[1].emit("data", Buffer.from([9, 5, 6]));
  assert.deepEqual(seen, [
    [0, [1, 0, 11, 0]],
    [9, [5, 6]],
  ]);
});

test("a feature report goes only down the path that declares it, padded to its size", async () => {
  const { Handle, log } = fakeHandles();
  const device = new DescribedDeviceAdapter(described(), { Handle });
  await device.open();
  await device.sendFeatureReport(0xa1, new Uint8Array([1, 2, 3]));
  const sent = log.filter(([kind]) => kind === "feature");
  assert.equal(sent.length, 1);
  assert.equal(sent[0][1], "config");
  assert.equal(sent[0][2].length, 64, "the id byte plus the declared 63");
  assert.deepEqual(sent[0][2].slice(0, 4), [0xa1, 1, 2, 3]);
});

test("a feature report is read at its declared size and the id slot dropped", async () => {
  const { Handle, log } = fakeHandles();
  const device = new DescribedDeviceAdapter(described(), { Handle });
  await device.open();
  const view = await device.receiveFeatureReport(0xa0);
  assert.deepEqual(log.find(([kind]) => kind === "get"), ["get", "config", 0xa0, 1041]);
  assert.equal(view.byteLength, 1040);
  assert.equal(view.getUint8(0), 1, "the payload starts after the id slot, even though it held 0");
});

test("an output report goes to the first declaring path that takes it, once", async () => {
  const { Handle, log } = fakeHandles();
  const device = new DescribedDeviceAdapter(described(), { Handle });
  await device.open();
  await device.sendReport(9, new Uint8Array([7]));
  const writes = log.filter(([kind]) => kind === "write");
  assert.equal(writes.length, 1);
  assert.equal(writes[0][1], "notify");
  assert.equal(writes[0][2].length, 16);
});

test("a path without parsed descriptors takes any report, as before", async () => {
  const { Handle, log } = fakeHandles();
  const razer = described({
    vendorId: 0x1532,
    paths: [{ path: "razer", collections: [collection(0xff00)], inputReports: {}, outputReports: {}, featureReports: {} }],
  });
  const device = new DescribedDeviceAdapter(razer, { Handle });
  await device.open();
  await device.sendFeatureReport(0, new Uint8Array([1, 2]));
  assert.deepEqual(log.find(([kind]) => kind === "feature")[2], [0, 1, 2], "no padding without a declared size");
  await device.receiveFeatureReport(0);
  assert.equal(log.find(([kind]) => kind === "get")[3], 91);
  assert.equal(device._open[0].device.listenerCount("data"), 1);
});

test("onlyUsagePage narrows the opened paths", async () => {
  const { Handle, log } = fakeHandles();
  const device = new DescribedDeviceAdapter(described(), { Handle, onlyUsagePage: 0xff02 });
  await device.open();
  assert.deepEqual(log.filter(([kind]) => kind === "open").map(([, path]) => path), ["notify"]);
});

// A stand-in registry: the real one's supports() checks are exercised by
// apply.mjs against hardware; these pin how Bridge uses the answers.
class FakeClient {
  constructor(device) {
    this.device = device;
  }
}
const drivers = [
  { brand: "Endgame Gear", supports: (device) => device.vendorId === 0x3367 && device.collections.some((c) => c.usagePage === 0xff01), create: (device) => new FakeClient(device) },
  { brand: "Keychron", supports: (device) => device.vendorId === 0x3434, create: (device) => new FakeClient(device) },
  { brand: "Broken", supports: () => { throw new Error("bad descriptor"); }, create: () => null },
];

test("identify gives each device the first claiming driver's brand, or null", () => {
  const devices = [
    described({ key: "egg" }),
    described({ key: "egg-mouse-only", collections: [collection(1, 2)] }),
    described({ key: "kc", vendorId: 0x3434 }),
    described({ key: "msi", vendorId: 0x1462 }),
  ];
  assert.deepEqual(identify(drivers, devices), [
    { key: "egg", brand: "Endgame Gear" },
    { key: "egg-mouse-only", brand: null },
    { key: "kc", brand: "Keychron" },
    { key: "msi", brand: null },
  ]);
});

test("a brand request uses only that brand's devices", () => {
  const { Handle } = fakeHandles();
  class Adapter extends DescribedDeviceAdapter {
    constructor(device, options) {
      super(device, { ...options, Handle });
    }
  }
  const devices = [described({ key: "egg" }), described({ key: "kc", vendorId: 0x3434 })];
  const pairs = registryPairs(drivers, devices, "keychron", { Adapter });
  assert.equal(pairs.length, 1);
  assert.equal(pairs[0].device.vendorId, 0x3434);
  assert.equal(pairs[0].candidate.exportName, "FakeClient");
  assert.equal(pairs[0].candidate.requireBrand, undefined);
});

test("matching on the status brand offers every claimed device", () => {
  const { Handle } = fakeHandles();
  class Adapter extends DescribedDeviceAdapter {
    constructor(device, options) {
      super(device, { ...options, Handle });
    }
  }
  const devices = [described({ key: "egg" }), described({ key: "kc", vendorId: 0x3434 }), described({ key: "msi", vendorId: 0x1462 })];
  const pairs = registryPairs(drivers, devices, "Attack Shark", { matchStatusBrand: true, Adapter });
  assert.equal(pairs.length, 2);
  assert.ok(pairs.every(({ candidate }) => candidate.requireBrand === "attack shark"));
});

test("every helper script parses", async () => {
  // apply.mjs and the other entry points run main() on import, so no test
  // loads them; a syntax error there would otherwise only show in Bridge.
  const { readdirSync } = await import("node:fs");
  const { spawnSync } = await import("node:child_process");
  const directory = new URL(".", import.meta.url);
  for (const file of readdirSync(directory).filter((name) => name.endsWith(".mjs"))) {
    const result = spawnSync(process.execPath, ["--check", new URL(file, directory).pathname.replace(/^\/([A-Za-z]:)/, "$1")]);
    assert.equal(result.status, 0, `${file}: ${result.stderr}`);
  }
});

test("knowsBrand ignores case", () => {
  assert.ok(knowsBrand(drivers, "endgame gear"));
  assert.ok(!knowsBrand(drivers, "Lamzu"));
});
