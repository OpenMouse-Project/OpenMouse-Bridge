import test from "node:test";
import { HidDeviceAdapter, probeVerifiedDevice } from "./hid-device-adapter.mjs";
import assert from "node:assert/strict";
import { automate } from "./automation.mjs";
import { ZaunkoenigHidClient } from "@openmouse/protocol/drivers/zaunkoenig/hid";
import { ZAUNKOENIG_M3K_PRODUCT_ID, ZAUNKOENIG_VERSION_REPORT_ID, zaunkoenigBuildConfigPayload } from "@openmouse/protocol/zaunkoenig";

test("installed protocol driver reads, acknowledges and restores over fake feature I/O", async () => {
  const info = { vendorId: 0x0483, productId: ZAUNKOENIG_M3K_PRODUCT_ID, path: "fake-only", serialNumber: "fake-serial", interface: 2 };
  let config = zaunkoenigBuildConfigPayload({ dpi: 800, pollingRateHz: 1000, pollingRateCode: 3, usbSpeed: "High", liftOffDistanceMm: 1, angleSnapping: false, primaryButton: "Left" }, info.productId);
  let writes = 0;
  const device = {
    vendorId: info.vendorId, productId: info.productId, productName: "M3K", opened: false,
    open: async () => { device.opened = true; }, close: async () => { device.opened = false; },
    receiveFeatureReport: async (id) => id === ZAUNKOENIG_VERSION_REPORT_ID ? new TextEncoder().encode("parawizard new v0.8") : config,
    sendFeatureReport: async (_id, data) => { config = new Uint8Array(data); writes++; },
  };
  const io = { list: () => [info], probe: async () => { const client = new ZaunkoenigHidClient(device); return { client, status: await client.readStatus() }; } };
  const request = { brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K" };
  const baseline = await automate({ ...request, action: "automatic-read" }, io);
  assert.equal(writes, 0);
  await automate({ ...request, action: "automatic-apply", identity: baseline.identity, dpi: 1600, pollingRateHz: 2000 }, io);
  const changed = await automate({ ...request, action: "automatic-read" }, io);
  assert.equal(changed.dpi, 1600);
  assert.equal(changed.pollingRateHz, 2000);
  await automate({ ...request, action: "automatic-apply", ...baseline }, io);
  const restored = await automate({ ...request, action: "automatic-read" }, io);
  assert.deepEqual(restored, baseline);
  assert.equal(writes, 4);
  assert.equal(device.opened, false);
});

test("production verified probe retains the same fake NodeHID handle through installed driver read/apply/restore", async () => {
  const info = { vendorId: 0x0483, productId: ZAUNKOENIG_M3K_PRODUCT_ID, product: "M3K", path: "node-fake", serialNumber: "node-serial", interface: 2 };
  let config = zaunkoenigBuildConfigPayload({ dpi: 800, pollingRateHz: 1000, pollingRateCode: 3, usbSpeed: "High", liftOffDistanceMm: 1, angleSnapping: false, primaryButton: "Left" }, info.productId);
  const calls = [];
  class FakeHID {
    constructor(path) { assert.equal(path, info.path); this.verified = false; calls.push("open"); }
    getDeviceInfo() { this.verified = true; calls.push("identity"); return { ...info }; }
    on() {}
    removeAllListeners() {}
    close() { calls.push("close"); }
    getFeatureReport(id) {
      assert.equal(this.verified, true);
      calls.push("report");
      return id === ZAUNKOENIG_VERSION_REPORT_ID ? [id, ...new TextEncoder().encode("parawizard new v0.8")] : [id, ...config];
    }
    sendFeatureReport(frame) { assert.equal(this.verified, true); config = Uint8Array.from(frame.slice(1)); }
  }
  const io = { list: () => [info], probe: (info, identity) => probeVerifiedDevice(info, identity, async (device) => {
    const client = new ZaunkoenigHidClient(device);
    return { client, status: await client.readStatus() };
  }, FakeHID) };
  const request = { brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K" };
  const baseline = await automate({ ...request, action: "automatic-read" }, io);
  await automate({ ...request, action: "automatic-apply", identity: baseline.identity, dpi: 1600, pollingRateHz: 2000 }, io);
  await automate({ ...request, action: "automatic-apply", ...baseline }, io);
  for (const name of ["open", "identity", "close"]) assert.equal(calls.filter((call) => call === name).length, 3);
  assert.deepEqual(calls.slice(0, 2), ["open", "identity"]);
});

test("verified probe closes its fake NodeHID handle when protocol probing fails", async () => {
  let closed = 0;
  class FakeHID {
    getDeviceInfo() { return endpoint; }
    on() {}
    removeAllListeners() {}
    close() { closed++; }
  }
  await assert.rejects(probeVerifiedDevice(endpoint, endpoint, async () => { throw new Error("protocol failed"); }, FakeHID), /protocol failed/);
  assert.equal(closed, 1);
});

test("opened NodeHID replacement is rejected before protocol probe and every handle closes", async () => {
  for (const action of ["automatic-read", "automatic-apply"]) {
    for (const field of ["vendorId", "productId", "path", "serialNumber", "interface", "missing", "throws", "noMethod"]) {
      const calls = [];
      class FakeHID {
        constructor(path) { calls.push(["open", path]); }
        getDeviceInfo() {
          calls.push("identity");
          if (field === "throws") throw new Error("identity unavailable");
          if (field === "missing") return {};
          return { ...endpoint, [field]: typeof endpoint[field] === "number" ? endpoint[field] + 1 : "replacement" };
        }
        on() { calls.push("listen"); }
        removeAllListeners() { calls.push("unlisten"); }
        close() { calls.push("close"); }
      }
      if (field === "noMethod") FakeHID.prototype.getDeviceInfo = undefined;
      const io = {
        list: () => [endpoint],
        probe: (info, identity) => probeVerifiedDevice(info, identity, async (device) => {
          calls.push("protocol-probe");
          return { client: { close: () => device.close() }, status };
        }, FakeHID),
      };
      await assert.rejects(automate({ brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K", action, identity: action === "automatic-apply" ? endpoint : undefined, dpi: 1600 }, io), /identity/);
      assert.equal(calls.includes("protocol-probe"), false);
      assert.equal(calls.filter((value) => value === "close").length, 1);
    }
  }
});

test("Full-speed automation and restoration preserve latent High-speed interval bits", async () => {
  const info = { vendorId: 0x0483, productId: ZAUNKOENIG_M3K_PRODUCT_ID, path: "full-fake", serialNumber: "full-serial", interface: 2 };
  let config = zaunkoenigBuildConfigPayload({ dpi: 800, pollingRateHz: 1000, pollingRateCode: 0, usbSpeed: "Full", liftOffDistanceMm: 1, angleSnapping: false, primaryButton: "Left" }, info.productId);
  const original = Uint8Array.from(config);
  let pollingCalls = 0;
  const device = {
    vendorId: info.vendorId, productId: info.productId, productName: "M3K", opened: false,
    open: async () => { device.opened = true; }, close: async () => { device.opened = false; },
    receiveFeatureReport: async (id) => id === ZAUNKOENIG_VERSION_REPORT_ID ? new TextEncoder().encode("parawizard new v0.8") : config,
    sendFeatureReport: async (_id, data) => { config = new Uint8Array(data); },
  };
  const io = { list: () => [info], probe: async () => {
    const client = new ZaunkoenigHidClient(device);
    const set = client.setPollingRate.bind(client);
    client.setPollingRate = async (rate) => { pollingCalls++; return await set(rate); };
    return { client, status: await client.readStatus() };
  } };
  const request = { brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K" };
  const baseline = await automate({ ...request, action: "automatic-read" }, io);
  await automate({ ...request, action: "automatic-apply", identity: baseline.identity, dpi: 1600, pollingRateHz: 1000 }, io);
  await automate({ ...request, action: "automatic-apply", ...baseline }, io);
  assert.equal(pollingCalls, 0);
  assert.deepEqual(config, original);
  assert.equal(device.opened, false);
});

const endpoint = { vendorId: 0x0483, productId: 1, path: "exact-path", serialNumber: "serial", interface: 2 };
const status = { brand: "Zaunkoenig", name: "M3K", dpi: 800, pollingRateHz: 1000, usbSpeed: "High" };
function fixture(infos = [endpoint], value = status) {
  const calls = [];
  const client = { readStatus: async () => { calls.push("read"); return value; }, close: async () => calls.push("close"), setDpi: async (dpi) => { calls.push(["dpi", dpi]); return dpi; }, setPollingRate: async (hz) => { calls.push(["hz", hz]); return hz; } };
  return { calls, io: { list: () => infos, probe: async () => ({ client, status: value }) } };
}
test("captures acknowledged values and exact endpoint without setting hardware", async () => {
  const { calls, io } = fixture();
  const result = await automate({ brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K", action: "automatic-read" }, io);
  assert.equal(result.dpi, 800);
  assert.equal(result.pollingRateHz, 1000);
  assert.deepEqual(result.identity, endpoint);
  assert.deepEqual(calls, ["close"]);
});
test("ambiguous endpoints and missing serial fail before probing", async () => {
  for (const infos of [[endpoint, { ...endpoint, path: "other" }], [{ ...endpoint, serialNumber: "" }], [{ ...endpoint, interface: undefined }]]) {
    const { calls, io } = fixture(infos);
    await assert.rejects(automate({ brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K", action: "automatic-read" }, io), /identity/);
    assert.deepEqual(calls, []);
  }
});
test("restores only exact captured endpoint and serial", async () => {
  const { calls, io } = fixture([{ ...endpoint, serialNumber: "replacement" }]);
  await assert.rejects(automate({ brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K", action: "automatic-apply", identity: endpoint, dpi: 800 }, io), /identity/);
  assert.deepEqual(calls, []);
});
test("unsupported readback and mismatched profile ownership fail without setters", async () => {
  for (const value of [{ ...status, dpi: null }, { ...status, name: "Other" }, { ...status, dpiY: 900 }]) {
    const { calls, io } = fixture([endpoint], value);
    await assert.rejects(automate({ brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K", action: "automatic-apply", identity: endpoint, dpi: 1600 }, io));
    assert.deepEqual(calls, ["close"]);
  }
  const { calls, io } = fixture();
  await assert.rejects(automate({ brand: "Pulsar", deviceId: "Pulsar:Mouse", action: "automatic-read" }, io), /unsupported.*readback/);
  assert.deepEqual(calls, []);
});
test("setter acknowledgements are required and client closes on failure", async () => {
  const { calls, io } = fixture();
  const open = io.probe;
  io.probe = async () => { const result = await open(); result.client.setDpi = async () => 1200; return result; };
  await assert.rejects(automate({ brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K", action: "automatic-apply", identity: endpoint, dpi: 1600 }, io), /acknowledge/);
  assert.equal(calls.at(-1), "close");
});
test("polling that would change USB speed fails before any DPI setter", async () => {
  const { calls, io } = fixture([endpoint], { ...status, usbSpeed: "Full" });
  await assert.rejects(automate({ brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K", action: "automatic-apply", identity: endpoint, dpi: 1600, pollingRateHz: 8000 }, io), /USB/);
  assert.deepEqual(calls, ["close"]);
});

test("equal effective polling never calls the setter at either supported USB speed", async () => {
  for (const usbSpeed of ["Full", "High"]) {
    const { calls, io } = fixture([endpoint], { ...status, usbSpeed });
    await automate({ brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K", action: "automatic-apply", identity: endpoint, pollingRateHz: 1000 }, io);
    assert.deepEqual(calls, ["close"]);
  }
});

test("unsupported USB speed/readback or rate fails before any write", async () => {
  for (const [usbSpeed, pollingRateHz] of [["Unknown", 1000], [undefined, 1000], ["High", 500], ["Full", 2000]]) {
    const { calls, io } = fixture([endpoint], { ...status, usbSpeed });
    await assert.rejects(automate({ brand: "Zaunkoenig", deviceId: "Zaunkoenig:M3K", action: "automatic-apply", identity: endpoint, dpi: 1600, pollingRateHz }, io), /USB/);
    assert.deepEqual(calls, ["close"]);
  }
});

test("manual adapter keeps its existing multi-path behavior without a live identity requirement", async () => {
  const calls = [];
  class FakeHID {
    constructor(path) { calls.push(["open", path]); }
    getDeviceInfo() { throw new Error("manual behavior must not gain automatic identity gating"); }
    on() {}
    removeAllListeners() {}
    close() { calls.push("close"); }
  }
  const device = new HidDeviceAdapter([endpoint, { ...endpoint, path: "manual-second" }], null, FakeHID);
  await device.open();
  assert.equal(device.opened, true);
  await device.close();
  assert.deepEqual(calls, [["open", endpoint.path], ["open", "manual-second"], "close", "close"]);
});
